use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use chrono::{Duration, NaiveTime};
use ubu_core::{Recurrence, RoutineTemplate, Store, TaskStatus, Tier};
use uuid::Uuid;

fn temp_store() -> (PathBuf, PathBuf) {
    let directory = std::env::temp_dir().join(format!("quick-ubu-cli-{}", Uuid::new_v4()));
    (directory.clone(), directory.join("store.json"))
}

fn quick_ubu(store: &Path, arguments: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_quick-ubu"))
        .arg("--store")
        .arg(store)
        .args(arguments)
        .output()
        .expect("quick-ubu must run")
}

fn assert_success(output: &Output) {
    assert!(
        output.status.success(),
        "command failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn add_pin_persists_a_scheduled_pinned_task() {
    let (directory, store_path) = temp_store();
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

    fs::remove_dir_all(directory).expect("temporary directory is removable");
}

#[test]
fn add_category_sets_it_and_omitting_the_flag_defaults_to_none() {
    let (directory, store_path) = temp_store();
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
    assert_eq!(plain_task.category, None);

    let replanned = quick_ubu(
        &store_path,
        &["replan", "--horizon", "2099-01-01T00:00:00Z"],
    );
    assert_success(&replanned);
    let replan_output = String::from_utf8(replanned.stdout).unwrap();
    assert!(replan_output.contains("Categorized task  business"));

    fs::remove_dir_all(directory).expect("temporary directory is removable");
}

#[test]
fn next_prints_the_expected_dynamic_task_and_window() {
    let (directory, store_path) = temp_store();
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

    fs::remove_dir_all(directory).expect("temporary directory is removable");
}

#[test]
fn next_prints_nothing_ready_for_an_empty_store() {
    let (directory, store_path) = temp_store();

    let output = quick_ubu(&store_path, &["next"]);
    assert_success(&output);
    assert_eq!(String::from_utf8(output.stdout).unwrap(), "nothing ready\n");

    if directory.exists() {
        fs::remove_dir_all(directory).expect("temporary directory is removable");
    }
}

#[test]
fn routine_import_list_and_generate_complete_the_cli_flow() {
    let (directory, store_path) = temp_store();
    fs::create_dir_all(&directory).expect("temporary directory is creatable");
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
            transparent: false,
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

    fs::remove_dir_all(directory).expect("temporary directory is removable");
}
