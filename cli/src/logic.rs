use chrono::{DateTime, Duration, Utc};
use ubu_core::{
    re_plan, AffectBudget, ComputeTarget, CoreError, DeferPolicy, DeterministicPlacer, Id,
    Objective, ObjectiveStatus, Planner, Provenance, Store, Task, TaskStatus, Tier, TimeWindow,
};
use uuid::Uuid;

use crate::persist::{resolve_objective_id, resolve_task_id};

pub struct AddInput {
    pub title: String,
    pub duration_minutes: i64,
    pub tier: Tier,
    pub affect_cost: i32,
    pub due: Option<DateTime<Utc>>,
    pub earliest_start: Option<DateTime<Utc>>,
    pub objective_prefixes: Vec<String>,
    pub blocked_by_prefixes: Vec<String>,
}

pub struct ObjectiveAddInput {
    pub title: String,
    pub tier: Tier,
    pub target_date: Option<DateTime<Utc>>,
}

pub struct TaskRow {
    pub id: Id,
    pub status: TaskStatus,
    pub tier: Tier,
    pub duration_minutes: i64,
    pub affect_cost: i32,
    pub due: Option<DateTime<Utc>>,
    pub title: String,
}

pub struct ReplanOutput {
    pub schedule: Vec<ScheduleRow>,
    pub objective_etas: Vec<ObjectiveEtaRow>,
    pub conflicts: Vec<ConflictRow>,
}

pub struct ScheduleRow {
    pub id: Id,
    pub title: String,
    pub window: TimeWindow,
}

pub struct ObjectiveEtaRow {
    pub title: String,
    pub eta: Option<DateTime<Utc>>,
}

pub struct ConflictRow {
    pub id: Id,
    pub title: String,
    pub reason: String,
}

pub fn parse_tier(value: &str) -> Result<Tier, String> {
    match value {
        "semi-public" => Ok(Tier::SemiPublic),
        "user-shared" => Ok(Tier::UserShared),
        "top-secret" => Ok(Tier::TopSecret),
        _ => Err(format!("unknown tier {value}")),
    }
}

pub fn parse_datetime(value: &str) -> Result<DateTime<Utc>, String> {
    DateTime::parse_from_rfc3339(value)
        .map(|datetime| datetime.with_timezone(&Utc))
        .map_err(|error| format!("invalid RFC3339 datetime {value}: {error}"))
}

pub fn add(store: &mut Store, input: AddInput) -> Result<Id, String> {
    let objective_ids = input
        .objective_prefixes
        .iter()
        .map(|prefix| resolve_objective_id(store, prefix))
        .collect::<Result<Vec<_>, _>>()?;
    let blocked_by = input
        .blocked_by_prefixes
        .iter()
        .map(|prefix| resolve_task_id(store, prefix))
        .collect::<Result<Vec<_>, _>>()?;

    let id = Uuid::new_v4();
    store.upsert_task(Task {
        id,
        tier: input.tier,
        title: input.title,
        detail: None,
        objective_ids,
        skills: Vec::new(),
        affect_cost: input.affect_cost,
        est_duration: Duration::minutes(input.duration_minutes),
        due: input.due,
        earliest_start: input.earliest_start,
        pinned: None,
        blocked_by,
        defer_policy: DeferPolicy::RescheduleAsap,
        status: TaskStatus::Backlog,
        provenance: Provenance::Manual,
        commitment: None,
    });
    Ok(id)
}

pub fn list(store: &Store) -> Vec<TaskRow> {
    store
        .tasks
        .values()
        .map(|task| TaskRow {
            id: task.id,
            status: task.status.clone(),
            tier: task.tier,
            duration_minutes: task.est_duration.num_minutes(),
            affect_cost: task.affect_cost,
            due: task.due,
            title: task.title.clone(),
        })
        .collect()
}

pub fn done(store: &mut Store, prefix: &str) -> Result<(), String> {
    set_status(store, prefix, TaskStatus::Done)
}

pub fn defer(store: &mut Store, prefix: &str) -> Result<(), String> {
    set_status(store, prefix, TaskStatus::Deferred)
}

pub fn objective_add(store: &mut Store, input: ObjectiveAddInput) -> Id {
    let id = Uuid::new_v4();
    store.upsert_objective(Objective {
        id,
        tier: input.tier,
        title: input.title,
        detail: None,
        target_date: input.target_date,
        status: ObjectiveStatus::Active,
    });
    id
}

pub fn replan(
    store: &Store,
    now: DateTime<Utc>,
    horizon: DateTime<Utc>,
    affect_cap: i32,
) -> Result<ReplanOutput, CoreError> {
    replan_with_planner(store, now, horizon, affect_cap, &DeterministicPlacer)
}

pub fn replan_with_planner(
    store: &Store,
    now: DateTime<Utc>,
    horizon: DateTime<Utc>,
    affect_cap: i32,
    planner: &dyn Planner,
) -> Result<ReplanOutput, CoreError> {
    let plan = re_plan(
        store,
        ComputeTarget::DesktopOllama,
        now,
        horizon,
        &[],
        &AffectBudget { cap: affect_cap },
        planner,
    )?;

    let mut schedule = plan
        .entries
        .into_iter()
        .map(|entry| ScheduleRow {
            id: entry.item,
            title: task_title(store, entry.item),
            window: entry.window,
        })
        .collect::<Vec<_>>();
    schedule.sort_by_key(|entry| entry.window.start);

    let objective_etas = plan
        .objective_etas
        .into_iter()
        .map(|(id, eta)| ObjectiveEtaRow {
            title: store
                .objectives
                .get(&id)
                .map(|objective| objective.title.clone())
                .unwrap_or_else(|| "<unknown>".to_string()),
            eta,
        })
        .collect();

    let conflicts = plan
        .conflicts
        .into_iter()
        .map(|conflict| ConflictRow {
            id: conflict.item,
            title: task_title(store, conflict.item),
            reason: conflict.reason,
        })
        .collect();

    Ok(ReplanOutput {
        schedule,
        objective_etas,
        conflicts,
    })
}

fn set_status(store: &mut Store, prefix: &str, status: TaskStatus) -> Result<(), String> {
    let id = resolve_task_id(store, prefix)?;
    let task = store
        .tasks
        .get_mut(&id)
        .expect("resolved task id must remain in the store");
    task.status = status;
    Ok(())
}

fn task_title(store: &Store, id: Id) -> String {
    store
        .tasks
        .get(&id)
        .map(|task| task.title.clone())
        .unwrap_or_else(|| "<unknown>".to_string())
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    #[test]
    fn tier_parsing_accepts_all_kebab_names_and_rejects_garbage() {
        assert_eq!(parse_tier("semi-public"), Ok(Tier::SemiPublic));
        assert_eq!(parse_tier("user-shared"), Ok(Tier::UserShared));
        assert_eq!(parse_tier("top-secret"), Ok(Tier::TopSecret));
        assert_eq!(
            parse_tier("garbage"),
            Err("unknown tier garbage".to_string())
        );
    }

    #[test]
    fn add_then_replan_starts_at_horizon_and_sets_objective_eta() {
        let mut store = Store::new();
        let objective_id = objective_add(
            &mut store,
            ObjectiveAddInput {
                title: "Dogfood CLI".to_string(),
                tier: Tier::UserShared,
                target_date: None,
            },
        );
        let task_id = add(
            &mut store,
            AddInput {
                title: "Run first loop".to_string(),
                duration_minutes: 30,
                tier: Tier::UserShared,
                affect_cost: 10,
                due: None,
                earliest_start: None,
                objective_prefixes: vec![objective_id.simple().to_string()[..8].to_string()],
                blocked_by_prefixes: Vec::new(),
            },
        )
        .expect("task should be added");
        let horizon = fixed_time();

        let output = replan(&store, horizon, horizon, 100).expect("replan should succeed");

        assert_eq!(output.schedule.len(), 1);
        assert_eq!(output.schedule[0].id, task_id);
        assert_eq!(output.schedule[0].window.start, horizon);
        assert_eq!(
            output.schedule[0].window.end,
            horizon + Duration::minutes(30)
        );
        assert_eq!(output.objective_etas.len(), 1);
        assert_eq!(
            output.objective_etas[0].eta,
            Some(horizon + Duration::minutes(30))
        );
        assert!(output.conflicts.is_empty());
    }

    #[test]
    fn over_cap_task_is_a_conflict_and_not_scheduled() {
        let mut store = Store::new();
        let task_id = add(
            &mut store,
            AddInput {
                title: "Too draining".to_string(),
                duration_minutes: 30,
                tier: Tier::UserShared,
                affect_cost: 101,
                due: None,
                earliest_start: None,
                objective_prefixes: Vec::new(),
                blocked_by_prefixes: Vec::new(),
            },
        )
        .expect("task should be added");
        let now = fixed_time();

        let output = replan(&store, now, now, 100).expect("replan should succeed");

        assert!(output.schedule.is_empty());
        assert_eq!(output.conflicts.len(), 1);
        assert_eq!(output.conflicts[0].id, task_id);
        assert_eq!(
            output.conflicts[0].reason,
            "affect_cost exceeds daily budget"
        );
    }

    fn fixed_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, 14, 0, 0).single().unwrap()
    }
}
