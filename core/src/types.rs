//! v1 data types and the crate error enum.
//!
//! Derive policy: `Debug, Clone, PartialEq, Serialize, Deserialize` everywhere,
//! plus `Eq` only where every field is `Eq`. Per the C-1 contract,
//! `chrono::Duration` is treated as non-`Eq`, so every type that (transitively)
//! carries one derives `PartialEq` only: [`Task`], [`Handle`], [`CommandKind`],
//! [`LogEntryKind`], [`LogEntry`].

use serde::{Deserialize, Serialize};

/// Stable identity for every stored entity.
pub type Id = uuid::Uuid;

/// Restrictiveness axis: `SemiPublic` lowest, `TopSecret` highest.
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
pub enum Tier {
    SemiPublic,
    UserShared,
    TopSecret,
}

impl Tier {
    pub fn level(self) -> u8 {
        match self {
            Tier::SemiPublic => 0,
            Tier::UserShared => 1,
            Tier::TopSecret => 2,
        }
    }
}

/// true  => full content visible to a target of this clearance (schedulable)
/// false => represented as a Handle only (fixed, opaque)
pub fn visible_as_content(task_tier: Tier, clearance: Tier) -> bool {
    task_tier.level() <= clearance.level()
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TimeWindow {
    pub start: chrono::DateTime<chrono::Utc>,
    pub end: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ObjectiveStatus {
    Active,
    Done,
    Dropped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Objective {
    pub id: Id,
    pub tier: Tier,
    pub title: String,
    pub detail: Option<String>,
    pub target_date: Option<chrono::DateTime<chrono::Utc>>,
    pub status: ObjectiveStatus,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TaskStatus {
    Backlog,
    Scheduled,
    Active,
    Done,
    Deferred,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DeferPolicy {
    RescheduleAsap,
    DeferUntil(chrono::DateTime<chrono::Utc>),
    ReturnToBacklog,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Provenance {
    Manual,
    Crawled { source: String, ref_id: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commitment {
    pub person: String,
    pub note: Option<String>,
}

/// `PartialEq` only: `est_duration` is a `chrono::Duration`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
    pub id: Id,
    pub tier: Tier,
    pub title: String,
    pub detail: Option<String>,
    pub objective_ids: Vec<Id>,
    pub skills: Vec<String>,
    /// signed: >0 draining, <0 restorative
    pub affect_cost: i32,
    pub est_duration: chrono::Duration,
    pub due: Option<chrono::DateTime<chrono::Utc>>,
    pub earliest_start: Option<chrono::DateTime<chrono::Utc>>,
    /// A fixed-time commitment: the task is anchored to this exact window and is
    /// NOT placed by the planner. `None` = a dynamic task the planner schedules.
    #[serde(default)]
    pub pinned: Option<TimeWindow>,
    /// precedence edges (dynamic)
    pub blocked_by: Vec<Id>,
    pub defer_policy: DeferPolicy,
    pub status: TaskStatus,
    pub provenance: Provenance,
    pub commitment: Option<Commitment>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum HandleStatus {
    Scheduled,
    Active,
    Deferred,
}

/// `PartialEq` only: `duration` is a `chrono::Duration`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Handle {
    pub id: Id,
    pub window: Option<TimeWindow>,
    pub duration: chrono::Duration,
    pub status: HandleStatus,
    pub deferrable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bundle {
    pub id: Id,
    pub members: std::collections::BTreeSet<Id>,
}

/// Closed relation set; incomparable = ABSENCE of a row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Relation {
    Strict,
    Indifferent,
}

/// `left`/`right` are Bundle ids; `Strict` means left ≻ right, `Indifferent`
/// means left ~ right.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Preference {
    pub left: Id,
    pub right: Id,
    pub relation: Relation,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ActualStatus {
    Ongoing,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum FactKind {
    Actual {
        item_id: Id,
        status: ActualStatus,
        actual: Option<TimeWindow>,
    },
}

/// `PartialEq` only: `Capture` carries a [`Task`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CommandKind {
    Defer {
        handle_id: Id,
    },
    Capture {
        task: Task,
    },
    EditDep {
        task_id: Id,
        blocked_by: Vec<Id>,
    },
    EditDue {
        task_id: Id,
        due: Option<chrono::DateTime<chrono::Utc>>,
    },
    EditPref {
        pref: Preference,
        remove: bool,
    },
}

/// `PartialEq` only: transitively carries a [`Task`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum LogEntryKind {
    Fact(FactKind),
    Command(CommandKind),
}

/// `PartialEq` only: transitively carries a [`Task`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LogEntry {
    pub id: Id,
    pub kind: LogEntryKind,
    pub at: chrono::DateTime<chrono::Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum CoreError {
    UnknownTask { id: Id },
    DanglingDependency { task: Id, missing: Id },
    DanglingObjective { task: Id, missing: Id },
    DanglingBundleMember { bundle: Id, missing: Id },
    DanglingPreferenceBundle { preference_index: usize, missing: Id },
    /// tasks left unresolved by Kahn
    DependencyCycle { involved: Vec<Id> },
    /// strict cycle across indifference classes
    PreferenceCycle { involved: Vec<Id> },
    /// C-1 resolves singletons only
    NonSingletonBundle { bundle: Id },
}
