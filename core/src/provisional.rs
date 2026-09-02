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
