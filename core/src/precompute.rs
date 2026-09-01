//! Deterministic precompute: `topo_order`, `resolve_preferences`,
//! `affect_violations`.
//!
//! Every traversal walks `BTreeMap`/`BTreeSet` structures and every tie is
//! broken by ascending `Id`, so identical input stores yield identical output.

use std::collections::{BTreeMap, BTreeSet};

use crate::store::Store;
use crate::types::{CoreError, Id};

/// Kahn's algorithm over all tasks: every id in `task.blocked_by` must precede
/// `task`. Among ready nodes the numerically-lowest `Id` is always emitted next.
///
/// A `blocked_by` entry referencing a non-existent task yields
/// [`CoreError::DanglingDependency`]; tasks left unresolved yield
/// [`CoreError::DependencyCycle`] with their ids sorted.
pub fn topo_order(store: &Store) -> Result<Vec<Id>, CoreError> {
    // `indegree[t]` counts unemitted predecessors; `dependents[d]` lists the
    // tasks blocked by `d`. Duplicate `blocked_by` entries count once each on
    // both sides, so they cancel out.
    let mut indegree: BTreeMap<Id, usize> = store.tasks.keys().map(|id| (*id, 0)).collect();
    let mut dependents: BTreeMap<Id, Vec<Id>> = BTreeMap::new();

    for (task_id, task) in &store.tasks {
        for blocker in &task.blocked_by {
            if !store.tasks.contains_key(blocker) {
                return Err(CoreError::DanglingDependency {
                    task: *task_id,
                    missing: *blocker,
                });
            }
            *indegree.entry(*task_id).or_insert(0) += 1;
            dependents.entry(*blocker).or_default().push(*task_id);
        }
    }

    let mut ready: BTreeSet<Id> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(id, _)| *id)
        .collect();

    let mut order = Vec::with_capacity(store.tasks.len());
    while let Some(next) = ready.iter().next().copied() {
        ready.remove(&next);
        order.push(next);
        if let Some(blocked) = dependents.get(&next) {
            for dependent in blocked {
                let degree = indegree
                    .get_mut(dependent)
                    .expect("dependents only holds known task ids");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert(*dependent);
                }
            }
        }
    }

    if order.len() != store.tasks.len() {
        let emitted: BTreeSet<Id> = order.into_iter().collect();
        let involved: Vec<Id> = store
            .tasks
            .keys()
            .filter(|id| !emitted.contains(id))
            .copied()
            .collect();
        return Err(CoreError::DependencyCycle { involved });
    }

    Ok(order)
}
