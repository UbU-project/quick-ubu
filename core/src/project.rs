//! Content-safe projection of the authoritative store and plan.

use serde::{Deserialize, Serialize};

use crate::{
    visible_as_content, Handle, HandleStatus, Objective, Plan, Store, Task, TaskStatus, Tier,
};

/// The content and opaque schedule surface available at a given clearance.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ShareableSlice {
    pub clearance: Tier,
    pub tasks: Vec<Task>,
    pub objectives: Vec<Objective>,
    pub handles: Vec<Handle>,
}

/// Project authoritative state into a content-safe slice for `clearance`.
pub fn project(store: &Store, plan: &Plan, clearance: Tier) -> ShareableSlice {
    let mut tasks: Vec<_> = store
        .tasks
        .values()
        .filter(|task| visible_as_content(task.tier, clearance))
        .cloned()
        .collect();
    tasks.sort_by_key(|task| task.id);

    let mut objectives: Vec<_> = store
        .objectives
        .values()
        .filter(|objective| visible_as_content(objective.tier, clearance))
        .cloned()
        .collect();
    objectives.sort_by_key(|objective| objective.id);

    let mut handles: Vec<_> = store
        .tasks
        .values()
        .filter(|task| !visible_as_content(task.tier, clearance))
        .filter_map(|task| {
            let status = match task.status {
                TaskStatus::Backlog | TaskStatus::Scheduled => HandleStatus::Scheduled,
                TaskStatus::Active => HandleStatus::Active,
                TaskStatus::Done | TaskStatus::Deferred => return None,
            };
            let entry = plan.entries.iter().find(|entry| entry.item == task.id)?;

            Some(Handle {
                id: task.id,
                window: Some(entry.window.clone()),
                duration: task.est_duration,
                status,
                deferrable: true,
            })
        })
        .collect();
    handles.sort_by_key(|handle| handle.id);

    ShareableSlice {
        clearance,
        tasks,
        objectives,
        handles,
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use chrono::{Duration, TimeZone, Utc};
    use proptest::prelude::*;
    use uuid::Uuid;

    use super::*;
    use crate::{
        DeferPolicy, ObjectiveStatus, PlanAuthority, Provenance, ScheduleEntry, TimeWindow,
    };

    fn id(value: u128) -> Uuid {
        Uuid::from_u128(value)
    }

    fn window(offset: i64) -> TimeWindow {
        let start = Utc.timestamp_opt(1_700_000_000 + offset, 0).unwrap();
        TimeWindow {
            start,
            end: start + Duration::minutes(30),
        }
    }

    fn task(id: Uuid, tier: Tier, status: TaskStatus, marker: &str) -> Task {
        Task {
            id,
            tier,
            title: format!("TASK_TITLE_{marker}_END"),
            detail: Some(format!("TASK_DETAIL_{marker}_END")),
            objective_ids: Vec::new(),
            skills: vec![format!("skill-{marker}")],
            affect_cost: 1,
            est_duration: Duration::minutes(30),
            due: None,
            earliest_start: None,
            category: None,
            pinned: None,
            transparent: false,
            blocked_by: Vec::new(),
            defer_policy: DeferPolicy::ReturnToBacklog,
            status,
            provenance: Provenance::Manual,
            reminders: Vec::new(),
            commitment: None,
        }
    }

    fn objective(id: Uuid, tier: Tier, marker: &str) -> Objective {
        Objective {
            id,
            tier,
            title: format!("OBJECTIVE_TITLE_{marker}_END"),
            detail: Some(format!("OBJECTIVE_DETAIL_{marker}_END")),
            target_date: None,
            status: ObjectiveStatus::Active,
        }
    }

    fn plan(entries: Vec<ScheduleEntry>) -> Plan {
        Plan {
            id: id(10_000),
            created_at: Utc.timestamp_opt(1_700_000_000, 0).unwrap(),
            authority: PlanAuthority::Authoritative,
            clearance: Tier::TopSecret,
            entries,
            objective_etas: BTreeMap::new(),
            conflicts: Vec::new(),
        }
    }

    fn entry(item: Uuid, offset: i64) -> ScheduleEntry {
        ScheduleEntry {
            item,
            window: window(offset),
            is_handle: false,
        }
    }

    fn tier(value: u8) -> Tier {
        match value {
            0 => Tier::SemiPublic,
            1 => Tier::UserShared,
            2 => Tier::TopSecret,
            _ => unreachable!(),
        }
    }

    fn task_status(value: u8) -> TaskStatus {
        match value {
            0 => TaskStatus::Backlog,
            1 => TaskStatus::Scheduled,
            2 => TaskStatus::Active,
            3 => TaskStatus::Done,
            4 => TaskStatus::Deferred,
            _ => unreachable!(),
        }
    }

    type TaskSpec = (u8, u8, bool, String);
    type ObjectiveSpec = (u8, String);

    fn arbitrary_store_and_plan() -> impl Strategy<Value = (Store, Plan)> {
        (
            prop::collection::vec((0u8..3, 0u8..5, any::<bool>(), "[A-Za-z0-9]{12}"), 0..12),
            prop::collection::vec((0u8..3, "[A-Za-z0-9]{12}"), 0..12),
        )
            .prop_map(
                |(task_specs, objective_specs): (Vec<TaskSpec>, Vec<ObjectiveSpec>)| {
                    let mut store = Store::new();
                    let mut entries = Vec::new();

                    for (index, (tier_value, status_value, scheduled, marker)) in
                        task_specs.into_iter().enumerate()
                    {
                        let task_id = id(100 + index as u128);
                        store.upsert_task(task(
                            task_id,
                            tier(tier_value),
                            task_status(status_value),
                            &format!("{index}_{marker}"),
                        ));
                        if scheduled {
                            entries.push(entry(task_id, index as i64 * 60));
                        }
                    }

                    for (index, (tier_value, marker)) in objective_specs.into_iter().enumerate() {
                        store.upsert_objective(objective(
                            id(1_000 + index as u128),
                            tier(tier_value),
                            &format!("{index}_{marker}"),
                        ));
                    }

                    (store, plan(entries))
                },
            )
    }

    fn arbitrary_clearance() -> impl Strategy<Value = Tier> {
        prop_oneof![
            Just(Tier::SemiPublic),
            Just(Tier::UserShared),
            Just(Tier::TopSecret),
        ]
    }

    #[test]
    fn top_secret_clearance_includes_all_content_and_no_handles() {
        let mut store = Store::new();
        for (index, tier) in [Tier::TopSecret, Tier::SemiPublic, Tier::UserShared]
            .into_iter()
            .enumerate()
        {
            store.upsert_task(task(
                id(3 - index as u128),
                tier,
                TaskStatus::Scheduled,
                &format!("task-{index}"),
            ));
            store.upsert_objective(objective(
                id(13 - index as u128),
                tier,
                &format!("objective-{index}"),
            ));
        }

        let slice = project(
            &store,
            &plan(vec![entry(id(1), 0), entry(id(2), 60), entry(id(3), 120)]),
            Tier::TopSecret,
        );

        assert_eq!(slice.clearance, Tier::TopSecret);
        assert_eq!(slice.tasks.len(), 3);
        assert_eq!(slice.objectives.len(), 3);
        assert!(slice.handles.is_empty());
        assert!(slice.tasks.windows(2).all(|pair| pair[0].id < pair[1].id));
        assert!(slice
            .objectives
            .windows(2)
            .all(|pair| pair[0].id < pair[1].id));
    }

    #[test]
    fn semi_public_clearance_filters_content_and_handles() {
        let mut store = Store::new();
        store.upsert_task(task(id(1), Tier::SemiPublic, TaskStatus::Backlog, "public"));
        store.upsert_task(task(
            id(2),
            Tier::UserShared,
            TaskStatus::Scheduled,
            "shared-scheduled",
        ));
        store.upsert_task(task(
            id(3),
            Tier::TopSecret,
            TaskStatus::Active,
            "secret-active",
        ));
        store.upsert_task(task(
            id(4),
            Tier::TopSecret,
            TaskStatus::Backlog,
            "secret-unscheduled",
        ));
        store.upsert_task(task(
            id(5),
            Tier::UserShared,
            TaskStatus::Done,
            "shared-done",
        ));
        store.upsert_objective(objective(id(11), Tier::SemiPublic, "public-objective"));
        store.upsert_objective(objective(id(12), Tier::UserShared, "shared-objective"));
        store.upsert_objective(objective(id(13), Tier::TopSecret, "secret-objective"));

        let slice = project(
            &store,
            &plan(vec![entry(id(2), 0), entry(id(3), 60), entry(id(5), 120)]),
            Tier::SemiPublic,
        );

        assert_eq!(slice.tasks, vec![store.tasks[&id(1)].clone()]);
        assert_eq!(slice.objectives, vec![store.objectives[&id(11)].clone()]);
        assert_eq!(
            slice.handles,
            vec![
                Handle {
                    id: id(2),
                    window: Some(window(0)),
                    duration: Duration::minutes(30),
                    status: HandleStatus::Scheduled,
                    deferrable: true,
                },
                Handle {
                    id: id(3),
                    window: Some(window(60)),
                    duration: Duration::minutes(30),
                    status: HandleStatus::Active,
                    deferrable: true,
                },
            ]
        );
        assert!(!slice.handles.iter().any(|handle| handle.id == id(4)));
        assert!(!slice.handles.iter().any(|handle| handle.id == id(5)));
    }

    proptest! {
        #[test]
        fn serialized_slice_never_contains_above_clearance_content(
            (store, plan) in arbitrary_store_and_plan(),
            clearance in arbitrary_clearance(),
        ) {
            let json = serde_json::to_string(&project(&store, &plan, clearance)).unwrap();

            for task in store.tasks.values().filter(|task| {
                !visible_as_content(task.tier, clearance)
            }) {
                prop_assert!(!json.contains(&task.title));
                if let Some(detail) = &task.detail {
                    prop_assert!(!json.contains(detail));
                }
            }
            for objective in store.objectives.values().filter(|objective| {
                !visible_as_content(objective.tier, clearance)
            }) {
                prop_assert!(!json.contains(&objective.title));
            }
        }

        #[test]
        fn projected_content_and_handles_obey_clearance_partition(
            (store, plan) in arbitrary_store_and_plan(),
            clearance in arbitrary_clearance(),
        ) {
            let slice = project(&store, &plan, clearance);

            let tasks_are_visible = slice.tasks.iter().all(|task| {
                visible_as_content(task.tier, clearance)
            });
            let objectives_are_visible = slice.objectives.iter().all(|objective| {
                visible_as_content(objective.tier, clearance)
            });
            prop_assert!(tasks_are_visible);
            prop_assert!(objectives_are_visible);
            for handle in &slice.handles {
                let source = store.tasks.get(&handle.id);
                prop_assert!(source.is_some());
                prop_assert!(!visible_as_content(source.unwrap().tier, clearance));
            }
        }
    }

    #[test]
    fn handle_shape_does_not_distinguish_source_tier() {
        let mut store = Store::new();
        store.upsert_task(task(
            id(1),
            Tier::UserShared,
            TaskStatus::Scheduled,
            "shared",
        ));
        store.upsert_task(task(
            id(2),
            Tier::TopSecret,
            TaskStatus::Scheduled,
            "secret",
        ));
        let shared_window = window(0);
        let slice = project(
            &store,
            &plan(vec![entry(id(1), 0), entry(id(2), 0)]),
            Tier::SemiPublic,
        );

        assert_eq!(slice.handles.len(), 2);
        assert_eq!(slice.handles[0].window, Some(shared_window.clone()));
        assert_eq!(slice.handles[1].window, Some(shared_window));
        assert_eq!(slice.handles[0].duration, slice.handles[1].duration);
        assert_eq!(slice.handles[0].status, slice.handles[1].status);
        assert_eq!(slice.handles[0].deferrable, slice.handles[1].deferrable);

        let mut shared_json = serde_json::to_value(&slice.handles[0]).unwrap();
        let mut secret_json = serde_json::to_value(&slice.handles[1]).unwrap();
        shared_json.as_object_mut().unwrap().remove("id");
        secret_json.as_object_mut().unwrap().remove("id");
        assert_eq!(shared_json, secret_json);
    }

    #[test]
    fn projection_is_deterministic() {
        let mut store = Store::new();
        store.upsert_task(task(id(2), Tier::TopSecret, TaskStatus::Active, "secret"));
        store.upsert_task(task(id(1), Tier::SemiPublic, TaskStatus::Backlog, "public"));
        store.upsert_objective(objective(id(3), Tier::SemiPublic, "objective"));
        let plan = plan(vec![entry(id(2), 0)]);

        assert_eq!(
            project(&store, &plan, Tier::SemiPublic),
            project(&store, &plan, Tier::SemiPublic)
        );
    }
}
