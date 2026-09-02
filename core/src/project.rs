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
