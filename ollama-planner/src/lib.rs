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

impl OllamaHttpTransport {
    fn request_body(&self, prompt: &str) -> serde_json::Value {
        json!({
            "model": self.model,
            "prompt": prompt,
            "stream": false,
            "format": "json",
            // API equivalent of `ollama run --think=false`. Thinking output is
            // already omitted by parse_generate_response (`--hidethinking`).
            "think": false,
        })
    }
}

#[derive(Deserialize)]
struct GenerateResponse {
    response: String,
    done: Option<bool>,
    done_reason: Option<String>,
    prompt_eval_count: Option<u64>,
    eval_count: Option<u64>,
    thinking: Option<String>,
}

fn parse_generate_response(body: &str, model: &str) -> Result<String, String> {
    let body: GenerateResponse = serde_json::from_str(body).map_err(|error| error.to_string())?;
    if body.response.trim().is_empty() {
        // Report completion metadata without exposing the prompt or thinking text.
        let diagnostics = json!({
            "model": model,
            "done": body.done,
            "done_reason": body.done_reason,
            "prompt_eval_count": body.prompt_eval_count,
            "eval_count": body.eval_count,
            "thinking_present": body.thinking.as_ref().is_some_and(|text| !text.trim().is_empty()),
        });
        return Err(format!(
            "Ollama returned an empty answer in the response field; completion diagnostics: {diagnostics}"
        ));
    }
    Ok(body.response)
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
        let body = self.request_body(prompt).to_string();
        let response = agent
            .post(&url)
            .set("Content-Type", "application/json")
            .send_string(&body)
            .map_err(|error| error.to_string())?;
        let body = response.into_string().map_err(|error| error.to_string())?;
        parse_generate_response(&body, &self.model)
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

#[cfg(test)]
mod tests {
    use chrono::{Duration as ChronoDuration, TimeZone, Utc};
    use serde_json::json;
    use ubu_core::{AffectBudget, Placeable, PlacementInput, PlacementOutput, Planner, TimeWindow};
    use uuid::Uuid;

    use super::*;

    #[derive(Clone)]
    struct StubTransport {
        response: Result<String, String>,
    }

    impl LlmTransport for StubTransport {
        fn generate(&self, _prompt: &str) -> Result<String, String> {
            self.response.clone()
        }
    }

    #[test]
    fn generation_request_disables_thinking_with_a_boolean() {
        let transport = OllamaHttpTransport {
            base_url: "http://unused.invalid".into(),
            model: "test-model".into(),
            timeout_secs: 300,
        };
        let body = transport.request_body("Return only JSON");
        assert_eq!(body["think"].as_bool(), Some(false));
        assert_eq!(body["stream"].as_bool(), Some(false));
        assert_eq!(body["format"], "json");
        assert!(body.get("hidethinking").is_none());
    }

    #[test]
    fn thinking_text_is_hidden_when_an_answer_is_present() {
        let answer = r#"{"dependencies":[],"preferences":[]}"#;
        let body = json!({
            "response": answer,
            "thinking": "private thinking text",
            "done": true,
            "done_reason": "stop",
        });
        assert_eq!(
            parse_generate_response(&body.to_string(), "test-model").unwrap(),
            answer
        );
    }

    #[test]
    fn empty_answers_report_completion_metadata_without_thinking_text() {
        for answer in ["", " \n\t"] {
            let body = json!({
                "response": answer,
                "done": true,
                "done_reason": "length",
                "prompt_eval_count": 120,
                "eval_count": 256,
                "thinking": "private thinking text",
            });
            let error = parse_generate_response(&body.to_string(), "test-model").unwrap_err();
            assert!(error.starts_with("Ollama returned an empty answer"));
            let (_, diagnostics) = error.split_once("completion diagnostics: ").unwrap();
            assert_eq!(
                serde_json::from_str::<serde_json::Value>(diagnostics).unwrap(),
                json!({
                    "model": "test-model",
                    "done": true,
                    "done_reason": "length",
                    "prompt_eval_count": 120,
                    "eval_count": 256,
                    "thinking_present": true,
                })
            );
            assert!(!error.contains("private thinking text"));
        }
    }

    #[test]
    fn empty_answer_without_optional_metadata_still_reports_clear_error() {
        let error = parse_generate_response(r#"{"response":""}"#, "test-model").unwrap_err();
        assert!(error.starts_with("Ollama returned an empty answer"));
        assert!(error.contains(r#""done_reason":null"#));
        assert!(error.contains(r#""thinking_present":false"#));
    }

    #[test]
    fn nonempty_answers_are_preserved_and_invalid_envelopes_still_fail() {
        for answer in [
            " {\"dependencies\":[],\"preferences\":[]}\n",
            "invalid advisor JSON",
        ] {
            let body = json!({"response": answer});
            assert_eq!(
                parse_generate_response(&body.to_string(), "test-model").unwrap(),
                answer
            );
        }
        for body in ["", "invalid JSON", "{}", r#"{"response":null}"#] {
            assert!(parse_generate_response(body, "test-model").is_err());
        }
    }

    #[test]
    fn valid_model_order_changes_placement_order_and_remains_feasible() {
        let input = independent_input();
        let first_id = input.items[0].task_id;
        let second_id = input.items[1].task_id;
        let planner = planner_returning(json!({ "order": [second_id, first_id] }).to_string());

        let output = planner.place(&input);

        assert_eq!(
            output
                .entries
                .iter()
                .map(|entry| entry.item)
                .collect::<Vec<_>>(),
            vec![second_id, first_id]
        );
        assert_ne!(output, deterministic_output(&input));
        assert!(output.conflicts.is_empty());
        assert_eq!(
            output.entries[0].window.start,
            input.items[1].earliest_floor
        );
        assert!(output.entries[0].window.end <= output.entries[1].window.start);
        assert_eq!(
            input.items.iter().map(|item| item.affect_cost).sum::<i32>(),
            10
        );
        assert_eq!(input.budget.cap, 10);
    }

    #[test]
    fn malformed_json_falls_back_to_deterministic_placement() {
        let input = independent_input();
        let planner = planner_returning("not json".to_string());

        assert_eq!(planner.place(&input), deterministic_output(&input));
    }

    #[test]
    fn order_missing_a_task_falls_back_to_deterministic_placement() {
        let input = independent_input();
        let planner = planner_returning(json!({ "order": [input.items[0].task_id] }).to_string());

        assert_eq!(planner.place(&input), deterministic_output(&input));
    }

    #[test]
    fn order_with_unknown_id_falls_back_to_deterministic_placement() {
        let input = independent_input();
        let planner =
            planner_returning(json!({ "order": [input.items[0].task_id, id(999)] }).to_string());

        assert_eq!(planner.place(&input), deterministic_output(&input));
    }

    #[test]
    fn order_violating_precedence_falls_back_to_deterministic_placement() {
        let input = precedence_input();
        let predecessor = input.items[0].task_id;
        let dependent = input.items[1].task_id;
        let planner = planner_returning(json!({ "order": [dependent, predecessor] }).to_string());

        assert_eq!(planner.place(&input), deterministic_output(&input));
    }

    #[test]
    fn transport_error_falls_back_to_deterministic_placement() {
        let input = independent_input();
        let planner = OllamaPlanner::new(StubTransport {
            response: Err("model unavailable".to_string()),
        });

        assert_eq!(planner.place(&input), deterministic_output(&input));
    }

    #[test]
    fn build_prompt_contains_ids_cap_and_dependent_precedence() {
        let input = precedence_input();

        let prompt = build_prompt(&input);

        for item in &input.items {
            assert!(prompt.contains(&item.task_id.to_string()));
        }
        assert!(prompt.contains("per-day affect cap is 10"));
        assert!(prompt.contains(&format!(
            "sched_predecessors=[\"{}\"]",
            input.items[0].task_id
        )));
    }

    #[test]
    fn parse_order_reads_uuid_array() {
        let expected = vec![id(2), id(1)];
        let text = json!({ "order": expected }).to_string();

        assert_eq!(parse_order(&text), Ok(expected));
    }

    #[test]
    fn is_valid_order_requires_an_exact_permutation() {
        let input = independent_input();
        let first = input.items[0].task_id;
        let second = input.items[1].task_id;

        assert!(is_valid_order(&input, &[second, first]));
        assert!(!is_valid_order(&input, &[first]));
        assert!(!is_valid_order(&input, &[first, first]));
        assert!(!is_valid_order(&input, &[first, id(999)]));
    }

    #[test]
    fn is_valid_order_requires_a_linear_extension() {
        let input = precedence_input();
        let predecessor = input.items[0].task_id;
        let dependent = input.items[1].task_id;

        assert!(is_valid_order(&input, &[predecessor, dependent]));
        assert!(!is_valid_order(&input, &[dependent, predecessor]));
    }

    #[test]
    fn reorder_permutes_items_and_preserves_all_input_fields() {
        let mut input = independent_input();
        input.fixed_occupied.push(TimeWindow {
            start: fixed_time() - ChronoDuration::hours(1),
            end: fixed_time(),
        });
        let expected_items = vec![input.items[1].clone(), input.items[0].clone()];
        let order = expected_items
            .iter()
            .map(|item| item.task_id)
            .collect::<Vec<_>>();

        let reordered = reorder(&input, &order);

        assert_eq!(reordered.items, expected_items);
        assert_eq!(reordered.fixed_occupied, input.fixed_occupied);
        assert_eq!(reordered.budget, input.budget);
    }

    fn planner_returning(response: String) -> OllamaPlanner<StubTransport> {
        OllamaPlanner::new(StubTransport {
            response: Ok(response),
        })
    }

    fn deterministic_output(input: &PlacementInput) -> PlacementOutput {
        ubu_core::DeterministicPlacer.place(input)
    }

    fn independent_input() -> PlacementInput {
        PlacementInput {
            items: vec![
                placeable(1, 60, 6, Vec::new()),
                placeable(2, 30, 4, Vec::new()),
            ],
            fixed_occupied: Vec::new(),
            budget: AffectBudget { cap: 10 },
        }
    }

    fn precedence_input() -> PlacementInput {
        PlacementInput {
            items: vec![
                placeable(1, 30, 5, Vec::new()),
                placeable(2, 30, 5, vec![id(1)]),
            ],
            fixed_occupied: Vec::new(),
            budget: AffectBudget { cap: 10 },
        }
    }

    fn placeable(
        value: u128,
        duration_minutes: i64,
        affect_cost: i32,
        sched_predecessors: Vec<Uuid>,
    ) -> Placeable {
        Placeable {
            task_id: id(value),
            duration: ChronoDuration::minutes(duration_minutes),
            affect_cost,
            earliest_floor: fixed_time(),
            due: None,
            sched_predecessors,
        }
    }

    fn fixed_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, 9, 0, 0).single().unwrap()
    }

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }
}
