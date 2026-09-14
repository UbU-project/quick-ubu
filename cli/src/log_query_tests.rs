use super::super::JsonBackend;
use super::*;
use chrono::{Duration, TimeZone};
use ubu_core::{ActualStatus, CommandKind, FactKind, LogEntryKind, TimeWindow};
use uuid::Uuid;

fn at(seconds: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 9, 11, 12, 0, 0).unwrap() + Duration::seconds(seconds)
}

fn done(id: u128, seconds: i64, actual: Option<TimeWindow>) -> LogEntry {
    LogEntry {
        id: Uuid::from_u128(id),
        at: at(seconds),
        kind: LogEntryKind::Fact(FactKind::Actual {
            item_id: Uuid::from_u128(id + 100),
            status: ActualStatus::Done,
            actual,
        }),
    }
}

fn command(id: u128, seconds: i64) -> LogEntry {
    LogEntry {
        id: Uuid::from_u128(id),
        at: at(seconds),
        kind: LogEntryKind::Command(CommandKind::RemoveTask {
            task_id: Uuid::from_u128(101),
        }),
    }
}

fn fixture() -> Vec<LogEntry> {
    let mut ongoing = done(5, 50, None);
    ongoing.kind = LogEntryKind::Fact(FactKind::Actual {
        item_id: Uuid::from_u128(105),
        status: ActualStatus::Ongoing,
        actual: None,
    });
    // Deliberately unsorted, with a tie, nullable actual, and newer non-completions.
    vec![
        done(
            1,
            20,
            Some(TimeWindow {
                start: at(0),
                end: at(10),
            }),
        ),
        done(2, 0, None),
        done(3, 20, None),
        done(4, 30, None),
        ongoing,
        command(6, 60),
        done(7, -10, None),
    ]
}

fn expected(id: u128, seconds: i64, actual: Option<TimeWindow>) -> CompletionFact {
    CompletionFact {
        item_id: Uuid::from_u128(id + 100),
        at: at(seconds),
        actual,
    }
}

fn json() -> JsonBackend {
    // Use the existing in-memory filesystem harness, as required by ST-1.
    JsonBackend {
        path: format!("memory/ql1-{}/store.json", Uuid::new_v4()).into(),
    }
}

#[test]
fn append_queries_filter_done_actuals_and_apply_half_open_bounds_and_limits() {
    for backend in [
        Box::new(SqliteBackend::in_memory().unwrap()) as Box<dyn StorageBackend>,
        Box::new(json()),
    ] {
        backend.append_log(&fixture()).unwrap();
        let window = vec![
            expected(2, 0, None),
            expected(
                1,
                20,
                Some(TimeWindow {
                    start: at(0),
                    end: at(10),
                }),
            ),
            expected(3, 20, None),
        ];
        assert_eq!(
            backend.completions_in_window(at(0), at(30)).unwrap(),
            window
        );
        assert_eq!(
            backend.recent_completions(2).unwrap(),
            vec![expected(4, 30, None), expected(3, 20, None)]
        );
        assert_eq!(
            backend.recent_completions(1).unwrap(),
            vec![expected(4, 30, None)]
        );
        assert!(backend.recent_completions(0).unwrap().is_empty());
        assert_eq!(backend.recent_completions(usize::MAX).unwrap().len(), 5);
        assert!(backend
            .completions_in_window(at(0), at(0))
            .unwrap()
            .is_empty());
        assert!(backend
            .completions_in_window(at(30), at(0))
            .unwrap()
            .is_empty());
        assert!(backend
            .completions_in_window(at(31), at(70))
            .unwrap()
            .is_empty());
        assert_eq!(backend.load().unwrap().log.len(), fixture().len());
    }
}

#[test]
fn json_matches_sqlite_after_save_append_and_duplicate_ids() {
    let sqlite = SqliteBackend::in_memory().unwrap();
    let json = json();
    let mut store = Store::new();
    store.ollama_model = Some("preserve model".into());
    store.category_colors.insert("Work".into(), "7".into());
    store.log = fixture();
    // A duplicate ID after a non-completion must not turn it into a completion.
    store.log.push(done(6, 100, None));
    let mut changed = done(1, 200, None);
    changed.kind = command(1, 200).kind;
    let batch = vec![
        changed,
        done(8, 40, None),
        done(8, 90, None),
        done(6, 100, None),
    ];
    for backend in [&sqlite as &dyn StorageBackend, &json] {
        backend.save(&store).unwrap();
        backend.append_log(&batch).unwrap();
        let loaded = backend.load().unwrap();
        backend.append_log(&batch).unwrap();
        assert_eq!(backend.load().unwrap(), loaded);
        assert_eq!(loaded.ollama_model, store.ollama_model);
        assert_eq!(loaded.category_colors, store.category_colors);
    }
    let window = sqlite.completions_in_window(at(-100), at(300)).unwrap();
    assert_eq!(window.len(), 6);
    assert_eq!(
        json.completions_in_window(at(-100), at(300)).unwrap(),
        window
    );
    let recent = sqlite.recent_completions(100).unwrap();
    assert_eq!(recent.first(), Some(&expected(8, 40, None)));
    assert_eq!(json.recent_completions(100).unwrap(), recent);
}

#[test]
fn fractional_bounds_are_exact_and_second_ties_match_sqlite_insertion_order() {
    let sqlite = SqliteBackend::in_memory().unwrap();
    let json = json();
    let mut late = done(1, 0, None);
    late.at += Duration::milliseconds(800);
    let mut early = done(2, 0, None);
    early.at += Duration::milliseconds(200);
    let mut next = done(3, 1, None);
    next.at += Duration::milliseconds(100);
    let entries = vec![late.clone(), early.clone(), next.clone()];
    for backend in [&sqlite as &dyn StorageBackend, &json] {
        backend.append_log(&entries).unwrap();
        assert_eq!(
            backend.completions_in_window(early.at, late.at).unwrap(),
            vec![completion_fact(&early).unwrap()]
        );
        assert_eq!(
            backend.completions_in_window(late.at, next.at).unwrap(),
            vec![completion_fact(&late).unwrap()]
        );
        assert_eq!(
            backend.completions_in_window(at(0), at(1)).unwrap(),
            vec![
                completion_fact(&late).unwrap(),
                completion_fact(&early).unwrap()
            ]
        );
        assert_eq!(
            backend.recent_completions(3).unwrap(),
            vec![
                completion_fact(&next).unwrap(),
                completion_fact(&early).unwrap(),
                completion_fact(&late).unwrap()
            ]
        );
    }
}

#[test]
fn save_and_append_set_metadata_and_queries_use_completion_index() {
    let backend = SqliteBackend::in_memory().unwrap();
    let entries = fixture();
    let mut store = Store::new();
    store.log = entries[..5].to_vec();
    backend.save(&store).unwrap();
    backend.append_log(&entries[5..]).unwrap();
    for entry in entries {
        let metadata: (Option<String>, i64) = backend
            .connection
            .query_row(
                "SELECT item_id, is_completion FROM log WHERE id = ?1",
                [entry.id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let item_id = completion_fact(&entry).map(|fact| fact.item_id.to_string());
        assert_eq!(metadata, (item_id.clone(), i64::from(item_id.is_some())));
    }
    let mut statement = backend
        .connection
        .prepare("PRAGMA index_info(idx_log_completion)")
        .unwrap();
    let columns: Vec<String> = statement
        .query_map([], |row| row.get(2))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert_eq!(columns, vec!["is_completion", "at"]);
    for sql in [
        "EXPLAIN QUERY PLAN SELECT data FROM log WHERE is_completion = 1 AND at >= 0 AND at < 1 ORDER BY at, rowid",
        "EXPLAIN QUERY PLAN SELECT data FROM log WHERE is_completion = 1 ORDER BY at DESC, rowid DESC LIMIT 2",
    ] {
        let mut statement = backend.connection.prepare(sql).unwrap();
        let plan: Vec<String> = statement.query_map([], |row| row.get(3)).unwrap().collect::<Result<_, _>>().unwrap();
        assert!(plan.iter().any(|line| line.contains("SEARCH log USING INDEX idx_log_completion")), "{plan:?}");
        assert!(!plan.iter().any(|line| line.contains("TEMP B-TREE") || line.contains("SCAN log")), "{plan:?}");
    }
}

#[test]
fn sqlite_queries_do_not_deserialize_noncompletions_or_out_of_range_rows() {
    let backend = SqliteBackend::in_memory().unwrap();
    backend.append_log(&[done(1, 0, None)]).unwrap();
    backend
        .connection
        .execute(
            "INSERT INTO log (id, at, data) VALUES ('bad-command', ?1, 'not JSON')",
            [at(10).timestamp()],
        )
        .unwrap();
    assert!(backend.load().is_err());
    assert_eq!(
        backend.recent_completions(10).unwrap(),
        vec![expected(1, 0, None)]
    );
    assert_eq!(
        backend.completions_in_window(at(0), at(1)).unwrap(),
        vec![expected(1, 0, None)]
    );
    backend
        .connection
        .execute(
            "INSERT INTO log (id, at, data, is_completion) VALUES ('bad-done', ?1, 'not JSON', 1)",
            [at(-10).timestamp()],
        )
        .unwrap();
    assert_eq!(
        backend.recent_completions(1).unwrap(),
        vec![expected(1, 0, None)]
    );
    assert_eq!(
        backend.completions_in_window(at(0), at(1)).unwrap(),
        vec![expected(1, 0, None)]
    );
    assert!(backend
        .recent_completions(2)
        .unwrap_err()
        .contains("invalid SQLite completion log entry"));
    assert!(backend
        .completions_in_window(at(-10), at(1))
        .unwrap_err()
        .contains("invalid SQLite completion log entry"));
}

#[test]
fn sqlite_append_rolls_back_the_whole_batch_on_insert_error() {
    let backend = SqliteBackend::in_memory().unwrap();
    let mut store = Store::new();
    store.log.push(done(1, 0, None));
    store.ollama_model = Some("preserve".into());
    backend.save(&store).unwrap();
    backend.connection.execute_batch(&format!(
        "CREATE TRIGGER reject_test_log BEFORE INSERT ON log WHEN NEW.id = '{}' BEGIN SELECT RAISE(ABORT, 'test append failure'); END;",
        Uuid::from_u128(3),
    )).unwrap();
    assert!(backend
        .append_log(&[done(2, 1, None), done(3, 2, None)])
        .unwrap_err()
        .contains("test append failure"));
    assert_eq!(backend.load().unwrap(), store);
    assert_eq!(
        backend.recent_completions(10).unwrap(),
        vec![expected(1, 0, None)]
    );
}

#[test]
fn saved_store_log_still_round_trips_without_using_query_methods() {
    for backend in [
        Box::new(SqliteBackend::in_memory().unwrap()) as Box<dyn StorageBackend>,
        Box::new(json()),
    ] {
        let mut store = Store::new();
        store.log = fixture();
        store.log.sort_by_key(|entry| entry.at.timestamp());
        store.ollama_model = Some("unchanged".into());
        backend.save(&store).unwrap();
        assert_eq!(backend.load().unwrap(), store);
        backend.save(&store).unwrap();
        assert_eq!(backend.load().unwrap(), store);
        assert_eq!(backend.recent_completions(100).unwrap().len(), 5);
    }
}

#[test]
fn empty_logs_and_empty_appends_have_no_completions() {
    for backend in [
        Box::new(SqliteBackend::in_memory().unwrap()) as Box<dyn StorageBackend>,
        Box::new(json()),
    ] {
        assert!(backend.recent_completions(1).unwrap().is_empty());
        assert!(backend
            .completions_in_window(at(0), at(1))
            .unwrap()
            .is_empty());
        backend.append_log(&[]).unwrap();
        assert_eq!(backend.load().unwrap(), Store::new());
    }
}
