#[cfg(not(test))]
use std::fs;
#[cfg(test)]
use crate::test_support::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use ubu_core::{CompletionFact, Id, LogEntry, Store};

#[path = "sqlite.rs"]
mod sqlite;
pub use sqlite::SqliteBackend;

pub trait StorageBackend {
    fn load(&self) -> Result<Store, String>;
    fn save(&self, store: &Store) -> Result<(), String>;
    fn append_log(&self, entries: &[LogEntry]) -> Result<(), String>;
    /// Latest Actual fact for one linked task, including its log ID for undo.
    fn latest_actual(&self, task_id: Id) -> Result<Option<LogEntry>, String>;
    fn completions_in_window(&self, from: DateTime<Utc>, to: DateTime<Utc>)
        -> Result<Vec<CompletionFact>, String>;
    fn recent_completions(&self, limit: usize) -> Result<Vec<CompletionFact>, String>;
}

/// Query extra facts so recent untagged completions do not exhaust prompt history.
pub fn build_history(
    backend: &dyn StorageBackend,
    store: &Store,
    limit: usize,
) -> Result<Vec<ubu_core::CompletedExample>, String> {
    let facts = backend.recent_completions(limit.saturating_mul(4))?;
    Ok(ubu_core::recent_completed_examples(store, &facts, limit))
}

fn completion_fact(entry: &LogEntry) -> Option<CompletionFact> {
    match &entry.kind {
        ubu_core::LogEntryKind::Fact(ubu_core::FactKind::Actual {
            item_id, status: ubu_core::ActualStatus::Done, actual,
        }) => Some(CompletionFact { item_id: *item_id, at: entry.at, actual: actual.clone() }),
        _ => None,
    }
}

fn actual_item_id(entry: &LogEntry) -> Option<Id> {
    match &entry.kind {
        ubu_core::LogEntryKind::Fact(ubu_core::FactKind::Actual { item_id, .. }) => Some(*item_id),
        _ => None,
    }
}

fn undone_completion(entry: &LogEntry) -> Option<(Id, Id)> {
    match &entry.kind {
        ubu_core::LogEntryKind::Command(ubu_core::CommandKind::UndoCompletion { task_id, completion_id }) => Some((*task_id, *completion_id)),
        _ => None,
    }
}

/// Query only tasks eligible for Calendar's existing 24-hour correction rule.
/// Storage access stays outside gcal's pure reconciliation function.
pub(crate) fn calendar_actuals(
    backend: &dyn StorageBackend, store: &Store, events: &[gcal::FetchedEvent], now: DateTime<Utc>,
) -> Result<std::collections::BTreeMap<Id, LogEntry>, String> {
    let candidates: std::collections::BTreeSet<_> = events.iter()
        .filter(|event| event.color_id.is_none() && event.end > now - chrono::Duration::hours(24))
        .map(|event| event.id.as_str()).collect();
    let mut actuals = std::collections::BTreeMap::new();
    for (task_id, event_id) in &store.calendar_links {
        if candidates.contains(event_id.as_str()) && store.tasks.get(task_id)
            .is_some_and(|task| task.pinned.is_none() && task.status == ubu_core::TaskStatus::Done) {
            if let Some(entry) = backend.latest_actual(*task_id)? { actuals.insert(*task_id, entry); }
        }
    }
    Ok(actuals)
}

/// Retained JSON backend for compatibility tests and callers of the storage trait.
#[allow(dead_code)]
pub struct JsonBackend {
    pub path: PathBuf,
}

#[allow(dead_code)] // The production CLI selects SQLite; JSON remains a compatibility backend.
impl JsonBackend {
    // Keep the log separate so loading/saving Store never reads historical rows.
    fn log_path(&self) -> PathBuf {
        let mut path = self.path.as_os_str().to_os_string();
        path.push(".log.json");
        PathBuf::from(path)
    }

    fn load_log(&self) -> Result<Vec<LogEntry>, String> {
        let path = self.log_path();
        match fs::read_to_string(&path) {
            Ok(contents) => serde_json::from_str(&contents)
                .map_err(|error| format!("failed to parse {}: {error}", path.display())),
            Err(error) if error.kind() == ErrorKind::NotFound => Ok(Vec::new()),
            Err(error) => Err(format!("failed to read {}: {error}", path.display())),
        }
    }
}

impl StorageBackend for JsonBackend {
    fn load(&self) -> Result<Store, String> {
        load(&self.path)
    }

    fn save(&self, store: &Store) -> Result<(), String> {
        save(&self.path, store)
    }

    fn append_log(&self, entries: &[LogEntry]) -> Result<(), String> {
        let mut log = self.load_log()?;
        let mut ids: std::collections::BTreeSet<_> = log.iter().map(|entry| entry.id).collect();
        for entry in entries {
            if ids.insert(entry.id) {
                log.push(entry.clone());
            }
        }
        save_json(&self.log_path(), serde_json::to_string_pretty(&log))
    }

    fn latest_actual(&self, task_id: Id) -> Result<Option<LogEntry>, String> {
        let log = self.load_log()?;
        let mut ids = std::collections::BTreeSet::new();
        Ok(log.iter().enumerate().filter(|(_, entry)| ids.insert(entry.id))
            .filter(|(_, entry)| actual_item_id(entry) == Some(task_id))
            .max_by_key(|(index, entry)| (entry.at, *index))
            .map(|(_, entry)| entry.clone()))
    }

    fn completions_in_window(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<CompletionFact>, String> {
        Ok(json_completions(&self.load_log()?)
            .into_iter()
            .filter(|fact| from <= fact.at && fact.at < to)
            .collect())
    }

    fn recent_completions(&self, limit: usize) -> Result<Vec<CompletionFact>, String> {
        Ok(json_completions(&self.load_log()?)
            .into_iter()
            .rev()
            .take(limit)
            .collect())
    }
}

#[allow(dead_code)]
fn json_completions(log: &[LogEntry]) -> Vec<CompletionFact> {
    // Match SQLite's first-write-wins IDs, including duplicate IDs in legacy JSON.
    let mut ids = std::collections::BTreeSet::new();
    let entries: Vec<_> = log.iter().filter(|entry| ids.insert(entry.id)).collect();
    let undone: std::collections::BTreeSet<_> = entries.iter()
        .filter_map(|entry| undone_completion(entry)).collect();
    let mut completions: Vec<_> = entries.into_iter()
        .filter_map(|entry| {
            let fact = completion_fact(entry)?;
            (!undone.contains(&(fact.item_id, entry.id))).then_some(fact)
        }).collect();
    // SQLite retains whole-second timestamps and uses insertion order for ties.
    completions.sort_by_key(|fact| fact.at.timestamp());
    completions
}

#[allow(dead_code)]
pub fn load(path: &Path) -> Result<Store, String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Store::default()),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };

    serde_json::from_str(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

#[allow(dead_code)]
pub fn save(path: &Path, store: &Store) -> Result<(), String> {
    save_json(path, serde_json::to_string_pretty(store))
}

fn save_json(path: &Path, contents: Result<String, serde_json::Error>) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }

    let temp_path = temp_sibling(path);
    let contents = match contents {
        Ok(contents) => contents,
        Err(error) => {
            let _ = fs::remove_file(&temp_path);
            return Err(format!("failed to serialize store: {error}"));
        }
    };
    if let Err(error) = fs::write(&temp_path, contents) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!("failed to write {}: {error}", path.display()));
    }
    if let Err(error) = fs::rename(&temp_path, path) {
        let _ = fs::remove_file(&temp_path);
        return Err(format!("failed to replace {}: {error}", path.display()));
    }

    Ok(())
}

#[allow(dead_code)]
fn temp_sibling(path: &Path) -> PathBuf {
    let mut temp_path = path.as_os_str().to_os_string();
    temp_path.push(".tmp");
    PathBuf::from(temp_path)
}

pub fn resolve_task_id(store: &Store, prefix: &str) -> Result<Id, String> {
    resolve_id(store.tasks.keys().copied(), prefix, "task")
}

pub fn resolve_objective_id(store: &Store, prefix: &str) -> Result<Id, String> {
    resolve_id(store.objectives.keys().copied(), prefix, "objective")
}

fn resolve_id(ids: impl Iterator<Item = Id>, prefix: &str, entity: &str) -> Result<Id, String> {
    let normalized_prefix = prefix.replace('-', "").to_lowercase();
    let mut matches = ids.filter(|id| id.simple().to_string().starts_with(&normalized_prefix));

    match (matches.next(), matches.next()) {
        (None, _) => Err(format!("no {entity} matches {prefix}")),
        (Some(id), None) => Ok(id),
        (Some(_), Some(_)) => Err(format!("ambiguous prefix {prefix}")),
    }
}

#[cfg(test)]
mod tests {
    use chrono::Duration;
    use ubu_core::{DeferPolicy, Objective, ObjectiveStatus, Provenance, Task, TaskStatus, Tier};
    use uuid::Uuid;

    use super::*;

    #[test]
    fn persistence_round_trips_a_store_with_a_task_and_objective() {
        let objective_id = Uuid::from_u128(100);
        let task_id = Uuid::from_u128(1);
        let mut store = Store::new();
        store.upsert_objective(objective(objective_id, "Ship U-1"));
        store.upsert_task(task(task_id, "Build CLI", vec![objective_id]));

        let directory = PathBuf::from("memory").join(format!("quick-ubu-{}", Uuid::new_v4()));
        let path = directory.join("nested/store.json");
        let backend = JsonBackend { path };
        backend.save(&store).expect("store should save");
        let loaded = backend.load().expect("store should load");

        assert_eq!(loaded, store);
        fs::remove_dir_all(directory).expect("memory directory should be removable");
    }

    #[test]
    fn json_save_replaces_existing_bytes_without_leaving_temp_sibling() {
        let directory = PathBuf::from("memory").join(format!("quick-ubu-{}", Uuid::new_v4()));
        let path = directory.join("store.json");
        fs::create_dir_all(&directory).expect("memory directory should be creatable");
        fs::write(&path, "old incomplete content").expect("existing file should be writable");

        let objective_id = Uuid::from_u128(100);
        let task_id = Uuid::from_u128(1);
        let mut store = Store::new();
        store.upsert_objective(objective(objective_id, "Atomic save"));
        store.upsert_task(task(task_id, "Replace old content", vec![objective_id]));
        let expected = serde_json::to_string_pretty(&store).expect("store should serialize");

        save(&path, &store).expect("store should atomically replace existing file");

        assert_eq!(fs::read_to_string(&path).unwrap(), expected);
        assert_eq!(load(&path), Ok(store));
        assert!(!fs::exists(temp_sibling(&path)));
        fs::remove_dir_all(directory).expect("memory directory should be removable");
    }

    #[test]
    fn id_prefix_resolution_handles_unique_ambiguous_and_unknown_prefixes() {
        let first = Uuid::parse_str("aaaaaaaa-0000-0000-0000-000000000001").unwrap();
        let second = Uuid::parse_str("aaaabbbb-0000-0000-0000-000000000002").unwrap();
        let objective_id = Uuid::parse_str("12345678-0000-0000-0000-000000000003").unwrap();
        let mut store = Store::new();
        store.upsert_task(task(first, "First", Vec::new()));
        store.upsert_task(task(second, "Second", Vec::new()));
        store.upsert_objective(objective(objective_id, "Objective"));

        assert_eq!(resolve_task_id(&store, "aaaab"), Ok(second));
        assert_eq!(
            resolve_task_id(&store, "aaaa"),
            Err("ambiguous prefix aaaa".to_string())
        );
        assert_eq!(
            resolve_task_id(&store, "ffff"),
            Err("no task matches ffff".to_string())
        );
        assert_eq!(resolve_objective_id(&store, "12345678"), Ok(objective_id));
        assert_eq!(
            resolve_objective_id(&store, &objective_id.to_string()),
            Ok(objective_id)
        );
    }

    fn objective(id: Id, title: &str) -> Objective {
        Objective {
            id,
            tier: Tier::UserShared,
            title: title.to_string(),
            detail: None,
            target_date: None,
            status: ObjectiveStatus::Active,
        }
    }

    fn task(id: Id, title: &str, objective_ids: Vec<Id>) -> Task {
        Task {
            id,
            tier: Tier::UserShared,
            title: title.to_string(),
            detail: None,
            objective_ids,
            skills: Vec::new(),
            tags: Vec::new(),
            affect_cost: 10,
            est_duration: Duration::minutes(30),
            due: None,
            earliest_start: None,
            category: None,
            pinned: None,
            transparent: false,
            blocked_by: Vec::new(),
            after: Vec::new(),
            must_finish_by: None,
            defer_policy: DeferPolicy::RescheduleAsap,
            status: TaskStatus::Backlog,
            provenance: Provenance::Manual,
            reminders: Vec::new(),
            commitment: None,
        }
    }
}
