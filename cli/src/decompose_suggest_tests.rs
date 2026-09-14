use super::*;
use crate::batch::{run_batch_operations, BatchOutcome};
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};

struct ScriptModel {
    replies: RefCell<VecDeque<Result<String, String>>>,
    prompts: RefCell<Vec<String>>,
}
impl ScriptModel {
    fn new(replies: Vec<Result<String, String>>) -> Self {
        Self {
            replies: RefCell::new(replies.into()),
            prompts: RefCell::new(vec![]),
        }
    }
}
impl LlmTransport for ScriptModel {
    fn generate(&self, prompt: &str) -> Result<String, String> {
        self.prompts.borrow_mut().push(prompt.into());
        self.replies
            .borrow_mut()
            .pop_front()
            .expect("unexpected model request")
    }
}
fn id(n: u128) -> Id {
    Id::from_u128(n)
}
fn pass(n: u128) -> String {
    format!("{}|decompose", id(n))
}
fn active_store(count: u128) -> Store {
    let mut store = Store::new();
    for n in 1..=count {
        let mut task = parent();
        task.id = id(n);
        task.title = format!("Task {n}");
        task.status = TaskStatus::Backlog;
        task.pinned = None;
        task.est_duration = Duration::minutes(30);
        store.upsert_task(task);
    }
    store
}
fn decline() -> Result<String, String> {
    Ok(r#"{"subtasks":[]}"#.into())
}
fn propose() -> Result<String, String> {
    model().reply
}

#[test]
fn suggestion_prompt_allows_declining_without_losing_parent_and_history_context() {
    let history = [CompletedExample {
        title: "Past task".into(),
        tags: vec!["focus".into()],
        category: None,
        duration: Duration::minutes(20),
        completed_at: at(),
    }];
    let prompt = build_decompose_suggest_prompt(&parent(), &history);
    assert!(prompt.contains("complex enough"));
    assert!(prompt.contains(r#"{"subtasks":[]}"#));
    let context: Value = serde_json::from_str(prompt.split_once("Context: ").unwrap().1).unwrap();
    assert_eq!(context["task"]["detail"], parent().detail.unwrap());
    assert_eq!(context["completed_examples"][0]["title"], "Past task");
    assert!(parse_decompose_response(&decline().unwrap())
        .unwrap()
        .is_empty());
    let proposal = parse_decompose_response(&propose().unwrap()).unwrap();
    assert_eq!(proposal[0].duration_minutes, 1);
    assert!(proposal[0].clamped);
    // The shared parser accepts declines, but fresh destructive decomposition does not.
    let mut store = active_store(1);
    let mut reviewer = Reviewer {
        result: Some(edited()),
        seen: vec![],
    };
    assert!(decompose_task(
        &mut store,
        id(1),
        &ScriptModel::new(vec![decline()]),
        &mut reviewer,
        0,
        at(),
        &mut |_| panic!("must not save")
    )
    .is_err());
    assert!(reviewer.seen.is_empty());
}

#[test]
fn eligibility_applies_minimum_pass_cap_pending_state_status_and_pinning_without_committing() {
    let mut store = active_store(9);
    store.tasks.get_mut(&id(1)).unwrap().est_duration = Duration::seconds(899);
    store.tasks.get_mut(&id(2)).unwrap().est_duration = Duration::minutes(15);
    store.batch_passes.insert(pass(3), 3);
    store.pending_decompositions.insert(id(4), edited());
    store.tasks.get_mut(&id(5)).unwrap().status = TaskStatus::Done;
    store.tasks.get_mut(&id(6)).unwrap().status = TaskStatus::Active;
    store.tasks.get_mut(&id(7)).unwrap().status = TaskStatus::Deferred;
    store.tasks.get_mut(&id(8)).unwrap().pinned = parent().pinned;
    store.tasks.get_mut(&id(9)).unwrap().status = TaskStatus::Scheduled;
    store.tasks.get_mut(&id(9)).unwrap().detail = None; // No extra clarification gate.
    let before = store.clone();
    let model = ScriptModel::new(vec![propose(), decline()]);
    let backend = SqliteBackend::in_memory().unwrap();
    let mut snapshots = vec![];
    assert_eq!(
        run_decompose_suggest_batch(
            &mut store,
            &model,
            3,
            15,
            0,
            &AtomicBool::new(false),
            &mut |s| {
                snapshots.push(s.clone());
                backend.save(s)
            }
        ),
        BatchOutcome::Completed
    );
    assert_eq!(snapshots.len(), 2);
    assert_eq!(backend.load().unwrap(), store);
    let titles: Vec<_> = model
        .prompts
        .borrow()
        .iter()
        .map(|p| {
            let context: Value =
                serde_json::from_str(p.split_once("Context: ").unwrap().1).unwrap();
            context["task"]["title"].as_str().unwrap().to_owned()
        })
        .collect();
    assert_eq!(titles, ["Task 2", "Task 9"]);
    let expected_proposal = parse_decompose_response(&propose().unwrap()).unwrap();
    assert_eq!(store.pending_decompositions[&id(2)], expected_proposal);
    let mut expected = before;
    expected
        .pending_decompositions
        .insert(id(2), expected_proposal);
    expected.batch_passes.insert(pass(2), 1);
    expected.batch_passes.insert(pass(9), 1);
    assert_eq!(store, expected); // No task, history, rewire or calendar mutations.
}

#[test]
fn declines_and_errors_count_attempts_and_stop_at_cap() {
    let mut store = active_store(3);
    let before = store.clone();
    let model = ScriptModel::new(vec![
        decline(),
        Err("offline".into()),
        Ok("bad JSON".into()),
    ]);
    let mut saves = 0;
    for _ in 0..2 {
        assert_eq!(
            run_decompose_suggest_batch(
                &mut store,
                &model,
                1,
                15,
                0,
                &AtomicBool::new(false),
                &mut |_| {
                    saves += 1;
                    Ok(())
                }
            ),
            BatchOutcome::Completed
        );
    }
    assert_eq!(saves, 3);
    assert_eq!(model.prompts.borrow().len(), 3);
    let mut expected = before;
    for n in 1..=3 {
        expected.batch_passes.insert(pass(n), 1);
    }
    assert_eq!(store, expected);
}

#[test]
fn interrupt_saves_first_suggestion_and_resume_skips_it() {
    let mut store = active_store(3);
    let before = store.clone();
    let flag = AtomicBool::new(false);
    let backend = SqliteBackend::in_memory().unwrap();
    let mut saves = 0;
    assert_eq!(
        run_decompose_suggest_batch(
            &mut store,
            &ScriptModel::new(vec![propose()]),
            3,
            15,
            0,
            &flag,
            &mut |s| {
                saves += 1;
                backend.save(s)?;
                flag.store(true, Ordering::SeqCst);
                Ok(())
            }
        ),
        BatchOutcome::Interrupted
    );
    assert_eq!(saves, 2); // Per-task save plus the interrupt boundary.
    store = backend.load().unwrap();
    assert_eq!(store.tasks, before.tasks);
    assert_eq!(store.pending_decompositions.len(), 1);
    assert_eq!(store.batch_passes.len(), 1);
    assert_eq!(store.batch_passes[&pass(1)], 1);
    flag.store(false, Ordering::SeqCst);
    let model = ScriptModel::new(vec![propose(), decline()]);
    assert_eq!(
        run_decompose_suggest_batch(&mut store, &model, 3, 15, 0, &flag, &mut |s| backend
            .save(s)),
        BatchOutcome::Completed
    );
    assert_eq!(model.prompts.borrow().len(), 2);
    assert_eq!(store.batch_passes[&pass(1)], 1);
}

#[test]
fn initial_and_final_interrupts_and_save_failure_stop_generation() {
    let mut store = active_store(1);
    let before = store.clone();
    let mut saves = 0;
    assert_eq!(
        run_decompose_suggest_batch(
            &mut store,
            &ScriptModel::new(vec![]),
            3,
            15,
            0,
            &AtomicBool::new(true),
            &mut |_| {
                saves += 1;
                Ok(())
            }
        ),
        BatchOutcome::Interrupted
    );
    assert_eq!(saves, 1);
    assert_eq!(store, before);
    let flag = AtomicBool::new(false);
    assert_eq!(
        run_decompose_suggest_batch(
            &mut store,
            &ScriptModel::new(vec![decline()]),
            3,
            15,
            0,
            &flag,
            &mut |_| {
                flag.store(true, Ordering::SeqCst);
                Ok(())
            }
        ),
        BatchOutcome::Interrupted
    );
    let mut store = active_store(2);
    assert!(matches!(
        run_decompose_suggest_batch(
            &mut store,
            &ScriptModel::new(vec![propose()]),
            3,
            15,
            0,
            &AtomicBool::new(false),
            &mut |_| Err("disk full".into())
        ),
        BatchOutcome::Failed(_)
    ));
    assert!(!store.batch_passes.contains_key(&pass(2)));
}

#[test]
fn default_batch_orders_tags_advise_clarify_decompose_and_only_decompose_is_isolated() {
    for only in [None, Some(crate::BatchOperation::Decompose)] {
        let mut store = active_store(1);
        crate::clarify::queue_clarification(&mut store, id(1)).unwrap();
        let original = store.tasks.clone();
        let default = only.is_none();
        let mut replies = vec![];
        if default {
            replies.extend([
                Ok(r#"{"tags":[]}"#.into()),
                Ok(r#"{"dependencies":[],"preferences":[]}"#.into()),
                Ok(r#"{"questions":[],"tags":[],"done":true}"#.into()),
            ]);
        }
        replies.push(propose());
        let model = ScriptModel::new(replies);
        let mut snapshots = vec![];
        assert_eq!(
            run_batch_operations(
                &mut store,
                &model,
                crate::batch_operations(only),
                3,
                25,
                0,
                5,
                15,
                &AtomicBool::new(false),
                &mut |s| {
                    snapshots.push(s.clone());
                    Ok(())
                }
            ),
            BatchOutcome::Completed
        );
        assert_eq!(model.prompts.borrow().len(), if default { 4 } else { 1 });
        assert!(model
            .prompts
            .borrow()
            .last()
            .unwrap()
            .starts_with("First judge whether"));
        assert_eq!(store.pending_decompositions.len(), 1);
        assert_eq!(store.tasks, original);
        assert!(store.decomposition_history.is_empty());
        if default {
            assert_eq!(
                snapshots[0].batch_passes.keys().collect::<Vec<_>>(),
                vec![&format!("{}|tags", id(1))]
            );
            assert!(snapshots[1]
                .batch_passes
                .contains_key(&format!("{}|advise", id(1))));
            assert!(snapshots[2].clarify_sessions.is_empty());
            assert!(snapshots[..3]
                .iter()
                .all(|s| s.pending_decompositions.is_empty()));
        } else {
            assert_eq!(store.batch_passes.len(), 1);
            assert_eq!(store.clarify_sessions.len(), 1);
        }
    }
}

#[test]
fn stored_review_uses_saved_chain_and_consumes_only_after_successful_commit() {
    for abort in [true, false] {
        let mut store = store();
        let proposal = parse_decompose_response(&propose().unwrap()).unwrap();
        store.pending_decompositions.insert(id(1), proposal.clone());
        store
            .pending_decompositions
            .insert(id(999), proposal.clone());
        let before = store.clone();
        let backend = SqliteBackend::in_memory().unwrap();
        backend.save(&store).unwrap();
        let mut reviewer = Reviewer {
            result: if abort { None } else { Some(edited()) },
            seen: vec![],
        };
        let mut saves = 0;
        let result =
            review_pending_decomposition(&mut store, id(1), &mut reviewer, at(), &mut |s| {
                saves += 1;
                backend.save(s)
            })
            .unwrap();
        assert_eq!(reviewer.seen, vec![proposal]);
        assert_eq!(saves, usize::from(!abort));
        assert_eq!(backend.load().unwrap(), store);
        if abort {
            assert_eq!(store, before);
            assert_eq!(result, None);
        } else {
            assert!(!store.pending_decompositions.contains_key(&id(1)));
            assert!(store.pending_decompositions.contains_key(&id(999)));
            assert_eq!(store.decomposition_history[0].parent, before.tasks[&id(1)]);
            for (index, child) in result.unwrap().child_ids.iter().enumerate() {
                assert_eq!(store.tasks[child].title, edited()[index].title);
            }
        }
    }
}

#[test]
fn fresh_commit_consumes_stale_suggestion_and_review_failures_preserve_it() {
    let mut store = store();
    store.pending_decompositions.insert(id(1), edited());
    let before = store.clone();
    let mut reviewer = Reviewer {
        result: Some(edited()),
        seen: vec![],
    };
    assert!(
        review_pending_decomposition(&mut store, id(2), &mut reviewer, at(), &mut |_| panic!(
            "no save"
        ))
        .is_err()
    );
    assert!(reviewer.seen.is_empty());
    assert!(
        review_pending_decomposition(&mut store, id(1), &mut reviewer, at(), &mut |_| Err(
            "disk full".into()
        ))
        .is_err()
    );
    assert_eq!(store, before);
    let model = model();
    let mut reviewer = Reviewer {
        result: Some(edited()),
        seen: vec![],
    };
    decompose_task(
        &mut store,
        id(1),
        &model,
        &mut reviewer,
        0,
        at(),
        &mut |_| Ok(()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(model.prompts.borrow().len(), 1);
    assert_eq!(reviewer.seen[0][0].title, "Draft");
    assert!(!store.pending_decompositions.contains_key(&id(1)));
}
