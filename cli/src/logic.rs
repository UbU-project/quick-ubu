use std::collections::BTreeSet;

use chrono::{DateTime, Duration, Utc};
use ubu_core::{
    next_task, re_plan, resolve_preferences, topo_order, AffectBudget, Bundle, ComputeTarget,
    CoreError, DecisionRecord, DecisionSource, DeferPolicy, DeterministicPlacer, Id, Objective,
    ObjectiveStatus, PendingDecision, Planner, Preference, Proposal, Provenance, Relation,
    Resolution, Store, Task, TaskStatus, Tier, TimeWindow,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Answer {
    AStrictB,
    BStrictA,
    Indifferent,
    Skip,
    Confirm,
    Reject,
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
    let blocked = resolve_task_id(store, task_prefix)?;
    let blocker = resolve_task_id(store, blocker_prefix)?;
    dep_add_ids(store, blocked, blocker)
}

pub fn dep_add_ids(store: &mut Store, blocked: Id, blocker: Id) -> Result<(), String> {
    reject_self_pair(blocked, blocker, "dependency")?;

    let task = store
        .tasks
        .get(&blocked)
        .ok_or_else(|| format!("no task matches {blocked}"))?;
    if task.blocked_by.contains(&blocker) {
        return Ok(());
    }
    let mut blocked_by = task.blocked_by.clone();
    blocked_by.push(blocker);
    commit_dependencies(store, blocked, blocked_by)
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
    pref_add_ids(store, a, b, eq)
}

pub fn pref_add_ids(store: &mut Store, a: Id, b: Id, eq: bool) -> Result<(), String> {
    reject_self_pair(a, b, "preference")?;
    if !store.tasks.contains_key(&a) {
        return Err(format!("no task matches {a}"));
    }
    if !store.tasks.contains_key(&b) {
        return Err(format!("no task matches {b}"));
    }

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

pub fn enqueue_incomparable_pairs(store: &mut Store) -> usize {
    let task_ids = store
        .tasks
        .values()
        .filter(|task| {
            matches!(task.status, TaskStatus::Backlog | TaskStatus::Scheduled)
                && task.pinned.is_none()
        })
        .map(|task| task.id)
        .collect::<Vec<_>>();
    let mut added = 0;

    for (index, a) in task_ids.iter().copied().enumerate() {
        for b in task_ids.iter().copied().skip(index + 1) {
            let pair = ordered_pair(a, b);
            let related = store.preferences.iter().any(|preference| {
                let Some(left) = singleton_task_for_bundle(store, preference.left) else {
                    return false;
                };
                let Some(right) = singleton_task_for_bundle(store, preference.right) else {
                    return false;
                };
                ordered_pair(left, right) == pair
            });
            let decided = store
                .decision_history
                .iter()
                .any(|record| preference_pair(&record.proposal) == Some(pair));
            let pending = store
                .pending_decisions
                .iter()
                .any(|decision| preference_pair(&decision.proposal) == Some(pair));
            if related || decided || pending {
                continue;
            }

            store.pending_decisions.push(PendingDecision {
                id: Uuid::new_v4(),
                source: DecisionSource::Elicitation,
                proposal: Proposal::Preference {
                    a,
                    b,
                    suggested: None,
                },
            });
            added += 1;
        }
    }

    added
}

pub fn resolve_decision(
    store: &mut Store,
    decision_id: Id,
    answer: Answer,
) -> Result<Resolution, String> {
    let index = store
        .pending_decisions
        .iter()
        .position(|decision| decision.id == decision_id)
        .ok_or_else(|| format!("no pending decision matches {decision_id}"))?;
    let proposal = store.pending_decisions[index].proposal.clone();

    let resolution = match (&proposal, answer) {
        (Proposal::Preference { a, b, .. }, Answer::AStrictB) => {
            pref_add_ids(store, *a, *b, false)?;
            Resolution::Confirmed
        }
        (Proposal::Preference { a, b, .. }, Answer::BStrictA) => {
            pref_add_ids(store, *b, *a, false)?;
            Resolution::Confirmed
        }
        (Proposal::Preference { a, b, .. }, Answer::Indifferent) => {
            pref_add_ids(store, *a, *b, true)?;
            Resolution::Confirmed
        }
        (Proposal::Preference { .. }, Answer::Skip) => Resolution::Skipped,
        (Proposal::Dependency { blocked, blocker }, Answer::Confirm) => {
            dep_add_ids(store, *blocked, *blocker)?;
            Resolution::Confirmed
        }
        (Proposal::Dependency { .. }, Answer::Reject) => Resolution::Rejected,
        (Proposal::Preference { .. }, Answer::Confirm | Answer::Reject) => {
            return Err("dependency answer is invalid for a preference decision".to_string());
        }
        (
            Proposal::Dependency { .. },
            Answer::AStrictB | Answer::BStrictA | Answer::Indifferent | Answer::Skip,
        ) => {
            return Err("preference answer is invalid for a dependency decision".to_string());
        }
    };

    store.decision_history.push(DecisionRecord {
        proposal,
        resolution: resolution.clone(),
        at: Utc::now(),
    });
    store.pending_decisions.remove(index);
    Ok(resolution)
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

fn singleton_task_for_bundle(store: &Store, bundle_id: Id) -> Option<Id> {
    let bundle = store.bundles.get(&bundle_id)?;
    (bundle.members.len() == 1)
        .then(|| bundle.members.iter().next().copied())
        .flatten()
}

fn preference_pair(proposal: &Proposal) -> Option<(Id, Id)> {
    match proposal {
        Proposal::Preference { a, b, .. } => Some(ordered_pair(*a, *b)),
        Proposal::Dependency { .. } => None,
    }
}

fn ordered_pair(a: Id, b: Id) -> (Id, Id) {
    if a < b {
        (a, b)
    } else {
        (b, a)
    }
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

    #[test]
    fn dep_add_adds_and_rejects_cycles_and_self_dependencies_atomically() {
        let (a, b, _) = graph_ids();
        let mut store = graph_store();

        assert_eq!(dep_add(&mut store, &prefix(a), &prefix(b)), Ok(()));
        assert_eq!(store.tasks[&a].blocked_by, vec![b]);

        let after_add = store.clone();
        assert_eq!(dep_add(&mut store, &prefix(a), &prefix(b)), Ok(()));
        assert_eq!(store, after_add);

        let cycle_error = dep_add(&mut store, &prefix(b), &prefix(a)).unwrap_err();
        assert!(cycle_error.contains("dependency cycle"));
        assert_eq!(store, after_add);

        let self_error = dep_add(&mut store, &prefix(a), &prefix(a)).unwrap_err();
        assert!(self_error.contains("self-dependency"));
        assert_eq!(store, after_add);
    }

    #[test]
    fn dep_rm_removes_and_dep_set_replaces_but_rejects_a_cycle_atomically() {
        let (a, b, c) = graph_ids();
        let mut store = graph_store();

        dep_add(&mut store, &prefix(a), &prefix(b)).unwrap();
        dep_rm(&mut store, &prefix(a), &prefix(b)).unwrap();
        assert!(store.tasks[&a].blocked_by.is_empty());

        dep_set(&mut store, &prefix(a), vec![prefix(b), prefix(c)]).unwrap();
        assert_eq!(store.tasks[&a].blocked_by, vec![b, c]);

        dep_set(&mut store, &prefix(a), vec![prefix(c)]).unwrap();
        dep_set(&mut store, &prefix(b), vec![prefix(a)]).unwrap();
        let before_cycle = store.clone();
        let error = dep_set(&mut store, &prefix(a), vec![prefix(b)]).unwrap_err();
        assert!(error.contains("dependency cycle"));
        assert_eq!(store, before_cycle);
    }

    #[test]
    fn dep_list_reports_one_task_or_all_tasks_with_dependencies() {
        let (a, b, c) = graph_ids();
        let mut store = graph_store();
        dep_add(&mut store, &prefix(a), &prefix(b)).unwrap();

        assert_eq!(
            dep_list(&store, None),
            Ok(vec![(prefix(a), "Alpha".to_string(), vec![prefix(b)],)])
        );
        assert_eq!(
            dep_list(&store, Some(prefix(c))),
            Ok(vec![(prefix(c), "Charlie".to_string(), Vec::new())])
        );
    }

    #[test]
    fn pref_add_creates_and_reuses_singletons_and_rejects_contradictions_atomically() {
        let (a, b, c) = graph_ids();
        let mut store = graph_store();

        pref_add(&mut store, &prefix(a), &prefix(b), false).unwrap();
        assert_eq!(store.bundles.len(), 2);
        assert_eq!(store.preferences.len(), 1);
        assert_eq!(store.preferences[0].relation, Relation::Strict);
        let a_bundle = store.preferences[0].left;

        pref_add(&mut store, &prefix(a), &prefix(c), true).unwrap();
        assert_eq!(store.bundles.len(), 3);
        assert_eq!(store.preferences.len(), 2);
        assert_eq!(store.preferences[1].relation, Relation::Indifferent);
        assert_eq!(store.preferences[1].left, a_bundle);

        let before_contradiction = store.clone();
        let error = pref_add(&mut store, &prefix(b), &prefix(a), false).unwrap_err();
        assert!(error.contains("preference cycle"));
        assert!(error.contains(&a.to_string()));
        assert!(error.contains(&b.to_string()));
        assert_eq!(store, before_contradiction);
    }

    #[test]
    fn pref_rm_removes_a_relation_regardless_of_direction() {
        let (a, b, _) = graph_ids();
        let mut store = graph_store();
        pref_add(&mut store, &prefix(a), &prefix(b), false).unwrap();

        pref_rm(&mut store, &prefix(b), &prefix(a)).unwrap();

        assert!(store.preferences.is_empty());
        assert_eq!(store.bundles.len(), 2);
    }

    #[test]
    fn pref_list_shows_preferences_and_resolved_high_to_low_ranking() {
        let (a, b, c) = graph_ids();
        let mut store = graph_store();
        pref_add(&mut store, &prefix(a), &prefix(b), false).unwrap();
        pref_add(&mut store, &prefix(b), &prefix(c), true).unwrap();

        let lines = pref_list(&store);

        assert_eq!(
            lines[0],
            format!("{} Alpha ≻ {} Bravo", prefix(a), prefix(b))
        );
        assert_eq!(
            lines[1],
            format!("{} Bravo ~ {} Charlie", prefix(b), prefix(c))
        );
        assert_eq!(lines[2], "ranking (high→low):");
        assert_eq!(lines[3], format!("1: {} Alpha", prefix(a)));
        assert_eq!(
            lines[4],
            format!("2: {} Bravo ~ {} Charlie", prefix(b), prefix(c))
        );
    }

    #[test]
    fn dependency_and_preference_commands_reject_unknown_and_ambiguous_prefixes() {
        let (a, b, _) = graph_ids();
        let ambiguous = Uuid::parse_str("aaaabbbb-0000-0000-0000-000000000004").unwrap();
        let mut store = graph_store();
        store.upsert_task(graph_task(ambiguous, "Ambiguous Alpha"));
        let original = store.clone();

        assert_eq!(
            dep_add(&mut store, "missing", &prefix(b)),
            Err("no task matches missing".to_string())
        );
        assert_eq!(
            dep_rm(&mut store, &prefix(a), "missing"),
            Err("no task matches missing".to_string())
        );
        assert_eq!(
            dep_set(&mut store, &prefix(b), vec!["aaaa".to_string()]),
            Err("ambiguous prefix aaaa".to_string())
        );
        assert_eq!(
            dep_list(&store, Some("aaaa".to_string())),
            Err("ambiguous prefix aaaa".to_string())
        );
        assert_eq!(
            pref_add(&mut store, "aaaa", &prefix(b), false),
            Err("ambiguous prefix aaaa".to_string())
        );
        assert_eq!(
            pref_rm(&mut store, &prefix(b), "missing"),
            Err("no task matches missing".to_string())
        );
        assert_eq!(store, original);
    }

    fn graph_ids() -> (Id, Id, Id) {
        (
            Uuid::parse_str("aaaaaaaa-0000-0000-0000-000000000001").unwrap(),
            Uuid::parse_str("bbbbbbbb-0000-0000-0000-000000000002").unwrap(),
            Uuid::parse_str("cccccccc-0000-0000-0000-000000000003").unwrap(),
        )
    }

    fn graph_store() -> Store {
        let (a, b, c) = graph_ids();
        let mut store = Store::new();
        store.upsert_task(graph_task(a, "Alpha"));
        store.upsert_task(graph_task(b, "Bravo"));
        store.upsert_task(graph_task(c, "Charlie"));
        store
    }

    fn graph_task(id: Id, title: &str) -> Task {
        Task {
            id,
            tier: Tier::UserShared,
            title: title.to_string(),
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
            blocked_by: Vec::new(),
            defer_policy: DeferPolicy::RescheduleAsap,
            status: TaskStatus::Backlog,
            provenance: Provenance::Manual,
            commitment: None,
        }
    }

    fn prefix(id: Id) -> String {
        id.simple().to_string()[..8].to_string()
    }

    fn fixed_time() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 1, 14, 0, 0).single().unwrap()
    }
}
