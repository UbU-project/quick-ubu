use std::fs;
use std::path::Path;

use rusqlite::{params, Connection};
use serde_json::{Map, Value};
use ubu_core::Store;

use super::StorageBackend;

pub struct SqliteBackend {
    connection: Connection,
}

impl SqliteBackend {
    pub fn open(path: &Path) -> Result<Self, String> {
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)
                .map_err(|error| format!("failed to create {}: {error}", parent.display()))?;
        }
        let connection = Connection::open(path)
            .map_err(|error| format!("failed to open SQLite store {}: {error}", path.display()))?;
        Self::initialize(connection)
    }

    #[allow(dead_code)]
    pub fn in_memory() -> Result<Self, String> {
        Self::initialize(Connection::open_in_memory().map_err(|error| error.to_string())?)
    }

    fn initialize(connection: Connection) -> Result<Self, String> {
        connection.execute_batch(
            "BEGIN;
             CREATE TABLE IF NOT EXISTS log (id TEXT PRIMARY KEY, at INTEGER NOT NULL, data TEXT NOT NULL);
             CREATE INDEX IF NOT EXISTS log_at ON log(at);
             CREATE TABLE IF NOT EXISTS objectives (id TEXT PRIMARY KEY, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS tasks (id TEXT PRIMARY KEY, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS routines (id TEXT PRIMARY KEY, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS bundles (id TEXT PRIMARY KEY, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS pending_decisions (id TEXT PRIMARY KEY, data TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS calendar_links (id TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS export_signatures (id TEXT PRIMARY KEY, value TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS category_colors (category TEXT PRIMARY KEY, color TEXT NOT NULL);
             CREATE TABLE IF NOT EXISTS singletons (key TEXT PRIMARY KEY, data TEXT NOT NULL);
             COMMIT;"
        ).map_err(|error| format!("failed to initialize SQLite store: {error}"))?;
        Ok(Self { connection })
    }
}

impl StorageBackend for SqliteBackend {
    fn load(&self) -> Result<Store, String> {
        // Keep every table on the same snapshot if another process saves.
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(|error| format!("failed to begin SQLite load: {error}"))?;
        let mut store = Store::new();
        macro_rules! entities {
            ($field:ident) => {
                store.$field = serde_json::from_value(read_entities(&tx, stringify!($field))?)
                    .map_err(|error| format!("invalid SQLite {}: {error}", stringify!($field)))?;
            };
        }
        entities!(objectives);
        entities!(tasks);
        entities!(routines);
        entities!(bundles);
        // Pending decisions are a Vec: rowid preserves their saved queue order.
        store.pending_decisions = serde_json::from_value(read_array(
            &tx,
            "SELECT data FROM pending_decisions ORDER BY rowid",
        )?)
        .map_err(|error| format!("invalid SQLite pending_decisions: {error}"))?;
        // rowid retains insertion order for equal timestamps, unlike random UUIDs.
        store.log =
            serde_json::from_value(read_array(&tx, "SELECT data FROM log ORDER BY at, rowid")?)
                .map_err(|error| format!("invalid SQLite log: {error}"))?;
        store.calendar_links = serde_json::from_value(read_strings(
            &tx,
            "SELECT id, value FROM calendar_links ORDER BY id",
        )?)
        .map_err(|error| format!("invalid SQLite calendar_links: {error}"))?;
        store.export_signatures = serde_json::from_value(read_strings(
            &tx,
            "SELECT id, value FROM export_signatures ORDER BY id",
        )?)
        .map_err(|error| format!("invalid SQLite export_signatures: {error}"))?;
        store.category_colors = serde_json::from_value(read_strings(
            &tx,
            "SELECT category, color FROM category_colors ORDER BY category",
        )?)
        .map_err(|error| format!("invalid SQLite category_colors: {error}"))?;
        let mut statement = tx
            .prepare("SELECT key, data FROM singletons")
            .map_err(|error| format!("failed to read SQLite singletons: {error}"))?;
        let rows = statement
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })
            .map_err(|error| error.to_string())?;
        for row in rows {
            let (key, data) = row.map_err(|error| error.to_string())?;
            macro_rules! singleton {
                ($field:ident) => {
                    store.$field = serde_json::from_str(&data)
                        .map_err(|error| format!("invalid SQLite {key}: {error}"))?
                };
            }
            match key.as_str() {
                "preferences" => singleton!(preferences),
                "decision_history" => singleton!(decision_history),
                "ollama_model" => singleton!(ollama_model),
                "poll_snapshot" => singleton!(poll_snapshot),
                _ => {}
            }
        }
        drop(statement);
        tx.commit()
            .map_err(|error| format!("failed to finish SQLite load: {error}"))?;
        Ok(store)
    }

    fn save(&self, store: &Store) -> Result<(), String> {
        let tx = self
            .connection
            .unchecked_transaction()
            .map_err(|error| format!("failed to begin SQLite save: {error}"))?;
        macro_rules! entities {
            ($field:ident) => {
                rewrite(
                    &tx,
                    stringify!($field),
                    "id, data",
                    store.$field.iter().map(|(id, entity)| {
                        serde_json::to_string(entity)
                            .map(|data| (id.to_string(), data))
                            .map_err(|error| error.to_string())
                    }),
                )?;
            };
        }
        entities!(objectives);
        entities!(tasks);
        entities!(routines);
        entities!(bundles);
        rewrite(
            &tx,
            "pending_decisions",
            "id, data",
            store.pending_decisions.iter().map(|entity| {
                serde_json::to_string(entity)
                    .map(|data| (entity.id.to_string(), data))
                    .map_err(|error| error.to_string())
            }),
        )?;
        rewrite(
            &tx,
            "calendar_links",
            "id, value",
            store
                .calendar_links
                .iter()
                .map(|(id, value)| Ok((id.to_string(), value.clone()))),
        )?;
        rewrite(
            &tx,
            "export_signatures",
            "id, value",
            store
                .export_signatures
                .iter()
                .map(|(id, value)| Ok((id.to_string(), value.clone()))),
        )?;
        rewrite(
            &tx,
            "category_colors",
            "category, color",
            store
                .category_colors
                .iter()
                .map(|(category, color)| Ok((category.clone(), color.clone()))),
        )?;
        rewrite(
            &tx,
            "singletons",
            "key, data",
            [
                ("preferences", serde_json::to_string(&store.preferences)),
                (
                    "decision_history",
                    serde_json::to_string(&store.decision_history),
                ),
                ("ollama_model", serde_json::to_string(&store.ollama_model)),
                ("poll_snapshot", serde_json::to_string(&store.poll_snapshot)),
            ]
            .into_iter()
            .map(|(key, data)| {
                data.map(|data| (key.to_string(), data))
                    .map_err(|error| error.to_string())
            }),
        )?;

        {
            let mut insert = tx
                .prepare("INSERT OR IGNORE INTO log (id, at, data) VALUES (?1, ?2, ?3)")
                .map_err(|error| format!("failed to prepare SQLite log append: {error}"))?;
            for entry in &store.log {
                let data = serde_json::to_string(entry)
                    .map_err(|error| format!("failed to serialize log: {error}"))?;
                insert
                    .execute(params![entry.id.to_string(), entry.at.timestamp(), data])
                    .map_err(|error| format!("failed to append SQLite log: {error}"))?;
            }
        }
        tx.commit()
            .map_err(|error| format!("failed to commit SQLite save: {error}"))
    }
}

// Identifiers here are internal constants, never user input.
fn rewrite(
    connection: &Connection,
    table: &str,
    columns: &str,
    rows: impl IntoIterator<Item = Result<(String, String), String>>,
) -> Result<(), String> {
    connection
        .execute(&format!("DELETE FROM {table}"), [])
        .map_err(|error| format!("failed to clear SQLite {table}: {error}"))?;
    let mut insert = connection
        .prepare(&format!("INSERT INTO {table} ({columns}) VALUES (?1, ?2)"))
        .map_err(|error| format!("failed to prepare SQLite {table}: {error}"))?;
    for row in rows {
        let (key, value) = row?;
        insert
            .execute(params![key, value])
            .map_err(|error| format!("failed to write SQLite {table}: {error}"))?;
    }
    Ok(())
}

fn read_entities(connection: &Connection, table: &str) -> Result<Value, String> {
    let mut statement = connection
        .prepare(&format!("SELECT id, data FROM {table} ORDER BY id"))
        .map_err(|error| format!("failed to read SQLite {table}: {error}"))?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut result = Map::new();
    for row in rows {
        let (id, data) = row.map_err(|error| error.to_string())?;
        result.insert(
            id,
            serde_json::from_str(&data)
                .map_err(|error| format!("invalid SQLite {table} JSON: {error}"))?,
        );
    }
    Ok(Value::Object(result))
}

fn read_array(connection: &Connection, query: &str) -> Result<Value, String> {
    let mut statement = connection
        .prepare(query)
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))
        .map_err(|error| error.to_string())?;
    let mut result = Vec::new();
    for row in rows {
        result.push(
            serde_json::from_str(&row.map_err(|error| error.to_string())?)
                .map_err(|error| format!("invalid SQLite JSON: {error}"))?,
        );
    }
    Ok(Value::Array(result))
}

fn read_strings(connection: &Connection, query: &str) -> Result<Value, String> {
    let mut statement = connection
        .prepare(query)
        .map_err(|error| error.to_string())?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })
        .map_err(|error| error.to_string())?;
    let mut result = Map::new();
    for row in rows {
        let (key, value) = row.map_err(|error| error.to_string())?;
        result.insert(key, Value::String(value));
    }
    Ok(Value::Object(result))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::{Duration, TimeZone, Utc};
    use ubu_core::{
        Bundle, CommandKind, DecisionRecord, DecisionSource, DeferPolicy, LogEntry, LogEntryKind,
        Objective, ObjectiveStatus, PendingDecision, Preference, Proposal, Provenance, Relation,
        Resolution, RoutineTemplate, Task, TaskStatus, Tier,
    };
    use uuid::Uuid;

    fn populated_store() -> Store {
        let id = Uuid::from_u128;
        let at = Utc.with_ymd_and_hms(2026, 9, 11, 10, 0, 0).unwrap();
        let mut store = Store::new();
        store.objectives.insert(
            id(1),
            Objective {
                id: id(1),
                tier: Tier::UserShared,
                title: "Ship 'SQLite' 🗓".into(),
                detail: Some("All fields".into()),
                target_date: Some(at),
                status: ObjectiveStatus::Active,
            },
        );
        store.tasks.insert(
            id(2),
            Task {
                id: id(2),
                tier: Tier::SemiPublic,
                title: "Persist task".into(),
                detail: Some("Details".into()),
                objective_ids: vec![id(1)],
                skills: vec!["Rust".into()],
                tags: vec!["work".into(), "雪".into()],
                affect_cost: 3,
                est_duration: Duration::minutes(25),
                due: Some(at),
                earliest_start: Some(at),
                category: Some("work's 🗓".into()),
                pinned: None,
                transparent: false,
                reminders: vec![10, 0],
                blocked_by: vec![],
                after: Vec::new(),
                must_finish_by: Some(at + Duration::hours(2)),
                defer_policy: DeferPolicy::ReturnToBacklog,
                status: TaskStatus::Backlog,
                provenance: Provenance::Manual,
                commitment: None,
            },
        );
        store
            .tasks
            .get_mut(&id(2))
            .unwrap()
            .after
            .push(ubu_core::AfterConstraint {
                task_id: id(5),
                offset: Duration::minutes(60),
            });
        let routines: Vec<RoutineTemplate> =
            serde_json::from_str(include_str!("../../docs/example-routine.json")).unwrap();
        for mut routine in routines {
            routine.dynamic = true;
            routine.latest_tod = Some(chrono::NaiveTime::from_hms_opt(20, 0, 0).unwrap());
            routine.after.push(ubu_core::RoutineAfter {
                template_id: id(999),
                offset: Duration::minutes(30),
            });
            store.routines.insert(routine.id, routine);
        }
        for n in [3, 4] {
            store.bundles.insert(
                id(n),
                Bundle {
                    id: id(n),
                    members: [id(2)].into_iter().collect(),
                },
            );
        }
        store.preferences.push(Preference {
            left: id(3),
            right: id(4),
            relation: Relation::Strict,
        });
        let proposal = Proposal::Preference {
            a: id(2),
            b: id(5),
            suggested: None,
        };
        // Queue order must survive even when IDs sort in the opposite direction.
        for n in [20, 10] {
            store.pending_decisions.push(PendingDecision {
                id: id(n),
                source: DecisionSource::Elicitation,
                proposal: proposal.clone(),
            });
        }
        store.decision_history.push(DecisionRecord {
            proposal,
            resolution: Resolution::Skipped,
            at,
        });
        store.calendar_links.insert(id(2), "event-'雪'".into());
        store
            .export_signatures
            .insert(id(2), "signature\nwith quotes: \"".into());
        store.category_colors.insert("work's 🗓".into(), "9".into());
        store.ollama_model = Some("local-model".into());
        store.poll_snapshot.insert(
            "calendar-event-雪".into(),
            "fingerprint|with\nquotes: \"".into(),
        );
        // The first two entries share a timestamp; their insertion order is meaningful.
        for (n, seconds) in [(30, 0), (29, 0), (28, 1)] {
            store.log.push(LogEntry {
                id: id(n),
                at: at + Duration::seconds(seconds),
                kind: LogEntryKind::Command(CommandKind::Capture {
                    task: store.tasks[&id(2)].clone(),
                }),
            });
        }
        store
    }

    #[test]
    fn legacy_json_tasks_without_tags_load_empty_and_tag_proposals_round_trip() {
        let mut store = populated_store();
        let mut value = serde_json::to_value(&store).unwrap();
        for task in value["tasks"].as_object_mut().unwrap().values_mut() {
            task.as_object_mut().unwrap().remove("tags");
        }
        let loaded: Store = serde_json::from_value(value).unwrap();
        assert!(loaded.tasks.values().all(|task| task.tags.is_empty()));
        store.pending_decisions.push(PendingDecision {
            id: Uuid::from_u128(900),
            source: DecisionSource::Advisor,
            proposal: Proposal::Tag {
                task_id: Uuid::from_u128(2),
                tag: "focus".into(),
            },
        });
        store.decision_history.push(DecisionRecord {
            proposal: store.pending_decisions.last().unwrap().proposal.clone(),
            resolution: Resolution::Rejected,
            at: store.log[0].at,
        });
        let sqlite = SqliteBackend::in_memory().unwrap();
        let json = crate::persist::JsonBackend {
            path: Path::new("memory").join(format!("{}.json", Uuid::new_v4())),
        };
        for backend in [&sqlite as &dyn StorageBackend, &json as &dyn StorageBackend] {
            backend.save(&store).unwrap();
            assert_eq!(backend.load().unwrap(), store);
        }
    }

    #[test]
    fn legacy_sqlite_and_json_stores_without_poll_snapshot_load_empty() {
        let backend = SqliteBackend::in_memory().unwrap();
        let mut store = populated_store();
        backend.save(&store).unwrap();
        backend
            .connection
            .execute("DELETE FROM singletons WHERE key = 'poll_snapshot'", [])
            .unwrap();
        store.poll_snapshot.clear();
        assert_eq!(backend.load().unwrap(), store);
        let mut legacy = serde_json::to_value(&store).unwrap();
        legacy.as_object_mut().unwrap().remove("poll_snapshot");
        let path = Path::new("memory").join(format!("{}.json", Uuid::new_v4()));
        crate::test_support::fs::write(&path, serde_json::to_vec(&legacy).unwrap()).unwrap();
        let json = crate::persist::JsonBackend { path };
        assert_eq!(json.load().unwrap(), store);
    }

    #[test]
    fn sqlite_round_trips_every_store_field_and_preserves_queue_and_log_ties() {
        let backend = SqliteBackend::in_memory().unwrap();
        assert_eq!(backend.load().unwrap(), Store::new());
        let store = populated_store();
        backend.save(&store).unwrap();
        assert_eq!(backend.load().unwrap(), store);
        assert_eq!(backend.load().unwrap(), store);
    }

    #[test]
    fn removing_entities_and_clearing_maps_vectors_and_model_rewrites_all_bounded_tables() {
        let backend = SqliteBackend::in_memory().unwrap();
        let mut store = populated_store();
        backend.save(&store).unwrap();
        store.tasks.remove(&Uuid::from_u128(2));
        backend.save(&store).unwrap();
        assert_eq!(backend.load().unwrap(), store);
        let cleared = Store {
            log: store.log,
            ..Store::new()
        };
        backend.save(&cleared).unwrap();
        assert_eq!(backend.load().unwrap(), cleared);
    }

    fn log_rows(backend: &SqliteBackend) -> Vec<(i64, String, i64, String)> {
        backend
            .connection
            .prepare("SELECT rowid, id, at, data FROM log ORDER BY at, rowid")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
            })
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap()
    }

    #[test]
    fn log_appends_without_duplicates_and_never_updates_or_deletes_existing_rows() {
        let backend = SqliteBackend::in_memory().unwrap();
        let mut store = populated_store();
        backend.save(&store).unwrap();
        let original_rows = log_rows(&backend);
        let mut entry = store.log.last().unwrap().clone();
        entry.id = Uuid::from_u128(40);
        entry.at += Duration::seconds(1);
        store.log.push(entry);
        backend.save(&store).unwrap();
        backend.save(&store).unwrap();
        assert_eq!(backend.load().unwrap(), store);
        assert_eq!(&log_rows(&backend)[..3], original_rows);
        let expected = store.clone();
        store.log.remove(0);
        store.log[0].at += Duration::days(10);
        store.log[0].kind = LogEntryKind::Command(CommandKind::RemoveTask {
            task_id: Uuid::from_u128(2),
        });
        backend.save(&store).unwrap();
        assert_eq!(backend.load().unwrap(), expected);
        assert_eq!(&log_rows(&backend)[..3], original_rows);
    }

    #[test]
    fn log_load_sorts_by_timestamp_then_stable_insertion_order_and_indexes_at() {
        let backend = SqliteBackend::in_memory().unwrap();
        let store = populated_store();
        let mut shuffled = store.clone();
        shuffled.log.rotate_right(1);
        backend.save(&shuffled).unwrap();
        assert_eq!(backend.load().unwrap(), store);
        let index_columns: Vec<String> = backend
            .connection
            .prepare("PRAGMA index_info(log_at)")
            .unwrap()
            .query_map([], |row| row.get(2))
            .unwrap()
            .collect::<rusqlite::Result<_>>()
            .unwrap();
        assert_eq!(index_columns, ["at"]);
        assert_eq!(log_rows(&backend)[0].2, store.log[0].at.timestamp());
    }

    #[test]
    fn failed_log_insert_rolls_back_all_bounded_rewrites_and_other_log_inserts() {
        let backend = SqliteBackend::in_memory().unwrap();
        let original = populated_store();
        backend.save(&original).unwrap();
        backend.connection.execute_batch(
            "CREATE TRIGGER reject_log BEFORE INSERT ON log WHEN NEW.id = '00000000-0000-0000-0000-000000000063' BEGIN SELECT RAISE(ABORT, 'injected failure'); END;"
        ).unwrap();
        let mut changed = Store::new();
        for n in [98, 99] {
            let mut entry = original.log[0].clone();
            entry.id = Uuid::from_u128(n);
            changed.log.push(entry);
        }
        assert!(backend
            .save(&changed)
            .unwrap_err()
            .contains("injected failure"));
        assert_eq!(backend.load().unwrap(), original);
        backend
            .connection
            .execute_batch("DROP TRIGGER reject_log")
            .unwrap();
        backend.save(&changed).unwrap();
        assert!(backend.load().unwrap().tasks.is_empty());
        assert_eq!(backend.load().unwrap().log.len(), original.log.len() + 2);
    }

    #[test]
    fn json_and_sqlite_backends_round_trip_equal_stores_through_the_trait() {
        let json = crate::persist::JsonBackend {
            path: Path::new("memory").join(format!("{}.json", Uuid::new_v4())),
        };
        let sqlite = SqliteBackend::in_memory().unwrap();
        let store = populated_store();
        for backend in [&json as &dyn StorageBackend, &sqlite as &dyn StorageBackend] {
            backend.save(&store).unwrap();
            assert_eq!(backend.load().unwrap(), store);
        }
        assert_eq!(json.load().unwrap(), sqlite.load().unwrap());
    }
}
