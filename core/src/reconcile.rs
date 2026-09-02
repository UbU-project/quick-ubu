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
