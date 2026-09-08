//! Pure time totals from pinned windows and timestamped dynamic completions.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::{ActualStatus, FactKind, LogEntryKind, Store};

/// Count pinned overlaps and each unpinned Done fact in the inclusive window.
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
