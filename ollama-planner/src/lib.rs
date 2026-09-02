use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::time::Duration;

use serde::Deserialize;
use serde_json::json;

pub trait LlmTransport {
    /// Returns the model's raw text response, or an error string.
    fn generate(&self, prompt: &str) -> Result<String, String>;
}

pub struct OllamaHttpTransport {
    pub base_url: String,
    pub model: String,
    pub timeout_secs: u64,
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
}

#[derive(Deserialize)]
struct OrderResponse {
    order: Vec<uuid::Uuid>,
}

impl LlmTransport for OllamaHttpTransport {
    fn generate(&self, prompt: &str) -> Result<String, String> {
        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));
        let agent = ureq::AgentBuilder::new()
            .timeout(Duration::from_secs(self.timeout_secs))
            .build();
        let body = json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
            "format": "json",
        })
        .to_string();
        let response = agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_string(&body)
            .map_err(|error| error.to_string())?;
        let body = response.into_string().map_err(|error| error.to_string())?;
        serde_json::from_str::<GenerateResponse>(&body)
            .map(|body| body.response)
            .map_err(|error| error.to_string())
    }
}

pub struct OllamaPlanner<T: LlmTransport> {
    transport: T,
    fallback: ubu_core::DeterministicPlacer,
}

impl<T: LlmTransport> OllamaPlanner<T> {
    pub fn new(transport: T) -> Self {
        Self {
            transport,
            fallback: ubu_core::DeterministicPlacer,
        }
    }
}

impl<T: LlmTransport> ubu_core::Planner for OllamaPlanner<T> {
    fn place(&self, input: &ubu_core::PlacementInput) -> ubu_core::PlacementOutput {
        let prompt = build_prompt(input);
        let text = match self.transport.generate(&prompt) {
            Ok(text) => text,
            Err(_) => return ubu_core::Planner::place(&self.fallback, input),
        };
        let order = match parse_order(&text) {
            Ok(order) => order,
            Err(_) => return ubu_core::Planner::place(&self.fallback, input),
        };
        if !is_valid_order(input, &order) {
            return ubu_core::Planner::place(&self.fallback, input);
        }

        let reordered = reorder(input, &order);
        ubu_core::Planner::place(&self.fallback, &reordered)
    }
}

pub fn build_prompt(input: &ubu_core::PlacementInput) -> String {
    let mut prompt = String::new();
    writeln!(
        prompt,
        "Order all schedulable tasks for deterministic placement. The scheduling horizon is already baked into each item's earliest_floor. The per-day affect cap is {}.",
        input.budget.cap
    )
    .expect("writing to a String cannot fail");
    writeln!(prompt, "Tasks:").expect("writing to a String cannot fail");

    for item in &input.items {
        let predecessors = serde_json::to_string(&item.sched_predecessors)
            .expect("UUID lists always serialize to JSON");
        let due = item
            .due
            .map(|due| due.to_rfc3339())
            .unwrap_or_else(|| "null".to_string());
        writeln!(
            prompt,
            "- task_id={} duration_minutes={} affect_cost={} earliest_floor={} due={} sched_predecessors={}",
            item.task_id,
            item.duration.num_minutes(),
            item.affect_cost,
            item.earliest_floor.to_rfc3339(),
            due,
            predecessors
        )
        .expect("writing to a String cannot fail");
    }

    prompt.push_str(
        "Return a JSON object {\"order\":[\"<task-id>\",...]} listing every task id exactly once. Respect precedence: place each task after all of its predecessors. Pace affect by interleaving restorative (negative affect_cost) items among draining ones, and do not front-load all high-drain work. Prefer earlier positions for items that must meet a due. Emit ONLY the JSON object.",
    );
    prompt
}

pub fn parse_order(text: &str) -> Result<Vec<uuid::Uuid>, String> {
    serde_json::from_str::<OrderResponse>(text)
        .map(|response| response.order)
        .map_err(|error| error.to_string())
}

pub fn is_valid_order(input: &ubu_core::PlacementInput, order: &[uuid::Uuid]) -> bool {
    if order.len() != input.items.len() {
        return false;
    }

    let item_ids: BTreeSet<_> = input.items.iter().map(|item| item.task_id).collect();
    if item_ids.len() != input.items.len() {
        return false;
    }

    let mut positions = BTreeMap::new();
    for (position, task_id) in order.iter().copied().enumerate() {
        if !item_ids.contains(&task_id) || positions.insert(task_id, position).is_some() {
            return false;
        }
    }

    input.items.iter().all(|item| {
        item.sched_predecessors
            .iter()
            .filter(|predecessor| item_ids.contains(predecessor))
            .all(|predecessor| positions[predecessor] < positions[&item.task_id])
    })
}

/// Permute `input.items` according to an order previously accepted by
/// [`is_valid_order`].
pub fn reorder(input: &ubu_core::PlacementInput, order: &[uuid::Uuid]) -> ubu_core::PlacementInput {
    let items_by_id: BTreeMap<_, _> = input
        .items
        .iter()
        .map(|item| (item.task_id, item))
        .collect();
    let items = order
        .iter()
        .map(|task_id| {
            (*items_by_id
                .get(task_id)
                .expect("order must be validated before reordering"))
            .clone()
        })
        .collect();

    ubu_core::PlacementInput {
        items,
        fixed_occupied: input.fixed_occupied.clone(),
        budget: input.budget.clone(),
    }
}
