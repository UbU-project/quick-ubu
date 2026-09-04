//! The concrete in-memory `Store`: `BTreeMap`-backed so every traversal is in
//! ascending `Id` order and therefore deterministic.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::decision::{DecisionRecord, PendingDecision};
use crate::routine::RoutineTemplate;
use crate::types::{Bundle, CoreError, Id, LogEntry, Objective, Preference, Task};

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Store {
    pub objectives: BTreeMap<Id, Objective>,
    pub tasks: BTreeMap<Id, Task>,
    #[serde(default)]
    pub routines: BTreeMap<Id, RoutineTemplate>,
    pub bundles: BTreeMap<Id, Bundle>,
    pub preferences: Vec<Preference>,
    #[serde(default)]
    pub pending_decisions: Vec<PendingDecision>,
    #[serde(default)]
    pub decision_history: Vec<DecisionRecord>,
    /// task id -> Google Calendar event id, for idempotent create-or-update.
    #[serde(default)]
    pub calendar_links: BTreeMap<Id, String>,
    /// append-only SessionLog
    pub log: Vec<LogEntry>,
}

impl Store {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert_objective(&mut self, objective: Objective) -> Option<Objective> {
        self.objectives.insert(objective.id, objective)
    }

    pub fn get_objective(&self, id: &Id) -> Option<&Objective> {
        self.objectives.get(id)
    }

    pub fn remove_objective(&mut self, id: &Id) -> Option<Objective> {
        self.objectives.remove(id)
    }

    pub fn list_objectives(&self) -> Vec<&Objective> {
        self.objectives.values().collect()
    }

    pub fn upsert_task(&mut self, task: Task) -> Option<Task> {
        self.tasks.insert(task.id, task)
    }

    pub fn get_task(&self, id: &Id) -> Option<&Task> {
        self.tasks.get(id)
    }

    pub fn remove_task(&mut self, id: &Id) -> Option<Task> {
        self.tasks.remove(id)
    }

    pub fn list_tasks(&self) -> Vec<&Task> {
        self.tasks.values().collect()
    }

    pub fn upsert_routine(&mut self, routine: RoutineTemplate) -> Option<RoutineTemplate> {
        self.routines.insert(routine.id, routine)
    }

    pub fn routines(&self) -> &BTreeMap<Id, RoutineTemplate> {
        &self.routines
    }

    pub fn remove_routine(&mut self, id: Id) -> Option<RoutineTemplate> {
        self.routines.remove(&id)
    }

    pub fn upsert_bundle(&mut self, bundle: Bundle) -> Option<Bundle> {
        self.bundles.insert(bundle.id, bundle)
    }

    pub fn get_bundle(&self, id: &Id) -> Option<&Bundle> {
        self.bundles.get(id)
    }

    pub fn remove_bundle(&mut self, id: &Id) -> Option<Bundle> {
        self.bundles.remove(id)
    }

    pub fn list_bundles(&self) -> Vec<&Bundle> {
        self.bundles.values().collect()
    }

    pub fn add_preference(&mut self, preference: Preference) {
        self.preferences.push(preference);
    }

    pub fn preferences(&self) -> &[Preference] {
        &self.preferences
    }

    pub fn upsert_calendar_link(&mut self, task_id: Id, event_id: String) -> Option<String> {
        self.calendar_links.insert(task_id, event_id)
    }

    pub fn calendar_link(&self, task_id: Id) -> Option<&String> {
        self.calendar_links.get(&task_id)
    }

    pub fn append_log(&mut self, entry: LogEntry) {
        self.log.push(entry);
    }

    pub fn log(&self) -> &[LogEntry] {
        &self.log
    }

    /// Referential integrity only: every `blocked_by`, `objective_ids`, bundle
    /// member, and preference bundle id resolves. Tier/projection invariants are
    /// M2, not here.
    ///
    /// Errors accumulate in a deterministic order: tasks by ascending id
    /// (`blocked_by` before `objective_ids`, each in field order), then bundles
    /// by ascending id, then preferences by index (`left` before `right`).
    pub fn validate(&self) -> Result<(), Vec<CoreError>> {
        let mut errors = Vec::new();

        for (task_id, task) in &self.tasks {
            for missing in &task.blocked_by {
                if !self.tasks.contains_key(missing) {
                    errors.push(CoreError::DanglingDependency {
                        task: *task_id,
                        missing: *missing,
                    });
                }
            }
            for missing in &task.objective_ids {
                if !self.objectives.contains_key(missing) {
                    errors.push(CoreError::DanglingObjective {
                        task: *task_id,
                        missing: *missing,
                    });
                }
            }
        }

        for (bundle_id, bundle) in &self.bundles {
            for missing in &bundle.members {
                if !self.tasks.contains_key(missing) {
                    errors.push(CoreError::DanglingBundleMember {
                        bundle: *bundle_id,
                        missing: *missing,
                    });
                }
            }
        }

        for (preference_index, preference) in self.preferences.iter().enumerate() {
            for missing in [preference.left, preference.right] {
                if !self.bundles.contains_key(&missing) {
                    errors.push(CoreError::DanglingPreferenceBundle {
                        preference_index,
                        missing,
                    });
                }
            }
        }

        if errors.is_empty() {
            Ok(())
        } else {
            Err(errors)
        }
    }
}
