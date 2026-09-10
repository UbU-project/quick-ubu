use super::stub_tests::{at, fetched_event, id, task};
use super::*;

fn linked_store() -> Store {
    let mut store = Store::new();
    store.upsert_task(task(1, "Dynamic", Tier::UserShared, false, None));
    store.upsert_calendar_link(id(1), "dynamic".into());
    store
}

#[tokio::test]
async fn confirmed_deleted_pinned_instance_is_removed_without_changing_its_routine() {
    let mut store = Store::new();
    let routine = ubu_core::RoutineTemplate {
        id: id(10),
        title: "Daily".into(),
        tier: Tier::UserShared,
        start_time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
        duration: Duration::minutes(30),
        affect_cost: 0,
        category: None,
        transparent: false,
        reminders: vec![],
        after: Vec::new(),
        recurrence: ubu_core::Recurrence::Daily,
    };
    store.upsert_routine(routine.clone());
    ubu_core::generate_routine_tasks(&mut store, at(0).date_naive(), 1, ubu_core::Tz::UTC);
    let task_id = *store.tasks.keys().next().unwrap();
    store.upsert_calendar_link(task_id, "pinned".into());
    store.export_signatures.insert(task_id, "signature".into());
    let transport = StubTransport::default();
    let fetched = fetch_import_events(
        &store,
        &transport,
        &calendar_import_window(at(0), None, None).unwrap(),
    )
    .await
    .unwrap();
    assert!(fetched.events.is_empty());
    assert_eq!(fetched.deleted, vec![task_id]);
    assert_eq!(*transport.get_calls.borrow(), vec!["pinned"]);
    assert!(store.tasks.contains_key(&task_id)); // Fetch does not mutate the store.
    let report = import_from_calendar(
        &mut store,
        &fetched.events,
        &fetched.deleted,
        at(0),
        Tier::UserShared,
        &BTreeMap::new(),
    );
    assert_eq!(report.removed, 1);
    assert!(!store.tasks.contains_key(&task_id));
    assert!(!store.calendar_links.contains_key(&task_id));
    assert!(!store.export_signatures.contains_key(&task_id));
    assert_eq!(store.routines[&routine.id], routine);
    assert_eq!(store.log.len(), 1);
    assert_eq!(
        store.log[0].kind,
        LogEntryKind::Command(ubu_core::CommandKind::RemoveTask { task_id })
    );
    assert_eq!(store.log[0].at, at(0));
    let generated =
        ubu_core::generate_routine_tasks(&mut store, at(0).date_naive(), 1, ubu_core::Tz::UTC);
    assert_eq!(generated.created, 1);
    assert!(store.tasks.contains_key(&task_id));
    assert_eq!(store.routines[&routine.id], routine);
}

#[tokio::test]
async fn confirmed_deleted_dynamic_task_is_removed_and_reimport_is_a_noop() {
    let mut store = linked_store();
    store.export_signatures.insert(id(1), "signature".into());
    let transport = StubTransport::default();
    let window = calendar_import_window(at(0), None, None).unwrap();
    let fetched = fetch_import_events(&store, &transport, &window)
        .await
        .unwrap();
    assert_eq!(fetched.deleted, vec![id(1)]);
    assert_eq!(*transport.get_calls.borrow(), vec!["dynamic"]);
    let report = import_from_calendar(
        &mut store,
        &fetched.events,
        &fetched.deleted,
        at(0),
        Tier::UserShared,
        &BTreeMap::new(),
    );
    assert_eq!(report.removed, 1);
    assert!(
        store.tasks.is_empty()
            && store.calendar_links.is_empty()
            && store.export_signatures.is_empty()
    );
    assert!(
        matches!(store.log[0].kind, LogEntryKind::Command(ubu_core::CommandKind::RemoveTask { task_id }) if task_id == id(1))
    );
    let removed = store.clone();
    let report = import_from_calendar(
        &mut store,
        &[],
        &fetched.deleted,
        at(1),
        Tier::UserShared,
        &BTreeMap::new(),
    );
    assert_eq!(report.removed, 0);
    assert_eq!(store, removed);
}

#[tokio::test]
async fn list_absence_with_a_successful_per_id_fetch_processes_moved_tasks_without_removal() {
    for pinned in [false, true] {
        let mut store = Store::new();
        store.upsert_task(task(1, "Moved", Tier::UserShared, pinned, None));
        store.upsert_calendar_link(id(1), "moved".into());
        let event = fetched_event("moved", "Moved", None, 60_000, 60_090);
        let transport = StubTransport {
            filter_dates: true,
            listed_events: vec![event.clone()],
            ..StubTransport::default()
        };
        let window = calendar_import_window(at(0), None, None).unwrap();
        assert!(transport
            .list_events(window.start, window.end)
            .await
            .unwrap()
            .is_empty());
        let fetched = fetch_import_events(&store, &transport, &window)
            .await
            .unwrap();
        assert!(fetched.deleted.is_empty());
        assert_eq!(fetched.events, vec![event.clone()]);
        assert_eq!(*transport.get_calls.borrow(), vec!["moved"]);
        let report = import_from_calendar(
            &mut store,
            &fetched.events,
            &fetched.deleted,
            at(0),
            Tier::UserShared,
            &BTreeMap::new(),
        );
        assert_eq!(report.removed, 0);
        assert_eq!(report.moved, usize::from(pinned));
        assert_eq!(report.resized, usize::from(!pinned));
        assert!(store.tasks.contains_key(&id(1)));
        assert_eq!(store.calendar_links[&id(1)], "moved");
        if pinned {
            assert_eq!(
                store.tasks[&id(1)].pinned,
                Some(TimeWindow {
                    start: event.start,
                    end: event.end
                })
            );
        } else {
            assert_eq!(store.tasks[&id(1)].est_duration, Duration::minutes(90));
        }
    }
}

#[tokio::test]
async fn deletion_detection_skips_done_tasks_and_links_without_tasks() {
    let mut store = linked_store();
    store.tasks.get_mut(&id(1)).unwrap().status = TaskStatus::Done;
    let mut pinned = task(2, "Done pin", Tier::UserShared, true, None);
    pinned.status = TaskStatus::Done;
    store.upsert_task(pinned);
    store.upsert_calendar_link(id(2), "done-pin".into());
    store.upsert_calendar_link(id(99), "orphan".into());
    let before = store.clone();
    let transport = StubTransport::default();
    let fetched = fetch_import_events(
        &store,
        &transport,
        &calendar_import_window(at(0), None, None).unwrap(),
    )
    .await
    .unwrap();
    assert!(fetched.deleted.is_empty());
    assert!(transport.get_calls.borrow().is_empty());
    assert_eq!(
        import_from_calendar(
            &mut store,
            &fetched.events,
            &fetched.deleted,
            at(0),
            Tier::UserShared,
            &BTreeMap::new()
        )
        .removed,
        0
    );
    assert_eq!(store, before);
}

#[test]
fn duplicate_deleted_ids_produce_one_removal_and_unknown_ids_are_ignored() {
    let mut store = linked_store();
    let report = import_from_calendar(
        &mut store,
        &[],
        &[id(1), id(1), id(99)],
        at(0),
        Tier::UserShared,
        &BTreeMap::new(),
    );
    assert_eq!(report.removed, 1);
    assert_eq!(store.log.len(), 1);
}

fn import(store: &mut Store, event: &FetchedEvent, now: DateTime<Utc>) -> ImportReport {
    import_from_calendar(
        store,
        std::slice::from_ref(event),
        &[],
        now,
        Tier::UserShared,
        &BTreeMap::new(),
    )
}

#[test]
fn import_window_always_looks_back_at_least_24_hours() {
    let now = at(10_000);
    for from in [
        None,
        Some(now),
        Some(now + Duration::days(1)),
        Some(now - Duration::hours(1)),
    ] {
        assert_eq!(
            calendar_import_window(now, from, None).unwrap(),
            TimeWindow {
                start: now - Duration::hours(24),
                end: now + Months::new(1),
            }
        );
    }
    let older = now - Duration::days(3);
    assert_eq!(
        calendar_import_window(now, Some(older), Some(now)).unwrap(),
        TimeWindow {
            start: older,
            end: now + Months::new(1)
        }
    );
    assert!(calendar_import_window(now, None, Some(now - Duration::days(2))).is_err());
}

#[tokio::test]
async fn linked_unfinished_events_are_imported_regardless_of_date() {
    // One event is older than the correction window; another is beyond the
    // discovery horizon. Neither may disappear from completion reconciliation.
    for (start, end) in [(-3000, -2940), (60_000, 60_060)] {
        let mut store = linked_store();
        let event = fetched_event("dynamic", "Dynamic", Some("8"), start, end);
        let transport = StubTransport {
            filter_dates: true,
            // An unrelated old event must not be captured as a side effect.
            listed_events: vec![
                event.clone(),
                fetched_event("unrelated", "Old", None, -4000, -3970),
            ],
            ..StubTransport::default()
        };
        let window = calendar_import_window(at(0), None, None).unwrap();
        let events = fetch_import_events(&store, &transport, &window)
            .await
            .unwrap()
            .events;
        assert_eq!(events, vec![event.clone()]);
        assert_eq!(*transport.get_calls.borrow(), vec!["dynamic"]);
        let report = import_from_calendar(
            &mut store,
            &events,
            &[],
            at(0),
            Tier::UserShared,
            &BTreeMap::new(),
        );
        assert_eq!(report.completed, 1);
        assert_eq!(report.captured, 0);
        assert_eq!(store.tasks[&id(1)].status, TaskStatus::Done);
        assert!(
            matches!(&store.log[0].kind, LogEntryKind::Fact(FactKind::Actual {
            status: ActualStatus::Done, actual: Some(window), ..
        }) if window.start == event.start && window.end == event.end)
        );
    }
}

#[test]
fn import_looks_a_calendar_month_ahead_including_short_months_and_year_rollover() {
    for (start, expected) in [
        ("2026-09-11T14:30:00Z", "2026-10-11T14:30:00Z"),
        ("2026-01-31T14:30:00Z", "2026-02-28T14:30:00Z"),
        ("2028-01-31T14:30:00Z", "2028-02-29T14:30:00Z"),
        ("2026-12-31T14:30:00Z", "2027-01-31T14:30:00Z"),
    ] {
        let now = DateTime::parse_from_rfc3339(start)
            .unwrap()
            .with_timezone(&Utc);
        let expected = DateTime::parse_from_rfc3339(expected)
            .unwrap()
            .with_timezone(&Utc);
        for to in [None, Some(now + Duration::days(7))] {
            assert_eq!(calendar_import_window(now, None, to).unwrap().end, expected);
        }
        let later = expected + Duration::days(10);
        assert_eq!(
            calendar_import_window(now, None, Some(later)).unwrap().end,
            later
        );
    }
    assert!(calendar_import_window(DateTime::<Utc>::MAX_UTC, None, None).is_err());
}

#[tokio::test]
async fn import_discovers_unlinked_events_beyond_the_old_seven_day_window() {
    let mut store = Store::new();
    let upcoming = fetched_event("upcoming", "Upcoming", Some("5"), 20 * 1440, 20 * 1440 + 60);
    let transport = StubTransport {
        filter_dates: true,
        listed_events: vec![
            upcoming.clone(),
            fetched_event("outside", "Outside", None, 40 * 1440, 40 * 1440 + 60),
        ],
        ..StubTransport::default()
    };
    let window = calendar_import_window(at(0), None, None).unwrap();
    let events = fetch_import_events(&store, &transport, &window)
        .await
        .unwrap()
        .events;
    assert_eq!(events, vec![upcoming]);
    let report = import_from_calendar(
        &mut store,
        &events,
        &[],
        at(0),
        Tier::UserShared,
        &BTreeMap::new(),
    );
    assert_eq!(report.captured, 1);
    assert_eq!(store.tasks.len(), 1);
}

#[tokio::test]
async fn discovery_includes_recent_completions_and_deduplicates_linked_events() {
    let mut store = linked_store();
    store.tasks.get_mut(&id(1)).unwrap().status = TaskStatus::Done;
    store.upsert_task(task(2, "Pending", Tier::UserShared, false, None));
    store.upsert_calendar_link(id(2), "pending".into());
    store.upsert_task(task(3, "Pinned", Tier::UserShared, true, None));
    store.upsert_calendar_link(id(3), "pinned-outside".into());
    let recent = fetched_event("dynamic", "Dynamic", None, -120, -60);
    let pending = fetched_event("pending", "Pending", None, 0, 30);
    let transport = StubTransport {
        filter_dates: true,
        listed_events: vec![recent.clone(), pending.clone(), pending],
        ..StubTransport::default()
    };
    let window = calendar_import_window(at(0), Some(at(0)), None).unwrap();
    let events = fetch_import_events(&store, &transport, &window)
        .await
        .unwrap()
        .events;
    assert_eq!(events.len(), 2);
    assert!(events.contains(&recent));
    assert_eq!(*transport.get_calls.borrow(), vec!["pinned-outside"]);

    // Old Done tasks are not fetched by ID outside the correction window.
    let old_transport = StubTransport::default();
    let events = fetch_import_events(&store, &old_transport, &window)
        .await
        .unwrap()
        .events;
    assert!(events.is_empty());
    assert_eq!(
        *old_transport.get_calls.borrow(),
        vec!["pending", "pinned-outside"]
    );
}

#[tokio::test]
async fn linked_fetch_failure_leaves_the_store_unchanged() {
    let store = linked_store();
    let before = store.clone();
    let transport = StubTransport {
        get_error: Some("unavailable".into()),
        ..StubTransport::default()
    };
    assert_eq!(
        fetch_import_events(
            &store,
            &transport,
            &calendar_import_window(at(0), None, None).unwrap()
        )
        .await
        .unwrap_err(),
        "unavailable"
    );
    assert_eq!(store, before);
}

#[test]
fn repeated_import_after_recolor_and_resize_preserves_duration_repairs() {
    let mut store = linked_store();
    let mut event = fetched_event("dynamic", "Dynamic", Some("8"), -120, -30);
    assert_eq!(import(&mut store, &event, at(0)).completed, 1);
    // Preserve the existing second-pass estimate repair, as requested.
    let second = import(&mut store, &event, at(1));
    assert_eq!(second.completed, 0);
    assert_eq!(second.resized, 1);
    assert_eq!(store.tasks[&id(1)].est_duration, Duration::minutes(90));
    let unchanged = store.clone();
    assert_eq!(import(&mut store, &event, at(2)).resized, 0);
    assert_eq!(store, unchanged);

    event.color_id = Some("5".into());
    event.start = at(-150);
    event.end = at(-20);
    assert_eq!(import(&mut store, &event, at(3)).resized, 1);
    assert_eq!(store.tasks[&id(1)].est_duration, Duration::minutes(130));
    assert_eq!(store.tasks[&id(1)].status, TaskStatus::Done);
    assert_eq!(
        store
            .log
            .iter()
            .filter(|entry| matches!(
                entry.kind,
                LogEntryKind::Fact(FactKind::Actual {
                    status: ActualStatus::Done,
                    ..
                })
            ))
            .count(),
        1
    );
    let unchanged = store.clone();
    import(&mut store, &event, at(4));
    assert_eq!(store, unchanged);
}

#[test]
fn removing_color_reopens_recent_calendar_completion_and_retracts_its_report() {
    let mut store = linked_store();
    let mut event = fetched_event("dynamic", "Dynamic", Some("8"), -60, -30);
    let original = store.clone();
    import(&mut store, &event, at(0));
    store
        .export_signatures
        .insert(id(1), "old signature".into());
    event.color_id = None;
    let report = import(&mut store, &event, at(1));
    assert_eq!(report.reopened, 1);
    assert_eq!(store.tasks[&id(1)].status, TaskStatus::Backlog);
    assert!(!store.export_signatures.contains_key(&id(1)));
    assert!(ubu_core::report_by_category(&store, at(0), at(0)).is_empty());
    let unchanged = store.clone();
    assert_eq!(import(&mut store, &event, at(2)).reopened, 0);
    assert_eq!(store, unchanged);
    let mut replayed = original;
    reconcile(&mut replayed, &store.log).unwrap();
    assert_eq!(replayed.tasks, store.tasks);

    // Correct the time and re-complete. Only the replacement completion counts.
    event.color_id = Some("5".into());
    event.start = at(-90);
    assert_eq!(import(&mut store, &event, at(3)).completed, 1);
    assert_eq!(
        ubu_core::report_by_category(&store, at(0), at(3))["(uncategorized)"],
        Duration::minutes(60)
    );
}

#[test]
fn undo_uses_the_event_end_time_and_does_not_undo_cli_done() {
    for (end, expected) in [(-1439, 1), (-1440, 0), (-1441, 0), (30, 1)] {
        let mut store = linked_store();
        let mut event = fetched_event("dynamic", "Dynamic", Some("8"), end - 30, end);
        import(&mut store, &event, at(-1));
        event.color_id = None;
        assert_eq!(import(&mut store, &event, at(0)).reopened, expected);
    }
    let mut store = linked_store();
    store.tasks.get_mut(&id(1)).unwrap().status = TaskStatus::Done;
    store.append_log(log_actual(id(1), ActualStatus::Done, None, at(-1)));
    let event = fetched_event("dynamic", "Dynamic", None, -60, -30);
    assert_eq!(import(&mut store, &event, at(0)).reopened, 0);
    assert_eq!(store.tasks[&id(1)].status, TaskStatus::Done);
}

// Exercise request construction, response parsing, and pagination entirely in
// memory. No sockets, credentials, or real HTTP requests are used.
fn stub_responses(
    responses: Vec<(u16, serde_json::Value)>,
) -> (reqwest::Url, GoogleCalendarTransport) {
    raw_stub_responses(
        responses
            .into_iter()
            .map(|(status, body)| (status, body.to_string()))
            .collect(),
    )
}

fn raw_stub_responses(responses: Vec<(u16, String)>) -> (reqwest::Url, GoogleCalendarTransport) {
    let transport = GoogleCalendarTransport::new("unused", "unused", "primary");
    transport
        .import_stub
        .import_responses
        .borrow_mut()
        .extend(responses);
    (
        reqwest::Url::parse("https://calendar.invalid/events").unwrap(),
        transport,
    )
}

fn google_event(event_id: &str) -> serde_json::Value {
    serde_json::json!({
        "id": event_id, "summary": "Dynamic", "colorId": "8",
        "start": { "dateTime": at(-60).to_rfc3339() },
        "end": { "dateTime": at(-30).to_rfc3339() }
    })
}

fn events_without_date_times() -> Vec<serde_json::Value> {
    vec![
        serde_json::json!({ "id": "all-day", "start": { "date": "2026-09-08" }, "end": { "date": "2026-09-09" } }),
        serde_json::json!({ "id": "multi-day", "colorId": "5", "start": { "date": "2026-09-08" }, "end": { "date": "2026-09-12" } }),
        serde_json::json!({ "id": "missing-start", "end": { "dateTime": at(0).to_rfc3339() } }),
        serde_json::json!({ "id": "missing-end", "start": { "dateTime": at(0).to_rfc3339() } }),
        serde_json::json!({ "id": "date-only-end", "start": { "dateTime": at(0).to_rfc3339() }, "end": { "date": "2026-09-12" } }),
        serde_json::json!({ "id": "null-start", "start": { "dateTime": null }, "end": { "dateTime": at(0).to_rfc3339() } }),
    ]
}

#[tokio::test]
async fn import_skips_untimed_events_and_continues_through_pages() {
    let mut mixed = events_without_date_times();
    mixed.push(google_event("dynamic"));
    let (url, transport) = stub_responses(vec![
        (
            200,
            serde_json::json!({ "items": events_without_date_times(), "nextPageToken": "next" }),
        ),
        (200, serde_json::json!({ "items": mixed })),
    ]);
    let events = transport
        .list_events_with_token(url, "test-token", at(-1440), at(10080))
        .await
        .unwrap();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].id, "dynamic");
    let mut store = linked_store();
    let report = import_from_calendar(
        &mut store,
        &events,
        &[],
        at(0),
        Tier::UserShared,
        &BTreeMap::new(),
    );
    assert_eq!(report.captured, 0);
    assert_eq!(report.completed, 1);
    assert_eq!(store.tasks.len(), 1);
    assert_eq!(store.tasks[&id(1)].status, TaskStatus::Done);
    assert_eq!(transport.import_stub.import_requests.borrow().len(), 2);
}

#[tokio::test]
async fn linked_fetch_skips_untimed_events_but_rejects_invalid_timestamps() {
    let fixtures = events_without_date_times();
    let mut responses: Vec<_> = fixtures.iter().cloned().map(|event| (200, event)).collect();
    let mut invalid = google_event("invalid");
    invalid["start"]["dateTime"] = serde_json::json!("not a timestamp");
    responses.push((200, invalid));
    let (url, transport) = stub_responses(responses);
    for _ in &fixtures {
        assert!(transport
            .get_event_with_token(url.clone(), "test-token")
            .await
            .unwrap()
            .is_none());
    }
    assert!(transport
        .get_event_with_token(url, "test-token")
        .await
        .unwrap_err()
        .contains("invalid Google Calendar start dateTime"));
    assert_eq!(
        transport.import_stub.import_requests.borrow().len(),
        fixtures.len() + 1
    );
}

#[tokio::test]
async fn list_parse_errors_include_the_entire_failing_page_body() {
    for body in [
        // Invalid field types still report the complete response body.
        "{\n  \"items\": [{\"id\": \"invalid\", \"start\": {\"dateTime\": 123}}]\n}\n".to_string(),
        // Invalid JSON must retain whitespace and content beyond typical log limits.
        format!(
            "not JSON\n{}\nend of response\n",
            "Calendar café 🌎 ".repeat(1000)
        ),
    ] {
        let (url, transport) = raw_stub_responses(vec![
            (200, "{\"items\": [], \"nextPageToken\": \"next\"}".into()),
            (200, body.clone()),
        ]);
        let error = transport
            .list_events_with_token(url, "test-token", at(-1440), at(10080))
            .await
            .unwrap_err();
        assert!(error.starts_with("failed to parse Google Calendar events: "));
        assert_eq!(error.split_once("\nResponse body:\n").unwrap().1, body);
        assert!(!error.contains("test-token"));
        assert_eq!(transport.import_stub.import_requests.borrow().len(), 2);
    }
}

#[tokio::test]
async fn linked_event_parse_errors_include_the_entire_body_and_schema_error() {
    let body = "{\n  \"id\": \"invalid\", \"start\": {\"dateTime\": 123}\n}\n";
    let (url, transport) = raw_stub_responses(vec![(200, body.into())]);
    let error = transport
        .get_event_with_token(url, "test-token")
        .await
        .unwrap_err();
    assert!(error.starts_with("failed to parse Google Calendar event: "));
    assert!(error.contains("invalid type: integer `123`, expected a string"));
    assert_eq!(error.split_once("\nResponse body:\n").unwrap().1, body);
    assert_eq!(transport.import_stub.import_requests.borrow().len(), 1);
}

#[tokio::test]
async fn pagination_imports_completion_on_a_later_page_even_after_an_empty_page() {
    let (url, transport) = stub_responses(vec![
        (
            200,
            serde_json::json!({ "items": [google_event("first")], "nextPageToken": "second +/=" }),
        ),
        (
            200,
            serde_json::json!({ "items": [], "nextPageToken": "third" }),
        ),
        (
            200,
            serde_json::json!({ "items": [google_event("dynamic")] }),
        ),
    ]);
    let events = transport
        .list_events_with_token(url, "test-token", at(-1440), at(10080))
        .await
        .unwrap();
    let requests = transport.import_stub.import_requests.borrow();
    assert_eq!(events.len(), 2);
    for (index, request) in requests.iter().enumerate() {
        let query: BTreeMap<_, _> = request.url().query_pairs().into_owned().collect();
        assert_eq!(query["timeMin"], at(-1440).to_rfc3339());
        assert_eq!(query["timeMax"], at(10080).to_rfc3339());
        assert_eq!(query["singleEvents"], "true");
        assert_eq!(
            query.get("pageToken").map(String::as_str),
            [None, Some("second +/="), Some("third")][index]
        );
    }
    let mut store = linked_store();
    let report = import_from_calendar(
        &mut store,
        &events,
        &[],
        at(0),
        Tier::UserShared,
        &BTreeMap::new(),
    );
    assert_eq!(report.completed, 1);
    assert_eq!(store.tasks[&id(1)].status, TaskStatus::Done);
}

#[tokio::test]
async fn pagination_errors_do_not_return_partial_results() {
    for second_page in [
        (503, serde_json::json!({ "error": "unavailable" })),
        (200, serde_json::json!({ "nextPageToken": "again" })),
    ] {
        let (url, transport) = stub_responses(vec![
            (
                200,
                serde_json::json!({ "items": [google_event("dynamic")], "nextPageToken": "again" }),
            ),
            second_page,
        ]);
        assert!(transport
            .list_events_with_token(url, "test-token", at(-1440), at(10080))
            .await
            .is_err());
        assert_eq!(transport.import_stub.import_requests.borrow().len(), 2);
    }
}

#[tokio::test]
async fn get_event_handles_past_events_deletions_and_errors() {
    let (url, transport) = stub_responses(vec![
        (200, google_event("dynamic")),
        (404, serde_json::json!({})),
        (410, serde_json::json!({})),
        (
            200,
            serde_json::json!({ "id": "deleted", "status": "cancelled" }),
        ),
        (403, serde_json::json!({ "error": "forbidden" })),
    ]);
    let event = transport
        .get_event_with_token(url.clone(), "test-token")
        .await
        .unwrap()
        .unwrap();
    assert_eq!(event.end, at(-30));
    for _ in 0..3 {
        assert!(transport
            .get_event_with_token(url.clone(), "test-token")
            .await
            .unwrap()
            .is_none());
    }
    assert!(transport
        .get_event_with_token(url, "test-token")
        .await
        .unwrap_err()
        .contains("403"));
    assert!(transport
        .import_stub
        .import_requests
        .borrow()
        .iter()
        .all(|request| request.method() == reqwest::Method::GET
            && request.url().path() == "/events"
            && request.url().query().is_none()));
}
