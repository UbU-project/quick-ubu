use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use ubu_core::{
    next_task, re_plan, resolve_preferences, topo_order, AffectBudget, Bundle, ComputeTarget,
    CoreError, DeferPolicy, DeterministicPlacer, Id, Objective, ObjectiveStatus, Planner,
    Preference, Provenance, Relation, Store, Task, TaskStatus, Tier, TimeWindow,
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
    pub pin: Option<DateTime<Utc>>,
    pub category: Option<String>,
    pub transparent: bool,
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
    pub category: Option<String>,
    pub transparent: bool,
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

pub type DependencyRow = (String, String, Vec<String>);

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
    let duration = Duration::minutes(input.duration_minutes);
    let pinned = input.pin.map(|start| TimeWindow {
        start,
        end: start + duration,
    });
    let status = if pinned.is_some() {
        TaskStatus::Scheduled
    } else {
        TaskStatus::Backlog
    };
    store.upsert_task(Task {
        id,
        tier: input.tier,
        title: input.title,
        detail: None,
        objective_ids,
        skills: Vec::new(),
        affect_cost: input.affect_cost,
        est_duration: duration,
        due: input.due,
        earliest_start: input.earliest_start,
        category: input.category,
        pinned,
        transparent: input.transparent,
        blocked_by,
        defer_policy: DeferPolicy::RescheduleAsap,
        status,
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

pub fn singleton_bundle_for(store: &mut Store, task_id: Id) -> Id {
    if let Some(bundle) = store
        .bundles
        .values()
        .find(|bundle| bundle.members.len() == 1 && bundle.members.contains(&task_id))
    {
        return bundle.id;
    }

    let id = Uuid::new_v4();
    store.upsert_bundle(Bundle {
        id,
        members: BTreeSet::from([task_id]),
    });
    id
}

pub fn dep_add(store: &mut Store, task_prefix: &str, blocker_prefix: &str) -> Result<(), String> {
    let task_id = resolve_task_id(store, task_prefix)?;
    let blocker_id = resolve_task_id(store, blocker_prefix)?;
    reject_self_pair(task_id, blocker_id, "dependency")?;

    let task = store
        .tasks
        .get(&task_id)
        .expect("resolved task id must remain in the store");
    if task.blocked_by.contains(&blocker_id) {
        return Ok(());
    }
    let mut blocked_by = task.blocked_by.clone();
    blocked_by.push(blocker_id);
    commit_dependencies(store, task_id, blocked_by)
}

pub fn dep_rm(store: &mut Store, task_prefix: &str, blocker_prefix: &str) -> Result<(), String> {
    let task_id = resolve_task_id(store, task_prefix)?;
    let blocker_id = resolve_task_id(store, blocker_prefix)?;
    let task = store
        .tasks
        .get_mut(&task_id)
        .expect("resolved task id must remain in the store");
    task.blocked_by.retain(|id| *id != blocker_id);
    Ok(())
}

pub fn dep_set(
    store: &mut Store,
    task_prefix: &str,
    blocker_prefixes: Vec<String>,
) -> Result<(), String> {
    let task_id = resolve_task_id(store, task_prefix)?;
    let blocked_by = blocker_prefixes
        .iter()
        .map(|prefix| resolve_task_id(store, prefix))
        .collect::<Result<Vec<_>, _>>()?;
    if blocked_by.contains(&task_id) {
        return Err(format!("task {task_id} cannot depend on itself"));
    }
    commit_dependencies(store, task_id, blocked_by)
}

pub fn dep_list(store: &Store, task_prefix: Option<String>) -> Result<Vec<DependencyRow>, String> {
    let tasks = match task_prefix {
        Some(prefix) => vec![resolve_task_id(store, &prefix)?],
        None => store
            .tasks
            .values()
            .filter(|task| !task.blocked_by.is_empty())
            .map(|task| task.id)
            .collect(),
    };

    Ok(tasks
        .into_iter()
        .map(|task_id| {
            let task = &store.tasks[&task_id];
            (
                short_task_id(task_id),
                task.title.clone(),
                task.blocked_by.iter().copied().map(short_task_id).collect(),
            )
        })
        .collect())
}

pub fn pref_add(store: &mut Store, a_prefix: &str, b_prefix: &str, eq: bool) -> Result<(), String> {
    let a = resolve_task_id(store, a_prefix)?;
    let b = resolve_task_id(store, b_prefix)?;
    reject_self_pair(a, b, "preference")?;

    let mut proposed = store.clone();
    let left = singleton_bundle_for(&mut proposed, a);
    let right = singleton_bundle_for(&mut proposed, b);
    proposed.add_preference(Preference {
        left,
        right,
        relation: if eq {
            Relation::Indifferent
        } else {
            Relation::Strict
        },
    });
    validate_preferences(&proposed)?;
    *store = proposed;
    Ok(())
}

pub fn pref_rm(store: &mut Store, a_prefix: &str, b_prefix: &str) -> Result<(), String> {
    let a = resolve_task_id(store, a_prefix)?;
    let b = resolve_task_id(store, b_prefix)?;
    let Some(left) = existing_singleton_bundle(store, a) else {
        return Ok(());
    };
    let Some(right) = existing_singleton_bundle(store, b) else {
        return Ok(());
    };

    store.preferences.retain(|preference| {
        !((preference.left == left && preference.right == right)
            || (preference.left == right && preference.right == left))
    });
    Ok(())
}

pub fn pref_list(store: &Store) -> Vec<String> {
    let mut lines = store
        .preferences()
        .iter()
        .map(|preference| {
            let relation = match preference.relation {
                Relation::Strict => "≻",
                Relation::Indifferent => "~",
            };
            format!(
                "{} {relation} {}",
                bundle_label(store, preference.left),
                bundle_label(store, preference.right)
            )
        })
        .collect::<Vec<_>>();

    match resolve_preferences(store) {
        Ok(classes) => {
            lines.push("ranking (high→low):".to_string());
            lines.extend(classes.into_iter().enumerate().map(|(index, class)| {
                format!(
                    "{}: {}",
                    index + 1,
                    class
                        .into_iter()
                        .map(|task_id| task_label(store, task_id))
                        .collect::<Vec<_>>()
                        .join(" ~ ")
                )
            }));
        }
        Err(error) => lines.push(format!("ranking error: {error:?}")),
    }
    lines
}

fn commit_dependencies(store: &mut Store, task_id: Id, blocked_by: Vec<Id>) -> Result<(), String> {
    let mut proposed = store.clone();
    proposed
        .tasks
        .get_mut(&task_id)
        .expect("resolved task id must remain in the store")
        .blocked_by = blocked_by.clone();
    validate_dependencies(&proposed)?;
    store
        .tasks
        .get_mut(&task_id)
        .expect("resolved task id must remain in the store")
        .blocked_by = blocked_by;
    Ok(())
}

fn validate_dependencies(store: &Store) -> Result<(), String> {
    match topo_order(store) {
        Ok(_) => Ok(()),
        Err(CoreError::DependencyCycle { involved }) => Err(format!(
            "dependency cycle involving tasks: {}",
            display_ids(&involved)
        )),
        Err(error) => Err(format!("dependency validation failed: {error:?}")),
    }
}

fn validate_preferences(store: &Store) -> Result<(), String> {
    match resolve_preferences(store) {
        Ok(_) => Ok(()),
        Err(CoreError::PreferenceCycle { involved }) => Err(format!(
            "preference cycle involving tasks: {}",
            display_ids(&involved)
        )),
        Err(error) => Err(format!("preference validation failed: {error:?}")),
    }
}

fn reject_self_pair(left: Id, right: Id, kind: &str) -> Result<(), String> {
    if left == right {
        Err(format!("task {left} cannot have a self-{kind}"))
    } else {
        Ok(())
    }
}

fn existing_singleton_bundle(store: &Store, task_id: Id) -> Option<Id> {
    store
        .bundles
        .values()
        .find(|bundle| bundle.members.len() == 1 && bundle.members.contains(&task_id))
        .map(|bundle| bundle.id)
}

fn bundle_label(store: &Store, bundle_id: Id) -> String {
    store
        .bundles
        .get(&bundle_id)
        .and_then(|bundle| {
            (bundle.members.len() == 1)
                .then(|| bundle.members.iter().next().copied())
                .flatten()
        })
        .map(|task_id| task_label(store, task_id))
        .unwrap_or_else(|| format!("bundle {}", short_task_id(bundle_id)))
}

fn task_label(store: &Store, task_id: Id) -> String {
    format!("{} {}", short_task_id(task_id), task_title(store, task_id))
}

fn short_task_id(id: Id) -> String {
    id.simple().to_string()[..8].to_string()
}

fn display_ids(ids: &[Id]) -> String {
    ids.iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(", ")
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
            category: task_category(store, entry.item),
            transparent: task_transparent(store, entry.item),
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

pub fn next(
    store: &Store,
    now: DateTime<Utc>,
    affect_cap: i32,
) -> Result<Option<ScheduleRow>, CoreError> {
    let plan = re_plan(
        store,
        ComputeTarget::DesktopOllama,
        now,
        now,
        &[],
        &AffectBudget { cap: affect_cap },
        &DeterministicPlacer,
    )?;

    Ok(next_task(store, &plan, now).and_then(|task_id| {
        plan.entries
            .iter()
            .find(|entry| entry.item == task_id)
            .map(|entry| ScheduleRow {
                id: task_id,
                title: task_title(store, task_id),
                category: task_category(store, task_id),
                transparent: task_transparent(store, task_id),
                window: entry.window.clone(),
            })
    }))
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

fn task_category(store: &Store, id: Id) -> Option<String> {
    store.tasks.get(&id).and_then(|task| task.category.clone())
}

fn task_transparent(store: &Store, id: Id) -> bool {
    store.tasks.get(&id).is_some_and(|task| task.transparent)
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
                pin: None,
                category: None,
                transparent: false,
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
                pin: None,
                category: None,
                transparent: false,
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
