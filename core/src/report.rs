//! Pure time totals from pinned windows and timestamped dynamic completions.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};

use crate::{ActualStatus, CommandKind, FactKind, LogEntryKind, Store};

/// Count pinned overlaps and each unretracted unpinned Done fact in the inclusive window.
/// Transparency and current task status do not affect inclusion. Categories
/// and estimated durations are read from the current task; missing tasks are skipped.
pub fn report_by_category(
    store: &Store,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
) -> BTreeMap<String, Duration> {
    let mut totals = BTreeMap::new();
    if from > to {
        return totals;
    }

    for task in store.tasks.values() {
        let Some(window) = &task.pinned else {
            continue;
        };
        let overlap = window.end.min(to) - window.start.max(from);
        if overlap > Duration::zero() {
            let category = task.category.as_deref().unwrap_or("(uncategorized)");
            *totals.entry(category.to_string()).or_default() += overlap;
        }
    }

    // A correction retracts its original fact even when the correction itself
    // falls outside the report window. Other completions of that task still count.
    let undone: BTreeSet<_> = store
        .log
        .iter()
        .filter_map(|entry| match &entry.kind {
            LogEntryKind::Command(CommandKind::UndoCompletion {
                task_id,
                completion_id,
            }) => Some((*task_id, *completion_id)),
            _ => None,
        })
        .collect();
    for entry in &store.log {
        if entry.at < from || entry.at > to {
            continue;
        }
        let LogEntryKind::Fact(FactKind::Actual {
            item_id,
            status: ActualStatus::Done,
            actual,
        }) = &entry.kind
        else {
            continue;
        };
        if undone.contains(&(*item_id, entry.id)) {
            continue;
        }
        let Some(task) = store.tasks.get(item_id) else {
            continue;
        };
        if task.pinned.is_some() {
            continue;
        }
        let duration = actual
            .as_ref()
            .map(|window| window.end - window.start)
            .unwrap_or(task.est_duration);
        let category = task.category.as_deref().unwrap_or("(uncategorized)");
        *totals.entry(category.to_string()).or_default() += duration;
    }

    totals
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        log_actual, log_defer, DeferPolicy, Id, Provenance, Task, TaskStatus, Tier, TimeWindow,
    };

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(minutes * 60, 0).unwrap()
    }

    fn window(start: i64, end: i64) -> TimeWindow {
        TimeWindow {
            start: at(start),
            end: at(end),
        }
    }

    fn task(id: u128, category: Option<&str>, pinned: Option<TimeWindow>) -> Task {
        Task {
            id: Id::from_u128(id),
            tier: Tier::UserShared,
            title: format!("Task {id}"),
            detail: None,
            objective_ids: Vec::new(),
            skills: Vec::new(),
            tags: Vec::new(),
            affect_cost: 0,
            est_duration: Duration::minutes(30),
            due: None,
            earliest_start: None,
            category: category.map(str::to_owned),
            pinned,
            transparent: false,
            reminders: Vec::new(),
            blocked_by: Vec::new(),
            after: Vec::new(),
            must_finish_by: None,
            defer_policy: DeferPolicy::RescheduleAsap,
            status: TaskStatus::Backlog,
            provenance: Provenance::Manual,
            commitment: None,
        }
    }

    #[test]
    fn pinned_windows_are_clamped_and_outside_or_touching_windows_are_excluded() {
        let mut store = Store::new();
        for task in [
            task(1, Some("inside"), Some(window(20, 50))),
            task(2, Some("outside"), Some(window(110, 150))),
            task(3, Some("clipped"), Some(window(-10, 10))),
            task(4, Some("clipped"), Some(window(90, 110))),
            task(5, Some("encompassing"), Some(window(-10, 110))),
            task(6, Some("touching"), Some(window(100, 120))),
            task(7, Some("touching"), Some(window(-20, 0))),
        ] {
            store.upsert_task(task);
        }
        let before = store.clone();
        assert_eq!(
            report_by_category(&store, at(0), at(100)),
            BTreeMap::from([
                ("inside".into(), Duration::minutes(30)),
                ("clipped".into(), Duration::minutes(20)),
                ("encompassing".into(), Duration::minutes(100)),
            ])
        );
        assert_eq!(store, before);
    }

    #[test]
    fn transparent_pinned_tasks_are_counted_regardless_of_status_or_tier() {
        let mut store = Store::new();
        let mut transparent = task(1, Some("personal"), Some(window(10, 70)));
        transparent.transparent = true;
        transparent.status = TaskStatus::Deferred;
        transparent.tier = Tier::TopSecret;
        store.upsert_task(transparent);
        assert_eq!(
            report_by_category(&store, at(0), at(100))["personal"],
            Duration::hours(1)
        );
    }

    #[test]
    fn dynamic_done_uses_actual_or_estimate_and_pinned_actual_is_not_double_counted() {
        let mut store = Store::new();
        for id in 1..=3 {
            store.upsert_task(task(id, Some("work"), None));
        }
        store.upsert_task(task(4, Some("routine"), Some(window(10, 20))));
        store.append_log(log_actual(
            Id::from_u128(1),
            ActualStatus::Done,
            Some(window(-60, 180)),
            at(50),
        ));
        store.append_log(log_actual(
            Id::from_u128(2),
            ActualStatus::Done,
            None,
            at(0),
        ));
        store.append_log(log_actual(
            Id::from_u128(3),
            ActualStatus::Done,
            None,
            at(100),
        ));
        store.append_log(log_actual(
            Id::from_u128(4),
            ActualStatus::Done,
            Some(window(0, 100)),
            at(50),
        ));
        assert_eq!(
            report_by_category(&store, at(0), at(100)),
            BTreeMap::from([
                ("work".into(), Duration::minutes(300)),
                ("routine".into(), Duration::minutes(10)),
            ])
        );
    }

    #[test]
    fn unrelated_logs_outside_completions_and_unlogged_done_tasks_are_not_counted() {
        let mut store = Store::new();
        let mut dynamic = task(1, Some("work"), None);
        dynamic.status = TaskStatus::Done;
        store.upsert_task(dynamic);
        store.append_log(log_actual(
            Id::from_u128(1),
            ActualStatus::Done,
            None,
            at(-1),
        ));
        store.append_log(log_actual(
            Id::from_u128(1),
            ActualStatus::Done,
            None,
            at(101),
        ));
        store.append_log(log_actual(
            Id::from_u128(1),
            ActualStatus::Ongoing,
            None,
            at(50),
        ));
        store.append_log(log_actual(
            Id::from_u128(999),
            ActualStatus::Done,
            None,
            at(50),
        ));
        store.append_log(log_defer(Id::from_u128(1), at(50)));
        assert!(report_by_category(&store, at(0), at(100)).is_empty());
    }

    #[test]
    fn uncategorized_pinned_and_dynamic_durations_share_a_bucket() {
        let mut store = Store::new();
        store.upsert_task(task(1, None, Some(window(10, 70))));
        store.upsert_task(task(2, None, None));
        store.append_log(log_actual(
            Id::from_u128(2),
            ActualStatus::Done,
            None,
            at(50),
        ));
        assert_eq!(
            report_by_category(&store, at(0), at(100)),
            BTreeMap::from([("(uncategorized)".into(), Duration::minutes(90)),])
        );
    }

    #[test]
    fn every_done_fact_is_counted_and_reversed_windows_are_empty() {
        let mut store = Store::new();
        store.upsert_task(task(1, Some("work"), None));
        store.append_log(log_actual(
            Id::from_u128(1),
            ActualStatus::Done,
            None,
            at(50),
        ));
        store.append_log(log_actual(
            Id::from_u128(1),
            ActualStatus::Done,
            None,
            at(50),
        ));
        assert_eq!(
            report_by_category(&store, at(50), at(50))["work"],
            Duration::hours(1)
        );
        assert!(report_by_category(&store, at(100), at(0)).is_empty());
    }
}
