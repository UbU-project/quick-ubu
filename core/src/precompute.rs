//! Deterministic precompute: `topo_order`, `resolve_preferences`,
//! `affect_violations`.
//!
//! Every traversal walks `BTreeMap`/`BTreeSet` structures and every tie is
//! broken by ascending `Id`, so identical input stores yield identical output.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

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

/// Union-find over `Id`s. Roots are kept at the numerically-lowest member, so a
/// root *is* the canonical class representative.
#[derive(Debug, Default)]
struct UnionFind {
    parent: BTreeMap<Id, Id>,
}

impl UnionFind {
    fn make_set(&mut self, id: Id) {
        self.parent.entry(id).or_insert(id);
    }

    fn find(&mut self, id: Id) -> Id {
        let mut root = id;
        while let Some(parent) = self.parent.get(&root).copied() {
            if parent == root {
                break;
            }
            root = parent;
        }
        // Path compression.
        let mut cursor = id;
        while let Some(parent) = self.parent.get(&cursor).copied() {
            if parent == root {
                break;
            }
            self.parent.insert(cursor, root);
            cursor = parent;
        }
        root
    }

    fn union(&mut self, left: Id, right: Id) {
        let (left_root, right_root) = (self.find(left), self.find(right));
        if left_root == right_root {
            return;
        }
        // Attach the higher root under the lower one to keep roots minimal.
        let (low, high) = if left_root < right_root {
            (left_root, right_root)
        } else {
            (right_root, left_root)
        };
        self.parent.insert(high, low);
    }
}

/// Resolve singleton-bundle preferences into a total order over indifference
/// classes, ranked high → low.
///
/// `Indifferent` preferences fuse their two tasks into one class; each `Strict`
/// (left ≻ right) preference adds a quotient edge class(left) → class(right).
/// The quotient is ordered with Kahn's algorithm, tie-broken by the class
/// representative (its numerically-lowest member) ascending; each returned class
/// is sorted by ascending `Id`. Only tasks appearing in at least one preference
/// are ranked — absence means incomparable.
pub fn resolve_preferences(store: &Store) -> Result<Vec<Vec<Id>>, CoreError> {
    // Resolve every preference to its (left task, right task) pair first, so
    // dangling/non-singleton bundles are reported before any ordering work.
    let mut pairs: Vec<(Id, Id, bool)> = Vec::with_capacity(store.preferences.len());
    for (preference_index, preference) in store.preferences.iter().enumerate() {
        let left = singleton_member(store, preference.left, preference_index)?;
        let right = singleton_member(store, preference.right, preference_index)?;
        let strict = matches!(preference.relation, crate::types::Relation::Strict);
        pairs.push((left, right, strict));
    }

    let mut union_find = UnionFind::default();
    for (left, right, _) in &pairs {
        union_find.make_set(*left);
        union_find.make_set(*right);
    }
    for (left, right, strict) in &pairs {
        if !*strict {
            union_find.union(*left, *right);
        }
    }

    // Indifference classes, keyed by their canonical representative.
    let mut classes: BTreeMap<Id, BTreeSet<Id>> = BTreeMap::new();
    let mentioned: Vec<Id> = union_find.parent.keys().copied().collect();
    for task_id in mentioned {
        let representative = union_find.find(task_id);
        classes.entry(representative).or_default().insert(task_id);
    }

    // Quotient graph: an edge means the source class ranks higher.
    let mut indegree: BTreeMap<Id, usize> = classes.keys().map(|rep| (*rep, 0)).collect();
    let mut lower: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
    for (left, right, strict) in &pairs {
        if !*strict {
            continue;
        }
        let (high, low) = (union_find.find(*left), union_find.find(*right));
        *indegree.entry(low).or_insert(0) += 1;
        lower.entry(high).or_default().push(low);
    }

    let mut ready: BTreeSet<Id> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(rep, _)| *rep)
        .collect();

    let mut ranked: Vec<Id> = Vec::with_capacity(classes.len());
    while let Some(next) = ready.iter().next().copied() {
        ready.remove(&next);
        ranked.push(next);
        if let Some(below) = lower.get(&next) {
            for representative in below {
                let degree = indegree
                    .get_mut(representative)
                    .expect("quotient edges only reference known classes");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert(*representative);
                }
            }
        }
    }

    if ranked.len() != classes.len() {
        let emitted: BTreeSet<Id> = ranked.into_iter().collect();
        let involved: Vec<Id> = classes
            .iter()
            .filter(|(rep, _)| !emitted.contains(rep))
            .flat_map(|(_, members)| members.iter().copied())
            .collect::<BTreeSet<Id>>()
            .into_iter()
            .collect();
        return Err(CoreError::PreferenceCycle { involved });
    }

    Ok(ranked
        .into_iter()
        .map(|rep| classes[&rep].iter().copied().collect())
        .collect())
}

/// The single member of a preference's bundle, or the matching error.
fn singleton_member(store: &Store, bundle_id: Id, preference_index: usize) -> Result<Id, CoreError> {
    let bundle = store
        .bundles
        .get(&bundle_id)
        .ok_or(CoreError::DanglingPreferenceBundle {
            preference_index,
            missing: bundle_id,
        })?;
    if bundle.members.len() != 1 {
        return Err(CoreError::NonSingletonBundle { bundle: bundle_id });
    }
    Ok(*bundle
        .members
        .iter()
        .next()
        .expect("length checked to be exactly one"))
}

/// Opaque placeholder until the planner supplies windows.
pub type WindowKey = String;

/// Max net affect load per window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffectBudget {
    pub cap: i32,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AffectViolation {
    pub window: WindowKey,
    pub load: i32,
    pub cap: i32,
    pub tasks: Vec<Id>,
}

/// Sum each group's `affect_cost` and report the windows over budget.
///
/// A group id absent from the store yields [`CoreError::UnknownTask`]. A
/// violation is emitted iff `load > budget.cap`, with `tasks` sorted by ascending
/// `Id`; violations come back sorted by `window` key ascending.
pub fn affect_violations(
    groups: &BTreeMap<WindowKey, Vec<Id>>,
    store: &Store,
    budget: &AffectBudget,
) -> Result<Vec<AffectViolation>, CoreError> {
    // `groups` is a BTreeMap, so iteration is already window-key ascending.
    let mut violations = Vec::new();
    for (window, task_ids) in groups {
        let mut load: i32 = 0;
        for task_id in task_ids {
            let task = store
                .tasks
                .get(task_id)
                .ok_or(CoreError::UnknownTask { id: *task_id })?;
            // Saturating: an overflowing sum is still unambiguously over cap.
            load = load.saturating_add(task.affect_cost);
        }
        if load > budget.cap {
            let mut tasks = task_ids.clone();
            tasks.sort();
            violations.push(AffectViolation {
                window: window.clone(),
                load,
                cap: budget.cap,
                tasks,
            });
        }
    }
    Ok(violations)
}
