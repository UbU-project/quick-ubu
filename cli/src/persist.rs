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
