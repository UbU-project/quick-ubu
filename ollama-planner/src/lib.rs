use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write;
use std::future::Future;
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
    pub total_timeout_secs: u64,
}

impl OllamaHttpTransport {
    fn request_body(&self, prompt: &str) -> serde_json::Value {
        json!({
            "model": self.model,
            "prompt": prompt,
            "stream": true,
            "format": "json",
            // API equivalent of `ollama run --think=false`. Thinking output is
            // already omitted from the assembled answer (`--hidethinking`).
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

#[cfg(test)]
fn parse_generate_response(body: &str, model: &str) -> Result<String, String> {
    let body: GenerateResponse = serde_json::from_str(body).map_err(|error| error.to_string())?;
    completed_answer(body, model)
}

fn completed_answer(body: GenerateResponse, model: &str) -> Result<String, String> {
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

trait GenerateStream {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String>;
}

impl GenerateStream for reqwest::Response {
    async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
        self.chunk()
            .await
            .map(|chunk| chunk.map(|bytes| bytes.to_vec()))
            .map_err(|error| format!("failed to read Ollama stream: {error}"))
    }
}

#[derive(Default)]
struct StreamAnswer {
    pending: Vec<u8>,
    text: String,
    thinking_present: bool,
    received_message: bool,
}

impl StreamAnswer {
    fn message(&mut self, line: &[u8], model: &str) -> Result<Option<String>, String> {
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(None);
        }
        let value: serde_json::Value = serde_json::from_slice(line)
            .map_err(|error| format!("invalid Ollama stream JSON: {error}"))?;
        if let Some(error) = value.get("error").and_then(|value| value.as_str()) {
            return Err(format!("Ollama stream error: {error}"));
        }
        let mut message: GenerateResponse = serde_json::from_value(value)
            .map_err(|error| format!("invalid Ollama stream message: {error}"))?;
        self.received_message = true;
        self.text.push_str(&message.response);
        self.thinking_present |= message
            .thinking
            .as_ref()
            .is_some_and(|text| !text.trim().is_empty());
        if message.done == Some(true) {
            message.response = std::mem::take(&mut self.text);
            // Preserve presence across chunks without retaining thinking content.
            message.thinking = self.thinking_present.then(|| "present".to_string());
            return completed_answer(message, model).map(Some);
        }
        Ok(None)
    }

    fn push(&mut self, bytes: &[u8], model: &str) -> Result<Option<String>, String> {
        self.pending.extend_from_slice(bytes);
        while let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line: Vec<_> = self.pending.drain(..=end).collect();
            if let Some(answer) = self.message(&line, model)? {
                return Ok(Some(answer));
            }
        }
        Ok(None)
    }
}

async fn generate_with_deadlines<S: GenerateStream>(
    open: impl Future<Output = Result<S, String>>,
    model: &str,
    first_timeout: Duration,
    total_timeout: Duration,
) -> Result<String, String> {
    let start = tokio::time::Instant::now();
    let first_deadline = start
        .checked_add(first_timeout)
        .ok_or_else(|| "Ollama first-response timeout is out of range".to_string())?;
    let total_deadline = start
        .checked_add(total_timeout)
        .ok_or_else(|| "Ollama total timeout is out of range".to_string())?;
    let first_error = || {
        format!(
            "Ollama first-response timeout after {}s: no complete stream message received",
            first_timeout.as_secs_f64()
        )
    };
    let total_error = || {
        format!(
            "Ollama total execution timeout after {}s; partial answer discarded",
            total_timeout.as_secs_f64()
        )
    };
    let run = async {
        let mut stream = tokio::time::timeout_at(first_deadline, open)
            .await
            .map_err(|_| first_error())??;
        let mut answer = StreamAnswer::default();
        loop {
            if tokio::time::Instant::now() >= total_deadline {
                return Err(total_error());
            }
            if !answer.received_message && tokio::time::Instant::now() >= first_deadline {
                return Err(first_error());
            }
            let chunk = if answer.received_message {
                stream.next_chunk().await?
            } else {
                tokio::time::timeout_at(first_deadline, stream.next_chunk())
                    .await
                    .map_err(|_| first_error())??
            };
            match chunk {
                Some(bytes) => {
                    if let Some(text) = answer.push(&bytes, model)? {
                        return Ok(text);
                    }
                }
                None => {
                    let last = std::mem::take(&mut answer.pending);
                    if let Some(text) = answer.message(&last, model)? {
                        return Ok(text);
                    }
                    return Err(
                        "Ollama stream ended before done=true; partial answer discarded"
                            .to_string(),
                    );
                }
            }
        }
    };
    tokio::time::timeout_at(total_deadline, run)
        .await
        .map_err(|_| total_error())?
}

#[derive(Deserialize)]
struct OrderResponse {
    order: Vec<uuid::Uuid>,
}

impl LlmTransport for OllamaHttpTransport {
    fn generate(&self, prompt: &str) -> Result<String, String> {
        let url = format!("{}/api/generate", self.base_url.trim_end_matches('/'));
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .map_err(|error| format!("failed to start Ollama runtime: {error}"))?;
        runtime.block_on(async {
            let client = reqwest::Client::builder()
                .build()
                .map_err(|error| format!("failed to initialize Ollama client: {error}"))?;
            let open = async {
                client
                    .post(url)
                    .json(&self.request_body(prompt))
                    .send()
                    .await
                    .map_err(|error| format!("failed to connect to Ollama: {error}"))?
                    .error_for_status()
                    .map_err(|error| format!("Ollama HTTP error: {error}"))
            };
            generate_with_deadlines(
                open,
                &self.model,
                Duration::from_secs(self.timeout_secs),
                Duration::from_secs(self.total_timeout_secs),
            )
            .await
        })
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

    struct FakeStream {
        chunks: std::collections::VecDeque<(u64, Vec<u8>)>,
        dropped: std::sync::Arc<std::sync::atomic::AtomicBool>,
    }

    impl FakeStream {
        fn new(chunks: impl IntoIterator<Item = (u64, Vec<u8>)>) -> Self {
            Self {
                chunks: chunks.into_iter().collect(),
                dropped: Default::default(),
            }
        }
    }

    impl Drop for FakeStream {
        fn drop(&mut self) {
            self.dropped
                .store(true, std::sync::atomic::Ordering::SeqCst);
        }
    }

    impl GenerateStream for FakeStream {
        async fn next_chunk(&mut self) -> Result<Option<Vec<u8>>, String> {
            if let Some((delay, bytes)) = self.chunks.pop_front() {
                tokio::time::sleep(Duration::from_secs(delay)).await;
                Ok(Some(bytes))
            } else {
                Ok(None)
            }
        }
    }

    fn record(response: &str, done: bool) -> Vec<u8> {
        format!("{}\n", json!({"response": response, "done": done})).into_bytes()
    }

    async fn stream_answer(stream: FakeStream) -> Result<String, String> {
        generate_with_deadlines(
            async { Ok(stream) },
            "test-model",
            Duration::from_secs(300),
            Duration::from_secs(900),
        )
        .await
    }

    #[tokio::test(start_paused = true)]
    async fn streaming_assembles_split_utf8_lines_and_final_fragment() {
        let mut bytes = record("{\"name\":\"café", false);
        bytes.extend(record("\"}", true));
        let split = bytes.iter().position(|b| *b == 0xc3).unwrap() + 1;
        let chunks = vec![(0, bytes[..split].to_vec()), (0, bytes[split..].to_vec())];
        assert_eq!(
            stream_answer(FakeStream::new(chunks)).await.unwrap(),
            "{\"name\":\"café\"}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn final_record_without_newline_is_accepted_at_eof() {
        let mut bytes = record("{}", true);
        bytes.pop();
        assert_eq!(
            stream_answer(FakeStream::new([(0, bytes)])).await.unwrap(),
            "{}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn thinking_presence_survives_until_empty_completion_diagnostics() {
        let thinking = format!(
            "{}\n",
            json!({"response":"", "thinking":"private thoughts", "done":false})
        )
        .into_bytes();
        let completion = format!(
            "{}\n",
            json!({"response":"", "done":true, "done_reason":"stop", "eval_count":30})
        )
        .into_bytes();
        let error = stream_answer(FakeStream::new([(0, thinking.clone()), (0, completion)]))
            .await
            .unwrap_err();
        assert!(error.contains("empty answer"));
        assert!(error.contains("\"thinking_present\":true"));
        assert!(error.contains("\"eval_count\":30"));
        assert!(!error.contains("private thoughts"));
        assert_eq!(
            stream_answer(FakeStream::new([(0, thinking), (0, record("{}", true))]))
                .await
                .unwrap(),
            "{}"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn invalid_or_incomplete_streams_discard_partial_answers() {
        for (bytes, expected) in [
            (b"not JSON\n".to_vec(), "invalid Ollama stream JSON"),
            (b"{}\n".to_vec(), "invalid Ollama stream message"),
            (
                b"{\"error\":\"model failed\"}\n".to_vec(),
                "Ollama stream error: model failed",
            ),
            (record("{}", false), "ended before done=true"),
            (Vec::new(), "ended before done=true"),
        ] {
            let error = stream_answer(FakeStream::new([(0, bytes)]))
                .await
                .unwrap_err();
            assert!(error.contains(expected), "{error}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn first_response_deadline_includes_connection_and_partial_records() {
        let start = tokio::time::Instant::now();
        let open = async {
            tokio::time::sleep(Duration::from_secs(200)).await;
            Ok(FakeStream::new([
                (50, b"\n{\"response\":".to_vec()),
                (60, b"\"{}\",\"done\":true}\n".to_vec()),
            ]))
        };
        let error = generate_with_deadlines(
            open,
            "test-model",
            Duration::from_secs(300),
            Duration::from_secs(900),
        )
        .await
        .unwrap_err();
        assert!(
            error.contains("first-response timeout after 300s"),
            "{error}"
        );
        assert_eq!(start.elapsed(), Duration::from_secs(300));
    }

    #[tokio::test(start_paused = true)]
    async fn deadlines_apply_while_waiting_for_http_headers() {
        for (first, total, expected, elapsed) in [
            (300, 900, "first-response timeout", 300),
            (300, 200, "total execution timeout", 200),
        ] {
            let start = tokio::time::Instant::now();
            let open = async {
                tokio::time::sleep(Duration::from_secs(700)).await;
                Ok(FakeStream::new([]))
            };
            let error = generate_with_deadlines(
                open,
                "test-model",
                Duration::from_secs(first),
                Duration::from_secs(total),
            )
            .await
            .unwrap_err();
            assert!(error.contains(expected), "{error}");
            assert_eq!(start.elapsed(), Duration::from_secs(elapsed));
        }
    }

    #[tokio::test(start_paused = true)]
    async fn generation_can_continue_past_first_response_deadline() {
        let start = tokio::time::Instant::now();
        let stream = FakeStream::new([(200, record("{", false)), (350, record("}", true))]);
        assert_eq!(stream_answer(stream).await.unwrap(), "{}");
        assert_eq!(start.elapsed(), Duration::from_secs(550));
    }

    #[tokio::test(start_paused = true)]
    async fn total_deadline_cancels_stalled_and_progressing_streams() {
        for chunks in [
            vec![(100, record("{", false)), (501, record("}", true))],
            vec![
                (200, record("{", false)),
                (200, record(" ", false)),
                (201, record("}", true)),
            ],
        ] {
            let start = tokio::time::Instant::now();
            let stream = FakeStream::new(chunks);
            let dropped = stream.dropped.clone();
            let error = stream_answer(stream).await.unwrap_err();
            assert!(
                error.contains("total execution timeout after 900s; partial answer discarded"),
                "{error}"
            );
            assert_eq!(start.elapsed(), Duration::from_secs(900));
            assert!(dropped.load(std::sync::atomic::Ordering::SeqCst));
        }
    }

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
            total_timeout_secs: 900,
        };
        let body = transport.request_body("Return only JSON");
        assert_eq!(body["think"].as_bool(), Some(false));
        assert_eq!(body["stream"].as_bool(), Some(true));
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
            must_finish_by: None,
            earliest_floor: fixed_time(),
            due: None,
            sched_predecessors,
            after_refs: Vec::new(),
        }
    }

    fn fixed_time() -> chrono::DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, 9, 0, 0).single().unwrap()
    }

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }
}
