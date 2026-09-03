//! Timestamp-ordered replay of facts and commands captured in a session log.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use crate::{
    ActualStatus, CommandKind, CoreError, DeferPolicy, FactKind, Id, LogEntry, LogEntryKind,
    Preference, Store, Task, TaskStatus, TimeWindow,
};

pub fn log_defer(handle_id: Id, at: DateTime<Utc>) -> LogEntry {
    LogEntry {
        id: Uuid::new_v4(),
        kind: LogEntryKind::Command(CommandKind::Defer { handle_id }),
        at,
    }
}

pub fn log_actual(
    item_id: Id,
    status: ActualStatus,
    actual: Option<TimeWindow>,
    at: DateTime<Utc>,
) -> LogEntry {
    LogEntry {
        id: Uuid::new_v4(),
        kind: LogEntryKind::Fact(FactKind::Actual {
            item_id,
            status,
            actual,
        }),
        at,
    }
}

pub fn log_capture(task: Task, at: DateTime<Utc>) -> LogEntry {
    LogEntry {
        id: Uuid::new_v4(),
        kind: LogEntryKind::Command(CommandKind::Capture { task }),
        at,
    }
}

pub fn log_edit_dep(task_id: Id, blocked_by: Vec<Id>, at: DateTime<Utc>) -> LogEntry {
    LogEntry {
        id: Uuid::new_v4(),
        kind: LogEntryKind::Command(CommandKind::EditDep {
            task_id,
            blocked_by,
        }),
        at,
    }
}

pub fn log_edit_due(task_id: Id, due: Option<DateTime<Utc>>, at: DateTime<Utc>) -> LogEntry {
    LogEntry {
        id: Uuid::new_v4(),
        kind: LogEntryKind::Command(CommandKind::EditDue { task_id, due }),
        at,
    }
}

pub fn log_edit_pin(task_id: Id, pinned: Option<TimeWindow>, at: DateTime<Utc>) -> LogEntry {
    LogEntry {
        id: Uuid::new_v4(),
        kind: LogEntryKind::Command(CommandKind::EditPin { task_id, pinned }),
        at,
    }
}

pub fn log_edit_pref(pref: Preference, remove: bool, at: DateTime<Utc>) -> LogEntry {
    LogEntry {
        id: Uuid::new_v4(),
        kind: LogEntryKind::Command(CommandKind::EditPref { pref, remove }),
        at,
    }
}

/// Replay `log` in timestamp order, preserving input order for equal timestamps.
pub fn reconcile(store: &mut Store, log: &[LogEntry]) -> Result<(), CoreError> {
    let mut ordered: Vec<_> = log.iter().collect();
    ordered.sort_by_key(|entry| entry.at);

    for entry in ordered {
        match &entry.kind {
            LogEntryKind::Fact(FactKind::Actual {
                item_id, status, ..
            }) => {
                let task = store
                    .tasks
                    .get_mut(item_id)
                    .ok_or(CoreError::UnknownTask { id: *item_id })?;
                task.status = match status {
                    ActualStatus::Done => TaskStatus::Done,
                    ActualStatus::Ongoing => TaskStatus::Active,
                };
            }
            LogEntryKind::Command(CommandKind::Defer { handle_id }) => {
                let task = store
                    .tasks
                    .get_mut(handle_id)
                    .ok_or(CoreError::UnknownTask { id: *handle_id })?;
                match task.defer_policy.clone() {
                    DeferPolicy::RescheduleAsap | DeferPolicy::ReturnToBacklog => {
                        task.status = TaskStatus::Backlog;
                    }
                    DeferPolicy::DeferUntil(until) => {
                        task.earliest_start = Some(until);
                        task.status = TaskStatus::Backlog;
                    }
                }
            }
            LogEntryKind::Command(CommandKind::Capture { task }) => {
                store.upsert_task(task.clone());
            }
            LogEntryKind::Command(CommandKind::EditDep {
                task_id,
                blocked_by,
            }) => {
                let task = store
                    .tasks
                    .get_mut(task_id)
                    .ok_or(CoreError::UnknownTask { id: *task_id })?;
                task.blocked_by = blocked_by.clone();
            }
            LogEntryKind::Command(CommandKind::EditDue { task_id, due }) => {
                let task = store
                    .tasks
                    .get_mut(task_id)
                    .ok_or(CoreError::UnknownTask { id: *task_id })?;
                task.due = *due;
            }
            LogEntryKind::Command(CommandKind::EditPin { task_id, pinned }) => {
                let task = store
                    .tasks
                    .get_mut(task_id)
                    .ok_or(CoreError::UnknownTask { id: *task_id })?;
                task.pinned = pinned.clone();
            }
            LogEntryKind::Command(CommandKind::EditPref { pref, remove }) => {
                if *remove {
                    if let Some(index) = store
                        .preferences
                        .iter()
                        .position(|existing| existing == pref)
                    {
                        store.preferences.remove(index);
                    }
                } else {
                    store.add_preference(pref.clone());
                }
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::{Provenance, Relation, Tier};

    fn id(value: u128) -> Id {
        Uuid::from_u128(value)
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + seconds, 0).unwrap()
    }

    fn task(id: Id, tier: Tier, defer_policy: DeferPolicy) -> Task {
        Task {
            id,
            tier,
            title: format!("task-{id}"),
            detail: None,
            objective_ids: Vec::new(),
            skills: Vec::new(),
            affect_cost: 0,
            est_duration: Duration::minutes(30),
            due: None,
            earliest_start: None,
            category: None,
            pinned: None,
            blocked_by: Vec::new(),
            defer_policy,
            status: TaskStatus::Scheduled,
            provenance: Provenance::Manual,
            commitment: None,
        }
    }

    fn preference(left: u128, right: u128) -> Preference {
        Preference {
            left: id(left),
            right: id(right),
            relation: Relation::Strict,
        }
    }

    fn assert_entry(entry: LogEntry, kind: LogEntryKind, expected_at: DateTime<Utc>) {
        assert_eq!(entry.kind, kind);
        assert_eq!(entry.at, expected_at);
    }

    #[test]
    fn constructors_build_the_requested_log_entries() {
        let timestamp = at(1);
        let actual_window = TimeWindow {
            start: at(2),
            end: at(3),
        };
        let captured = task(id(2), Tier::UserShared, DeferPolicy::RescheduleAsap);
        let pref = preference(10, 11);

        assert_entry(
            log_defer(id(1), timestamp),
            LogEntryKind::Command(CommandKind::Defer { handle_id: id(1) }),
            timestamp,
        );
        assert_entry(
            log_actual(
                id(1),
                ActualStatus::Ongoing,
                Some(actual_window.clone()),
                timestamp,
            ),
            LogEntryKind::Fact(FactKind::Actual {
                item_id: id(1),
                status: ActualStatus::Ongoing,
                actual: Some(actual_window),
            }),
            timestamp,
        );
        assert_entry(
            log_capture(captured.clone(), timestamp),
            LogEntryKind::Command(CommandKind::Capture { task: captured }),
            timestamp,
        );
        assert_entry(
            log_edit_dep(id(1), vec![id(2), id(3)], timestamp),
            LogEntryKind::Command(CommandKind::EditDep {
                task_id: id(1),
                blocked_by: vec![id(2), id(3)],
            }),
            timestamp,
        );
        assert_entry(
            log_edit_due(id(1), Some(at(20)), timestamp),
            LogEntryKind::Command(CommandKind::EditDue {
                task_id: id(1),
                due: Some(at(20)),
            }),
            timestamp,
        );
        assert_entry(
            log_edit_pref(pref.clone(), true, timestamp),
            LogEntryKind::Command(CommandKind::EditPref { pref, remove: true }),
            timestamp,
        );
    }

    #[test]
    fn each_defer_policy_applies_its_required_mutation() {
        let until = at(500);
        let cases = [
            (DeferPolicy::RescheduleAsap, None),
            (DeferPolicy::ReturnToBacklog, None),
            (DeferPolicy::DeferUntil(until), Some(until)),
        ];

        for (index, (policy, expected_earliest_start)) in cases.into_iter().enumerate() {
            let task_id = id(index as u128 + 1);
            let mut store = Store::new();
            store.upsert_task(task(task_id, Tier::SemiPublic, policy));

            assert_eq!(reconcile(&mut store, &[log_defer(task_id, at(1))]), Ok(()));
            let deferred = &store.tasks[&task_id];
            assert_eq!(deferred.status, TaskStatus::Backlog);
            assert_eq!(deferred.earliest_start, expected_earliest_start);
        }
    }

    #[test]
    fn actual_done_and_ongoing_set_the_task_status() {
        for (index, (actual_status, expected_status)) in [
            (ActualStatus::Done, TaskStatus::Done),
            (ActualStatus::Ongoing, TaskStatus::Active),
        ]
        .into_iter()
        .enumerate()
        {
            let task_id = id(index as u128 + 1);
            let mut store = Store::new();
            store.upsert_task(task(task_id, Tier::SemiPublic, DeferPolicy::RescheduleAsap));

            assert_eq!(
                reconcile(
                    &mut store,
                    &[log_actual(task_id, actual_status, None, at(1))]
                ),
                Ok(())
            );
            assert_eq!(store.tasks[&task_id].status, expected_status);
        }
    }

    #[test]
    fn capture_edits_and_preferences_are_applied() {
        let mut store = Store::new();
        let existing_id = id(1);
        let captured = task(id(2), Tier::UserShared, DeferPolicy::ReturnToBacklog);
        let pref = preference(10, 11);
        store.upsert_task(task(
            existing_id,
            Tier::SemiPublic,
            DeferPolicy::RescheduleAsap,
        ));

        let additions = [
            log_capture(captured.clone(), at(1)),
            log_edit_dep(existing_id, vec![captured.id], at(2)),
            log_edit_due(existing_id, Some(at(100)), at(3)),
            log_edit_pref(pref.clone(), false, at(4)),
        ];
        assert_eq!(reconcile(&mut store, &additions), Ok(()));
        assert_eq!(store.tasks[&captured.id], captured);
        assert_eq!(store.tasks[&existing_id].blocked_by, vec![id(2)]);
        assert_eq!(store.tasks[&existing_id].due, Some(at(100)));
        assert_eq!(store.preferences, vec![pref.clone()]);

        assert_eq!(
            reconcile(&mut store, &[log_edit_pref(pref, true, at(5))]),
            Ok(())
        );
        assert!(store.preferences.is_empty());
    }

    #[test]
    fn reconcile_orders_entries_by_timestamp() {
        let captured = task(id(1), Tier::SemiPublic, DeferPolicy::RescheduleAsap);
        let capture_first = [
            log_actual(captured.id, ActualStatus::Done, None, at(2)),
            log_capture(captured.clone(), at(1)),
        ];
        let mut store = Store::new();

        assert_eq!(reconcile(&mut store, &capture_first), Ok(()));
        assert_eq!(store.tasks[&captured.id].status, TaskStatus::Done);

        let actual_first = [
            log_capture(captured.clone(), at(2)),
            log_actual(captured.id, ActualStatus::Done, None, at(1)),
        ];
        let mut store = Store::new();
        assert_eq!(
            reconcile(&mut store, &actual_first),
            Err(CoreError::UnknownTask { id: captured.id })
        );
        assert!(!store.tasks.contains_key(&captured.id));
    }

    #[test]
    fn blind_actual_resolves_a_top_secret_task() {
        let task_id = id(1);
        let mut store = Store::new();
        store.upsert_task(task(task_id, Tier::TopSecret, DeferPolicy::RescheduleAsap));

        assert_eq!(
            reconcile(
                &mut store,
                &[log_actual(task_id, ActualStatus::Done, None, at(1))]
            ),
            Ok(())
        );
        assert_eq!(store.tasks[&task_id].status, TaskStatus::Done);
    }

    #[test]
    fn task_referencing_commands_and_facts_reject_unknown_ids() {
        let unknown = id(99);
        let entries = [
            log_defer(unknown, at(1)),
            log_actual(unknown, ActualStatus::Done, None, at(1)),
            log_edit_dep(unknown, vec![id(1)], at(1)),
            log_edit_due(unknown, Some(at(100)), at(1)),
        ];

        for entry in entries {
            assert_eq!(
                reconcile(&mut Store::new(), &[entry]),
                Err(CoreError::UnknownTask { id: unknown })
            );
        }
    }

    #[test]
    fn reconcile_is_deterministic_and_equal_at_order_is_stable() {
        let first_id = id(1);
        let second_id = id(2);
        let mut original = Store::new();
        original.upsert_task(task(
            first_id,
            Tier::SemiPublic,
            DeferPolicy::RescheduleAsap,
        ));
        original.upsert_task(task(
            second_id,
            Tier::UserShared,
            DeferPolicy::ReturnToBacklog,
        ));
        let timestamp = at(1);
        let log = vec![
            log_actual(first_id, ActualStatus::Done, None, timestamp),
            log_defer(second_id, timestamp),
        ];
        let mut left = original.clone();
        let mut right = original.clone();

        assert_eq!(reconcile(&mut left, &log), Ok(()));
        assert_eq!(reconcile(&mut right, &log), Ok(()));
        assert_eq!(left, right);

        let mut reordered = original.clone();
        let reversed_nonconflicting = [log[1].clone(), log[0].clone()];
        assert_eq!(reconcile(&mut reordered, &reversed_nonconflicting), Ok(()));
        assert_eq!(left, reordered);

        let conflicting = [
            log_actual(first_id, ActualStatus::Done, None, timestamp),
            log_actual(first_id, ActualStatus::Ongoing, None, timestamp),
        ];
        let mut forward = original.clone();
        let mut reverse = original;
        assert_eq!(reconcile(&mut forward, &conflicting), Ok(()));
        assert_eq!(
            reconcile(
                &mut reverse,
                &[conflicting[1].clone(), conflicting[0].clone()]
            ),
            Ok(())
        );
        assert_eq!(forward.tasks[&first_id].status, TaskStatus::Active);
        assert_eq!(reverse.tasks[&first_id].status, TaskStatus::Done);
    }
}
