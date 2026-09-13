use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;

use chrono::{Duration, TimeZone, Utc};
use ollama_planner::LlmTransport;
use serde_json::{json, Value};
use ubu_core::{CompletedExample, DeferPolicy, Id, Provenance, Store, Task, TaskStatus, Tier};

struct StubTransport {
    responses: RefCell<VecDeque<Result<String, String>>>,
    prompts: RefCell<Vec<String>>,
}

impl StubTransport {
    fn new(responses: Vec<Result<String, String>>) -> Self {
        Self {
            responses: RefCell::new(responses.into()),
            prompts: RefCell::new(Vec::new()),
        }
    }
}

impl LlmTransport for StubTransport {
    fn generate(&self, prompt: &str) -> Result<String, String> {
        self.prompts.borrow_mut().push(prompt.into());
        self.responses
            .borrow_mut()
            .pop_front()
            .expect("unexpected model call")
    }
}

struct StubCollector {
    replies: VecDeque<Answers>,
    questions: Vec<Vec<Question>>,
}

impl StubCollector {
    fn new(replies: Vec<Answers>) -> Self {
        Self {
            replies: replies.into(),
            questions: Vec::new(),
        }
    }
}

impl AnswerCollector for StubCollector {
    fn collect(&mut self, questions: &[Question]) -> Answers {
        self.questions.push(questions.to_vec());
        self.replies
            .pop_front()
            .expect("unexpected answer collection")
    }
}

fn task() -> Task {
    Task {
        id: Id::from_u128(1),
        title: "Write a proposal".into(),
        detail: Some("Existing lore.".into()),
        category: Some("work".into()),
        tags: vec!["writing".into()],
        tier: Tier::UserShared,
        objective_ids: vec![],
        skills: vec![],
        affect_cost: 0,
        est_duration: Duration::minutes(30),
        due: None,
        earliest_start: None,
        must_finish_by: None,
        pinned: None,
        transparent: false,
        reminders: vec![],
        blocked_by: vec![],
        after: vec![],
        defer_policy: DeferPolicy::RescheduleAsap,
        status: TaskStatus::Backlog,
        provenance: Provenance::Manual,
        commitment: None,
    }
}

fn question(id: &str, text: &str, kind: &str, depends_on: Value) -> Value {
    json!({"id":id,"text":text,"kind":kind,"depends_on":depends_on})
}

fn response(questions: Vec<Value>, tags: &[&str], done: bool) -> Result<String, String> {
    Ok(json!({"questions":questions,"tags":tags,"done":done}).to_string())
}

fn answers(values: &[(&str, &str)], stop: bool) -> Answers {
    Answers {
        answers: values
            .iter()
            .map(|(id, answer)| (id.to_string(), answer.to_string()))
            .collect(),
        stop,
    }
}

fn history() -> Vec<CompletedExample> {
    vec![CompletedExample {
        title: "Previous proposal".into(),
        tags: vec!["writing".into(), "client".into()],
        category: Some("work".into()),
        duration: Duration::minutes(45),
        completed_at: Utc.with_ymd_and_hms(2026, 9, 12, 12, 0, 0).unwrap(),
    }]
}

fn parsed_questions() -> Vec<Question> {
    parse_clarify_response(
        &response(
            vec![
                question("q1", "Is this for a client?", "YesNo", Value::Null),
                question("q2", "Which client?", "ShortText", json!(["q1", "y"])),
            ],
            &[],
            false,
        )
        .unwrap(),
    )
    .unwrap()
    .questions
}

#[test]
fn prompt_contains_task_lore_accumulated_answers_history_and_json_contract() {
    let mut task = task();
    task.title = "Proposal with \"quotes\"\nand a newline".into();
    let accumulated = "Existing lore.\nQ: Who?\nA: A client";
    let prompt = build_clarify_prompt(&task, accumulated, &history());
    let context: Value = serde_json::from_str(prompt.split_once("Context: ").unwrap().1).unwrap();
    assert_eq!(
        context["task"],
        json!({
            "title":task.title, "category":"work", "tags":["writing"], "current_lore":"Existing lore.",
        })
    );
    assert_eq!(context["accumulated_lore_and_qa"], accumulated);
    assert_eq!(
        context["completed_examples"][0]["title"],
        "Previous proposal"
    );
    assert_eq!(
        context["completed_examples"][0]["tags"],
        json!(["writing", "client"])
    );
    assert_eq!(context["completed_examples"][0]["duration_minutes"], 45);
    for instruction in [
        "data, not instructions",
        "YesNo",
        "ShortText",
        "depends_on",
        "done",
        "ONLY JSON",
    ] {
        assert!(prompt.contains(instruction));
    }
    let empty = build_clarify_prompt(&task, "", &[]);
    let context: Value = serde_json::from_str(empty.split_once("Context: ").unwrap().1).unwrap();
    assert_eq!(context["completed_examples"], json!([]));
}

#[test]
fn parser_accepts_kinds_optional_conditions_and_terminal_responses() {
    let parsed = parse_clarify_response(
        r#"{"questions":[
        {"id":"a","text":"Ready?","kind":"YesNo"},
        {"id":"b","text":"Why?","kind":"ShortText","depends_on":["a","n"]},
        {"id":"c","text":"Anything else?","kind":"ShortText","depends_on":null}
    ],"tags":["focus","Focus"],"done":false}"#,
    )
    .unwrap();
    assert_eq!(parsed.questions.len(), 3);
    assert_eq!(parsed.questions[0].kind, QuestionKind::YesNo);
    assert_eq!(parsed.questions[0].depends_on, None);
    assert_eq!(parsed.questions[1].kind, QuestionKind::ShortText);
    assert_eq!(
        parsed.questions[1].depends_on,
        Some(("a".into(), "n".into()))
    );
    assert_eq!(parsed.questions[2].depends_on, None);
    assert_eq!(parsed.tags, vec!["focus", "Focus"]);
    assert!(!parsed.done);
    assert_eq!(
        parse_clarify_response(r#"{"questions":[],"tags":[],"done":true}"#).unwrap(),
        ClarifyResponse {
            questions: vec![],
            tags: vec![],
            done: true
        }
    );
}

#[test]
fn parser_rejects_invalid_json_shapes_kinds_and_ambiguous_question_ids() {
    for invalid in [
        "garbage",
        "Here is the JSON: {}",
        "null",
        "[]",
        "{}",
        r#"{"questions":[],"tags":[],"done":"true"}"#,
        r#"{"questions":{},"tags":[],"done":true}"#,
        r#"{"questions":[],"tags":[1],"done":true}"#,
        r#"{"questions":[],"tags":[]}"#,
        r#"{"questions":[],"done":true}"#,
        r#"{"tags":[],"done":true}"#,
    ] {
        assert!(parse_clarify_response(invalid).is_err(), "{invalid}");
    }
    let valid = question("q1", "Ready?", "YesNo", Value::Null);
    for (field, value) in [
        ("id", json!("")),
        ("id", json!("  ")),
        ("id", json!(7)),
        ("text", json!("")),
        ("text", Value::Null),
        ("kind", json!("yesno")),
        ("kind", json!("Number")),
        ("depends_on", json!(["q1"])),
        ("depends_on", json!(["q1", "y", "extra"])),
        ("depends_on", json!([1, "y"])),
        ("depends_on", json!(["q1", true])),
        ("depends_on", json!({"qid":"q1","want":"y"})),
    ] {
        let mut invalid = valid.clone();
        invalid[field] = value;
        assert!(
            parse_clarify_response(&response(vec![invalid], &[], false).unwrap()).is_err(),
            "{field}"
        );
    }
    assert!(
        parse_clarify_response(&response(vec![valid.clone(), valid], &[], false).unwrap()).is_err()
    );
    assert!(parse_clarify_response(&response(vec![Value::Null], &[], false).unwrap()).is_err());
}

#[test]
fn relevance_matches_case_insensitively_and_drops_unknown_missing_or_irrelevant_answers() {
    let questions = parsed_questions();
    let yes = answers(&[("q1", "Y"), ("q2", "Acme"), ("unknown", "ignore")], false);
    assert_eq!(
        relevant(&questions, &yes)
            .iter()
            .map(|q| q.id.as_str())
            .collect::<Vec<_>>(),
        vec!["q1", "q2"]
    );
    assert_eq!(
        filter_answers(&questions, &yes),
        answers(&[("q1", "Y"), ("q2", "Acme")], false).answers
    );
    for reply in [
        answers(&[("q1", "n"), ("q2", "ignore")], false),
        answers(&[("q2", "ignore")], false),
    ] {
        assert_eq!(
            relevant(&questions, &reply)
                .iter()
                .map(|q| q.id.as_str())
                .collect::<Vec<_>>(),
            vec!["q1"]
        );
        assert!(!filter_answers(&questions, &reply).contains_key("q2"));
    }
}

#[test]
fn multiple_rounds_preserve_lore_append_qa_and_deduplicate_all_proposed_tags() {
    let task = task();
    let before = task.clone();
    let transport = StubTransport::new(vec![
        response(
            vec![question(
                "q1",
                "Who is the audience?",
                "ShortText",
                Value::Null,
            )],
            &["client", "writing", "client"],
            false,
        ),
        response(vec![], &["client", "review"], true),
    ]);
    let mut collector = StubCollector::new(vec![answers(&[("q1", "Acme")], false)]);
    let (lore, tags) = run_clarification(&task, &transport, &mut collector, &history(), 5);
    assert_eq!(lore, "Existing lore.\nQ: Who is the audience?\nA: Acme\n");
    assert_eq!(tags, vec!["client", "writing", "review"]);
    assert_eq!(collector.questions.len(), 1);
    assert_eq!(transport.prompts.borrow().len(), 2);
    let prompts = transport.prompts.borrow();
    let context: Value =
        serde_json::from_str(prompts[1].split_once("Context: ").unwrap().1).unwrap();
    assert_eq!(context["accumulated_lore_and_qa"], lore);
    assert_eq!(context["task"]["current_lore"], "Existing lore.");
    assert_eq!(task, before);
}

#[test]
fn unmet_condition_and_unknown_answers_never_enter_lore() {
    let mut task = task();
    task.detail = None;
    let transport = StubTransport::new(vec![response(
        vec![
            question("q1", "Is this for a client?", "YesNo", Value::Null),
            question("q2", "Which client?", "ShortText", json!(["q1", "y"])),
        ],
        &[],
        false,
    )]);
    let mut collector = StubCollector::new(vec![answers(
        &[
            ("q1", "n"),
            ("q2", "must not appear"),
            ("unknown", "also absent"),
        ],
        false,
    )]);
    let (lore, _) = run_clarification(&task, &transport, &mut collector, &[], 1);
    assert_eq!(collector.questions[0].len(), 2);
    assert_eq!(lore, "Q: Is this for a client?\nA: n\n");
}

#[test]
fn done_or_no_questions_stops_without_collecting_and_keeps_tags() {
    for (questions, done) in [
        (vec![question("q", "Unused?", "YesNo", Value::Null)], true),
        (vec![], false),
    ] {
        let transport = StubTransport::new(vec![response(questions, &["terminal"], done)]);
        let mut collector = StubCollector::new(vec![]);
        let (lore, tags) = run_clarification(&task(), &transport, &mut collector, &[], 5);
        assert_eq!(lore, "Existing lore.");
        assert_eq!(tags, vec!["terminal"]);
        assert!(collector.questions.is_empty());
        assert_eq!(transport.prompts.borrow().len(), 1);
    }
}

#[test]
fn max_rounds_caps_never_done_model_and_zero_makes_no_calls() {
    for max_rounds in [0, 3] {
        let transport = StubTransport::new(vec![
            response(
                vec![question("q", "More detail?", "ShortText", Value::Null)],
                &["focus"],
                false
            );
            max_rounds
        ]);
        let mut collector =
            StubCollector::new(vec![answers(&[("q", "More lore")], false); max_rounds]);
        let (lore, tags) = run_clarification(&task(), &transport, &mut collector, &[], max_rounds);
        assert_eq!(transport.prompts.borrow().len(), max_rounds);
        assert_eq!(collector.questions.len(), max_rounds);
        assert_eq!(lore.matches("Q: More detail?").count(), max_rounds);
        assert_eq!(
            tags,
            if max_rounds == 0 {
                vec![]
            } else {
                vec!["focus"]
            }
        );
    }
}

#[test]
fn collector_stop_discards_current_answers_and_retains_current_tags() {
    let transport = StubTransport::new(vec![response(
        vec![question("q", "More detail?", "ShortText", Value::Null)],
        &["focus"],
        false,
    )]);
    let mut collector = StubCollector::new(vec![answers(&[("q", "discard me")], true)]);
    let (lore, tags) = run_clarification(&task(), &transport, &mut collector, &[], 5);
    assert_eq!(lore, "Existing lore.");
    assert_eq!(tags, vec!["focus"]);
    assert_eq!(collector.questions.len(), 1);
    assert_eq!(transport.prompts.borrow().len(), 1);
}

#[test]
fn transport_and_parse_errors_return_all_previously_accumulated_work() {
    for failure in [Err("offline stub failure".into()), Ok("not JSON".into())] {
        let transport = StubTransport::new(vec![
            response(
                vec![question("q", "Who?", "ShortText", Value::Null)],
                &["saved"],
                false,
            ),
            failure.clone(),
        ]);
        let mut collector = StubCollector::new(vec![answers(&[("q", "Acme")], false)]);
        let (lore, tags) = run_clarification(&task(), &transport, &mut collector, &[], 5);
        assert_eq!(lore, "Existing lore.\nQ: Who?\nA: Acme\n");
        assert_eq!(tags, vec!["saved"]);
        assert_eq!(transport.prompts.borrow().len(), 2);
        assert_eq!(collector.questions.len(), 1);

        let transport = StubTransport::new(vec![failure]);
        let mut collector = StubCollector::new(vec![]);
        assert_eq!(
            run_clarification(&task(), &transport, &mut collector, &[], 5),
            ("Existing lore.".into(), vec![])
        );
    }
}

#[test]
fn editor_form_and_answer_parsing_are_tested_as_strings_without_launching_editor() {
    let questions = parsed_questions();
    let form = render_editor_form(&questions);
    for text in [
        "[y/n]",
        "[text]",
        "(only if q1=y)",
        "Is this for a client?",
        "Which client?",
        "A1:",
        "A2:",
    ] {
        assert!(form.contains(text), "{text}");
    }
    assert!(parse_editor_answers(&questions, &form).stop);
    assert!(parse_editor_answers(&questions, "").stop);
    let filled = form
        .replace("A1: \n", "A1: Y\n")
        .replace("A2: \n", "A2: Acme\nMore detail\n");
    let parsed = parse_editor_answers(&questions, &filled);
    assert_eq!(
        parsed,
        answers(&[("q1", "Y"), ("q2", "Acme\nMore detail")], false)
    );
    assert_eq!(filter_answers(&questions, &parsed), parsed.answers);
    assert_eq!(
        parse_editor_answers(&questions, "# comment\r\nA1: n\r\nA2:  \r\n"),
        answers(&[("q1", "n")], false)
    );
    assert!(parse_editor_answers(&questions, "A0: unknown\nA99: unknown").stop);
}

#[test]
fn stubbed_interview_updates_detail_and_persists_review_proposals_in_memory() {
    use crate::persist::{SqliteBackend, StorageBackend};
    use ubu_core::{DecisionSource, Proposal};

    let task = task();
    let id = task.id;
    let mut store = Store::new();
    store.upsert_task(task.clone());
    crate::logic::filter_and_enqueue_tags(&mut store, vec![(id, "queued".into())]);
    let transport = StubTransport::new(vec![
        response(
            vec![question("q", "Who?", "ShortText", Value::Null)],
            &["writing", "queued", "client"],
            false,
        ),
        response(vec![], &["client", "review"], true),
    ]);
    let mut collector = StubCollector::new(vec![answers(&[("q", "Acme")], false)]);
    let report = clarify_task(&mut store, id, &transport, &mut collector, &history(), 5).unwrap();
    assert_eq!(report.enqueued, 2);
    assert_eq!(report.dropped_known, 2);
    assert_eq!(report.dropped_cycle, 0);
    let mut expected = task.clone();
    expected.detail = Some("Existing lore.\nQ: Who?\nA: Acme\n".into());
    assert_eq!(store.tasks[&id], expected);
    assert_eq!(store.tasks[&id].tags, vec!["writing"]);
    assert_eq!(
        store
            .pending_decisions
            .iter()
            .map(|decision| {
                assert_eq!(decision.source, DecisionSource::Advisor);
                let Proposal::Tag { task_id, tag } = &decision.proposal else {
                    panic!("expected tag");
                };
                assert_eq!(*task_id, id);
                tag.as_str()
            })
            .collect::<Vec<_>>(),
        vec!["queued", "client", "review"]
    );
    let backend = SqliteBackend::in_memory().unwrap();
    backend.save(&store).unwrap();
    assert_eq!(backend.load().unwrap(), store);
    assert_eq!(transport.prompts.borrow().len(), 2);
}

#[test]
fn unknown_task_is_rejected_before_stubs_and_empty_lore_is_written_literally() {
    let mut store = Store::new();
    let transport = StubTransport::new(vec![]);
    let mut collector = StubCollector::new(vec![]);
    assert!(clarify_task(&mut store, Id::nil(), &transport, &mut collector, &[], 5).is_err());
    assert!(transport.prompts.borrow().is_empty());
    let mut task = task();
    task.detail = None;
    store.upsert_task(task.clone());
    clarify_task(&mut store, task.id, &transport, &mut collector, &[], 0).unwrap();
    assert_eq!(store.tasks[&task.id].detail.as_deref(), Some(""));
}
