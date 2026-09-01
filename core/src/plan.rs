//! Disposable planning output and target/authority metadata.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{Id, Tier, TimeWindow};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ComputeTarget {
    DesktopOllama,
    HostedLlm,
}

impl ComputeTarget {
    pub fn clearance(self) -> Tier {
        match self {
            ComputeTarget::DesktopOllama => Tier::TopSecret,
            ComputeTarget::HostedLlm => Tier::SemiPublic,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PlanAuthority {
    Authoritative,
    Provisional,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScheduleEntry {
    pub item: Id,
    pub window: TimeWindow,
    pub is_handle: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Conflict {
    pub item: Id,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Plan {
    pub id: Id,
    pub created_at: DateTime<Utc>,
    pub authority: PlanAuthority,
    pub clearance: Tier,
    pub entries: Vec<ScheduleEntry>,
    pub objective_etas: BTreeMap<Id, Option<DateTime<Utc>>>,
    pub conflicts: Vec<Conflict>,
}
