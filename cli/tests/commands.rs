use crate::test_support::{self, fs, Output};
use std::path::{Path, PathBuf};

use chrono::{Duration, NaiveTime};
use ubu_core::{Recurrence, RoutineTemplate, Store, TaskStatus, Tier};
use uuid::Uuid;

fn memory_store() -> (PathBuf, PathBuf) {
    let directory = PathBuf::from("memory").join(format!("quick-ubu-cli-{}", Uuid::new_v4()));
    (directory.clone(), directory.join("store.db"))
}

fn quick_ubu(store: &Path, arguments: &[&str]) -> Output {
    test_support::run(store, arguments, "")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn quick_ubu_with_input(store: &Path, command: &str, input: &str) -> Output {
    test_support::run(store, &[command], input)
}

#[test]
fn prioritize_enqueues_before_review_and_quitting_keeps_the_queue_in_order() {
    let (directory, store_path) = memory_store();
    for title in ["Alpha", "Bravo", "Charlie"] {
        assert_success(&quick_ubu(
            &store_path,
            &["add", "--title", title, "--duration", "30"],
        ));
    }
    let output = quick_ubu_with_input(&store_path, "prioritize", "q\n");
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.starts_with("enqueued 3\n"));
    assert_eq!(stdout.matches("Preference:").count(), 1);
    let before: Store = serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    let task_ids: Vec<_> = before.tasks.keys().copied().collect();
    let pairs: Vec<_> = before
        .pending_decisions
        .iter()
        .map(|decision| {
            let ubu_core::Proposal::Preference { a, b, .. } = &decision.proposal else {
                panic!("expected preference")
            };
            (*a, *b)
        })
        .collect();
    assert_eq!(
        pairs,
        vec![
            (task_ids[0], task_ids[1]),
            (task_ids[0], task_ids[2]),
            (task_ids[1], task_ids[2])
        ]
    );
    let output = quick_ubu_with_input(&store_path, "review", "q\n");
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout)
            .unwrap()
            .matches("Preference:")
            .count(),
        1
    );
    let after: Store = serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    assert_eq!(after, before);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn review_presents_each_pending_pair_once_and_drains_all_skipped_decisions() {
    let (directory, store_path) = memory_store();
    for title in ["Alpha", "Bravo", "Charlie"] {
        assert_success(&quick_ubu(
            &store_path,
            &["add", "--title", title, "--duration", "30"],
        ));
    }
    assert_success(&quick_ubu_with_input(&store_path, "prioritize", "q\n"));
    let before: Store = serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    let output = quick_ubu_with_input(&store_path, "review", "s\ns\ns\n");
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert_eq!(stdout.matches("Preference:").count(), 3);
    let after: Store = serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    assert!(after.pending_decisions.is_empty());
    assert_eq!(after.decision_history.len(), 3);
    for decision in before.pending_decisions {
        assert_eq!(
            after
                .decision_history
                .iter()
                .filter(|record| record.proposal == decision.proposal)
                .count(),
            1
        );
    }
    assert!(after
        .decision_history
        .iter()
        .all(|record| record.resolution == ubu_core::Resolution::Skipped));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn done_persists_completion_and_report_reads_it_without_mutating_store() {
    let (directory, store_path) = memory_store();
    let added = quick_ubu(
        &store_path,
        &[
            "add",
            "--title",
            "Work",
            "--duration",
            "90",
            "--category",
            "work",
        ],
    );
    assert_success(&added);
    let task_id = String::from_utf8(added.stdout).unwrap();
    let before = chrono::Utc::now();
    assert_success(&quick_ubu(&store_path, &["done", task_id.trim()]));
    let after = chrono::Utc::now();
    let contents = fs::read_to_string(&store_path).unwrap();
    let store: Store = serde_json::from_str(&contents).unwrap();
    let id = Uuid::parse_str(task_id.trim()).unwrap();
    assert_eq!(store.tasks[&id].status, TaskStatus::Done);
    assert_eq!(store.log.len(), 1);
    assert!(before <= store.log[0].at && store.log[0].at <= after);
    assert_eq!(
        store.log[0].kind,
        ubu_core::LogEntryKind::Fact(ubu_core::FactKind::Actual {
            item_id: id,
            status: ubu_core::ActualStatus::Done,
            actual: None,
        })
    );
    let report = quick_ubu(&store_path, &["report"]);
    assert_success(&report);
    assert_eq!(
        String::from_utf8(report.stdout).unwrap(),
        "work   1h 30m\nTotal   1h 30m\n"
    );
    assert_eq!(fs::read_to_string(&store_path).unwrap(), contents);
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn report_cli_clamps_transparent_pins_and_sorts_categories() {
    let (directory, store_path) = memory_store();
    for args in [
        vec![
            "add",
            "--title",
            "Personal",
            "--duration",
            "90",
            "--pin",
            "2026-08-31T23:30:00Z",
            "--category",
            "personal",
            "--transparent",
        ],
        vec![
            "add",
            "--title",
            "Work",
            "--duration",
            "120",
            "--pin",
            "2026-09-01T10:00:00Z",
            "--category",
            "work",
        ],
        vec![
            "add",
            "--title",
            "Other",
            "--duration",
            "15",
            "--pin",
            "2026-09-01T12:00:00Z",
        ],
    ] {
        assert_success(&quick_ubu(&store_path, &args));
    }
    let report = quick_ubu(
        &store_path,
        &["report", "--from", "2026-09-01", "--to", "2026-09-02"],
    );
    assert_success(&report);
    assert_eq!(
        String::from_utf8(report.stdout).unwrap(),
        "work   2h 0m\npersonal   1h 0m\n(uncategorized)   0h 15m\nTotal   3h 15m\n"
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn report_empty_store_and_invalid_windows_leave_the_store_empty() {
    let (_directory, store_path) = memory_store();
    let report = quick_ubu(&store_path, &["report", "--days", "14"]);
    assert_success(&report);
    assert_eq!(String::from_utf8(report.stdout).unwrap(), "Total   0h 0m\n");
    for args in [
        vec!["report", "--from", "2026-02-30"],
        vec!["report", "--from", "2026-09-02", "--to", "2026-09-01"],
    ] {
        let report = quick_ubu(&store_path, &args);
        assert!(!report.status.success());
        assert!(!report.stderr.is_empty());
    }
    assert!(fs::exists(&store_path));
    assert_eq!(
        serde_json::from_str::<Store>(&fs::read_to_string(&store_path).unwrap()).unwrap(),
        Store::new()
    );
}

#[test]
fn set_color_persists_and_color_list_reflects_overrides() {
    let (directory, store_path) = memory_store();
    let defaults = quick_ubu(&store_path, &["color-list"]);
    assert_success(&defaults);
    let stdout = String::from_utf8(defaults.stdout).unwrap();
    assert_eq!(stdout.lines().count(), 11);
    assert!(stdout.lines().any(|line| line == "personal  3"));
    assert!(fs::exists(&store_path));
    assert_eq!(
        serde_json::from_str::<Store>(&fs::read_to_string(&store_path).unwrap()).unwrap(),
        Store::new()
    );

    for color in ["5", "7"] {
        assert_success(&quick_ubu(&store_path, &["set-color", "personal", color]));
        let store: Store = serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
        assert_eq!(store.category_colors.len(), 1);
        assert_eq!(store.category_colors["personal"], color);

        let output = quick_ubu(&store_path, &["color-list"]);
        assert_success(&output);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.lines().count(), 11);
        assert!(stdout
            .lines()
            .any(|line| line == format!("personal  {color}")));
        assert!(stdout.lines().any(|line| line == "work  9"));
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn add_repeatable_reminders_persist_and_appear_in_replan() {
    let (directory, store_path) = memory_store();
    assert_success(&quick_ubu(
        &store_path,
        &[
            "add",
            "--title",
            "Notify me",
            "--duration",
            "15",
            "--reminder",
            "10",
            "--reminder",
            "0",
        ],
    ));
    assert_success(&quick_ubu(
        &store_path,
        &["add", "--title", "Quiet task", "--duration", "15"],
    ));
    let store: Store = serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    assert_eq!(
        store
            .tasks
            .values()
            .find(|task| task.title == "Notify me")
            .unwrap()
            .reminders,
        vec![10, 0]
    );
    assert!(store
        .tasks
        .values()
        .find(|task| task.title == "Quiet task")
        .unwrap()
        .reminders
        .is_empty());
    let output = quick_ubu(
        &store_path,
        &["replan", "--horizon", "2099-01-01T00:00:00Z"],
    );
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout
        .lines()
        .any(|line| line.contains("Notify me") && line.contains("reminders:[10,0]m")));
    assert!(stdout
        .lines()
        .any(|line| line.contains("Quiet task") && !line.contains("reminders:")));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn canonical_routines_import_list_generate_and_replan_with_reminders() {
    let (directory, store_path) = memory_store();
    let example = Path::new(env!("CARGO_MANIFEST_DIR")).join("../docs/example-routine.json");
    assert_success(&quick_ubu(
        &store_path,
        &["routine-import", example.to_str().unwrap()],
    ));
    let output = quick_ubu(&store_path, &["routine-list"]);
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout.contains("MonthlyFirstWorkday"));
    assert!(stdout.contains("QuarterlyFirstWorkday"));
    assert!(stdout
        .lines()
        .any(|line| line.contains("Daily check-in") && line.contains("reminders:[0]m")));
    assert!(stdout
        .lines()
        .any(|line| line.contains("Weekly review") && line.contains("reminders:[10,0]m")));
    assert!(stdout
        .lines()
        .any(|line| line.contains("Monthly planning") && !line.contains("reminders:")));
    assert_success(&quick_ubu(
        &store_path,
        &[
            "generate",
            "--from",
            "2099-01-01",
            "--days",
            "7",
            "--tz",
            "UTC",
        ],
    ));
    let store: Store = serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    assert!(store
        .tasks
        .values()
        .any(|task| task.title == "Monthly planning"));
    assert!(store
        .tasks
        .values()
        .any(|task| task.title == "Quarterly planning"));
    assert!(store
        .tasks
        .values()
        .filter(|task| task.title == "Daily check-in")
        .all(|task| task.reminders == vec![0]));
    let output = quick_ubu(
        &store_path,
        &["replan", "--horizon", "2099-01-01T00:00:00Z"],
    );
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).unwrap();
    assert!(stdout
        .lines()
        .any(|line| line.contains("Daily check-in") && line.contains("reminders:[0]m")));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn set_model_command_persists_and_replaces_the_model() {
    let (directory, store_path) = memory_store();
    for model in ["first-model", "replacement-model"] {
        assert_success(&quick_ubu(&store_path, &["set-model", model]));
        let store: Store = serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
        assert_eq!(store.ollama_model.as_deref(), Some(model));
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn advise_and_ollama_replan_without_model_fail_before_http_or_save() {
    let (_, store_path) = memory_store();
    // There is no configured model, so neither command can reach the transport.
    for arguments in [vec!["advise"], vec!["replan", "--planner", "ollama"]] {
        let output = quick_ubu(&store_path, &arguments);
        assert!(!output.status.success());
        assert_eq!(
            String::from_utf8(output.stderr).unwrap(),
            "quick-ubu: no ollama model set; run: quick-ubu set-model <name>\n"
        );
        assert!(fs::exists(&store_path));
        assert_eq!(
            serde_json::from_str::<Store>(&fs::read_to_string(&store_path).unwrap()).unwrap(),
            Store::new()
        );
    }
}

#[test]
fn add_pin_persists_a_scheduled_pinned_task() {
    let (directory, store_path) = memory_store();
    let output = quick_ubu(
        &store_path,
        &[
            "add",
            "--title",
            "Calendar commitment",
            "--duration",
            "45",
            "--pin",
            "2030-01-02T15:00:00Z",
        ],
    );
    assert_success(&output);

    let store: Store = serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    let task = store.tasks.values().next().expect("one task was added");
    let pinned = task.pinned.as_ref().expect("task is pinned");
    assert_eq!(task.status, TaskStatus::Scheduled);
    assert_eq!(pinned.start.to_rfc3339(), "2030-01-02T15:00:00+00:00");
    assert_eq!(pinned.end.to_rfc3339(), "2030-01-02T15:45:00+00:00");

    fs::remove_dir_all(directory).expect("memory directory is removable");
}

#[test]
fn add_category_sets_it_and_omitting_the_flag_defaults_to_none() {
    let (directory, store_path) = memory_store();
    let categorized = quick_ubu(
        &store_path,
        &[
            "add",
            "--title",
            "Categorized task",
            "--duration",
            "30",
            "--category",
            "business",
            "--transparent",
        ],
    );
    assert_success(&categorized);
    let uncategorized = quick_ubu(
        &store_path,
        &["add", "--title", "Plain task", "--duration", "15"],
    );
    assert_success(&uncategorized);

    let store: Store = serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    let categorized_task = store
        .tasks
        .values()
        .find(|task| task.title == "Categorized task")
        .unwrap();
    let plain_task = store
        .tasks
        .values()
        .find(|task| task.title == "Plain task")
        .unwrap();
    assert_eq!(categorized_task.category.as_deref(), Some("business"));
    assert!(categorized_task.transparent);
    assert_eq!(plain_task.category, None);
    assert!(!plain_task.transparent);

    let replanned = quick_ubu(
        &store_path,
        &["replan", "--horizon", "2099-01-01T00:00:00Z"],
    );
    assert_success(&replanned);
    let replan_output = String::from_utf8(replanned.stdout).unwrap();
    assert!(replan_output.contains("Categorized task  business  transparent"));

    fs::remove_dir_all(directory).expect("memory directory is removable");
}

#[test]
fn next_prints_the_expected_dynamic_task_and_window() {
    let (directory, store_path) = memory_store();
    let pinned = quick_ubu(
        &store_path,
        &[
            "add",
            "--title",
            "Pinned future",
            "--duration",
            "30",
            "--pin",
            "2099-01-01T00:00:00Z",
        ],
    );
    assert_success(&pinned);
    let dynamic = quick_ubu(
        &store_path,
        &["add", "--title", "Expected next", "--duration", "30"],
    );
    assert_success(&dynamic);

    let output = quick_ubu(&store_path, &["next"]);
    assert_success(&output);
    let stdout = String::from_utf8(output.stdout).expect("stdout is UTF-8");
    assert!(stdout.contains("Expected next"));
    assert!(!stdout.contains("Pinned future"));
    assert!(stdout.contains('–'));
    assert_eq!(stdout.matches("+00:00").count(), 2);

    fs::remove_dir_all(directory).expect("memory directory is removable");
}

#[test]
fn next_prints_nothing_ready_for_an_empty_store() {
    let (directory, store_path) = memory_store();

    let output = quick_ubu(&store_path, &["next"]);
    assert_success(&output);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "nothing ready\n");

    if fs::exists(&directory) {
        fs::remove_dir_all(directory).expect("memory directory is removable");
    }
}

#[test]
fn routine_import_list_and_generate_complete_the_cli_flow() {
    let (directory, store_path) = memory_store();
    fs::create_dir_all(&directory).expect("memory directory is creatable");
    let import_path = directory.join("routines.json");
    let routines = vec![
        RoutineTemplate {
            id: Uuid::from_u128(1),
            title: "Morning focus".to_string(),
            tier: Tier::UserShared,
            start_time: NaiveTime::from_hms_opt(6, 30, 0).unwrap(),
            duration: Duration::minutes(45),
            affect_cost: 2,
            category: Some("personal".to_string()),
            transparent: true,
            reminders: Vec::new(),
            after: Vec::new(),
            dynamic: false,
            latest_tod: None,
            recurrence: Recurrence::Daily,
        },
        RoutineTemplate {
            id: Uuid::from_u128(2),
            title: "Pay bills".to_string(),
            tier: Tier::SemiPublic,
            start_time: NaiveTime::from_hms_opt(18, 0, 0).unwrap(),
            duration: Duration::minutes(15),
            affect_cost: 1,
            category: None,
            transparent: false,
            reminders: Vec::new(),
            after: Vec::new(),
            dynamic: false,
            latest_tod: None,
            recurrence: Recurrence::MonthlyDay {
                days: [1, 15].into_iter().collect(),
            },
        },
    ];
    fs::write(
        &import_path,
        serde_json::to_string_pretty(&routines).unwrap(),
    )
    .expect("routine import fixture is writable");

    let imported = quick_ubu(
        &store_path,
        &["routine-import", import_path.to_str().unwrap()],
    );
    assert_success(&imported);
    assert_eq!(String::from_utf8(imported.stdout).unwrap(), "imported 2\n");
    let imported_store: Store =
        serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    assert_eq!(imported_store.routines().len(), 2);
    assert!(imported_store.tasks.is_empty());

    let listed = quick_ubu(&store_path, &["routine-list"]);
    assert_success(&listed);
    let list_output = String::from_utf8(listed.stdout).unwrap();
    assert!(list_output.contains("Morning focus"));
    assert!(list_output.contains("Morning focus  personal"));
    assert!(list_output.contains("Morning focus  personal  transparent"));
    assert!(list_output.contains("user-shared"));
    assert!(list_output.contains("06:30:00"));
    assert!(list_output.contains("2700s"));
    assert!(list_output.contains("Daily"));
    assert!(list_output.contains("Pay bills"));
    assert!(list_output.contains("MonthlyDay[1,15]"));

    let generated = quick_ubu(
        &store_path,
        &[
            "generate",
            "--from",
            "2030-01-01",
            "--days",
            "2",
            "--tz",
            "UTC",
        ],
    );
    assert_success(&generated);
    assert_eq!(
        String::from_utf8(generated.stdout).unwrap(),
        "created 3, skipped 0\n"
    );
    let generated_store: Store =
        serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    assert_eq!(generated_store.tasks.len(), 3);
    assert!(generated_store
        .tasks
        .values()
        .all(|task| task.status == TaskStatus::Scheduled && task.pinned.is_some()));

    let repeated = quick_ubu(
        &store_path,
        &[
            "generate",
            "--from",
            "2030-01-01",
            "--days",
            "2",
            "--tz",
            "UTC",
        ],
    );
    assert_success(&repeated);
    assert_eq!(
        String::from_utf8(repeated.stdout).unwrap(),
        "created 0, skipped 3\n"
    );

    fs::remove_dir_all(directory).expect("memory directory is removable");
}

#[test]
fn generate_daily_routines_uses_all_requested_local_dates_after_release() {
    let (directory, store_path) = memory_store();
    fs::create_dir_all(&directory).unwrap();
    let mut store = Store::new();
    for (id, recurrence) in [
        (1, Recurrence::Daily),
        (
            2,
            Recurrence::MonthlyDay {
                days: [10].into_iter().collect(),
            },
        ),
    ] {
        store.upsert_routine(RoutineTemplate {
            id: Uuid::from_u128(id),
            title: format!("routine-{id}"),
            tier: Tier::UserShared,
            start_time: NaiveTime::from_hms_opt(0, 30, 0).unwrap(),
            duration: Duration::minutes(30),
            affect_cost: 0,
            category: None,
            transparent: false,
            reminders: Vec::new(),
            after: Vec::new(),
            dynamic: false,
            latest_tod: None,
            recurrence,
        });
    }
    test_support::seed(&store_path, &store);
    // The temporary launch gate was removed: all requested dates generate.
    for (days, expected) in [
        ("2", "created 3, skipped 0\n"),
        ("4", "created 2, skipped 3\n"),
    ] {
        let output = quick_ubu(
            &store_path,
            &[
                "generate",
                "--from",
                "2026-09-09",
                "--days",
                days,
                "--tz",
                "Pacific/Kiritimati",
            ],
        );
        assert_success(&output);
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
    let store: Store = serde_json::from_str(&fs::read_to_string(&store_path).unwrap()).unwrap();
    assert_eq!(store.tasks.len(), 5);
    let tz = ubu_core::Tz::Pacific__Kiritimati;
    let mut daily_dates = store
        .tasks
        .values()
        .filter(|task| task.title == "routine-1")
        .map(|task| {
            task.pinned
                .as_ref()
                .unwrap()
                .start
                .with_timezone(&tz)
                .date_naive()
                .to_string()
        })
        .collect::<Vec<_>>();
    daily_dates.sort();
    assert_eq!(
        daily_dates,
        ["2026-09-09", "2026-09-10", "2026-09-11", "2026-09-12"]
    );
    let output = quick_ubu(
        &store_path,
        &[
            "generate",
            "--from",
            "2026-09-12",
            "--days",
            "1",
            "--tz",
            "Pacific/Kiritimati",
        ],
    );
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        "created 0, skipped 1\n"
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn after_commands_persist_list_replan_and_remove_with_atomic_rejection() {
    let (directory, path) = memory_store();
    for title in ["First", "Second"] {
        assert_success(&quick_ubu(
            &path,
            &["add", "--title", title, "--duration", "30"],
        ));
    }
    let loaded: Store = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let first = loaded
        .tasks
        .values()
        .find(|task| task.title == "First")
        .unwrap()
        .id;
    let second = loaded
        .tasks
        .values()
        .find(|task| task.title == "Second")
        .unwrap()
        .id;
    let first_arg = first.to_string();
    let second_arg = second.to_string();
    for offset in ["30", "60", "-10", "60"] {
        assert_success(&quick_ubu(
            &path,
            &["after-add", &second_arg, &first_arg, offset],
        ));
    }
    let before = fs::read_to_string(&path).unwrap();
    let store: Store = serde_json::from_str(&before).unwrap();
    assert_eq!(
        store.tasks[&second].after,
        vec![ubu_core::AfterConstraint {
            task_id: first,
            offset: Duration::minutes(60)
        }]
    );
    for args in [vec!["after-list"], vec!["after-list", &second_arg]] {
        let output = quick_ubu(&path, &args);
        assert_success(&output);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.contains("Second"));
        assert!(text.contains(": 60m"));
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }
    for args in [
        vec!["after-add", &first_arg, &second_arg, "30"],
        vec!["dep-add", &first_arg, &second_arg],
        vec!["after-add", &second_arg, &first_arg, "invalid"],
        vec!["after-add", &second_arg, &first_arg, "9223372036854775807"],
    ] {
        let output = quick_ubu(&path, &args);
        assert!(!output.status.success());
        assert!(!output.stderr.is_empty());
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }
    let now = "2026-09-11T00:00:00Z".parse().unwrap();
    let plan = ubu_core::re_plan(
        &store,
        ubu_core::ComputeTarget::DesktopOllama,
        now,
        now,
        &[],
        &ubu_core::AffectBudget { cap: 100 },
        &ubu_core::DeterministicPlacer,
    )
    .unwrap();
    let first_end = plan
        .entries
        .iter()
        .find(|entry| entry.item == first)
        .unwrap()
        .window
        .end;
    let second_start = plan
        .entries
        .iter()
        .find(|entry| entry.item == second)
        .unwrap()
        .window
        .start;
    assert_eq!(second_start, first_end + Duration::minutes(60));
    assert_success(&quick_ubu(&path, &["after-rm", &second_arg, &first_arg]));
    let loaded: Store = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert!(loaded.tasks[&second].after.is_empty());
    let output = quick_ubu(&path, &["after-list"]);
    assert_success(&output);
    assert!(output.stdout.is_empty());
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn add_must_finish_by_persists_rfc3339_and_rejects_invalid_input_atomically() {
    let (directory, path) = memory_store();
    assert_success(&quick_ubu(
        &path,
        &[
            "add",
            "--title",
            "Windowed",
            "--duration",
            "30",
            "--earliest-start",
            "2099-09-11T09:00:00-04:00",
            "--must-finish-by",
            "2099-09-11T09:30:00-04:00",
        ],
    ));
    assert_success(&quick_ubu(
        &path,
        &["add", "--title", "Floating", "--duration", "30"],
    ));
    let before = fs::read_to_string(&path).unwrap();
    let store: Store = serde_json::from_str(&before).unwrap();
    let bounded = store
        .tasks
        .values()
        .find(|task| task.title == "Windowed")
        .unwrap();
    assert_eq!(
        bounded.must_finish_by,
        Some("2099-09-11T13:30:00Z".parse().unwrap())
    );
    assert_eq!(
        bounded.earliest_start,
        Some("2099-09-11T13:00:00Z".parse().unwrap())
    );
    assert!(store
        .tasks
        .values()
        .find(|task| task.title == "Floating")
        .unwrap()
        .must_finish_by
        .is_none());
    for invalid in ["not-a-date", "2099-09-11", "2099-09-11T09:30:00"] {
        let output = quick_ubu(
            &path,
            &[
                "add",
                "--title",
                "Invalid",
                "--duration",
                "30",
                "--must-finish-by",
                invalid,
            ],
        );
        assert!(!output.status.success());
        assert!(String::from_utf8(output.stderr)
            .unwrap()
            .contains("invalid RFC3339 datetime"));
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn routine_import_and_generate_persist_dynamic_local_day_windows() {
    let (directory, path) = memory_store();
    let import_path = directory.join("dynamic-routines.json");
    let mut routines: Vec<RoutineTemplate> =
        serde_json::from_str(include_str!("../../docs/example-routine.json")).unwrap();
    routines.truncate(1);
    routines[0].dynamic = true;
    routines[0].start_time = NaiveTime::from_hms_opt(8, 0, 0).unwrap();
    routines[0].latest_tod = Some(NaiveTime::from_hms_opt(10, 0, 0).unwrap());
    fs::write(&import_path, serde_json::to_string(&routines).unwrap()).unwrap();
    assert_success(&quick_ubu(
        &path,
        &["routine-import", import_path.to_str().unwrap()],
    ));
    for expected in ["created 1, skipped 0\n", "created 0, skipped 1\n"] {
        let output = quick_ubu(
            &path,
            &[
                "generate",
                "--from",
                "2099-09-11",
                "--days",
                "1",
                "--tz",
                "UTC",
            ],
        );
        assert_success(&output);
        assert_eq!(String::from_utf8(output.stdout).unwrap(), expected);
    }
    let store: Store = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(store.routines[&routines[0].id], routines[0]);
    let task = store.tasks.values().next().unwrap();
    assert!(task.pinned.is_none());
    assert_eq!(
        task.earliest_start,
        Some("2099-09-11T08:00:00Z".parse().unwrap())
    );
    assert_eq!(
        task.must_finish_by,
        Some("2099-09-11T10:00:00Z".parse().unwrap())
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn review_and_prioritize_skip_queued_decisions_after_cli_completion() {
    for command in ["review", "prioritize"] {
        let (directory, path) = memory_store();
        for title in ["Finished task", "Remaining A", "Remaining B"] {
            assert_success(&quick_ubu(
                &path,
                &["add", "--title", title, "--duration", "30"],
            ));
        }
        assert_success(&quick_ubu_with_input(&path, "prioritize", "q\n"));
        let before: Store = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(before.pending_decisions.len(), 3);
        let finished = before
            .tasks
            .values()
            .find(|task| task.title == "Finished task")
            .unwrap()
            .id;
        assert_success(&quick_ubu(&path, &["done", &finished.to_string()]));
        let output = quick_ubu_with_input(&path, command, "s\n");
        assert_success(&output);
        let stdout = String::from_utf8(output.stdout).unwrap();
        assert_eq!(stdout.matches("Preference:").count(), 1);
        assert!(stdout.contains("Remaining A"));
        assert!(stdout.contains("Remaining B"));
        assert!(!stdout.contains("Finished task"));
        let after: Store = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(after.tasks[&finished].status, TaskStatus::Done);
        assert_eq!(after.pending_decisions.len(), 2); // Retained for completion repairs.
        assert_eq!(after.decision_history.len(), 1);
        let output = quick_ubu_with_input(&path, command, "");
        assert_success(&output);
        assert!(!String::from_utf8(output.stdout)
            .unwrap()
            .contains("Preference:"));
        assert_eq!(
            serde_json::from_str::<Store>(&fs::read_to_string(&path).unwrap()).unwrap(),
            after
        );
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn review_and_prioritize_present_tags_first_and_persist_confirmation_or_rejection() {
    for (command, answer, expected) in [
        ("review", "c\nq\n", ubu_core::Resolution::Confirmed),
        ("prioritize", "r\nq\n", ubu_core::Resolution::Rejected),
    ] {
        let (directory, path) = memory_store();
        for title in ["First", "Second"] {
            assert_success(&quick_ubu(
                &path,
                &["add", "--title", title, "--duration", "30"],
            ));
        }
        let mut store: Store = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        let ids = store.tasks.keys().copied().collect::<Vec<_>>();
        store.pending_decisions = vec![
            ubu_core::PendingDecision {
                id: Uuid::from_u128(100),
                source: ubu_core::DecisionSource::Elicitation,
                proposal: ubu_core::Proposal::Preference {
                    a: ids[0],
                    b: ids[1],
                    suggested: None,
                },
            },
            ubu_core::PendingDecision {
                id: Uuid::from_u128(101),
                source: ubu_core::DecisionSource::Advisor,
                proposal: ubu_core::Proposal::Tag {
                    task_id: ids[0],
                    tag: "focus".into(),
                },
            },
        ];
        test_support::seed(&path, &store);
        let output = quick_ubu_with_input(&path, command, answer);
        assert_success(&output);
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(text.find("Tag:").unwrap() < text.find("Preference:").unwrap());
        assert!(text.contains("[c] confirm, [r] reject"));
        let loaded: Store = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(loaded.pending_decisions.len(), 1);
        assert_eq!(loaded.decision_history[0].resolution, expected);
        assert_eq!(
            loaded.tasks[&ids[0]].tags,
            if expected == ubu_core::Resolution::Confirmed {
                vec!["focus"]
            } else {
                vec![]
            }
        );
        fs::remove_dir_all(directory).unwrap();
    }
}

#[test]
fn suggest_tags_parses_model_override_and_requires_model_before_http() {
    use clap::Parser;
    for (args, expected) in [
        (vec!["quick-ubu", "suggest-tags"], None),
        (
            vec!["quick-ubu", "suggest-tags", "--model", "local-test"],
            Some("local-test".to_owned()),
        ),
    ] {
        let cli = crate::Cli::try_parse_from(args).unwrap();
        let crate::Command::SuggestTags { model, .. } = cli.command else {
            panic!("expected suggest-tags");
        };
        assert_eq!(model, expected);
    }
    let (directory, path) = memory_store();
    let output = quick_ubu(&path, &["suggest-tags"]);
    assert!(!output.status.success());
    assert_eq!(
        String::from_utf8(output.stderr).unwrap(),
        "quick-ubu: no ollama model set; run: quick-ubu set-model <name>\n"
    );
    assert_eq!(
        serde_json::from_str::<Store>(&fs::read_to_string(&path).unwrap()).unwrap(),
        Store::new()
    );
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn classifiers_parse_default_custom_and_disabled_history_without_http() {
    use clap::Parser;
    for command in ["suggest-tags", "advise"] {
        for (extra, expected) in [
            (vec![], 20),
            (vec!["--history", "0"], 0),
            (vec!["--history", "7"], 7),
        ] {
            let mut args = vec!["quick-ubu", command];
            args.extend(extra);
            let cli = crate::Cli::try_parse_from(args).unwrap();
            let history = match cli.command {
                crate::Command::SuggestTags { history, .. }
                | crate::Command::Advise { history, .. } => history,
                _ => panic!("expected classifier"),
            };
            assert_eq!(history, expected);
        }
        for invalid in ["-1", "1.5", "abc", "18446744073709551616"] {
            assert!(
                crate::Cli::try_parse_from(["quick-ubu", command, "--history", invalid]).is_err()
            );
        }
    }
}

#[test]
fn classifiers_parse_batching_and_selection_flags_without_http() {
    use clap::Parser;
    for command in ["suggest-tags", "advise"] {
        for custom in [false, true] {
            let mut args = vec!["quick-ubu", command];
            if custom {
                args.extend(["--batch-size", "3", "--category", "work", "--limit", "7"]);
                if command == "suggest-tags" {
                    args.push("--untagged");
                } else {
                    args.extend(["--tag", "focus"]);
                }
            }
            let (size, category, limit) = match crate::Cli::try_parse_from(args).unwrap().command {
                crate::Command::SuggestTags {
                    batch_size,
                    category,
                    limit,
                    untagged,
                    ..
                } => {
                    assert_eq!(untagged, custom);
                    (batch_size.get(), category, limit)
                }
                crate::Command::Advise {
                    batch_size,
                    category,
                    limit,
                    tag,
                    ..
                } => {
                    assert_eq!(tag.as_deref(), custom.then_some("focus"));
                    (batch_size.get(), category, limit)
                }
                _ => panic!("expected classifier"),
            };
            assert_eq!(size, if custom { 3 } else { 25 });
            assert_eq!(category.as_deref(), custom.then_some("work"));
            assert_eq!(limit, custom.then_some(7));
        }
        for invalid in ["0", "-1", "1.5", "abc", "18446744073709551616"] {
            assert!(
                crate::Cli::try_parse_from(["quick-ubu", command, "--batch-size", invalid])
                    .is_err()
            );
        }
        for invalid in ["-1", "1.5", "abc", "18446744073709551616"] {
            assert!(
                crate::Cli::try_parse_from(["quick-ubu", command, "--limit", invalid]).is_err()
            );
        }
    }
    assert!(crate::Cli::try_parse_from(["quick-ubu", "suggest-tags", "--tag", "focus"]).is_err());
    assert!(crate::Cli::try_parse_from(["quick-ubu", "advise", "--untagged"]).is_err());
}

#[test]
fn classifier_handlers_apply_empty_selection_filters_and_save_without_http() {
    let (directory, path) = memory_store();
    assert_success(&quick_ubu(&path, &["set-model", "unused-stub"]));
    for command in ["suggest-tags", "advise"] {
        let output = quick_ubu(&path, &[command]);
        assert_success(&output);
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "batches run 0, failed 0, enqueued 0, dropped_known 0, dropped_cycle 0\n"
        );
    }
    assert_success(&quick_ubu(
        &path,
        &[
            "add",
            "--title",
            "Tagged task",
            "--duration",
            "30",
            "--category",
            "work",
        ],
    ));
    let mut store: Store = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    store.tasks.values_mut().next().unwrap().tags = vec!["focus".into()];
    test_support::seed(&path, &store);
    for args in [
        vec!["suggest-tags", "--limit", "0"],
        vec!["advise", "--limit", "0"],
        vec!["suggest-tags", "--category", "absent"],
        vec!["advise", "--category", "absent"],
        vec!["suggest-tags", "--untagged"],
        vec!["advise", "--tag", "absent"],
    ] {
        let output = quick_ubu(&path, &args);
        assert_success(&output);
        assert!(output.stderr.is_empty());
        assert_eq!(
            String::from_utf8(output.stdout).unwrap(),
            "batches run 0, failed 0, enqueued 0, dropped_known 0, dropped_cycle 0\n"
        );
        assert_eq!(
            serde_json::from_str::<Store>(&fs::read_to_string(&path).unwrap()).unwrap(),
            store
        );
    }
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn clarify_parses_defaults_overrides_and_rejects_invalid_arguments() {
    use clap::Parser;

    for (args, expected_model, rounds, history) in [
        (vec!["quick-ubu", "clarify", "abcd"], None, 5, 20),
        (
            vec![
                "quick-ubu",
                "clarify",
                "abcd",
                "--model",
                "test",
                "--max-rounds",
                "2",
                "--history",
                "7",
            ],
            Some("test"),
            2,
            7,
        ),
        (
            vec![
                "quick-ubu",
                "clarify",
                "abcd",
                "--max-rounds",
                "0",
                "--history",
                "0",
            ],
            None,
            0,
            0,
        ),
    ] {
        let crate::Command::Clarify {
            prefix,
            model,
            max_rounds,
            history: actual_history,
        } = crate::Cli::try_parse_from(args).unwrap().command
        else {
            panic!("expected clarify");
        };
        assert_eq!(prefix, "abcd");
        assert_eq!(model.as_deref(), expected_model);
        assert_eq!(max_rounds, rounds);
        assert_eq!(actual_history, history);
    }
    assert!(crate::Cli::try_parse_from(["quick-ubu", "clarify"]).is_err());
    for flag in ["--max-rounds", "--history"] {
        for invalid in ["-1", "1.5", "abc", "18446744073709551616"] {
            assert!(
                crate::Cli::try_parse_from(["quick-ubu", "clarify", "abcd", flag, invalid])
                    .is_err()
            );
        }
    }
}

#[test]
fn clarify_resolves_task_and_model_before_any_transport_or_editor_use() {
    let (directory, path) = memory_store();
    assert_success(&quick_ubu(
        &path,
        &["add", "--title", "Task", "--duration", "30"],
    ));
    let store: Store = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let task_id = store.tasks.keys().next().unwrap().to_string();
    let output = quick_ubu(&path, &["clarify", "not-a-task"]);
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("no task matches"));
    let output = quick_ubu(&path, &["clarify", &task_id]);
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("no ollama model set"));
    assert_eq!(
        serde_json::from_str::<Store>(&fs::read_to_string(&path).unwrap()).unwrap(),
        store
    );
    assert_success(&quick_ubu(
        &path,
        &["add", "--title", "Other task", "--duration", "30"],
    ));
    let output = quick_ubu(&path, &["clarify", ""]);
    assert!(!output.status.success());
    assert!(String::from_utf8(output.stderr)
        .unwrap()
        .contains("ambiguous prefix"));
    fs::remove_dir_all(directory).unwrap();
}

#[test]
fn clarify_zero_round_command_saves_lore_without_model_calls_or_editor_use() {
    let (directory, path) = memory_store();
    assert_success(&quick_ubu(
        &path,
        &["add", "--title", "Task", "--duration", "30"],
    ));
    let mut store: Store = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    let id = *store.tasks.keys().next().unwrap();
    let prefix = id.simple().to_string()[..8].to_owned();
    let output = quick_ubu(
        &path,
        &[
            "clarify",
            &prefix,
            "--model",
            "unused",
            "--max-rounds",
            "0",
            "--history",
            "0",
        ],
    );
    assert_success(&output);
    assert_eq!(
        String::from_utf8(output.stdout).unwrap(),
        format!("clarified {id}; enqueued 0, dropped_known 0, dropped_cycle 0\n")
    );
    assert!(output.stderr.is_empty());
    store.tasks.get_mut(&id).unwrap().detail = Some(String::new());
    assert_eq!(
        serde_json::from_str::<Store>(&fs::read_to_string(&path).unwrap()).unwrap(),
        store
    );
    // Persisted model follows the same resolution path as the other commands.
    assert_success(&quick_ubu(&path, &["set-model", "unused"]));
    store.ollama_model = Some("unused".into());
    store.tasks.get_mut(&id).unwrap().detail = Some("Existing detail".into());
    test_support::seed(&path, &store);
    assert_success(&quick_ubu(
        &path,
        &["clarify", &prefix, "--max-rounds", "0"],
    ));
    assert_eq!(
        serde_json::from_str::<Store>(&fs::read_to_string(&path).unwrap()).unwrap(),
        store
    );
    fs::remove_dir_all(directory).unwrap();
}
