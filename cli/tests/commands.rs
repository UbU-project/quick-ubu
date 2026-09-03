use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use ubu_core::{Store, TaskStatus};
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
