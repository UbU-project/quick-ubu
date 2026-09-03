//! Device-side provisional re-planning over a redacted shareable slice.

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};

use crate::{
    re_plan, AffectBudget, ComputeTarget, CoreError, Handle, Id, Plan, PlanAuthority, Planner,
    ShareableSlice, Store,
};

pub fn provisional_replan(
    slice: &ShareableSlice,
    deferred: &BTreeSet<Id>,
    planned_at: DateTime<Utc>,
    horizon_start: DateTime<Utc>,
    budget: &AffectBudget,
    planner: &dyn Planner,
) -> Result<Plan, CoreError> {
    let mut store = Store::new();
    for task in &slice.tasks {
        store.upsert_task(task.clone());
    }
    for objective in &slice.objectives {
        store.upsert_objective(objective.clone());
    }

    let active_handles: Vec<Handle> = slice
        .handles
        .iter()
        .filter(|handle| !deferred.contains(&handle.id))
        .cloned()
        .collect();

    let mut plan = re_plan(
        &store,
        ComputeTarget::DesktopOllama,
        planned_at,
        horizon_start,
        &active_handles,
        budget,
        planner,
    )?;
    plan.authority = PlanAuthority::Provisional;
    plan.clearance = slice.clearance;

    Ok(plan)
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use proptest::prelude::*;
    use uuid::Uuid;

    use super::*;
    use crate::{
        DeferPolicy, DeterministicPlacer, HandleStatus, Provenance, Task, TaskStatus, Tier,
        TimeWindow,
    };

    fn id(value: u128) -> Id {
        Uuid::from_u128(value)
    }

    fn at(minutes: i64) -> DateTime<Utc> {
        Utc.timestamp_opt(1_700_000_000 + minutes * 60, 0).unwrap()
    }

    fn task(value: u128, minutes: i64) -> Task {
        Task {
            id: id(value),
            tier: Tier::UserShared,
            title: format!("task-{value}"),
            detail: None,
            objective_ids: Vec::new(),
            skills: Vec::new(),
            affect_cost: 0,
            est_duration: Duration::minutes(minutes),
            due: None,
            earliest_start: None,
            category: None,
            pinned: None,
            blocked_by: Vec::new(),
            defer_policy: DeferPolicy::ReturnToBacklog,
            status: TaskStatus::Backlog,
            provenance: Provenance::Manual,
            commitment: None,
        }
    }

    fn handle(value: u128, start_minutes: i64, duration_minutes: i64) -> Handle {
        Handle {
            id: id(value),
            window: Some(TimeWindow {
                start: at(start_minutes),
                end: at(start_minutes + duration_minutes),
            }),
            duration: Duration::minutes(duration_minutes),
            status: HandleStatus::Scheduled,
            deferrable: true,
        }
    }

    fn slice(tasks: Vec<Task>, handles: Vec<Handle>) -> ShareableSlice {
        ShareableSlice {
            clearance: Tier::UserShared,
            tasks,
            objectives: Vec::new(),
            handles,
        }
    }

    fn replan(slice: &ShareableSlice, deferred: &BTreeSet<Id>) -> Plan {
        provisional_replan(
            slice,
            deferred,
            at(0),
            at(0),
            &AffectBudget { cap: 100 },
            &DeterministicPlacer,
        )
        .expect("independent slice tasks are valid")
    }

    fn overlaps(left: &TimeWindow, right: &TimeWindow) -> bool {
        left.start < right.end && right.start < left.end
    }

    fn occupied_slice() -> ShareableSlice {
        slice(vec![task(1, 45), task(2, 45)], vec![handle(100, 60, 60)])
    }

    #[test]
    fn schedules_slice_tasks_around_active_handles_with_slice_metadata() {
        let slice = occupied_slice();
        let plan = replan(&slice, &BTreeSet::new());
        let occupied = slice.handles[0].window.as_ref().unwrap();

        assert_eq!(plan.entries.len(), 2);
        assert!(plan.entries.iter().all(|entry| !entry.is_handle));
        assert!(plan
            .entries
            .iter()
            .all(|entry| !overlaps(&entry.window, occupied)));
        assert_eq!(plan.authority, PlanAuthority::Provisional);
        assert_eq!(plan.clearance, slice.clearance);
    }

    #[test]
    fn deferring_a_handle_frees_its_window() {
        let slice = occupied_slice();
        let occupied = slice.handles[0].window.as_ref().unwrap();
        let active_plan = replan(&slice, &BTreeSet::new());
        let deferred = BTreeSet::from([slice.handles[0].id]);
        let deferred_plan = replan(&slice, &deferred);

        assert!(active_plan
            .entries
            .iter()
            .all(|entry| !overlaps(&entry.window, occupied)));
        assert!(deferred_plan
            .entries
            .iter()
            .any(|entry| overlaps(&entry.window, occupied)));
    }

    proptest! {
        #[test]
        fn non_deferred_handle_windows_are_never_overlapped(
            task_durations in prop::collection::vec(1i64..121, 1..9),
            handle_specs in prop::collection::vec((0i64..481, 1i64..121), 0..9),
        ) {
            let tasks = task_durations
                .iter()
                .enumerate()
                .map(|(index, minutes)| task(index as u128 + 1, *minutes))
                .collect();
            let handles: Vec<_> = handle_specs
                .iter()
                .enumerate()
                .map(|(index, (start, duration))| {
                    handle(index as u128 + 1_000, *start, *duration)
                })
                .collect();
            let slice = slice(tasks, handles);
            let plan = replan(&slice, &BTreeSet::new());

            prop_assert_eq!(plan.entries.len(), task_durations.len());
            for entry in &plan.entries {
                for handle in &slice.handles {
                    if let Some(window) = &handle.window {
                        prop_assert!(!overlaps(&entry.window, window));
                    }
                }
            }
        }
    }

    #[test]
    fn empty_preferences_fall_back_to_deterministic_id_order() {
        let slice = slice(vec![task(3, 30), task(1, 30), task(2, 30)], Vec::new());
        let plan = replan(&slice, &BTreeSet::new());

        assert_eq!(
            plan.entries
                .iter()
                .map(|entry| entry.item)
                .collect::<Vec<_>>(),
            vec![id(1), id(2), id(3)]
        );
    }

    #[test]
    fn provisional_replanning_is_deterministic_except_for_plan_identity() {
        let slice = occupied_slice();
        let deferred = BTreeSet::new();
        let left = replan(&slice, &deferred);
        let right = replan(&slice, &deferred);

        assert_eq!(left.entries, right.entries);
        assert_eq!(left.conflicts, right.conflicts);
        assert_eq!(left.objective_etas, right.objective_etas);
    }
}
