//! Recent tagged completions used as classifier context, never active tasks.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::{ActualStatus, FactKind, LogEntryKind, Store, TaskStatus};

#[derive(Debug, Clone, PartialEq)]
pub struct CompletedExample {
    pub title: String,
    pub tags: Vec<String>,
    pub category: Option<String>,
    pub duration: Duration,
    pub completed_at: DateTime<Utc>,
}

/// The `limit` most-recently-completed tasks that HAVE at least one tag.
pub fn recent_completed_examples(store: &Store, limit: usize) -> Vec<CompletedExample> {
    if limit == 0 {
        return Vec::new();
    }
    let mut completions = BTreeMap::new();
    for entry in &store.log {
        if let LogEntryKind::Fact(FactKind::Actual {
            item_id,
            status: ActualStatus::Done,
            ..
        }) = &entry.kind
        {
            let latest = completions.entry(*item_id).or_insert(entry.at);
            *latest = (*latest).max(entry.at);
        }
    }
    let mut examples: Vec<_> = completions
        .into_iter()
        .filter_map(|(id, completed_at)| {
            let task = store.tasks.get(&id)?;
            if task.status != TaskStatus::Done || task.tags.is_empty() {
                return None;
            }
            Some(CompletedExample {
                title: task.title.clone(),
                tags: task.tags.clone(),
                category: task.category.clone(),
                duration: task.est_duration,
                completed_at,
            })
        })
        .collect();
    // Stable sorting preserves ascending task ID order for tied timestamps.
    examples.sort_by_key(|example| std::cmp::Reverse(example.completed_at));
    examples.truncate(limit);
    examples
}
