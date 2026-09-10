use super::stub_tests::{at, fetched_event, id, task};
use super::*;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::thread;
use std::time::{Duration as StdDuration, Instant};

fn linked_store() -> Store {
    let mut store = Store::new();
    store.upsert_task(task(1, "Dynamic", Tier::UserShared, false, None));
    store.upsert_calendar_link(id(1), "dynamic".into());
    store
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
            .unwrap().events;
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
        .unwrap().events;
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
        .unwrap().events;
    assert_eq!(events.len(), 2);
    assert!(events.contains(&recent));
    assert_eq!(*transport.get_calls.borrow(), vec!["pinned-outside"]);

    // Old Done tasks are not fetched by ID outside the correction window.
    let old_transport = StubTransport::default();
    let events = fetch_import_events(&store, &old_transport, &window)
        .await
        .unwrap().events;
    assert!(events.is_empty());
    assert_eq!(*old_transport.get_calls.borrow(), vec!["pending", "pinned-outside"]);
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

// Exercise the actual reqwest pagination/parsing path without credentials or
// external network access. Each response closes its connection for simplicity.
fn http_server(
    responses: Vec<(u16, serde_json::Value)>,
) -> (reqwest::Url, thread::JoinHandle<Vec<String>>) {
    raw_http_server(
        responses
            .into_iter()
            .map(|(status, body)| (status, body.to_string()))
            .collect(),
    )
}

fn raw_http_server(
    responses: Vec<(u16, String)>,
) -> (reqwest::Url, thread::JoinHandle<Vec<String>>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let url =
        reqwest::Url::parse(&format!("http://{}/events", listener.local_addr().unwrap())).unwrap();
    let handle = thread::spawn(move || {
        let deadline = Instant::now() + StdDuration::from_secs(10);
        let mut requests = Vec::new();
        for (status, body) in responses {
            let mut socket = loop {
                match listener.accept() {
                    Ok((socket, _)) => break socket,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        assert!(
                            Instant::now() < deadline,
                            "timed out waiting for HTTP request"
                        );
                        thread::sleep(StdDuration::from_millis(5));
                    }
                    Err(error) => panic!("accept failed: {error}"),
                }
            };
            socket
                .set_read_timeout(Some(StdDuration::from_secs(5)))
                .unwrap();
            let mut request = Vec::new();
            while !request.ends_with(b"\r\n\r\n") {
                let mut byte = [0];
                socket.read_exact(&mut byte).unwrap();
                request.push(byte[0]);
            }
            requests.push(String::from_utf8(request).unwrap());
            write!(socket, "HTTP/1.1 {status} Test\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
        }
        requests
    });
    (url, handle)
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
    let (url, server) = http_server(vec![
        (
            200,
            serde_json::json!({ "items": events_without_date_times(), "nextPageToken": "next" }),
        ),
        (200, serde_json::json!({ "items": mixed })),
    ]);
    let transport = GoogleCalendarTransport::new("unused", "unused", "primary");
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
    assert_eq!(server.join().unwrap().len(), 2);
}

#[tokio::test]
async fn linked_fetch_skips_untimed_events_but_rejects_invalid_timestamps() {
    let fixtures = events_without_date_times();
    let mut responses: Vec<_> = fixtures.iter().cloned().map(|event| (200, event)).collect();
    let mut invalid = google_event("invalid");
    invalid["start"]["dateTime"] = serde_json::json!("not a timestamp");
    responses.push((200, invalid));
    let (url, server) = http_server(responses);
    let transport = GoogleCalendarTransport::new("unused", "unused", "primary");
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
    assert_eq!(server.join().unwrap().len(), fixtures.len() + 1);
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
        let (url, server) = raw_http_server(vec![
            (200, "{\"items\": [], \"nextPageToken\": \"next\"}".into()),
            (200, body.clone()),
        ]);
        let transport = GoogleCalendarTransport::new("unused", "unused", "primary");
        let error = transport
            .list_events_with_token(url, "test-token", at(-1440), at(10080))
            .await
            .unwrap_err();
        assert!(error.starts_with("failed to parse Google Calendar events: "));
        assert_eq!(error.split_once("\nResponse body:\n").unwrap().1, body);
        assert!(!error.contains("test-token"));
        assert_eq!(server.join().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn linked_event_parse_errors_include_the_entire_body_and_schema_error() {
    let body = "{\n  \"id\": \"invalid\", \"start\": {\"dateTime\": 123}\n}\n";
    let (url, server) = raw_http_server(vec![(200, body.into())]);
    let transport = GoogleCalendarTransport::new("unused", "unused", "primary");
    let error = transport
        .get_event_with_token(url, "test-token")
        .await
        .unwrap_err();
    assert!(error.starts_with("failed to parse Google Calendar event: "));
    assert!(error.contains("invalid type: integer `123`, expected a string"));
    assert_eq!(error.split_once("\nResponse body:\n").unwrap().1, body);
    assert_eq!(server.join().unwrap().len(), 1);
}

#[tokio::test]
async fn pagination_imports_completion_on_a_later_page_even_after_an_empty_page() {
    let (url, server) = http_server(vec![
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
    let transport = GoogleCalendarTransport::new("unused", "unused", "primary");
    let events = transport
        .list_events_with_token(url, "test-token", at(-1440), at(10080))
        .await
        .unwrap();
    let requests = server.join().unwrap();
    assert_eq!(events.len(), 2);
    for (index, request) in requests.iter().enumerate() {
        let target = request
            .lines()
            .next()
            .unwrap()
            .split_whitespace()
            .nth(1)
            .unwrap();
        let url = reqwest::Url::parse(&format!("http://localhost{target}")).unwrap();
        let query: BTreeMap<_, _> = url.query_pairs().into_owned().collect();
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
        let (url, server) = http_server(vec![
            (
                200,
                serde_json::json!({ "items": [google_event("dynamic")], "nextPageToken": "again" }),
            ),
            second_page,
        ]);
        let transport = GoogleCalendarTransport::new("unused", "unused", "primary");
        assert!(transport
            .list_events_with_token(url, "test-token", at(-1440), at(10080))
            .await
            .is_err());
        assert_eq!(server.join().unwrap().len(), 2);
    }
}

#[tokio::test]
async fn get_event_handles_past_events_deletions_and_errors() {
    let (url, server) = http_server(vec![
        (200, google_event("dynamic")),
        (404, serde_json::json!({})),
        (410, serde_json::json!({})),
        (
            200,
            serde_json::json!({ "id": "deleted", "status": "cancelled" }),
        ),
        (403, serde_json::json!({ "error": "forbidden" })),
    ]);
    let transport = GoogleCalendarTransport::new("unused", "unused", "primary");
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
    assert!(server
        .join()
        .unwrap()
        .iter()
        .all(|request| request.starts_with("GET /events HTTP/1.1\r\n")));
}
