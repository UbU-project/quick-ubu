use super::*;
use crate::persist::{SqliteBackend, StorageBackend};
use std::cell::RefCell;
use ubu_core::{Commitment, DeferPolicy, Tier, TimeWindow};

#[path = "decompose_rewire_tests.rs"]
mod rewiring;

fn at() -> DateTime<Utc> {
    DateTime::from_timestamp(1_800_000_000, 123).unwrap()
}

fn parent() -> Task {
    Task {
        id: Id::from_u128(1),
        title: "Ship café / plan".into(),
        tier: Tier::TopSecret,
        detail: Some("Clarified lore\nwith \"quotes\".".into()),
        category: Some("work".into()),
        tags: vec!["focus".into(), "writing".into()],
        objective_ids: vec![Id::from_u128(90)],
        skills: vec!["Rust".into()],
        affect_cost: 9,
        est_duration: Duration::minutes(60),
        due: Some(at()),
        earliest_start: Some(at()),
        must_finish_by: Some(at() + Duration::hours(8)),
        pinned: Some(TimeWindow {
            start: at(),
            end: at() + Duration::hours(1),
        }),
        transparent: true,
        reminders: vec![10, 0],
        blocked_by: vec![Id::from_u128(2)],
        after: vec![AfterConstraint {
            task_id: Id::from_u128(3),
            offset: Duration::minutes(7),
        }],
        defer_policy: DeferPolicy::DeferUntil(at()),
        status: TaskStatus::Scheduled,
        provenance: Provenance::Crawled {
            source: "source".into(),
            ref_id: "ref".into(),
        },
        commitment: Some(Commitment {
            person: "Colleague".into(),
            note: Some("Promise".into()),
        }),
    }
}

fn store() -> Store {
    let mut store = Store::new();
    let parent = parent();
    store
        .calendar_links
        .insert(parent.id, "parent-event".into());
    store
        .export_signatures
        .insert(parent.id, "parent-signature".into());
    store.pending_event_deletions.push("older-event".into());
    store.upsert_task(parent);
    store
}

struct Model {
    reply: Result<String, String>,
    prompts: RefCell<Vec<String>>,
}
impl LlmTransport for Model {
    fn generate(&self, prompt: &str) -> Result<String, String> {
        self.prompts.borrow_mut().push(prompt.into());
        self.reply.clone()
    }
}
fn model() -> Model {
    Model {
        reply: Ok(
            r#"{"subtasks":[{"title":"Draft","duration_minutes":0,"offset_minutes":0}]}"#.into(),
        ),
        prompts: RefCell::new(vec![]),
    }
}
struct Reviewer {
    result: Option<Vec<SubTaskProposal>>,
    seen: Vec<Vec<SubTaskProposal>>,
}
impl DecompositionReviewer for Reviewer {
    fn review(&mut self, proposal: &[SubTaskProposal]) -> Option<Vec<SubTaskProposal>> {
        self.seen.push(proposal.to_vec());
        self.result.clone()
    }
}
fn edited() -> Vec<SubTaskProposal> {
    [
        ("Edited first", 0, 999),
        ("Second", 17, 5),
        ("Third", 3, -2),
    ]
    .into_iter()
    .map(
        |(title, duration_minutes, offset_minutes)| SubTaskProposal {
            title: title.into(),
            duration_minutes,
            offset_minutes,
            clamped: false,
        },
    )
    .collect()
}

#[test]
fn prompt_includes_lore_context_and_independent_durations() {
    let example = CompletedExample {
        title: "Past".into(),
        tags: vec!["focus".into()],
        category: Some("work".into()),
        duration: Duration::minutes(40),
        completed_at: at(),
    };
    let prompt = build_decompose_prompt(&parent(), &[example]);
    let context: Value = serde_json::from_str(prompt.split_once("Context: ").unwrap().1).unwrap();
    assert_eq!(context["task"]["title"], parent().title);
    assert_eq!(context["task"]["detail"], parent().detail.unwrap());
    assert_eq!(context["task"]["tags"], json!(parent().tags));
    assert_eq!(context["task"]["category"], "work");
    assert_eq!(context["task"]["est_duration_minutes"], 60);
    assert_eq!(context["completed_examples"][0]["duration_minutes"], 40);
    assert!(prompt.contains("independent integer duration_minutes"));
    assert!(prompt.contains("offset_minutes"));
}

#[test]
fn parse_clamps_zero_negative_and_preserves_independent_positive_minutes() {
    for (minutes, expected, clamped) in
        [(0, 1, true), (-5, 1, true), (1, 1, false), (25, 25, false)]
    {
        let parsed = parse_decompose_response(
            &json!({"subtasks":[{
                "title":"Step", "duration_minutes":minutes, "offset_minutes":4
            }]})
            .to_string(),
        )
        .unwrap();
        assert_eq!(parsed[0].duration_minutes, expected);
        assert_eq!(parsed[0].clamped, clamped);
        assert_eq!(parsed[0].offset_minutes, 4);
    }
    for invalid in [
        "bad",
        "{}",
        r#"{"subtasks":[]}"#,
        r#"{"subtasks":[{"title":"Step","duration_minutes":1}]}"#,
    ] {
        assert!(parse_decompose_response(invalid).is_err());
    }
    for row in [
        json!({"title":" ","duration_minutes":1,"offset_minutes":0}),
        json!({"title":"Step","duration_minutes":1.5,"offset_minutes":0}),
        json!({"title":"Step","duration_minutes":"1","offset_minutes":0}),
        json!({"title":"Step","duration_minutes":i64::MAX,"offset_minutes":0}),
        json!({"title":"Step","duration_minutes":1,"offset_minutes":i64::MIN}),
    ] {
        assert!(parse_decompose_response(&json!({"subtasks":[row]}).to_string()).is_err());
    }
}

#[test]
fn review_form_round_trips_quoted_titles_annotations_edits_and_empty_abort_without_editor() {
    let mut proposal = edited();
    proposal[0].title = "Quotes \" / café\nsecond line # comment".into();
    normalize(&mut proposal).unwrap();
    let form = render_review(&proposal);
    assert!(form.contains("# clamped to 1 minute"));
    assert!(form.contains("COMMIT"));
    assert_eq!(parse_review(&form).unwrap(), Some(proposal));
    let edited = parse_review("# ignored\n\"Added step\" / -7 / 12\n")
        .unwrap()
        .unwrap();
    assert_eq!(edited[0].title, "Added step");
    assert_eq!(edited[0].duration_minutes, 1);
    assert!(edited[0].clamped);
    assert_eq!(parse_review(" \n# abort\n").unwrap(), None);
    for invalid in [
        "broken line",
        "\"\" / 1 / 0",
        "\"Step\" / x / 0",
        "\"Step\" / 1 / 1.5",
    ] {
        assert!(parse_review(invalid).is_err());
    }
}

#[test]
fn reviewed_commit_creates_dynamic_chain_retires_parent_and_saves_snapshot_once() {
    let mut store = store();
    let original = parent();
    let model = model();
    let mut reviewer = Reviewer {
        result: Some(edited()),
        seen: vec![],
    };
    let backend = SqliteBackend::in_memory().unwrap();
    backend.save(&store).unwrap();
    let mut saves = 0;
    let summary = decompose_task(
        &mut store,
        original.id,
        &model,
        &mut reviewer,
        20,
        at(),
        &mut |s| {
            saves += 1;
            backend.save(s)
        },
    )
    .unwrap()
    .unwrap();
    assert_eq!(saves, 1);
    assert_eq!(backend.load().unwrap(), store);
    assert_eq!(summary.clamped, 1);
    assert_eq!(summary.child_ids.len(), 3);
    assert_eq!(reviewer.seen.len(), 1);
    assert!(reviewer.seen[0][0].clamped);
    assert_eq!(reviewer.seen[0][0].duration_minutes, 1);
    assert!(!store.tasks.contains_key(&original.id));
    assert!(!store.calendar_links.contains_key(&original.id));
    assert!(!store.export_signatures.contains_key(&original.id));
    assert_eq!(
        store.pending_event_deletions,
        ["older-event", "parent-event"]
    );
    assert_eq!(store.decomposition_history.len(), 1);
    let record = &store.decomposition_history[0];
    assert_eq!(
        serde_json::to_vec(&record.parent).unwrap(),
        serde_json::to_vec(&original).unwrap()
    );
    assert_eq!(record.child_ids, summary.child_ids);
    assert_eq!(record.at, at());
    assert!(!record.id.is_nil());
    assert_eq!(store.tasks.len(), 3);
    for (index, id) in summary.child_ids.iter().enumerate() {
        let child = &store.tasks[id];
        assert_eq!(child.title, edited()[index].title);
        assert_eq!(child.est_duration, Duration::minutes([1, 17, 3][index]));
        assert_eq!(child.tier, original.tier);
        assert_eq!(child.category, original.category);
        assert_eq!(child.tags, original.tags);
        assert_eq!(child.defer_policy, original.defer_policy);
        assert_eq!(child.provenance, Provenance::Manual);
        assert_eq!(child.status, TaskStatus::Backlog);
        assert_eq!(child.pinned, None);
        assert_eq!(child.detail, None);
        assert!(
            child.objective_ids.is_empty() && child.skills.is_empty() && child.reminders.is_empty()
        );
        assert_eq!(child.affect_cost, 0);
        assert!(!child.transparent);
        assert_eq!(child.due, None);
        assert_eq!(child.commitment, None);
        if index == 0 {
            assert_eq!(child.blocked_by, original.blocked_by);
            assert_eq!(child.earliest_start, original.earliest_start);
            assert_eq!(child.must_finish_by, original.must_finish_by);
            assert_eq!(child.after, original.after);
        } else {
            assert!(child.blocked_by.is_empty());
            assert_eq!(child.earliest_start, None);
            assert_eq!(child.must_finish_by, None);
            assert_eq!(
                child.after,
                vec![AfterConstraint {
                    task_id: summary.child_ids[index - 1],
                    offset: Duration::minutes(edited()[index].offset_minutes)
                }]
            );
        }
    }
}

#[test]
fn abort_and_failed_save_leave_store_and_persistence_byte_unchanged() {
    for abort in [true, false] {
        let mut store = store();
        let bytes = serde_json::to_vec(&store).unwrap();
        let backend = SqliteBackend::in_memory().unwrap();
        backend.save(&store).unwrap();
        let mut reviewer = Reviewer {
            result: if abort { None } else { Some(edited()) },
            seen: vec![],
        };
        let mut saves = 0;
        let result = decompose_task(
            &mut store,
            parent().id,
            &model(),
            &mut reviewer,
            0,
            at(),
            &mut |_| {
                saves += 1;
                Err("save failed".into())
            },
        );
        if abort {
            assert_eq!(result.unwrap(), None);
        } else {
            assert!(result.is_err());
        }
        assert_eq!(saves, usize::from(!abort));
        assert_eq!(serde_json::to_vec(&store).unwrap(), bytes);
        assert_eq!(serde_json::to_vec(&backend.load().unwrap()).unwrap(), bytes);
    }
}

#[test]
fn generation_parse_review_validation_and_unknown_task_errors_do_not_mutate_store() {
    for reply in [Err("model unavailable".into()), Ok("invalid JSON".into())] {
        let mut store = store();
        let before = store.clone();
        let mut reviewer = Reviewer {
            result: Some(edited()),
            seen: vec![],
        };
        let transport = Model {
            reply,
            prompts: RefCell::new(vec![]),
        };
        assert!(decompose_task(
            &mut store,
            parent().id,
            &transport,
            &mut reviewer,
            0,
            at(),
            &mut |_| panic!("must not save")
        )
        .is_err());
        assert!(reviewer.seen.is_empty());
        assert_eq!(store, before);
    }
    for edited in [
        vec![],
        vec![SubTaskProposal {
            title: " ".into(),
            duration_minutes: 1,
            offset_minutes: 0,
            clamped: false,
        }],
    ] {
        let mut store = store();
        let before = store.clone();
        let mut reviewer = Reviewer {
            result: Some(edited),
            seen: vec![],
        };
        assert!(decompose_task(
            &mut store,
            parent().id,
            &model(),
            &mut reviewer,
            0,
            at(),
            &mut |_| panic!("must not save")
        )
        .is_err());
        assert_eq!(store, before);
    }
    let transport = model();
    assert!(decompose_task(
        &mut store(),
        Id::nil(),
        &transport,
        &mut Reviewer {
            result: None,
            seen: vec![]
        },
        0,
        at(),
        &mut |_| panic!("must not save")
    )
    .is_err());
    assert!(transport.prompts.borrow().is_empty());
}

#[test]
fn unlinked_parent_and_single_child_do_not_queue_a_calendar_deletion() {
    let mut store = store();
    store.calendar_links.clear();
    let prior = store.pending_event_deletions.clone();
    let mut reviewer = Reviewer {
        result: Some(vec![edited()[0].clone()]),
        seen: vec![],
    };
    let summary = decompose_task(
        &mut store,
        parent().id,
        &model(),
        &mut reviewer,
        0,
        at(),
        &mut |_| Ok(()),
    )
    .unwrap()
    .unwrap();
    assert_eq!(summary.child_ids.len(), 1);
    assert_eq!(store.pending_event_deletions, prior);
}

fn committed_store() -> (Store, Vec<Id>) {
    let mut store = store();
    let summary = decompose_task(
        &mut store,
        parent().id,
        &model(),
        &mut Reviewer {
            result: Some(edited()),
            seen: vec![],
        },
        0,
        at(),
        &mut |_| Ok(()),
    )
    .unwrap()
    .unwrap();
    (store, summary.child_ids)
}

#[test]
fn undo_restores_full_dc1_snapshot_removes_children_and_round_trips_sqlite() {
    let (mut store, children) = committed_store();
    let original = parent();
    let previous_deletions = store.pending_event_deletions.clone();
    for (index, id) in children.iter().enumerate() {
        store
            .calendar_links
            .insert(*id, format!("child-event-{index}"));
        store
            .export_signatures
            .insert(*id, format!("child-signature-{index}"));
        store.tasks.get_mut(id).unwrap().title = "Edited since decomposition".into();
    }
    let mut unrelated = original.clone();
    unrelated.id = Id::from_u128(900);
    store.upsert_task(unrelated.clone());
    store
        .calendar_links
        .insert(unrelated.id, "unrelated-event".into());
    store
        .export_signatures
        .insert(unrelated.id, "unrelated-signature".into());
    let backend = SqliteBackend::in_memory().unwrap();
    backend.save(&store).unwrap();
    store = backend.load().unwrap();
    assert_eq!(store.decomposition_history.len(), 1);
    undo_decomposition(&mut store, 0).unwrap();
    backend.save(&store).unwrap();
    assert_eq!(backend.load().unwrap(), store);
    assert_eq!(
        serde_json::to_vec(&store.tasks[&original.id]).unwrap(),
        serde_json::to_vec(&original).unwrap()
    );
    for id in &children {
        assert!(!store.tasks.contains_key(id));
        assert!(!store.calendar_links.contains_key(id));
        assert!(!store.export_signatures.contains_key(id));
    }
    assert!(!store.calendar_links.contains_key(&original.id));
    assert!(!store.export_signatures.contains_key(&original.id));
    let mut expected = previous_deletions;
    expected.extend((0..3).map(|n| format!("child-event-{n}")));
    assert_eq!(store.pending_event_deletions, expected);
    assert!(store.decomposition_history.is_empty());
    assert_eq!(store.tasks[&unrelated.id], unrelated);
    assert_eq!(store.calendar_links[&unrelated.id], "unrelated-event");
    assert_eq!(
        store.export_signatures[&unrelated.id],
        "unrelated-signature"
    );
}

#[test]
fn undo_tolerates_missing_children_cleans_stale_links_and_removes_completed_children() {
    let (mut store, children) = committed_store();
    store.tasks.remove(&children[0]);
    store.tasks.remove(&children[1]);
    store
        .calendar_links
        .insert(children[1], "stale-child-event".into());
    store
        .export_signatures
        .insert(children[1], "stale-signature".into());
    store.tasks.get_mut(&children[2]).unwrap().status = TaskStatus::Done;
    undo_decomposition(&mut store, 0).unwrap();
    assert_eq!(store.tasks.len(), 1);
    assert_eq!(store.tasks[&parent().id], parent());
    assert!(store.calendar_links.is_empty());
    assert!(store.export_signatures.is_empty());
    assert_eq!(
        store.pending_event_deletions,
        ["older-event", "parent-event", "stale-child-event"]
    );
    assert!(store.decomposition_history.is_empty());

    let (mut store, _) = committed_store();
    store.tasks.clear();
    undo_decomposition(&mut store, 0).unwrap();
    assert_eq!(store.tasks[&parent().id], parent());
    let after = store.clone();
    assert!(undo_decomposition(&mut store, 0).is_err());
    assert_eq!(store, after);
}

#[test]
fn undo_invalid_index_is_unchanged_and_existing_parent_is_replaced_by_snapshot() {
    let (mut store, _) = committed_store();
    let before = store.clone();
    for invalid in [1, usize::MAX] {
        assert!(undo_decomposition(&mut store, invalid).is_err());
        assert_eq!(store, before);
    }
    let mut changed = parent();
    changed.title = "Reintroduced parent".into();
    store.upsert_task(changed);
    undo_decomposition(&mut store, 0).unwrap();
    assert_eq!(store.tasks[&parent().id], parent());
}

#[test]
fn undo_selection_uses_last_record_and_retired_parent_id_or_title_prefixes() {
    let mut store = Store::new();
    assert!(resolve_decomposition_index(&store, None).is_err());
    assert!(resolve_decomposition_index(&store, Some("missing")).is_err());
    for (id, title, timestamp) in [
        ("aabbccdd-1111-2222-3333-444455556666", "Café Alpha", at()),
        (
            "eeffccdd-1111-2222-3333-444455556666",
            "Café Beta",
            at() - Duration::days(1),
        ),
    ] {
        let mut parent = parent();
        parent.id = Id::parse_str(id).unwrap();
        parent.title = title.into();
        store.decomposition_history.push(DecompositionRecord {
            id: Id::new_v4(),
            parent,
            child_ids: vec![],
            rewires: vec![],
            at: timestamp,
        });
    }
    assert_eq!(resolve_decomposition_index(&store, None).unwrap(), 1);
    for prefix in ["AABBCCDD-1111", "aabbccdd1111", "aabb", "CAFÉ A"] {
        assert_eq!(
            resolve_decomposition_index(&store, Some(prefix)).unwrap(),
            0
        );
    }
    assert_eq!(
        resolve_decomposition_index(&store, Some("café b")).unwrap(),
        1
    );
    assert!(resolve_decomposition_index(&store, Some("Café"))
        .unwrap_err()
        .contains("ambiguous"));
    assert!(resolve_decomposition_index(&store, Some("unknown"))
        .unwrap_err()
        .contains("no decomposition"));
    let record_id = store.decomposition_history[0].id.to_string();
    assert!(resolve_decomposition_index(&store, Some(&record_id)).is_err());
    // ID and title matches participate in the same ambiguity check.
    store.decomposition_history[1].parent.title = "aabb title".into();
    assert!(resolve_decomposition_index(&store, Some("aabb"))
        .unwrap_err()
        .contains("ambiguous"));
    store.decomposition_history[1].parent.title = "Café Alpha".into();
    assert!(resolve_decomposition_index(&store, Some("Café Alpha"))
        .unwrap_err()
        .contains("ambiguous"));
}

#[test]
fn undo_outer_record_does_not_recursively_remove_nested_children_or_history() {
    let (mut store, children) = committed_store();
    let nested = decompose_task(
        &mut store,
        children[0],
        &model(),
        &mut Reviewer {
            result: Some(edited()),
            seen: vec![],
        },
        0,
        at(),
        &mut |_| Ok(()),
    )
    .unwrap()
    .unwrap();
    let nested_record = store.decomposition_history[1].clone();
    undo_decomposition(&mut store, 0).unwrap();
    assert_eq!(store.decomposition_history, vec![nested_record]);
    for id in nested.child_ids {
        assert!(store.tasks.contains_key(&id));
    }
    for id in children {
        assert!(!store.tasks.contains_key(&id));
    }
    assert_eq!(store.tasks[&parent().id], parent());
}
