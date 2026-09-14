use super::*;

fn id(n: u128) -> Id {
    Id::from_u128(n)
}
fn after(n: u128, minutes: i64) -> AfterConstraint {
    AfterConstraint {
        task_id: id(n),
        offset: Duration::minutes(minutes),
    }
}
fn referrer(n: u128, blocked_by: Vec<Id>, after: Vec<AfterConstraint>) -> Task {
    let mut task = parent();
    task.id = id(n);
    task.title = format!("Referrer {n}");
    task.blocked_by = blocked_by;
    task.after = after;
    task.objective_ids.clear();
    task.pinned = None;
    task
}
fn rewire_store() -> Store {
    let mut store = store();
    store.tasks.get_mut(&id(1)).unwrap().objective_ids.clear();
    for task in [
        referrer(2, vec![], vec![]),
        referrer(3, vec![], vec![]),
        referrer(100, vec![id(2), id(1), id(3), id(1), id(2)], vec![]),
        referrer(101, vec![], vec![after(3, 2), after(1, 10)]),
        referrer(102, vec![id(1)], vec![after(1, -5)]),
        referrer(103, vec![id(2), id(2)], vec![after(3, 7)]),
        referrer(104, vec![id(100)], vec![after(101, 3)]),
    ] {
        store.upsert_task(task);
    }
    store
}
fn decompose(
    store: &mut Store,
    save: &mut dyn FnMut(&Store) -> Result<(), String>,
) -> DecompositionSummary {
    decompose_task(
        store,
        id(1),
        &model(),
        &mut Reviewer {
            result: Some(edited()),
            seen: vec![],
        },
        0,
        at(),
        save,
    )
    .unwrap()
    .unwrap()
}

#[test]
fn commit_records_external_rewires_and_undo_restores_references_and_offsets_after_sqlite_reload() {
    let mut store = rewire_store();
    let before = store.clone();
    let backend = SqliteBackend::in_memory().unwrap();
    let mut saves = 0;
    let summary = decompose(&mut store, &mut |s| {
        saves += 1;
        backend.save(s)
    });
    assert_eq!(saves, 1);
    store = backend.load().unwrap();
    let last = *summary.child_ids.last().unwrap();
    assert_eq!(store.tasks[&id(100)].blocked_by, vec![id(2), last, id(3)]);
    assert_eq!(
        store.tasks[&id(101)].after,
        vec![
            after(3, 2),
            AfterConstraint {
                task_id: last,
                offset: Duration::minutes(10)
            }
        ]
    );
    assert_eq!(store.tasks[&id(102)].blocked_by, vec![last]);
    assert_eq!(
        store.tasks[&id(102)].after,
        vec![AfterConstraint {
            task_id: last,
            offset: Duration::minutes(-5)
        }]
    );
    for n in [2, 3, 103, 104] {
        assert_eq!(store.tasks[&id(n)], before.tasks[&id(n)]);
    }
    let record = &store.decomposition_history[0];
    assert_eq!(record.parent, before.tasks[&id(1)]);
    assert_eq!(
        record.rewires,
        vec![
            Rewire {
                task_id: id(100),
                kind: RewireKind::BlockedBy
            },
            Rewire {
                task_id: id(101),
                kind: RewireKind::After(Duration::minutes(10))
            },
            Rewire {
                task_id: id(102),
                kind: RewireKind::BlockedBy
            },
            Rewire {
                task_id: id(102),
                kind: RewireKind::After(Duration::minutes(-5))
            },
        ]
    );
    assert!(store.validate().is_ok());
    for (index, child_id) in summary.child_ids.iter().enumerate() {
        let child = &store.tasks[child_id];
        assert!(!record
            .rewires
            .iter()
            .any(|rewire| rewire.task_id == *child_id));
        if index == 0 {
            assert_eq!(child.blocked_by, before.tasks[&id(1)].blocked_by);
            assert_eq!(child.after, before.tasks[&id(1)].after);
        } else {
            assert_eq!(child.after[0].task_id, summary.child_ids[index - 1]);
            assert_eq!(
                child.after[0].offset,
                Duration::minutes(edited()[index].offset_minutes)
            );
        }
    }
    // Undo restores the recorded offset even if the user edited just the gap.
    store.tasks.get_mut(&id(101)).unwrap().after[1].offset = Duration::minutes(999);
    undo_decomposition(&mut store, 0).unwrap();
    backend.save(&store).unwrap();
    store = backend.load().unwrap();
    assert_eq!(store.tasks[&id(1)], before.tasks[&id(1)]);
    assert_eq!(store.tasks[&id(100)].blocked_by, vec![id(2), id(1), id(3)]);
    for n in [101, 102, 103, 104] {
        assert_eq!(store.tasks[&id(n)], before.tasks[&id(n)]);
    }
    assert!(store.decomposition_history.is_empty());
    assert!(store
        .tasks
        .values()
        .all(|task| !task.blocked_by.contains(&last)
            && task
                .after
                .iter()
                .all(|constraint| constraint.task_id != last)));
    assert!(store.validate().is_ok());
}

#[test]
fn undo_skips_removed_or_retargeted_referrers_and_only_reverses_recorded_kinds() {
    let mut store = rewire_store();
    let last = *decompose(&mut store, &mut |_| Ok(()))
        .child_ids
        .last()
        .unwrap();
    store.tasks.remove(&id(101));
    store.tasks.get_mut(&id(102)).unwrap().blocked_by = vec![id(2), id(2)];
    store.tasks.get_mut(&id(102)).unwrap().after = vec![after(3, 99)];
    let edited = store.tasks[&id(102)].clone();
    // A recorded blocked_by task gains a separate after reference after commit.
    store
        .tasks
        .get_mut(&id(100))
        .unwrap()
        .after
        .push(AfterConstraint {
            task_id: last,
            offset: Duration::minutes(8),
        });
    store.upsert_task(referrer(
        105,
        vec![last],
        vec![AfterConstraint {
            task_id: last,
            offset: Duration::minutes(12),
        }],
    ));
    let unrecorded = store.tasks[&id(105)].clone();
    undo_decomposition(&mut store, 0).unwrap();
    assert!(!store.tasks.contains_key(&id(101)));
    assert_eq!(store.tasks[&id(102)], edited);
    assert_eq!(store.tasks[&id(105)], unrecorded);
    assert_eq!(store.tasks[&id(100)].blocked_by, vec![id(2), id(1), id(3)]);
    assert_eq!(store.tasks[&id(100)].after[0].task_id, last);
}

#[test]
fn parent_and_children_are_excluded_even_when_the_parent_has_self_references() {
    let mut store = store();
    store.tasks.get_mut(&id(1)).unwrap().blocked_by.push(id(1));
    store.tasks.get_mut(&id(1)).unwrap().after.push(after(1, 4));
    let original = store.tasks[&id(1)].clone();
    let summary = decompose(&mut store, &mut |_| Ok(()));
    assert_eq!(store.decomposition_history[0].parent, original);
    assert!(store.decomposition_history[0].rewires.is_empty());
    assert_eq!(
        store.tasks[&summary.child_ids[0]].blocked_by,
        original.blocked_by
    );
    assert_eq!(store.tasks[&summary.child_ids[0]].after, original.after);
}

#[test]
fn blocked_by_dedup_preserves_first_occurrence_order_in_both_directions() {
    let mut ids = vec![id(2), id(1), id(3), id(1), id(4), id(3)];
    assert!(replace_blocked_by(&mut ids, id(1), id(4)));
    assert_eq!(ids, vec![id(2), id(4), id(3)]);
    ids.extend([id(1), id(4), id(2)]);
    assert!(replace_blocked_by(&mut ids, id(4), id(1)));
    assert_eq!(ids, vec![id(2), id(1), id(3)]);
    let mut absent = vec![id(2), id(2)];
    assert!(!replace_blocked_by(&mut absent, id(1), id(4)));
    assert_eq!(absent, vec![id(2), id(2)]);
}

#[test]
fn legacy_record_without_rewires_loads_and_undo_does_not_infer_reversals() {
    let mut store = rewire_store();
    decompose(&mut store, &mut |_| Ok(()));
    let referrer_before = store.tasks[&id(100)].clone();
    let mut json = serde_json::to_value(&store).unwrap();
    json["decomposition_history"][0]
        .as_object_mut()
        .unwrap()
        .remove("rewires");
    let record: DecompositionRecord =
        serde_json::from_value(json["decomposition_history"][0].clone()).unwrap();
    assert!(record.rewires.is_empty());
    let legacy: Store = serde_json::from_value(json).unwrap();
    let backend = SqliteBackend::in_memory().unwrap();
    backend.save(&legacy).unwrap();
    store = backend.load().unwrap();
    assert!(store.decomposition_history[0].rewires.is_empty());
    undo_decomposition(&mut store, 0).unwrap();
    assert_eq!(store.tasks[&id(100)], referrer_before);
    assert_eq!(store.tasks[&id(1)], record.parent);
    assert!(store.decomposition_history.is_empty());
}

#[test]
fn abort_or_failed_save_preserves_external_references_and_history() {
    for abort in [true, false] {
        let mut store = rewire_store();
        let bytes = serde_json::to_vec(&store).unwrap();
        let mut saves = 0;
        let result = decompose_task(
            &mut store,
            id(1),
            &model(),
            &mut Reviewer {
                result: if abort { None } else { Some(edited()) },
                seen: vec![],
            },
            0,
            at(),
            &mut |s| {
                saves += 1;
                assert_eq!(s.decomposition_history[0].rewires.len(), 4);
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
    }
}

#[test]
fn multiple_after_entries_are_recorded_separately_and_undo_follows_record_order() {
    let mut store = store();
    store.upsert_task(referrer(
        100,
        vec![],
        vec![after(1, 10), after(3, 4), after(1, 20)],
    ));
    let summary = decompose(&mut store, &mut |_| Ok(()));
    let last = *summary.child_ids.last().unwrap();
    assert_eq!(
        store.tasks[&id(100)].after,
        vec![
            AfterConstraint {
                task_id: last,
                offset: Duration::minutes(10)
            },
            after(3, 4),
            AfterConstraint {
                task_id: last,
                offset: Duration::minutes(20)
            },
        ]
    );
    assert_eq!(
        store.decomposition_history[0].rewires,
        vec![
            Rewire {
                task_id: id(100),
                kind: RewireKind::After(Duration::minutes(10))
            },
            Rewire {
                task_id: id(100),
                kind: RewireKind::After(Duration::minutes(20))
            },
        ]
    );
    undo_decomposition(&mut store, 0).unwrap();
    // Literal per-Rewire rule: the first After record reverses all matching
    // targets, so later records find none still pointing to the last child.
    assert_eq!(
        store.tasks[&id(100)].after,
        vec![after(1, 10), after(3, 4), after(1, 10)]
    );
}
