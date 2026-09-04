//! Deterministic soft placement behind the [`Planner`] boundary.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, NaiveDate, Utc};

use crate::plan::{ComputeTarget, Conflict, Plan, PlanAuthority, ScheduleEntry};
use crate::precompute::{resolve_preferences, topo_order, AffectBudget};
use crate::store::Store;
use crate::types::{visible_as_content, CoreError, Handle, Id, TaskStatus, TimeWindow};

#[derive(Debug, Clone, PartialEq)]
pub struct Placeable {
    pub task_id: Id,
    pub duration: Duration,
    pub affect_cost: i32,
    pub earliest_floor: DateTime<Utc>,
    pub due: Option<DateTime<Utc>>,
    pub sched_predecessors: Vec<Id>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlacementInput {
    pub items: Vec<Placeable>,
    pub fixed_occupied: Vec<TimeWindow>,
    pub budget: AffectBudget,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlacementOutput {
    pub entries: Vec<ScheduleEntry>,
    pub conflicts: Vec<Conflict>,
}

pub trait Planner {
    fn place(&self, input: &PlacementInput) -> PlacementOutput;
}

pub struct DeterministicPlacer;

impl Planner for DeterministicPlacer {
    fn place(&self, input: &PlacementInput) -> PlacementOutput {
        let mut occupied = input.fixed_occupied.clone();
        occupied.sort_by_key(|window| (window.start, window.end));

        let mut placed_end: BTreeMap<Id, DateTime<Utc>> = BTreeMap::new();
        let mut day_affect: BTreeMap<NaiveDate, i32> = BTreeMap::new();
        let mut entries = Vec::new();
        let mut conflicts = Vec::new();

        for item in &input.items {
            let mut start_floor = item.earliest_floor;
            let mut predecessor_unplaced = false;
            for predecessor in &item.sched_predecessors {
                if let Some(end) = placed_end.get(predecessor) {
                    start_floor = start_floor.max(*end);
                } else {
                    predecessor_unplaced = true;
                    break;
                }
            }
            if predecessor_unplaced {
                conflicts.push(Conflict {
                    item: item.task_id,
                    reason: "predecessor unplaced".to_string(),
                });
                continue;
            }

            if item.affect_cost > input.budget.cap {
                conflicts.push(Conflict {
                    item: item.task_id,
                    reason: "affect_cost exceeds daily budget".to_string(),
                });
                continue;
            }

            let mut start = start_floor;
            loop {
                start = earliest_gap(start, item.duration, &occupied);
                let day = start.date_naive();
                let load = day_affect.get(&day).copied().unwrap_or(0);
                if load + item.affect_cost <= input.budget.cap {
                    break;
                }
                start = next_day_start(day);
            }

            let end = start + item.duration;
            let window = TimeWindow { start, end };
            entries.push(ScheduleEntry {
                item: item.task_id,
                window: window.clone(),
                is_handle: false,
            });
            occupied.push(window);
            occupied.sort_by_key(|window| (window.start, window.end));
            placed_end.insert(item.task_id, end);
            // P-1 charges a task wholly to its start day, even across midnight.
            *day_affect.entry(start.date_naive()).or_insert(0) += item.affect_cost;

            if item.due.is_some_and(|due| end > due) {
                conflicts.push(Conflict {
                    item: item.task_id,
                    reason: "placed after due date".to_string(),
                });
            }
        }

        PlacementOutput { entries, conflicts }
    }
}

pub fn re_plan(
    store: &Store,
    target: ComputeTarget,
    planned_at: DateTime<Utc>,
    horizon_start: DateTime<Utc>,
    fixed_blocks: &[Handle],
    budget: &AffectBudget,
    planner: &dyn Planner,
) -> Result<Plan, CoreError> {
    let clearance = target.clearance();
    let authority = match target {
        ComputeTarget::DesktopOllama => PlanAuthority::Authoritative,
        ComputeTarget::HostedLlm => PlanAuthority::Provisional,
    };

    // These checks deliberately stay outside the planner trait.
    topo_order(store)?;
    let preference_classes = resolve_preferences(store)?;
    let rank: BTreeMap<Id, usize> = preference_classes
        .into_iter()
        .enumerate()
        .flat_map(|(index, class)| class.into_iter().map(move |task_id| (task_id, index)))
        .collect();

    let candidates: BTreeSet<Id> = store
        .tasks
        .values()
        .filter(|task| {
            matches!(task.status, TaskStatus::Backlog | TaskStatus::Scheduled)
                && visible_as_content(task.tier, clearance)
        })
        .map(|task| task.id)
        .collect();
    let pinned_candidates: BTreeSet<Id> = candidates
        .iter()
        .filter(|task_id| store.tasks[task_id].pinned.is_some())
        .copied()
        .collect();
    let unpinned_candidates: BTreeSet<Id> =
        candidates.difference(&pinned_candidates).copied().collect();
    let order = candidate_order(store, &unpinned_candidates, &rank);

    let mut items = Vec::new();
    let mut conflicts = Vec::new();
    for task_id in order {
        let task = &store.tasks[&task_id];
        let mut earliest_floor = task
            .earliest_start
            .unwrap_or(horizon_start)
            .max(horizon_start);
        let mut sched_predecessors = Vec::new();
        let mut unresolved_hidden_precedence = false;
        let mut predecessor_in_flight = false;

        for predecessor_id in &task.blocked_by {
            let predecessor = &store.tasks[predecessor_id];
            if predecessor.status == TaskStatus::Done {
                continue;
            }

            if let Some(window) = &predecessor.pinned {
                earliest_floor = earliest_floor.max(window.end);
                continue;
            }

            let fixed_end = fixed_blocks
                .iter()
                .filter(|block| block.id == *predecessor_id)
                .filter_map(|block| block.window.as_ref().map(|window| window.end))
                .max();
            if let Some(end) = fixed_end {
                earliest_floor = earliest_floor.max(end);
            } else if unpinned_candidates.contains(predecessor_id) {
                // Keep excluded candidates here so the placer surfaces the
                // downstream "predecessor unplaced" cascade.
                sched_predecessors.push(*predecessor_id);
            } else if !visible_as_content(predecessor.tier, clearance) {
                unresolved_hidden_precedence = true;
            } else {
                predecessor_in_flight = true;
            }
        }

        if unresolved_hidden_precedence {
            conflicts.push(Conflict {
                item: task.id,
                reason: "unresolved hidden precedence".to_string(),
            });
            continue;
        }

        if predecessor_in_flight {
            conflicts.push(Conflict {
                item: task.id,
                reason: "predecessor in flight".to_string(),
            });
            continue;
        }

        items.push(Placeable {
            task_id: task.id,
            duration: task.est_duration,
            affect_cost: task.affect_cost,
            earliest_floor,
            due: task.due,
            sched_predecessors,
        });
    }

    let mut fixed_occupied: Vec<TimeWindow> = fixed_blocks
        .iter()
        .filter_map(|block| block.window.clone())
        .collect();
    fixed_occupied.extend(
        pinned_candidates
            .iter()
            .filter_map(|task_id| store.tasks[task_id].pinned.clone()),
    );
    let output = planner.place(&PlacementInput {
        items,
        fixed_occupied,
        budget: budget.clone(),
    });
    conflicts.extend(output.conflicts);

    let mut entries = output.entries;
    entries.extend(pinned_candidates.iter().map(|task_id| {
        ScheduleEntry {
            item: *task_id,
            window: store.tasks[task_id]
                .pinned
                .clone()
                .expect("pinned candidate has a window"),
            is_handle: false,
        }
    }));
    entries.sort_by_key(|entry| (entry.window.start, entry.item));

    let entry_ends: BTreeMap<Id, DateTime<Utc>> = entries
        .iter()
        .map(|entry| (entry.item, entry.window.end))
        .collect();
    let objective_etas = store
        .objectives
        .keys()
        .map(|objective_id| {
            let mut latest: Option<DateTime<Utc>> = None;
            let mut all_scheduled = true;
            for task in store
                .tasks
                .values()
                .filter(|task| task.objective_ids.contains(objective_id))
            {
                match entry_ends.get(&task.id) {
                    Some(end) => latest = Some(latest.map_or(*end, |current| current.max(*end))),
                    None => all_scheduled = false,
                }
            }
            (*objective_id, all_scheduled.then_some(latest).flatten())
        })
        .collect();

    Ok(Plan {
        id: uuid::Uuid::new_v4(),
        created_at: planned_at,
        authority,
        clearance,
        entries,
        objective_etas,
        conflicts,
    })
}

pub fn next_task(store: &Store, plan: &Plan, now: DateTime<Utc>) -> Option<Id> {
    plan.entries
        .iter()
        .filter(|entry| !entry.is_handle && entry.window.end > now)
        .filter(|entry| {
            store
                .tasks
                .get(&entry.item)
                .is_some_and(|task| task.pinned.is_none())
        })
        .min_by_key(|entry| (entry.window.start, entry.item))
        .map(|entry| entry.item)
}

fn candidate_order(
    store: &Store,
    candidates: &BTreeSet<Id>,
    rank: &BTreeMap<Id, usize>,
) -> Vec<Id> {
    let mut indegree: BTreeMap<Id, usize> =
        candidates.iter().map(|task_id| (*task_id, 0)).collect();
    let mut dependents: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
    for task_id in candidates {
        for predecessor_id in &store.tasks[task_id].blocked_by {
            if candidates.contains(predecessor_id) {
                *indegree.get_mut(task_id).expect("candidate has indegree") += 1;
                dependents
                    .entry(*predecessor_id)
                    .or_default()
                    .push(*task_id);
            }
        }
    }

    let priority = |task_id: Id| (rank.get(&task_id).copied().unwrap_or(usize::MAX), task_id);
    let mut ready: BTreeSet<(usize, Id)> = indegree
        .iter()
        .filter(|(_, degree)| **degree == 0)
        .map(|(task_id, _)| priority(*task_id))
        .collect();
    let mut order = Vec::with_capacity(candidates.len());
    while let Some(next) = ready.iter().next().copied() {
        ready.remove(&next);
        let task_id = next.1;
        order.push(task_id);
        if let Some(blocked) = dependents.get(&task_id) {
            for dependent_id in blocked {
                let degree = indegree
                    .get_mut(dependent_id)
                    .expect("dependent is a candidate");
                *degree -= 1;
                if *degree == 0 {
                    ready.insert(priority(*dependent_id));
                }
            }
        }
    }

    debug_assert_eq!(order.len(), candidates.len());
    order
}

fn earliest_gap(
    mut candidate: DateTime<Utc>,
    duration: Duration,
    occupied: &[TimeWindow],
) -> DateTime<Utc> {
    loop {
        let end = candidate + duration;
        match occupied
            .iter()
            .find(|window| candidate < window.end && window.start < end)
        {
            Some(window) => candidate = candidate.max(window.end),
            None => return candidate,
        }
    }
}

fn next_day_start(day: NaiveDate) -> DateTime<Utc> {
    day.succ_opt()
        .and_then(|next| next.and_hms_opt(0, 0, 0))
        .expect("the placement horizon must fit chrono's date range")
        .and_utc()
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use uuid::Uuid;

    use super::*;
    use crate::{DeferPolicy, Provenance, Task, Tier};

    fn id(value: u128) -> Id {
        Uuid::from_u128(value)
    }

    fn at(seconds: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + seconds, 0).unwrap()
    }

    fn task(value: u128, status: TaskStatus, blocked_by: Vec<Id>) -> Task {
        Task {
            id: id(value),
            tier: Tier::UserShared,
            title: format!("task-{value}"),
            detail: None,
            objective_ids: Vec::new(),
            skills: Vec::new(),
            affect_cost: 0,
            est_duration: Duration::minutes(30),
            due: None,
            earliest_start: None,
            category: None,
            pinned: None,
            transparent: false,
            blocked_by,
            defer_policy: DeferPolicy::ReturnToBacklog,
            status,
            provenance: Provenance::Manual,
            commitment: None,
        }
    }

    fn plan_with_predecessor_status(status: TaskStatus) -> Plan {
        let mut store = Store::new();
        store.upsert_task(task(1, status, Vec::new()));
        store.upsert_task(task(2, TaskStatus::Backlog, vec![id(1)]));

        re_plan(
            &store,
            ComputeTarget::DesktopOllama,
            at(0),
            at(0),
            &[],
            &AffectBudget { cap: 10 },
            &DeterministicPlacer,
        )
        .expect("the dependency graph is valid")
    }

    fn assert_predecessor_in_flight(status: TaskStatus) {
        let plan = plan_with_predecessor_status(status);

        assert!(!plan.entries.iter().any(|entry| entry.item == id(2)));
        assert!(plan.conflicts.contains(&Conflict {
            item: id(2),
            reason: "predecessor in flight".to_string(),
        }));
    }

    #[test]
    fn active_predecessor_conflicts_and_excludes_dependent() {
        assert_predecessor_in_flight(TaskStatus::Active);
    }

    #[test]
    fn deferred_predecessor_conflicts_and_excludes_dependent() {
        assert_predecessor_in_flight(TaskStatus::Deferred);
    }

    #[test]
    fn done_predecessor_allows_dependent_to_be_scheduled() {
        let plan = plan_with_predecessor_status(TaskStatus::Done);

        assert!(plan.entries.iter().any(|entry| entry.item == id(2)));
        assert!(!plan.conflicts.iter().any(|conflict| conflict.item == id(2)));
    }
}
