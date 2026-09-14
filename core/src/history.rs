//! Recent tagged completions used as classifier context, never active tasks.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};

use crate::{CompletionFact, Store, TaskStatus};

#[derive(Debug, Clone, PartialEq)]
pub struct CompletedExample {
    pub title: String,
    pub tags: Vec<String>,
    pub category: Option<String>,
    pub duration: Duration,
    pub completed_at: DateTime<Utc>,
}

/// Build tagged examples from the caller's bounded completion query.
pub fn recent_completed_examples(store: &Store, facts: &[CompletionFact]) -> Vec<CompletedExample> {
    let mut completions = BTreeMap::new();
    for fact in facts {
        let latest = completions.entry(fact.item_id).or_insert(fact.at);
        *latest = (*latest).max(fact.at);
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
    examples
}
