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
