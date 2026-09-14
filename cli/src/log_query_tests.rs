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
        assert_eq!(backend.load().unwrap(), Store::new());
    }
}

#[test]
fn json_matches_sqlite_after_save_append_and_duplicate_ids() {
    let sqlite = SqliteBackend::in_memory().unwrap();
    let json = json();
    let mut store = Store::new();
    store.ollama_model = Some("preserve model".into());
    store.category_colors.insert("Work".into(), "7".into());
    let mut entries = fixture();
    // A duplicate ID after a non-completion must not turn it into a completion.
    entries.push(done(6, 100, None));
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
        backend.append_log(&entries).unwrap();
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
fn append_sets_metadata_and_queries_use_completion_index() {
    let backend = SqliteBackend::in_memory().unwrap();
    let entries = fixture();
    let store = Store::new();
    backend.save(&store).unwrap();
    backend.append_log(&entries[..5]).unwrap();
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
        "EXPLAIN QUERY PLAN SELECT data FROM log WHERE is_completion = 1 AND NOT EXISTS (SELECT 1 FROM log_completion_undos u WHERE u.task_id = log.item_id AND u.completion_id = log.id) AND at >= 0 AND at < 1 ORDER BY at, rowid",
        "EXPLAIN QUERY PLAN SELECT data FROM log WHERE is_completion = 1 AND NOT EXISTS (SELECT 1 FROM log_completion_undos u WHERE u.task_id = log.item_id AND u.completion_id = log.id) ORDER BY at DESC, rowid DESC LIMIT 2",
    ] {
        let mut statement = backend.connection.prepare(sql).unwrap();
        let plan: Vec<String> = statement.query_map([], |row| row.get(3)).unwrap().collect::<Result<_, _>>().unwrap();
        assert!(plan.iter().any(|line| line.contains("SEARCH log USING INDEX idx_log_completion")), "{plan:?}");
        assert!(plan.iter().any(|line| line.contains("SEARCH u USING COVERING INDEX")), "{plan:?}");
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
    assert_eq!(backend.load().unwrap(), Store::new());
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
    backend.append_log(&[done(1, 0, None)]).unwrap();
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
        backend
            .connection
            .query_row("SELECT count(*) FROM log_actuals", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        1
    );
    assert_eq!(
        backend.recent_completions(10).unwrap(),
        vec![expected(1, 0, None)]
    );
}

#[test]
fn saved_store_round_trips_and_leaves_backend_log_untouched() {
    for backend in [
        Box::new(SqliteBackend::in_memory().unwrap()) as Box<dyn StorageBackend>,
        Box::new(json()),
    ] {
        let mut store = Store::new();
        backend.append_log(&fixture()).unwrap();
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

#[test]
fn completions_survive_separate_load_append_save_sessions() {
    for backend in [
        Box::new(SqliteBackend::in_memory().unwrap()) as Box<dyn StorageBackend>,
        Box::new(json()),
    ] {
        for (id, seconds) in [(1, 20), (2, 0), (3, 30)] {
            let mut store = backend.load().unwrap();
            store.ollama_model = Some(format!("session-{id}"));
            backend.append_log(&[done(id, seconds, None)]).unwrap();
            backend.save(&store).unwrap();
        }
        let loaded = backend.load().unwrap();
        assert_eq!(loaded.ollama_model.as_deref(), Some("session-3"));
        assert!(serde_json::to_value(&loaded).unwrap().get("log").is_none());
        assert_eq!(
            backend.recent_completions(3).unwrap(),
            vec![
                expected(3, 30, None),
                expected(1, 20, None),
                expected(2, 0, None)
            ]
        );
        assert_eq!(
            backend.completions_in_window(at(0), at(30)).unwrap(),
            vec![expected(2, 0, None), expected(1, 20, None)]
        );
    }
}

#[test]
fn json_store_load_and_save_do_not_read_or_rewrite_the_adjacent_log() {
    let backend = json();
    let mut store = Store::new();
    backend.save(&store).unwrap();
    crate::test_support::fs::write(backend.log_path(), "invalid log JSON").unwrap();
    assert_eq!(backend.load().unwrap(), store);
    store.ollama_model = Some("save without reading history".into());
    backend.save(&store).unwrap();
    assert_eq!(backend.load().unwrap(), store);
    assert_eq!(
        crate::test_support::fs::read_to_string(backend.log_path()).unwrap(),
        "invalid log JSON"
    );
    assert!(backend.recent_completions(1).is_err());
}

#[test]
fn calendar_undo_and_recompletion_preserve_reports_across_sessions() {
    for backend in [
        Box::new(SqliteBackend::in_memory().unwrap()) as Box<dyn StorageBackend>,
        Box::new(json()),
    ] {
        let task_id = Uuid::from_u128(2);
        let mut store = super::tests::populated_store();
        let task = store.tasks.get_mut(&task_id).unwrap();
        task.pinned = None;
        task.status = ubu_core::TaskStatus::Backlog;
        task.category = Some("work".into());
        store.calendar_links.insert(task_id, "calendar-task".into());
        backend.save(&store).unwrap();
        let mut event = gcal::FetchedEvent {
            id: "calendar-task".into(),
            summary: "Task".into(),
            color_id: Some("8".into()),
            start: at(-3600),
            end: at(-1800),
            transparent: false,
        };
        let import = |event: &gcal::FetchedEvent, now| {
            let mut store = backend.load().unwrap();
            let events = std::slice::from_ref(event);
            let actuals =
                super::super::calendar_actuals(backend.as_ref(), &store, events, now).unwrap();
            let (report, entries) = gcal::import_from_calendar(
                &mut store,
                events,
                &[],
                now,
                ubu_core::Tier::UserShared,
                &Default::default(),
                &actuals,
            );
            backend.append_log(&entries).unwrap();
            backend.save(&store).unwrap();
            (report, entries)
        };
        let (report, entries) = import(&event, at(0));
        assert_eq!(report.completed, 1);
        let completion_id = entries[0].id;
        assert_eq!(backend.recent_completions(1).unwrap().len(), 1);
        event.color_id = None;
        let (report, entries) = import(&event, at(1));
        assert_eq!(report.reopened, 1);
        assert!(entries.iter().any(|entry| matches!(entry.kind,
            LogEntryKind::Command(CommandKind::UndoCompletion { completion_id: id, .. }) if id == completion_id)));
        assert_eq!(
            backend.load().unwrap().tasks[&task_id].status,
            ubu_core::TaskStatus::Backlog
        );
        // The correction is outside this window, but still retracts the fact.
        assert!(backend
            .completions_in_window(at(0), at(1))
            .unwrap()
            .is_empty());
        assert!(backend.recent_completions(10).unwrap().is_empty());
        assert_eq!(import(&event, at(2)).0.reopened, 0);
        event.color_id = Some("5".into());
        event.start = at(-5400);
        assert_eq!(import(&event, at(3)).0.completed, 1);
        let facts = backend.completions_in_window(at(0), at(4)).unwrap();
        assert_eq!(facts.len(), 1);
        assert_eq!(backend.recent_completions(1).unwrap(), facts);
        let totals = ubu_core::report_by_category(&backend.load().unwrap(), &facts, at(0), at(4));
        assert_eq!(totals["work"], Duration::minutes(60));
    }
}

#[test]
fn latest_actual_is_targeted_includes_ongoing_and_preserves_full_timestamp_ties() {
    for backend in [
        Box::new(SqliteBackend::in_memory().unwrap()) as Box<dyn StorageBackend>,
        Box::new(json()),
    ] {
        let mut first = done(1, 0, None);
        first.at += Duration::milliseconds(900);
        let mut earlier = first.clone();
        earlier.id = Uuid::from_u128(2);
        earlier.at -= Duration::milliseconds(500);
        backend.append_log(&[first.clone(), earlier]).unwrap();
        assert_eq!(
            backend.latest_actual(Uuid::from_u128(101)).unwrap(),
            Some(first.clone())
        );
        let mut ongoing = first.clone();
        ongoing.id = Uuid::from_u128(3);
        ongoing.kind = LogEntryKind::Fact(FactKind::Actual {
            item_id: Uuid::from_u128(101),
            status: ActualStatus::Ongoing,
            actual: None,
        });
        backend
            .append_log(&[ongoing.clone(), command(4, 1000), done(5, 1000, None)])
            .unwrap();
        assert_eq!(
            backend.latest_actual(Uuid::from_u128(101)).unwrap(),
            Some(ongoing)
        );
        assert!(backend
            .latest_actual(Uuid::from_u128(999))
            .unwrap()
            .is_none());
        // A duplicate log ID cannot index a different task or retract a completion.
        let mut duplicate = done(1, 10, None);
        duplicate.kind = LogEntryKind::Command(CommandKind::UndoCompletion {
            task_id: Uuid::from_u128(101),
            completion_id: first.id,
        });
        backend.append_log(&[duplicate]).unwrap();
        assert_eq!(backend.recent_completions(10).unwrap().len(), 3);
    }
}

#[test]
fn ql1_backfill_is_atomic_preserves_rows_and_does_not_replay_store() {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE log(id TEXT PRIMARY KEY, at INTEGER NOT NULL, data TEXT NOT NULL, item_id TEXT, is_completion INTEGER NOT NULL DEFAULT 0)").unwrap();
    let completion = done(
        1,
        0,
        Some(TimeWindow {
            start: at(-60),
            end: at(0),
        }),
    );
    let undo = ubu_core::log_undo_completion(Uuid::from_u128(101), completion.id, at(100));
    for entry in [&completion, &undo] {
        connection
            .execute(
                "INSERT INTO log(id, at, data, item_id, is_completion) VALUES (?1, ?2, ?3, ?4, ?5)",
                params![
                    entry.id.to_string(),
                    entry.at.timestamp(),
                    serde_json::to_string(entry).unwrap(),
                    completion_fact(entry).map(|fact| fact.item_id.to_string()),
                    i64::from(completion_fact(entry).is_some())
                ],
            )
            .unwrap();
    }
    let expected_rows: Vec<(i64, String, i64, String)> = connection
        .prepare("SELECT rowid, id, at, data FROM log ORDER BY at, rowid")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
        })
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    let backend = SqliteBackend::initialize(connection).unwrap();
    assert_eq!(
        backend.latest_actual(Uuid::from_u128(101)).unwrap(),
        Some(completion)
    );
    assert!(backend.recent_completions(10).unwrap().is_empty());
    assert_eq!(backend.load().unwrap(), Store::new());
    let rows = super::tests::log_rows(&backend);
    assert_eq!(rows, expected_rows);
    // A subsequent initialization doesn't parse old rows again.
    backend
        .connection
        .execute(
            "INSERT INTO log(id, at, data) VALUES ('unrelated', 0, 'invalid JSON')",
            [],
        )
        .unwrap();
    let backend = SqliteBackend::initialize(backend.connection).unwrap();
    assert_eq!(backend.load().unwrap(), Store::new());
    assert_eq!(super::tests::log_rows(&backend).len(), rows.len() + 1);
    assert!(backend.recent_completions(10).unwrap().is_empty());
}

#[test]
fn undo_only_retracts_the_named_task_and_limits_apply_after_corrections() {
    for backend in [
        Box::new(SqliteBackend::in_memory().unwrap()) as Box<dyn StorageBackend>,
        Box::new(json()),
    ] {
        let entries = [done(1, 0, None), done(2, 1, None), done(3, 2, None)];
        backend.append_log(&entries).unwrap();
        backend
            .append_log(&[
                ubu_core::log_undo_completion(Uuid::from_u128(999), entries[0].id, at(5)),
                ubu_core::log_undo_completion(Uuid::from_u128(103), entries[2].id, at(5)),
            ])
            .unwrap();
        assert_eq!(
            backend.recent_completions(2).unwrap(),
            vec![expected(2, 1, None), expected(1, 0, None)]
        );
    }
}

#[test]
fn done_handler_keeps_append_and_save_failures_distinct() {
    use clap::Parser;
    for fail_append in [true, false] {
        let backend = SqliteBackend::in_memory().unwrap();
        let original = super::tests::populated_store();
        backend.save(&original).unwrap();
        let trigger = if fail_append {
            "CREATE TRIGGER fail BEFORE INSERT ON log BEGIN SELECT RAISE(ABORT, 'append failure'); END;"
        } else {
            "CREATE TRIGGER fail BEFORE DELETE ON tasks BEGIN SELECT RAISE(ABORT, 'save failure'); END;"
        };
        backend.connection.execute_batch(trigger).unwrap();
        let task_id = Uuid::from_u128(2).to_string();
        let command = crate::Cli::try_parse_from(["quick-ubu", "done", &task_id]).unwrap();
        let error = crate::run_with_backend(command, &backend).unwrap_err();
        assert!(error.contains(if fail_append {
            "append failure"
        } else {
            "save failure"
        }));
        assert_eq!(backend.load().unwrap(), original);
        assert_eq!(
            backend.recent_completions(10).unwrap().len(),
            usize::from(!fail_append)
        );
    }
}

#[test]
fn latest_actual_query_uses_index_without_loading_other_tasks() {
    let backend = SqliteBackend::in_memory().unwrap();
    backend.append_log(&[done(1, 0, None)]).unwrap();
    backend
        .connection
        .execute(
            "INSERT INTO log(id, at, data) VALUES ('bad', 0, 'invalid JSON')",
            [],
        )
        .unwrap();
    assert_eq!(
        backend.latest_actual(Uuid::from_u128(101)).unwrap(),
        Some(done(1, 0, None))
    );
    let mut statement = backend.connection.prepare(
        "EXPLAIN QUERY PLAN SELECT log.data FROM log_actuals a JOIN log ON log.id = a.log_id WHERE a.item_id = 'task' ORDER BY a.at DESC, a.nanos DESC, a.rowid DESC LIMIT 1"
    ).unwrap();
    let plan: Vec<String> = statement
        .query_map([], |row| row.get(3))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(
        plan.iter()
            .any(|line| line.contains("SEARCH a USING INDEX idx_log_actual")),
        "{plan:?}"
    );
    assert!(
        !plan
            .iter()
            .any(|line| line.contains("SCAN") || line.contains("TEMP B-TREE")),
        "{plan:?}"
    );
}
