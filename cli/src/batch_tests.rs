use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;

use chrono::{Duration, TimeZone, Utc};
use serde_json::{json, Value};
use ubu_core::{DeferPolicy, Provenance, Task, TaskStatus, Tier, TimeWindow};

struct StubTransport {
    replies: RefCell<VecDeque<Result<String, String>>>,
    prompts: RefCell<Vec<String>>,
}

impl StubTransport {
    fn new(replies: Vec<Result<String, String>>) -> Self {
        Self {
            replies: RefCell::new(replies.into()),
            prompts: RefCell::new(vec![]),
        }
    }
}

impl LlmTransport for StubTransport {
    fn generate(&self, prompt: &str) -> Result<String, String> {
        self.prompts.borrow_mut().push(prompt.to_owned());
        self.replies
            .borrow_mut()
            .pop_front()
            .expect("unexpected model call")
    }
}

fn id(n: u128) -> Id {
    Id::from_u128(n)
}

fn store(count: u128) -> Store {
    let mut store = Store::new();
    for n in 1..=count {
        store.upsert_task(Task {
            id: id(n),
            title: format!("Task {n}"),
            tier: Tier::UserShared,
            detail: None,
            category: None,
            tags: vec![],
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
        });
    }
    store
}

fn tag_reply(count: usize) -> Result<String, String> {
    Ok(
        json!({"tags": (1..=count).map(|n| json!({"task":n,"tag":"focus"})).collect::<Vec<_>>()})
            .to_string(),
    )
}

fn advice_reply() -> Result<String, String> {
    Ok(r#"{"dependencies":[{"blocked":2,"blocker":1}],"preferences":[]}"#.into())
}

#[test]
fn task_progress_is_visible_before_model_calls_with_operation_totals_and_chunk_ranges() {
    struct ProgressTransport {
        stub: StubTransport,
        output_at_calls: RefCell<Vec<String>>,
    }
    impl LlmTransport for ProgressTransport {
        fn generate(&self, prompt: &str) -> Result<String, String> {
            self.output_at_calls
                .borrow_mut()
                .push(crate::test_support::take_stdout());
            self.stub.generate(prompt)
        }
    }
    for op in ["clarify", "tags", "advise"] {
        crate::test_support::take_stdout();
        let mut store = store(4);
        store.tasks.get_mut(&id(4)).unwrap().status = TaskStatus::Done;
        store.tasks.get_mut(&id(1)).unwrap().detail = Some("First line\nSecond line".into());
        let replies = if op == "clarify" {
            for n in 1..=3 {
                crate::clarify::queue_clarification(&mut store, id(n)).unwrap();
            }
            store.clarify_sessions.get_mut(&id(2)).unwrap().round = 2;
            vec![Ok(r#"{"questions":[],"tags":[],"done":true}"#.into()); 3]
        } else if op == "tags" {
            vec![tag_reply(2), tag_reply(1)]
        } else {
            vec![Ok(r#"{"dependencies":[],"preferences":[]}"#.into()); 2]
        };
        let transport = ProgressTransport {
            stub: StubTransport::new(replies),
            output_at_calls: RefCell::new(vec![]),
        };
        assert_eq!(
            run_batch_operations(
                &mut store,
                &transport,
                &[op],
                3,
                2,
                0,
                5,
                15,
                &AtomicBool::new(false),
                &mut |_| Ok(())
            ),
            BatchOutcome::Completed
        );
        let output = transport.output_at_calls.borrow();
        let groups = if op == "clarify" {
            vec![vec![2], vec![1], vec![3]]
        } else {
            vec![vec![1, 2], vec![3]]
        };
        assert_eq!(output.len(), groups.len());
        let mut position = 0;
        for (text, group) in output.iter().zip(groups) {
            for n in group {
                position += 1;
                assert!(text.contains(&format!(
                    "batch {op}: Ollama processing task {position}/3: Task {n} ({})",
                    id(n)
                )));
            }
            assert!(!text.contains("Task 4"));
        }
        let combined = output.join("\n");
        assert!(combined.contains("detail: First line\nSecond line"));
        assert!(combined.contains("detail: (none)"));
        if op != "clarify" {
            assert!(output[0].contains(&format!(
                "batch {op}: Ollama processing tasks 1-2/3 together"
            )));
            assert!(!output[0].contains("task 3/3"));
            assert!(output[1].contains("task 3/3"));
        }
    }
}

fn rows(prompt: &str) -> Vec<Value> {
    prompt
        .lines()
        .filter_map(|line| {
            let (index, data) = line.strip_prefix('[')?.split_once("] ")?;
            index.parse::<usize>().ok()?;
            Some(serde_json::from_str(data).unwrap())
        })
        .collect()
}

#[test]
fn eligibility_respects_active_unpinned_tasks_per_op_caps_and_counts_generate_errors() {
    let mut store = store(8);
    store.batch_passes.insert(format!("{}|tags", id(1)), 2);
    store.batch_passes.insert(format!("{}|advise", id(1)), 3);
    store.batch_passes.insert(format!("{}|tags", id(2)), 3);
    store.tasks.get_mut(&id(3)).unwrap().status = TaskStatus::Done;
    let now = Utc.with_ymd_and_hms(2026, 9, 13, 12, 0, 0).unwrap();
    store.tasks.get_mut(&id(4)).unwrap().pinned = Some(TimeWindow {
        start: now,
        end: now + Duration::minutes(30),
    });
    store.tasks.get_mut(&id(5)).unwrap().detail = Some("Already clarified".into());
    store.tasks.get_mut(&id(6)).unwrap().status = TaskStatus::Scheduled;
    store.tasks.get_mut(&id(7)).unwrap().status = TaskStatus::Active;
    store.tasks.get_mut(&id(8)).unwrap().status = TaskStatus::Deferred;
    let transport = StubTransport::new(vec![Err("stub network failure".into()), tag_reply(1)]);
    let mut snapshots = vec![];
    assert_eq!(
        run_batch(
            &mut store,
            &transport,
            &["tags"],
            3,
            2,
            0,
            &AtomicBool::new(false),
            &mut |store| {
                snapshots.push(store.clone());
                Ok(())
            }
        ),
        BatchOutcome::Completed
    );
    assert_eq!(transport.prompts.borrow().len(), 2);
    assert_eq!(
        rows(&transport.prompts.borrow()[0])
            .iter()
            .map(|row| row["title"].as_str().unwrap())
            .collect::<Vec<_>>(),
        vec!["Task 1", "Task 5"]
    );
    for (n, passes) in [(1, 3), (2, 3), (5, 1), (6, 1)] {
        assert_eq!(store.batch_passes[&format!("{}|tags", id(n))], passes);
    }
    for n in [3, 4, 7, 8] {
        assert!(!store.batch_passes.contains_key(&format!("{}|tags", id(n))));
    }
    assert_eq!(store.batch_passes[&format!("{}|advise", id(1))], 3);
    assert_eq!(snapshots.len(), 2);
    assert!(snapshots[0].pending_decisions.is_empty());
    assert_eq!(store.pending_decisions.len(), 1);
    assert_eq!(
        store.pending_decisions[0].proposal,
        ubu_core::Proposal::Tag {
            task_id: id(6),
            tag: "focus".into()
        }
    );
}

#[test]
fn repeated_runs_bound_generate_and_parse_failures_by_the_cap() {
    let mut store = store(1);
    let transport = StubTransport::new(vec![Err("offline stub".into()), Ok("invalid JSON".into())]);
    let mut saves = 0;
    for expected_passes in [1, 2, 2] {
        assert_eq!(
            run_batch(
                &mut store,
                &transport,
                &["tags"],
                2,
                25,
                0,
                &AtomicBool::new(false),
                &mut |_| {
                    saves += 1;
                    Ok(())
                }
            ),
            BatchOutcome::Completed
        );
        assert_eq!(
            store.batch_passes[&format!("{}|tags", id(1))],
            expected_passes
        );
    }
    assert_eq!(transport.prompts.borrow().len(), 2);
    assert_eq!(saves, 2);
    assert!(store.pending_decisions.is_empty());
}

#[test]
fn default_operations_enqueue_all_batches_and_save_once_per_chunk() {
    let mut store = store(3);
    for task in store.tasks.values_mut() {
        task.detail = Some("Already clarified".into());
    }
    let transport = StubTransport::new(vec![
        tag_reply(2),
        tag_reply(1),
        advice_reply(),
        Ok(r#"{"dependencies":[],"preferences":[]}"#.into()),
    ]);
    let mut snapshots = vec![];
    assert_eq!(
        run_batch_operations(
            &mut store,
            &transport,
            crate::batch_operations(None),
            3,
            2,
            0,
            5,
            u32::MAX,
            &AtomicBool::new(false),
            &mut |store| {
                snapshots.push(store.clone());
                Ok(())
            }
        ),
        BatchOutcome::Completed
    );
    assert_eq!(transport.prompts.borrow().len(), 4);
    assert_eq!(snapshots.len(), 4);
    assert_eq!(
        snapshots
            .iter()
            .map(|s| s.batch_passes.len())
            .collect::<Vec<_>>(),
        vec![2, 3, 5, 6]
    );
    assert_eq!(
        snapshots
            .iter()
            .map(|s| s.pending_decisions.len())
            .collect::<Vec<_>>(),
        vec![2, 3, 4, 4]
    );
    for op in ["tags", "advise"] {
        for n in 1..=3 {
            assert_eq!(store.batch_passes[&format!("{}|{op}", id(n))], 1);
        }
    }
    assert_eq!(
        store.pending_decisions[3].proposal,
        ubu_core::Proposal::Dependency {
            blocked: id(2),
            blocker: id(1)
        }
    );
}

#[test]
fn only_operation_restricts_proposals_and_pass_counters() {
    for (choice, op, reply, queued) in [
        (crate::BatchOperation::Tags, "tags", tag_reply(2), 2),
        (crate::BatchOperation::Advise, "advise", advice_reply(), 1),
    ] {
        let mut store = store(2);
        let transport = StubTransport::new(vec![reply]);
        assert_eq!(
            run_batch(
                &mut store,
                &transport,
                crate::batch_operations(Some(choice)),
                3,
                25,
                0,
                &AtomicBool::new(false),
                &mut |_| Ok(())
            ),
            BatchOutcome::Completed
        );
        assert_eq!(transport.prompts.borrow().len(), 1);
        assert_eq!(store.pending_decisions.len(), queued);
        assert_eq!(store.batch_passes.len(), 2);
        assert!(store
            .batch_passes
            .keys()
            .all(|key| key.ends_with(&format!("|{op}"))));
    }
}

#[test]
fn interrupt_after_first_chunk_saves_progress_stops_and_resumes_from_saved_counters() {
    let mut store = store(3);
    let interrupted = AtomicBool::new(false);
    let transport = StubTransport::new(vec![tag_reply(2)]);
    let mut snapshots = vec![];
    assert_eq!(
        run_batch(
            &mut store,
            &transport,
            &["tags"],
            1,
            2,
            0,
            &interrupted,
            &mut |store| {
                snapshots.push(store.clone());
                interrupted.store(true, Ordering::SeqCst);
                Ok(())
            }
        ),
        BatchOutcome::Interrupted
    );
    assert_eq!(transport.prompts.borrow().len(), 1);
    assert_eq!(snapshots.len(), 2); // chunk save, then the requested interrupt save
    assert_eq!(snapshots[0].pending_decisions.len(), 2);
    assert_eq!(snapshots[0].batch_passes.len(), 2);
    assert_eq!(snapshots[0], snapshots[1]);

    let mut resumed = snapshots.pop().unwrap();
    let transport = StubTransport::new(vec![tag_reply(1)]);
    interrupted.store(false, Ordering::SeqCst);
    let mut saves = 0;
    assert_eq!(
        run_batch(
            &mut resumed,
            &transport,
            &["tags"],
            1,
            2,
            0,
            &interrupted,
            &mut |_| {
                saves += 1;
                Ok(())
            }
        ),
        BatchOutcome::Completed
    );
    assert_eq!(rows(&transport.prompts.borrow()[0])[0]["title"], "Task 3");
    assert_eq!(saves, 1);
    assert_eq!(resumed.pending_decisions.len(), 3);
    assert_eq!(resumed.batch_passes.len(), 3);
}

#[test]
fn interrupt_during_last_request_is_saved_and_never_returns_completed() {
    struct InterruptingTransport<'a>(&'a AtomicBool);
    impl LlmTransport for InterruptingTransport<'_> {
        fn generate(&self, _: &str) -> Result<String, String> {
            self.0.store(true, Ordering::SeqCst);
            tag_reply(1)
        }
    }
    let mut store = store(1);
    let interrupted = AtomicBool::new(false);
    let transport = InterruptingTransport(&interrupted);
    let mut snapshots = vec![];
    assert_eq!(
        run_batch(
            &mut store,
            &transport,
            &["tags"],
            3,
            25,
            0,
            &interrupted,
            &mut |store| {
                snapshots.push(store.clone());
                Ok(())
            }
        ),
        BatchOutcome::Interrupted
    );
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0].pending_decisions.len(), 1);
    assert_eq!(snapshots[0].batch_passes[&format!("{}|tags", id(1))], 1);
}

#[test]
fn already_interrupted_saves_even_when_no_work_remains() {
    for count in [0, 2] {
        let mut store = store(count);
        let before = store.clone();
        let transport = StubTransport::new(vec![]);
        let mut saves = 0;
        assert_eq!(
            run_batch(
                &mut store,
                &transport,
                &["tags"],
                0,
                25,
                0,
                &AtomicBool::new(true),
                &mut |saved| {
                    assert_eq!(saved, &before);
                    saves += 1;
                    Ok(())
                }
            ),
            BatchOutcome::Interrupted
        );
        assert_eq!(saves, 1);
        assert!(transport.prompts.borrow().is_empty());
    }
}

#[test]
fn setup_and_save_failures_are_fatal_without_running_further_chunks() {
    for (ops, size) in [(vec!["tags", "unknown"], 25), (vec!["tags"], 0)] {
        let mut store = store(2);
        let before = store.clone();
        let transport = StubTransport::new(vec![]);
        assert!(matches!(
            run_batch(
                &mut store,
                &transport,
                &ops,
                3,
                size,
                0,
                &AtomicBool::new(false),
                &mut |_| panic!("setup failure must not save")
            ),
            BatchOutcome::Failed(_)
        ));
        assert_eq!(store, before);
    }
    let mut store = store(3);
    let transport = StubTransport::new(vec![tag_reply(2)]);
    assert_eq!(
        run_batch(
            &mut store,
            &transport,
            &["tags"],
            3,
            2,
            0,
            &AtomicBool::new(false),
            &mut |_| Err("disk full".into())
        ),
        BatchOutcome::Failed("failed to save batch progress: disk full".into())
    );
    assert_eq!(transport.prompts.borrow().len(), 1);
    assert!(!store.batch_passes.contains_key(&format!("{}|tags", id(3))));
    let transport = StubTransport::new(vec![]);
    assert!(matches!(
        run_batch(
            &mut store,
            &transport,
            &["tags"],
            3,
            2,
            0,
            &AtomicBool::new(true),
            &mut |_| Err("disk full".into())
        ),
        BatchOutcome::Failed(_)
    ));
}

#[test]
fn category_order_history_and_batch_local_indices_are_preserved() {
    use ubu_core::{ActualStatus, FactKind, LogEntry, LogEntryKind};
    let mut store = store(3);
    store.tasks.get_mut(&id(1)).unwrap().category = Some("zeta".into());
    store.tasks.get_mut(&id(2)).unwrap().category = Some("alpha".into());
    let done = store.tasks.get_mut(&id(3)).unwrap();
    done.status = TaskStatus::Done;
    done.tags = vec!["history".into()];
    store.append_log(LogEntry {
        id: id(100),
        at: Utc.with_ymd_and_hms(2026, 9, 13, 12, 0, 0).unwrap(),
        kind: LogEntryKind::Fact(FactKind::Actual {
            item_id: id(3),
            status: ActualStatus::Done,
            actual: None,
        }),
    });
    let transport = StubTransport::new(vec![tag_reply(1), tag_reply(1)]);
    assert_eq!(
        run_batch(
            &mut store,
            &transport,
            &["tags"],
            3,
            1,
            1,
            &AtomicBool::new(false),
            &mut |_| Ok(())
        ),
        BatchOutcome::Completed
    );
    let prompts = transport.prompts.borrow();
    assert_eq!(rows(&prompts[0])[0]["title"], "Task 2");
    assert_eq!(rows(&prompts[1])[0]["title"], "Task 1");
    assert!(prompts.iter().all(|p| p.contains("- Task 3  →  [history]")));
}

#[test]
fn zero_cap_and_empty_store_make_no_calls_and_maximum_cap_does_not_overflow() {
    let transport = StubTransport::new(vec![]);
    for (mut store, cap) in [(store(0), 3), (store(2), 0)] {
        assert_eq!(
            run_batch(
                &mut store,
                &transport,
                &["tags", "advise"],
                cap,
                25,
                0,
                &AtomicBool::new(false),
                &mut |_| panic!("no chunks to save")
            ),
            BatchOutcome::Completed
        );
    }
    let mut store = store(1);
    store
        .batch_passes
        .insert(format!("{}|tags", id(1)), u32::MAX - 1);
    let transport = StubTransport::new(vec![tag_reply(1)]);
    assert_eq!(
        run_batch(
            &mut store,
            &transport,
            &["tags"],
            u32::MAX,
            25,
            0,
            &AtomicBool::new(false),
            &mut |_| Ok(())
        ),
        BatchOutcome::Completed
    );
    assert_eq!(store.batch_passes[&format!("{}|tags", id(1))], u32::MAX);
}

#[test]
fn outcome_exit_codes_only_allow_shutdown_after_completion() {
    assert_eq!(BatchOutcome::Completed.exit_code(), 0);
    assert_eq!(BatchOutcome::Interrupted.exit_code(), 130);
    assert_eq!(BatchOutcome::Failed("fatal".into()).exit_code(), 1);
}
