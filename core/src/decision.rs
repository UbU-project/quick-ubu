use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::Id;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum DecisionSource {
    Elicitation,
    Advisor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrefSuggestion {
    AStrictB,
    BStrictA,
    Indifferent,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Proposal {
    Dependency {
        blocked: Id,
        blocker: Id,
    },
    Preference {
        a: Id,
        b: Id,
        suggested: Option<PrefSuggestion>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PendingDecision {
    pub id: Id,
    pub source: DecisionSource,
    pub proposal: Proposal,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Resolution {
    Confirmed,
    Rejected,
    Skipped,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecisionRecord {
    pub proposal: Proposal,
    pub resolution: Resolution,
    pub at: DateTime<Utc>,
}
