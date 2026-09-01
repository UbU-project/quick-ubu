use std::fs;
use std::io::ErrorKind;
use std::path::Path;

use ubu_core::{Id, Store};

pub fn load(path: &Path) -> Result<Store, String> {
    let contents = match fs::read_to_string(path) {
        Ok(contents) => contents,
        Err(error) if error.kind() == ErrorKind::NotFound => return Ok(Store::default()),
        Err(error) => return Err(format!("failed to read {}: {error}", path.display())),
    };

    serde_json::from_str(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

pub fn save(path: &Path, store: &Store) -> Result<(), String> {
    if let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    {
        fs::create_dir_all(parent)
            .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
    }

    let contents = serde_json::to_string_pretty(store)
        .map_err(|error| format!("failed to serialize store: {error}"))?;
    fs::write(path, contents)
        .map_err(|error| format!("failed to write {}: {error}", path.display()))
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

        let directory = std::env::temp_dir().join(format!("quick-ubu-{}", Uuid::new_v4()));
        let path = directory.join("nested/store.json");
        save(&path, &store).expect("store should save");
        let loaded = load(&path).expect("store should load");

        assert_eq!(loaded, store);
        std::fs::remove_dir_all(directory).expect("temporary directory should be removable");
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
            affect_cost: 10,
            est_duration: Duration::minutes(30),
            due: None,
            earliest_start: None,
            blocked_by: Vec::new(),
            defer_policy: DeferPolicy::RescheduleAsap,
            status: TaskStatus::Backlog,
            provenance: Provenance::Manual,
            commitment: None,
        }
    }
}
