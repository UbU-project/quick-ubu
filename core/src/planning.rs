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
    pub must_finish_by: Option<DateTime<Utc>>,
    pub due: Option<DateTime<Utc>>,
    pub sched_predecessors: Vec<Id>,
    pub after_refs: Vec<(Id, Duration)>,
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

        'items: for item in &input.items {
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

            let mut after_error = None;
            for (reference, offset) in &item.after_refs {
                match placed_end.get(reference) {
                    Some(end) => match end.checked_add_signed(*offset) {
                        Some(floor) => start_floor = start_floor.max(floor),
                        None => {
                            after_error = Some("after-reference offset out of range");
                            break;
                        }
                    },
                    None => {
                        after_error = Some("after-reference unplaced");
                        break;
                    }
                }
            }
            if let Some(reason) = after_error {
                conflicts.push(Conflict {
                    item: item.task_id,
                    reason: reason.to_string(),
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
                // Bound every candidate before advancing the affect-budget scan.
                // Equality fits: this is a completion ceiling, not a start cutoff.
                if item.must_finish_by.is_some_and(|deadline| start + item.duration > deadline) {
                    conflicts.push(Conflict {
                        item: item.task_id,
                        reason: "does not fit before deadline".to_string(),
                    });
                    continue 'items;
                }
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
    validate_temporal_dependencies(store)?;
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
    let order = candidate_order(store, &unpinned_candidates, &rank)?;

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

        let mut after_refs = Vec::new();
        let mut after_error = None;
        for reference in &task.after {
            let Some(predecessor) = store.tasks.get(&reference.task_id) else {
                after_error = Some("unresolved after-reference");
                break;
            };
            if predecessor.status == TaskStatus::Done {
                continue;
            }
            let fixed_end = predecessor
                .pinned
                .as_ref()
                .map(|window| window.end)
                .or_else(|| {
                    fixed_blocks
                        .iter()
                        .filter(|block| block.id == reference.task_id)
                        .filter_map(|block| block.window.as_ref().map(|window| window.end))
                        .max()
                });
            if let Some(end) = fixed_end {
                match end.checked_add_signed(reference.offset) {
                    Some(floor) => earliest_floor = earliest_floor.max(floor),
                    None => {
                        after_error = Some("after-reference offset out of range");
                        break;
                    }
                }
            } else if unpinned_candidates.contains(&reference.task_id) {
                after_refs.push((reference.task_id, reference.offset));
            } else {
                after_error = Some("unresolved after-reference");
                break;
            }
        }
        if let Some(reason) = after_error {
            conflicts.push(Conflict {
                item: task.id,
                reason: reason.to_string(),
            });
            continue;
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
            must_finish_by: task.must_finish_by,
            due: task.due,
            sched_predecessors,
            after_refs,
        });
    }

    let mut fixed_occupied: Vec<TimeWindow> = fixed_blocks
        .iter()
        .filter_map(|block| block.window.clone())
        .collect();
    fixed_occupied.extend(
        pinned_candidates
            .iter()
            .filter(|task_id| !store.tasks[task_id].transparent)
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

/// Validate the combined precedence graph without changing `topo_order`'s API.
/// Missing after references are left for planning to surface as task conflicts.
/// As with dependency validation, cycles include all stored tasks and statuses.
pub fn validate_temporal_dependencies(store: &Store) -> Result<(), CoreError> {
    topo_order(store)?;
    candidate_order(
        store,
        &store.tasks.keys().copied().collect(),
        &BTreeMap::new(),
    )?;
    Ok(())
}

fn candidate_order(
    store: &Store,
    candidates: &BTreeSet<Id>,
    rank: &BTreeMap<Id, usize>,
) -> Result<Vec<Id>, CoreError> {
    let mut indegree: BTreeMap<Id, usize> =
        candidates.iter().map(|task_id| (*task_id, 0)).collect();
    let mut dependents: BTreeMap<Id, Vec<Id>> = BTreeMap::new();
    for task_id in candidates {
        let task = &store.tasks[task_id];
        let predecessors: BTreeSet<_> = task
            .blocked_by
            .iter()
            .copied()
            .chain(task.after.iter().map(|reference| reference.task_id))
            .collect();
        for predecessor_id in &predecessors {
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

    if order.len() != candidates.len() {
        return Err(CoreError::DependencyCycle {
            involved: indegree
                .into_iter()
                .filter(|(_, degree)| *degree > 0)
                .map(|(id, _)| id)
                .collect(),
        });
    }
    Ok(order)
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
            after: Vec::new(),
            must_finish_by: None,
            defer_policy: DeferPolicy::ReturnToBacklog,
            status,
            provenance: Provenance::Manual,
            reminders: Vec::new(),
            commitment: None,
        }
    }

    fn after_store() -> Store {
        let mut store = Store::new();
        store.upsert_task(task(9, TaskStatus::Backlog, vec![]));
        let mut successor = task(1, TaskStatus::Backlog, vec![]);
        successor.after.push(crate::AfterConstraint {
            task_id: id(9),
            offset: Duration::minutes(60),
        });
        store.upsert_task(successor);
        store
    }

    fn after_plan(
        store: &Store,
        fixed: &[Handle],
        target: ComputeTarget,
    ) -> Result<Plan, CoreError> {
        re_plan(
            store,
            target,
            at(0),
            at(0),
            fixed,
            &AffectBudget { cap: 10 },
            &DeterministicPlacer,
        )
    }

    fn window(plan: &Plan, value: u128) -> &TimeWindow {
        &plan
            .entries
            .iter()
            .find(|entry| entry.item == id(value))
            .unwrap()
            .window
    }

    fn after_conflict(plan: &Plan, reason: &str) {
        assert!(!plan.entries.iter().any(|entry| entry.item == id(1)));
        assert!(plan.conflicts.contains(&Conflict {
            item: id(1),
            reason: reason.into()
        }));
    }

    #[test]
    fn after_orders_before_rank_and_uses_placed_end_plus_sixty_minutes() {
        let store = after_store();
        assert_eq!(
            candidate_order(
                &store,
                &store.tasks.keys().copied().collect(),
                &[(id(1), 0), (id(9), 1)].into_iter().collect()
            )
            .unwrap(),
            vec![id(9), id(1)]
        );
        let plan = after_plan(&store, &[], ComputeTarget::DesktopOllama).unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(
            window(&plan, 1).start,
            window(&plan, 9).end + Duration::minutes(60)
        );
    }

    #[test]
    fn after_pinned_and_fixed_block_references_use_end_plus_thirty_minutes() {
        for pinned in [true, false] {
            let mut store = after_store();
            store.tasks.get_mut(&id(1)).unwrap().after[0].offset = Duration::minutes(30);
            let reference = store.tasks.get_mut(&id(9)).unwrap();
            reference.status = TaskStatus::Active;
            let fixed_window = TimeWindow {
                start: at(0),
                end: at(7200),
            };
            let mut fixed = vec![];
            if pinned {
                reference.pinned = Some(fixed_window);
            } else {
                fixed.push(Handle {
                    id: id(9),
                    window: Some(fixed_window),
                    duration: Duration::hours(2),
                    status: crate::HandleStatus::Active,
                    deferrable: false,
                });
            }
            let plan = after_plan(&store, &fixed, ComputeTarget::DesktopOllama).unwrap();
            assert_eq!(window(&plan, 1).start, at(9000));
        }
    }

    #[test]
    fn after_done_reference_contributes_no_floor_even_with_future_pin() {
        let mut store = after_store();
        let reference = store.tasks.get_mut(&id(9)).unwrap();
        reference.status = TaskStatus::Done;
        reference.pinned = Some(TimeWindow {
            start: at(7200),
            end: at(9000),
        });
        let plan = after_plan(&store, &[], ComputeTarget::DesktopOllama).unwrap();
        assert_eq!(window(&plan, 1).start, at(0));
        assert!(plan.conflicts.is_empty());
    }

    #[test]
    fn after_missing_hidden_active_and_deferred_references_conflict() {
        for mode in ["missing", "hidden", "active", "deferred"] {
            let mut store = after_store();
            store.tasks.get_mut(&id(1)).unwrap().tier = Tier::SemiPublic;
            match mode {
                "missing" => {
                    store.tasks.remove(&id(9));
                }
                "hidden" => store.tasks.get_mut(&id(9)).unwrap().tier = Tier::TopSecret,
                "active" => store.tasks.get_mut(&id(9)).unwrap().status = TaskStatus::Active,
                _ => store.tasks.get_mut(&id(9)).unwrap().status = TaskStatus::Deferred,
            }
            let target = if mode == "hidden" {
                ComputeTarget::HostedLlm
            } else {
                ComputeTarget::DesktopOllama
            };
            after_conflict(
                &after_plan(&store, &[], target).unwrap(),
                "unresolved after-reference",
            );
        }
    }

    #[test]
    fn after_unplaceable_and_excluded_references_cascade_to_unplaced_conflict() {
        for excluded in [false, true] {
            let mut store = after_store();
            if excluded {
                store
                    .tasks
                    .get_mut(&id(9))
                    .unwrap()
                    .after
                    .push(crate::AfterConstraint {
                        task_id: id(99),
                        offset: Duration::zero(),
                    });
            } else {
                store.tasks.get_mut(&id(9)).unwrap().affect_cost = 11;
            }
            after_conflict(
                &after_plan(&store, &[], ComputeTarget::DesktopOllama).unwrap(),
                "after-reference unplaced",
            );
        }
    }

    #[test]
    fn after_only_and_mixed_cycles_are_dependency_errors() {
        for mixed in [false, true] {
            let mut store = after_store();
            let reference = store.tasks.get_mut(&id(9)).unwrap();
            if mixed {
                reference.blocked_by.push(id(1));
            } else {
                reference.after.push(crate::AfterConstraint {
                    task_id: id(1),
                    offset: Duration::zero(),
                });
            }
            assert_eq!(
                after_plan(&store, &[], ComputeTarget::DesktopOllama),
                Err(CoreError::DependencyCycle {
                    involved: vec![id(1), id(9)]
                })
            );
        }
    }

    #[test]
    fn after_combines_multiple_floors_and_duplicate_dependency_edges() {
        let mut store = after_store();
        let mut other = task(8, TaskStatus::Backlog, vec![]);
        other.pinned = Some(TimeWindow {
            start: at(0),
            end: at(7200),
        });
        store.upsert_task(other);
        let successor = store.tasks.get_mut(&id(1)).unwrap();
        successor.blocked_by = vec![id(9)];
        successor.after.push(crate::AfterConstraint {
            task_id: id(8),
            offset: Duration::minutes(90),
        });
        let plan = after_plan(&store, &[], ComputeTarget::DesktopOllama).unwrap();
        assert_eq!(window(&plan, 1).start, at(12600));
        store.tasks.get_mut(&id(1)).unwrap().earliest_start = Some(at(13000));
        let plan = after_plan(&store, &[], ComputeTarget::DesktopOllama).unwrap();
        assert!(plan.conflicts.is_empty());
        assert_eq!(window(&plan, 1).start, at(13000));
    }

    #[test]
    fn after_signed_offset_keeps_forward_order_and_respects_other_floors() {
        let mut store = after_store();
        store.tasks.get_mut(&id(1)).unwrap().after[0].offset = Duration::minutes(-10);
        let plan = after_plan(&store, &[], ComputeTarget::DesktopOllama).unwrap();
        // The reference still places first; occupied time prevents overlap.
        assert_eq!(window(&plan, 1).start, window(&plan, 9).end);
    }

    #[test]
    fn after_offset_datetime_overflow_conflicts_instead_of_panicking() {
        for pinned in [false, true] {
            let mut store = after_store();
            store.tasks.get_mut(&id(1)).unwrap().after[0].offset = Duration::MAX;
            if pinned {
                store.tasks.get_mut(&id(9)).unwrap().pinned = Some(TimeWindow {
                    start: at(0),
                    end: at(1800),
                });
            }
            after_conflict(
                &after_plan(&store, &[], ComputeTarget::DesktopOllama).unwrap(),
                "after-reference offset out of range",
            );
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
