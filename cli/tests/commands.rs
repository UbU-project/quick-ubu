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
fn generate_blocks_daily_routines_before_launch_using_local_dates() {
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
            recurrence,
        });
    }
    test_support::seed(&store_path, &store);
    // Entirely before launch: monthly routines still generate, daily ones do not.
    for (days, expected) in [
        ("2", "created 1, skipped 0\n"),
        ("4", "created 2, skipped 1\n"),
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
    assert_eq!(store.tasks.len(), 3);
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
    assert_eq!(daily_dates, ["2026-09-11", "2026-09-12"]);
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
