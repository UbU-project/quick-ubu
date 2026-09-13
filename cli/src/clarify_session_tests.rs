use super::*;
use crate::batch::{run_batch_operations, BatchOutcome};
use crate::persist::{SqliteBackend, StorageBackend};
use std::sync::atomic::{AtomicBool, Ordering};
use ubu_core::Proposal;

fn ready_store(rounds: &[u32]) -> Store {
    let mut store = Store::new();
    for (index, round) in rounds.iter().enumerate() {
        let mut task = task();
        task.id = Id::from_u128(index as u128 + 1);
        task.title = format!("Task {}", index + 1);
        let id = task.id;
        store.upsert_task(task);
        queue_clarification(&mut store, id).unwrap();
        store.clarify_sessions.get_mut(&id).unwrap().round = *round;
    }
    store
}

fn questions_reply() -> Result<String, String> {
    response(
        vec![question("q", "Who is this for?", "ShortText", Value::Null)],
        &["client"],
        false,
    )
}

fn context(prompt: &str) -> Value {
    serde_json::from_str(prompt.split_once("Context: ").unwrap().1).unwrap()
}

#[test]
fn queued_lifecycle_survives_reload_between_generation_answers_and_finalization() {
    let mut store = ready_store(&[0]);
    let id = Id::from_u128(1);
    let original = store.tasks[&id].clone();
    assert_eq!(store.clarify_sessions[&id].round, 0);
    assert_eq!(store.clarify_sessions[&id].accumulated, "Existing lore.");
    let transport = StubTransport::new(vec![
        response(
            vec![
                question("q1", "Is this for a client?", "YesNo", Value::Null),
                question("q2", "Which client?", "ShortText", json!(["q1", "y"])),
            ],
            &["client"],
            false,
        ),
        response(vec![], &["client", "writing", "review"], true),
    ]);
    let backend = SqliteBackend::in_memory().unwrap();
    let mut saves = 0;
    assert_eq!(
        run_clarify_batch(
            &mut store,
            &transport,
            5,
            20,
            &AtomicBool::new(false),
            &mut |s| {
                saves += 1;
                backend.save(s)
            }
        ),
        BatchOutcome::Completed
    );
    store = backend.load().unwrap();
    assert_eq!(saves, 1);
    assert_eq!(store.clarify_sessions[&id].round, 1);
    assert_eq!(store.clarify_sessions[&id].pending, parsed_questions());
    assert_eq!(store.tasks[&id], original);
    let mut collector = StubCollector::new(vec![answers(&[("q1", "Y"), ("q2", "Acme")], false)]);
    let report = answer_sessions(&mut store, Some(id), &mut collector, 5, &mut |s| {
        backend.save(s)
    })
    .unwrap();
    assert_eq!(report.answered, 1);
    assert_eq!(report.finalized, 0);
    store = backend.load().unwrap();
    let accumulated = "Existing lore.\nQ: Is this for a client?\nA: Y\nQ: Which client?\nA: Acme\n";
    assert_eq!(store.clarify_sessions[&id].accumulated, accumulated);
    assert!(store.clarify_sessions[&id].pending.is_empty());
    assert_eq!(store.tasks[&id], original);
    assert_eq!(
        run_clarify_batch(
            &mut store,
            &transport,
            5,
            20,
            &AtomicBool::new(false),
            &mut |s| backend.save(s)
        ),
        BatchOutcome::Completed
    );
    store = backend.load().unwrap();
    assert!(store.clarify_sessions.is_empty());
    assert_eq!(store.tasks[&id].detail.as_deref(), Some(accumulated));
    assert_eq!(store.tasks[&id].tags, original.tags);
    assert_eq!(
        store
            .pending_decisions
            .iter()
            .map(|d| match &d.proposal {
                Proposal::Tag { tag, .. } => tag.as_str(),
                _ => panic!("expected tag"),
            })
            .collect::<Vec<_>>(),
        vec!["client", "review"]
    );
    assert_eq!(
        context(&transport.prompts.borrow()[1])["accumulated_lore_and_qa"],
        accumulated
    );
    assert_eq!(collector.questions.len(), 1);
}

#[test]
fn ready_sessions_run_highest_round_first_with_id_ties_and_skip_pending_or_capped() {
    let mut store = ready_store(&[0, 2, 1, 2, 0, 5]);
    store
        .clarify_sessions
        .get_mut(&Id::from_u128(5))
        .unwrap()
        .pending = parsed_questions();
    let before = store.clone();
    let transport = StubTransport::new(vec![questions_reply(); 4]);
    let mut saves = 0;
    assert_eq!(
        run_clarify_batch(
            &mut store,
            &transport,
            5,
            0,
            &AtomicBool::new(false),
            &mut |_| {
                saves += 1;
                Ok(())
            }
        ),
        BatchOutcome::Completed
    );
    assert_eq!(saves, 4);
    assert_eq!(
        transport
            .prompts
            .borrow()
            .iter()
            .map(|p| context(p)["task"]["title"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        vec!["Task 2", "Task 4", "Task 3", "Task 1"]
    );
    for n in [5, 6] {
        assert_eq!(
            store.clarify_sessions[&Id::from_u128(n)],
            before.clarify_sessions[&Id::from_u128(n)]
        );
    }
    assert_eq!(store.clarify_sessions[&Id::from_u128(2)].round, 3);
}

#[test]
fn interrupt_after_first_task_saves_most_advanced_progress_and_leaves_others_untouched() {
    let mut store = ready_store(&[0, 2, 1]);
    let before = store.clone();
    let flag = AtomicBool::new(false);
    let transport = StubTransport::new(vec![questions_reply()]);
    let mut snapshots = vec![];
    assert_eq!(
        run_clarify_batch(&mut store, &transport, 5, 0, &flag, &mut |s| {
            snapshots.push(s.clone());
            flag.store(true, Ordering::SeqCst);
            Ok(())
        }),
        BatchOutcome::Interrupted
    );
    assert_eq!(snapshots.len(), 2);
    assert_eq!(snapshots[0], snapshots[1]);
    assert_eq!(store.clarify_sessions[&Id::from_u128(2)].round, 3);
    assert!(!store.clarify_sessions[&Id::from_u128(2)].pending.is_empty());
    for n in [1, 3] {
        assert_eq!(
            store.clarify_sessions[&Id::from_u128(n)],
            before.clarify_sessions[&Id::from_u128(n)]
        );
    }
    assert_eq!(transport.prompts.borrow().len(), 1);
}

#[test]
fn cap_with_useful_questions_finalizes_after_answers_without_another_generation() {
    let mut store = ready_store(&[4]);
    let id = Id::from_u128(1);
    let transport = StubTransport::new(vec![questions_reply()]);
    assert_eq!(
        run_clarify_batch(
            &mut store,
            &transport,
            5,
            0,
            &AtomicBool::new(false),
            &mut |_| Ok(())
        ),
        BatchOutcome::Completed
    );
    assert_eq!(store.clarify_sessions[&id].round, 5);
    assert!(!store.clarify_sessions[&id].pending.is_empty());
    // An awaiting session must not ask another question, even at the cap.
    assert_eq!(
        run_clarify_batch(
            &mut store,
            &transport,
            5,
            0,
            &AtomicBool::new(false),
            &mut |_| panic!("no ready task")
        ),
        BatchOutcome::Completed
    );
    let mut collector = StubCollector::new(vec![answers(&[("q", "Acme")], false)]);
    let report = answer_sessions(&mut store, Some(id), &mut collector, 5, &mut |_| Ok(())).unwrap();
    assert_eq!(report.finalized, 1);
    assert_eq!(report.queued, 1);
    assert!(store.clarify_sessions.is_empty());
    assert!(store.tasks[&id]
        .detail
        .as_ref()
        .unwrap()
        .contains("A: Acme"));
    assert_eq!(transport.prompts.borrow().len(), 1);
}

#[test]
fn empty_question_rounds_advance_until_cap_and_then_finalize() {
    let mut store = ready_store(&[0]);
    let transport = StubTransport::new(vec![response(vec![], &["client"], false); 2]);
    let id = Id::from_u128(1);
    for _ in 0..2 {
        assert_eq!(
            run_clarify_batch(
                &mut store,
                &transport,
                3,
                0,
                &AtomicBool::new(false),
                &mut |_| Ok(())
            ),
            BatchOutcome::Completed
        );
    }
    assert_eq!(store.clarify_sessions[&id].round, 2);
    assert!(store.clarify_sessions[&id].pending.is_empty());
    let transport = StubTransport::new(vec![response(vec![], &["client"], false)]);
    assert_eq!(
        run_clarify_batch(
            &mut store,
            &transport,
            3,
            0,
            &AtomicBool::new(false),
            &mut |_| Ok(())
        ),
        BatchOutcome::Completed
    );
    assert!(store.clarify_sessions.is_empty());
    assert_eq!(store.tasks[&id].detail.as_deref(), Some("Existing lore."));
    assert_eq!(store.pending_decisions.len(), 1);
}

#[test]
fn done_finalizes_even_with_questions_and_custom_caps_are_respected() {
    let mut store = ready_store(&[0]);
    let transport = StubTransport::new(vec![response(
        vec![question("q", "Unused?", "YesNo", Value::Null)],
        &["done-tag"],
        true,
    )]);
    assert_eq!(
        run_clarify_batch(
            &mut store,
            &transport,
            2,
            0,
            &AtomicBool::new(false),
            &mut |_| Ok(())
        ),
        BatchOutcome::Completed
    );
    assert!(store.clarify_sessions.is_empty());
    let mut store = ready_store(&[1]);
    let transport = StubTransport::new(vec![questions_reply()]);
    run_clarify_batch(
        &mut store,
        &transport,
        2,
        0,
        &AtomicBool::new(false),
        &mut |_| Ok(()),
    );
    let mut collector = StubCollector::new(vec![answers(&[("q", "Client")], false)]);
    assert_eq!(
        answer_sessions(&mut store, None, &mut collector, 2, &mut |_| Ok(()))
            .unwrap()
            .finalized,
        1
    );
}

#[test]
fn model_errors_preserve_sessions_and_do_not_block_other_ready_tasks() {
    let mut store = ready_store(&[2, 1, 0]);
    store
        .clarify_sessions
        .get_mut(&Id::from_u128(1))
        .unwrap()
        .tags = vec!["old-tag".into()];
    let before = store.clone();
    let transport = StubTransport::new(vec![
        Err("offline stub".into()),
        Ok(r#"{"questions":[{"id":"bad"}],"tags":["must-not-keep"],"done":false}"#.into()),
        response(vec![], &["new-tag"], true),
    ]);
    let mut snapshots = vec![];
    assert_eq!(
        run_clarify_batch(
            &mut store,
            &transport,
            5,
            0,
            &AtomicBool::new(false),
            &mut |s| {
                snapshots.push(s.clone());
                Ok(())
            }
        ),
        BatchOutcome::Completed
    );
    assert_eq!(snapshots.len(), 3);
    for n in [1, 2] {
        assert_eq!(
            store.clarify_sessions[&Id::from_u128(n)],
            before.clarify_sessions[&Id::from_u128(n)]
        );
    }
    assert!(!store.clarify_sessions.contains_key(&Id::from_u128(3)));
    assert_eq!(store.pending_decisions.len(), 1);
}

#[test]
fn answer_stop_keeps_session_untouched_and_stops_before_other_forms() {
    let mut store = ready_store(&[1, 1]);
    for session in store.clarify_sessions.values_mut() {
        session.pending = parsed_questions();
    }
    let before = store.clone();
    let mut collector = StubCollector::new(vec![answers(&[("q1", "y")], true)]);
    let report = answer_sessions(&mut store, None, &mut collector, 5, &mut |_| {
        panic!("stopped without changes")
    })
    .unwrap();
    assert!(report.stopped);
    assert_eq!(report.answered, 0);
    assert_eq!(collector.questions.len(), 1);
    assert_eq!(store, before);
}

#[test]
fn answer_one_then_all_filters_conditions_and_saves_each_answered_task() {
    let mut store = ready_store(&[1, 1, 1]);
    for session in store.clarify_sessions.values_mut() {
        session.pending = parsed_questions();
    }
    let untouched = store.clarify_sessions[&Id::from_u128(1)].clone();
    let mut collector = StubCollector::new(vec![
        answers(&[("q1", "n"), ("q2", "irrelevant")], false),
        answers(&[("q1", "Y"), ("q2", "First")], false),
        answers(&[("q1", "Y"), ("q2", "Third")], false),
    ]);
    let mut saves = 0;
    assert_eq!(
        answer_sessions(
            &mut store,
            Some(Id::from_u128(2)),
            &mut collector,
            5,
            &mut |_| {
                saves += 1;
                Ok(())
            }
        )
        .unwrap()
        .answered,
        1
    );
    assert!(!store.clarify_sessions[&Id::from_u128(2)]
        .accumulated
        .contains("irrelevant"));
    assert_eq!(store.clarify_sessions[&Id::from_u128(1)], untouched);
    assert_eq!(
        answer_sessions(&mut store, None, &mut collector, 5, &mut |_| {
            saves += 1;
            Ok(())
        })
        .unwrap()
        .answered,
        2
    );
    assert_eq!(saves, 3);
    assert!(store
        .clarify_sessions
        .values()
        .all(|s| s.pending.is_empty()));
    assert!(store.clarify_sessions[&Id::from_u128(1)]
        .accumulated
        .contains("A: First"));
}

#[test]
fn preexisting_and_last_task_interrupts_and_save_errors_are_nonzero() {
    let mut store = ready_store(&[0, 0]);
    let before = store.clone();
    let transport = StubTransport::new(vec![]);
    let mut saves = 0;
    assert_eq!(
        run_clarify_batch(
            &mut store,
            &transport,
            5,
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
    let transport = StubTransport::new(vec![questions_reply()]);
    assert!(matches!(
        run_clarify_batch(
            &mut store,
            &transport,
            5,
            0,
            &AtomicBool::new(false),
            &mut |_| Err("disk full".into())
        ),
        BatchOutcome::Failed(_)
    ));
    assert_eq!(
        store.clarify_sessions[&Id::from_u128(2)],
        before.clarify_sessions[&Id::from_u128(2)]
    );
    let mut store = ready_store(&[0]);
    let transport = StubTransport::new(vec![response(vec![], &[], true)]);
    let flag = AtomicBool::new(false);
    let outcome = run_clarify_batch(&mut store, &transport, 5, 0, &flag, &mut |_| {
        flag.store(true, Ordering::SeqCst);
        Ok(())
    });
    assert_eq!(outcome, BatchOutcome::Interrupted);
    assert_eq!(outcome.exit_code(), 130);
    assert!(store.clarify_sessions.is_empty());
}

#[test]
fn queued_session_is_not_overwritten_and_orphan_is_preserved_without_model_call() {
    let mut store = ready_store(&[2]);
    let id = Id::from_u128(1);
    store.clarify_sessions.get_mut(&id).unwrap().pending = parsed_questions();
    let before = store.clone();
    assert!(!queue_clarification(&mut store, id).unwrap());
    assert_eq!(store, before);
    assert!(queue_clarification(&mut store, Id::nil()).is_err());
    store.clarify_sessions.get_mut(&id).unwrap().pending.clear();
    store.tasks.remove(&id);
    let before = store.clone();
    let transport = StubTransport::new(vec![]);
    assert_eq!(
        run_clarify_batch(
            &mut store,
            &transport,
            5,
            0,
            &AtomicBool::new(false),
            &mut |_| Ok(())
        ),
        BatchOutcome::Completed
    );
    assert_eq!(store, before);
}

#[test]
fn default_dispatch_includes_clarify_and_only_clarify_does_not_run_classifiers() {
    for only in [None, Some(crate::BatchOperation::Clarify)] {
        let mut store = ready_store(&[0]);
        let default = only.is_none();
        let mut replies = vec![];
        if default {
            replies.push(Ok(r#"{"tags":[]}"#.into()));
            replies.push(Ok(r#"{"dependencies":[],"preferences":[]}"#.into()));
        }
        replies.push(questions_reply());
        let transport = StubTransport::new(replies);
        assert_eq!(
            run_batch_operations(
                &mut store,
                &transport,
                crate::batch_operations(only),
                3,
                25,
                0,
                5,
                &AtomicBool::new(false),
                &mut |_| Ok(())
            ),
            BatchOutcome::Completed
        );
        assert_eq!(
            transport.prompts.borrow().len(),
            if default { 3 } else { 1 }
        );
        assert_eq!(store.batch_passes.len(), if default { 2 } else { 0 });
        assert_eq!(store.clarify_sessions[&Id::from_u128(1)].round, 1);
    }
}

#[test]
fn answer_save_failure_stops_before_next_form_and_ready_sessions_do_not_open_forms() {
    let mut store = ready_store(&[1, 1]);
    let mut collector = StubCollector::new(vec![]);
    assert_eq!(
        answer_sessions(&mut store, None, &mut collector, 5, &mut |_| panic!(
            "no answers"
        ))
        .unwrap(),
        AnswerReport::default()
    );
    for session in store.clarify_sessions.values_mut() {
        session.pending = parsed_questions();
    }
    let original_second = store.clarify_sessions[&Id::from_u128(2)].clone();
    let mut collector = StubCollector::new(vec![answers(&[("q1", "n")], false)]);
    assert!(
        answer_sessions(&mut store, None, &mut collector, 5, &mut |_| Err(
            "disk full".into()
        ))
        .is_err()
    );
    assert_eq!(store.clarify_sessions[&Id::from_u128(2)], original_second);
}
