//! Google Calendar export behind a transport boundary.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ubu_core::{
    log_actual, log_capture, log_edit_duration, log_edit_pin, reconcile, visible_as_content,
    ActualStatus, DeferPolicy, Id, Plan, Provenance, Store, Task, TaskStatus, Tier, TimeWindow,
};
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod};

const CALENDAR_SCOPE: &str = "https://www.googleapis.com/auth/calendar";
const CALENDAR_API_BASE: &str = "https://www.googleapis.com/calendar/v3/calendars";
const CAPTURE_NAMESPACE: Id = Id::from_u128(0xfbb8_2411_158b_4a86_9f69_42d19fec7587);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    pub summary: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub color_id: Option<String>,
    pub transparent: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedEvent {
    pub id: String,
    pub summary: String,
    pub color_id: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub transparent: bool,
}

#[allow(async_fn_in_trait)]
pub trait CalendarTransport {
    async fn create_event(&self, event: &CalendarEvent) -> Result<String, String>;

    async fn update_event(&self, event_id: &str, event: &CalendarEvent) -> Result<(), String>;

    async fn list_events(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<FetchedEvent>, String>;
}

/// Real Google transport. This code is compiled, but automated tests never
/// exercise it; the operator must verify credentials, OAuth, and API behavior at
/// runtime against a live Google Calendar endpoint.
pub struct GoogleCalendarTransport {
    credentials_path: PathBuf,
    token_cache_path: PathBuf,
    calendar_id: String,
    client: reqwest::Client,
}

impl GoogleCalendarTransport {
    pub fn new(
        credentials_path: impl Into<PathBuf>,
        token_cache_path: impl Into<PathBuf>,
        calendar_id: impl Into<String>,
    ) -> Self {
        Self {
            credentials_path: credentials_path.into(),
            token_cache_path: token_cache_path.into(),
            calendar_id: calendar_id.into(),
            client: reqwest::Client::new(),
        }
    }

    async fn access_token(&self) -> Result<String, String> {
        let secret = yup_oauth2::read_application_secret(&self.credentials_path)
            .await
            .map_err(|error| {
                format!(
                    "failed to read Google credentials {}: {error}",
                    self.credentials_path.display()
                )
            })?;
        let authenticator =
            InstalledFlowAuthenticator::builder(secret, InstalledFlowReturnMethod::HTTPRedirect)
                .persist_tokens_to_disk(&self.token_cache_path)
                .build()
                .await
                .map_err(|error| format!("failed to initialize Google OAuth: {error}"))?;
        let token = authenticator
            .token(&[CALENDAR_SCOPE])
            .await
            .map_err(|error| format!("failed to obtain Google OAuth token: {error}"))?;

        token
            .token()
            .map(str::to_owned)
            .ok_or_else(|| "Google OAuth returned no access token".to_string())
    }

    fn event_url(&self, event_id: Option<&str>) -> reqwest::Url {
        let mut url = reqwest::Url::parse(CALENDAR_API_BASE)
            .expect("the constant Google Calendar API URL is valid");
        let mut segments = url
            .path_segments_mut()
            .expect("the Google Calendar API URL supports path segments");
        segments
            .pop_if_empty()
            .push(&self.calendar_id)
            .push("events");
        if let Some(event_id) = event_id {
            segments.push(event_id);
        }
        drop(segments);
        url
    }

    async fn response_error(response: reqwest::Response) -> String {
        let status = response.status();
        let body = response
            .text()
            .await
            .unwrap_or_else(|error| format!("failed to read response body: {error}"));
        format!("Google Calendar API returned {status}: {body}")
    }
}

#[derive(Serialize)]
struct GoogleEventBody {
    summary: String,
    start: GoogleEventTime,
    end: GoogleEventTime,
    #[serde(rename = "colorId", skip_serializing_if = "Option::is_none")]
    color_id: Option<String>,
    transparency: String,
}

#[derive(Deserialize, Serialize)]
struct GoogleEventTime {
    #[serde(rename = "dateTime")]
    date_time: String,
}

impl From<&CalendarEvent> for GoogleEventBody {
    fn from(event: &CalendarEvent) -> Self {
        Self {
            summary: event.summary.clone(),
            start: GoogleEventTime {
                date_time: event.start.to_rfc3339(),
            },
            end: GoogleEventTime {
                date_time: event.end.to_rfc3339(),
            },
            color_id: event.color_id.clone(),
            transparency: if event.transparent {
                "transparent".to_string()
            } else {
                "opaque".to_string()
            },
        }
    }
}

#[derive(Deserialize)]
struct CreatedEvent {
    id: String,
}

#[derive(Deserialize)]
struct ListedEvents {
    #[serde(default)]
    items: Vec<ListedEvent>,
}

#[derive(Deserialize)]
struct ListedEvent {
    id: String,
    #[serde(default)]
    summary: String,
    #[serde(rename = "colorId")]
    color_id: Option<String>,
    start: GoogleEventTime,
    end: GoogleEventTime,
    #[serde(default)]
    transparency: Option<String>,
}

impl TryFrom<ListedEvent> for FetchedEvent {
    type Error = String;

    fn try_from(event: ListedEvent) -> Result<Self, Self::Error> {
        Ok(Self {
            id: event.id,
            summary: event.summary,
            color_id: event.color_id,
            start: DateTime::parse_from_rfc3339(&event.start.date_time)
                .map_err(|error| format!("invalid Google Calendar start dateTime: {error}"))?
                .with_timezone(&Utc),
            end: DateTime::parse_from_rfc3339(&event.end.date_time)
                .map_err(|error| format!("invalid Google Calendar end dateTime: {error}"))?
                .with_timezone(&Utc),
            transparent: event.transparency.as_deref() == Some("transparent"),
        })
    }
}

impl CalendarTransport for GoogleCalendarTransport {
    async fn create_event(&self, event: &CalendarEvent) -> Result<String, String> {
        let token = self.access_token().await?;
        let response = self
            .client
            .post(self.event_url(None))
            .bearer_auth(token)
            .json(&GoogleEventBody::from(event))
            .send()
            .await
            .map_err(|error| format!("failed to create Google Calendar event: {error}"))?;
        if !response.status().is_success() {
            return Err(Self::response_error(response).await);
        }

        response
            .json::<CreatedEvent>()
            .await
            .map(|created| created.id)
            .map_err(|error| format!("failed to parse created Google Calendar event: {error}"))
    }

    async fn update_event(&self, event_id: &str, event: &CalendarEvent) -> Result<(), String> {
        let token = self.access_token().await?;
        let response = self
            .client
            .patch(self.event_url(Some(event_id)))
            .bearer_auth(token)
            .json(&GoogleEventBody::from(event))
            .send()
            .await
            .map_err(|error| format!("failed to update Google Calendar event: {error}"))?;
        if !response.status().is_success() {
            return Err(Self::response_error(response).await);
        }

        Ok(())
    }

    /// Compiled but not exercised by automated tests; live behavior is verified
    /// by the operator against Google Calendar.
    async fn list_events(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<FetchedEvent>, String> {
        let token = self.access_token().await?;
        let query = [
            ("timeMin", from.to_rfc3339()),
            ("timeMax", to.to_rfc3339()),
            ("singleEvents", "true".to_string()),
        ];
        let response = self
            .client
            .get(self.event_url(None))
            .bearer_auth(token)
            .query(&query)
            .send()
            .await
            .map_err(|error| format!("failed to list Google Calendar events: {error}"))?;
        if !response.status().is_success() {
            return Err(Self::response_error(response).await);
        }

        let events = response
            .json::<ListedEvents>()
            .await
            .map_err(|error| format!("failed to parse Google Calendar events: {error}"))?;
        events
            .items
            .into_iter()
            .map(FetchedEvent::try_from)
            .collect()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportReport {
    pub created: usize,
    pub updated: usize,
}

pub async fn export_plan<T: CalendarTransport>(
    store: &mut Store,
    plan: &Plan,
    transport: &T,
    color_map: &BTreeMap<String, String>,
    calendar_clearance: Tier,
) -> Result<ExportReport, String> {
    let mut report = ExportReport {
        created: 0,
        updated: 0,
    };

    for entry in &plan.entries {
        let task = store
            .tasks
            .get(&entry.item)
            .ok_or_else(|| format!("plan entry references unknown task {}", entry.item))?;
        let visible = visible_as_content(task.tier, calendar_clearance);
        let event = CalendarEvent {
            summary: if visible {
                task.title.clone()
            } else {
                "Busy".to_string()
            },
            start: entry.window.start,
            end: entry.window.end,
            color_id: if visible && task.pinned.is_some() {
                task.category
                    .as_ref()
                    .and_then(|category| color_map.get(category))
                    .cloned()
            } else {
                None
            },
            transparent: task.transparent,
        };
        let existing_event_id = store.calendar_link(entry.item).cloned();

        if let Some(event_id) = existing_event_id {
            transport.update_event(&event_id, &event).await?;
            report.updated += 1;
        } else {
            let event_id = transport.create_event(&event).await?;
            store.upsert_calendar_link(entry.item, event_id);
            report.created += 1;
        }
    }

    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportReport {
    pub captured: usize,
    pub completed: usize,
    pub moved: usize,
    pub resized: usize,
}

pub fn import_from_calendar(
    store: &mut Store,
    events: &[FetchedEvent],
    now: DateTime<Utc>,
    captured_tier: Tier,
    color_to_category: &BTreeMap<String, String>,
) -> ImportReport {
    let event_to_task: BTreeMap<String, Id> = store
        .calendar_links
        .iter()
        .map(|(task_id, event_id)| (event_id.clone(), *task_id))
        .collect();
    let mut entries = Vec::new();
    let mut captured_links = Vec::new();
    let mut report = ImportReport {
        captured: 0,
        completed: 0,
        moved: 0,
        resized: 0,
    };

    for event in events {
        let window = TimeWindow {
            start: event.start,
            end: event.end,
        };
        if let Some(task_id) = event_to_task.get(&event.id).copied() {
            let Some(task) = store.tasks.get(&task_id) else {
                continue;
            };
            if task.pinned.is_none() {
                if event.color_id.is_some() && task.status != TaskStatus::Done {
                    entries.push(log_actual(task_id, ActualStatus::Done, Some(window), now));
                    report.completed += 1;
                } else {
                    let new_dur = window.end - window.start;
                    if new_dur > chrono::Duration::zero() && new_dur != task.est_duration {
                        entries.push(log_edit_duration(task_id, new_dur, now));
                        report.resized += 1;
                    }
                }
            } else if task.pinned.as_ref() != Some(&window) {
                entries.push(log_edit_pin(task_id, Some(window), now));
                report.moved += 1;
            }
            continue;
        }

        let task_id = Id::new_v5(&CAPTURE_NAMESPACE, event.id.as_bytes());
        let is_commitment = event.color_id.is_some();
        let category = event
            .color_id
            .as_ref()
            .and_then(|color_id| color_to_category.get(color_id))
            .cloned();
        let task = Task {
            id: task_id,
            tier: captured_tier,
            title: event.summary.clone(),
            detail: None,
            objective_ids: Vec::new(),
            skills: Vec::new(),
            affect_cost: 0,
            est_duration: event.end - event.start,
            due: None,
            earliest_start: None,
            category: if is_commitment { category } else { None },
            pinned: is_commitment.then_some(window),
            transparent: event.transparent,
            blocked_by: Vec::new(),
            defer_policy: DeferPolicy::RescheduleAsap,
            status: if is_commitment {
                TaskStatus::Scheduled
            } else {
                TaskStatus::Backlog
            },
            provenance: Provenance::Manual,
            commitment: None,
        };
        entries.push(log_capture(task, now));
        captured_links.push((task_id, event.id.clone()));
        report.captured += 1;
    }

    reconcile(store, &entries).expect("calendar import entries must reference known tasks");
    for (task_id, event_id) in captured_links {
        store.upsert_calendar_link(task_id, event_id);
    }
    store.log.extend(entries);

    report
}

#[cfg(test)]
#[derive(Debug, Clone, PartialEq, Eq)]
enum StubCall {
    Create(CalendarEvent),
    Update {
        event_id: String,
        event: CalendarEvent,
    },
}

#[cfg(test)]
#[derive(Default)]
struct StubTransport {
    calls: std::cell::RefCell<Vec<StubCall>>,
    next_id: std::cell::Cell<usize>,
    create_error: Option<String>,
    update_error: Option<String>,
    listed_events: Vec<FetchedEvent>,
}

#[cfg(test)]
impl StubTransport {
    fn with_create_error(error: &str) -> Self {
        Self {
            create_error: Some(error.to_string()),
            ..Self::default()
        }
    }

    fn with_update_error(error: &str) -> Self {
        Self {
            update_error: Some(error.to_string()),
            ..Self::default()
        }
    }

    fn with_events(events: Vec<FetchedEvent>) -> Self {
        Self {
            listed_events: events,
            ..Self::default()
        }
    }
}

#[cfg(test)]
impl CalendarTransport for StubTransport {
    async fn create_event(&self, event: &CalendarEvent) -> Result<String, String> {
        self.calls
            .borrow_mut()
            .push(StubCall::Create(event.clone()));
        if let Some(error) = &self.create_error {
            return Err(error.clone());
        }

        let id = self.next_id.get() + 1;
        self.next_id.set(id);
        Ok(format!("event-{id}"))
    }

    async fn update_event(&self, event_id: &str, event: &CalendarEvent) -> Result<(), String> {
        self.calls.borrow_mut().push(StubCall::Update {
            event_id: event_id.to_string(),
            event: event.clone(),
        });
        if let Some(error) = &self.update_error {
            return Err(error.clone());
        }

        Ok(())
    }

    async fn list_events(
        &self,
        _from: DateTime<Utc>,
        _to: DateTime<Utc>,
    ) -> Result<Vec<FetchedEvent>, String> {
        Ok(self.listed_events.clone())
    }
}

#[cfg(test)]
mod stub_tests {
    use super::*;
    use chrono::Duration;
    use ubu_core::{
        re_plan, AffectBudget, ComputeTarget, DeferPolicy, DeterministicPlacer, Id, PlanAuthority,
        Provenance, ScheduleEntry, Task, TaskStatus, TimeWindow,
    };

    fn event() -> CalendarEvent {
        CalendarEvent {
            summary: "test".to_string(),
            start: DateTime::from_timestamp(0, 0).unwrap(),
            end: DateTime::from_timestamp(60, 0).unwrap(),
            color_id: None,
            transparent: false,
        }
    }

    #[tokio::test]
    async fn stub_transport_records_calls_and_injects_errors() {
        let create_stub = StubTransport::with_create_error("create failed");
        assert_eq!(
            create_stub.create_event(&event()).await,
            Err("create failed".to_string())
        );
        assert!(matches!(
            create_stub.calls.borrow().as_slice(),
            [StubCall::Create(_)]
        ));

        let update_stub = StubTransport::with_update_error("update failed");
        assert_eq!(
            update_stub.update_event("known", &event()).await,
            Err("update failed".to_string())
        );
        assert!(matches!(
            update_stub.calls.borrow().as_slice(),
            [StubCall::Update { event_id, .. }] if event_id == "known"
        ));
    }

    #[tokio::test]
    async fn stub_transport_lists_configured_events() {
        let expected = FetchedEvent {
            id: "google-event".to_string(),
            summary: "Fetched".to_string(),
            color_id: Some("5".to_string()),
            start: DateTime::from_timestamp(0, 0).unwrap(),
            end: DateTime::from_timestamp(60, 0).unwrap(),
            transparent: false,
        };
        let stub = StubTransport::with_events(vec![expected.clone()]);

        assert_eq!(
            stub.list_events(
                DateTime::from_timestamp(0, 0).unwrap(),
                DateTime::from_timestamp(120, 0).unwrap(),
            )
            .await,
            Ok(vec![expected])
        );
    }

    #[test]
    fn google_event_body_writes_both_transparency_values() {
        let mut calendar_event = event();
        let opaque = serde_json::to_value(GoogleEventBody::from(&calendar_event)).unwrap();
        assert_eq!(opaque["transparency"], "opaque");

        calendar_event.transparent = true;
        let transparent = serde_json::to_value(GoogleEventBody::from(&calendar_event)).unwrap();
        assert_eq!(transparent["transparency"], "transparent");
    }

    #[test]
    fn listed_event_reads_transparent_and_defaults_other_values_to_opaque() {
        let listed = |transparency: Option<&str>| ListedEvent {
            id: "event".to_string(),
            summary: "Summary".to_string(),
            color_id: None,
            start: GoogleEventTime {
                date_time: at(0).to_rfc3339(),
            },
            end: GoogleEventTime {
                date_time: at(30).to_rfc3339(),
            },
            transparency: transparency.map(str::to_owned),
        };

        assert!(
            FetchedEvent::try_from(listed(Some("transparent")))
                .unwrap()
                .transparent
        );
        assert!(
            !FetchedEvent::try_from(listed(Some("opaque")))
                .unwrap()
                .transparent
        );
        assert!(!FetchedEvent::try_from(listed(None)).unwrap().transparent);
    }

    fn id(value: u128) -> Id {
        Id::from_u128(value)
    }

    fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(minutes * 60, 0).unwrap()
    }

    fn task(value: u128, title: &str, tier: Tier, pinned: bool, category: Option<&str>) -> Task {
        Task {
            id: id(value),
            tier,
            title: title.to_string(),
            detail: Some(format!("detail for {title}")),
            objective_ids: Vec::new(),
            skills: vec!["private skill".to_string()],
            affect_cost: 0,
            est_duration: Duration::minutes(30),
            due: None,
            earliest_start: None,
            category: category.map(str::to_owned),
            pinned: pinned.then(|| TimeWindow {
                start: at(value as i64),
                end: at(value as i64 + 30),
            }),
            transparent: false,
            blocked_by: Vec::new(),
            defer_policy: DeferPolicy::RescheduleAsap,
            status: TaskStatus::Scheduled,
            provenance: Provenance::Manual,
            commitment: None,
        }
    }

    fn plan(task_ids: &[u128]) -> Plan {
        Plan {
            id: id(10_000),
            created_at: at(0),
            authority: PlanAuthority::Authoritative,
            clearance: Tier::TopSecret,
            entries: task_ids
                .iter()
                .enumerate()
                .map(|(index, value)| ScheduleEntry {
                    item: id(*value),
                    window: TimeWindow {
                        start: at(index as i64 * 30),
                        end: at(index as i64 * 30 + 30),
                    },
                    is_handle: false,
                })
                .collect(),
            objective_etas: BTreeMap::new(),
            conflicts: Vec::new(),
        }
    }

    fn call_event(call: &StubCall) -> &CalendarEvent {
        match call {
            StubCall::Create(event) | StubCall::Update { event, .. } => event,
        }
    }

    fn fetched_event(
        event_id: &str,
        summary: &str,
        color_id: Option<&str>,
        start_minutes: i64,
        end_minutes: i64,
    ) -> FetchedEvent {
        FetchedEvent {
            id: event_id.to_string(),
            summary: summary.to_string(),
            color_id: color_id.map(str::to_owned),
            start: at(start_minutes),
            end: at(end_minutes),
            transparent: false,
        }
    }

    fn linked_task_id(store: &Store, event_id: &str) -> Id {
        store
            .calendar_links
            .iter()
            .find_map(|(task_id, linked_event_id)| {
                (linked_event_id == event_id).then_some(*task_id)
            })
            .expect("event is linked")
    }

    #[tokio::test]
    async fn export_creates_one_event_per_entry_and_populates_links() {
        let mut store = Store::new();
        store.upsert_task(task(1, "First", Tier::UserShared, false, None));
        store.upsert_task(task(2, "Second", Tier::UserShared, false, None));
        let plan = plan(&[2, 1]);
        let transport = StubTransport::default();

        let report = export_plan(
            &mut store,
            &plan,
            &transport,
            &BTreeMap::new(),
            Tier::UserShared,
        )
        .await
        .unwrap();

        assert_eq!(
            report,
            ExportReport {
                created: 2,
                updated: 0,
            }
        );
        assert_eq!(store.calendar_links.len(), 2);
        assert_eq!(
            store.calendar_link(id(2)).map(String::as_str),
            Some("event-1")
        );
        assert_eq!(
            store.calendar_link(id(1)).map(String::as_str),
            Some("event-2")
        );
        let calls = transport.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert!(calls.iter().all(|call| matches!(call, StubCall::Create(_))));
        assert_eq!(call_event(&calls[0]).summary, "Second");
        assert_eq!(call_event(&calls[1]).summary, "First");
    }

    #[tokio::test]
    async fn second_export_updates_every_existing_event_without_creating() {
        let mut store = Store::new();
        store.upsert_task(task(1, "First", Tier::UserShared, false, None));
        store.upsert_task(task(2, "Second", Tier::UserShared, false, None));
        let plan = plan(&[1, 2]);
        let transport = StubTransport::default();
        export_plan(
            &mut store,
            &plan,
            &transport,
            &BTreeMap::new(),
            Tier::UserShared,
        )
        .await
        .unwrap();
        transport.calls.borrow_mut().clear();

        let report = export_plan(
            &mut store,
            &plan,
            &transport,
            &BTreeMap::new(),
            Tier::UserShared,
        )
        .await
        .unwrap();

        assert_eq!(
            report,
            ExportReport {
                created: 0,
                updated: 2,
            }
        );
        let calls = transport.calls.borrow();
        assert!(matches!(
            calls.as_slice(),
            [
                StubCall::Update { event_id: first, .. },
                StubCall::Update { event_id: second, .. }
            ] if first == "event-1" && second == "event-2"
        ));
    }

    #[tokio::test]
    async fn top_secret_content_never_reaches_the_transport() {
        const SECRET_TITLE: &str = "Never transmit this title";
        let mut store = Store::new();
        store.upsert_task(task(
            1,
            SECRET_TITLE,
            Tier::TopSecret,
            true,
            Some("secret-category"),
        ));
        store.upsert_task(task(2, "Visible title", Tier::UserShared, false, None));
        let transport = StubTransport::default();

        export_plan(
            &mut store,
            &plan(&[1, 2]),
            &transport,
            &BTreeMap::from([("secret-category".to_string(), "11".to_string())]),
            Tier::UserShared,
        )
        .await
        .unwrap();

        let calls = transport.calls.borrow();
        let events: Vec<&CalendarEvent> = calls.iter().map(call_event).collect();
        assert!(events.iter().all(|event| event.summary != SECRET_TITLE));
        assert!(events
            .iter()
            .any(|event| event.summary == "Busy" && event.color_id.is_none()));
    }

    #[tokio::test]
    async fn only_pinned_tasks_receive_their_mapped_category_color() {
        let mut store = Store::new();
        store.upsert_task(task(1, "Pinned", Tier::UserShared, true, Some("personal")));
        store.upsert_task(task(
            2,
            "Dynamic",
            Tier::UserShared,
            false,
            Some("personal"),
        ));
        let transport = StubTransport::default();

        export_plan(
            &mut store,
            &plan(&[1, 2]),
            &transport,
            &BTreeMap::from([("personal".to_string(), "5".to_string())]),
            Tier::UserShared,
        )
        .await
        .unwrap();

        let calls = transport.calls.borrow();
        let pinned = calls
            .iter()
            .map(call_event)
            .find(|event| event.summary == "Pinned")
            .unwrap();
        let dynamic = calls
            .iter()
            .map(call_event)
            .find(|event| event.summary == "Dynamic")
            .unwrap();
        assert_eq!(pinned.color_id.as_deref(), Some("5"));
        assert_eq!(dynamic.color_id, None);
    }

    #[tokio::test]
    async fn export_preserves_transparency_even_for_a_redacted_task() {
        let mut store = Store::new();
        let mut secret = task(1, "Secret", Tier::TopSecret, true, None);
        secret.transparent = true;
        store.upsert_task(secret);
        let transport = StubTransport::default();

        export_plan(
            &mut store,
            &plan(&[1]),
            &transport,
            &BTreeMap::new(),
            Tier::UserShared,
        )
        .await
        .unwrap();

        let calls = transport.calls.borrow();
        let exported = call_event(&calls[0]);
        assert_eq!(exported.summary, "Busy");
        assert!(exported.transparent);
    }

    #[test]
    fn captured_event_preserves_transparency_on_the_new_task() {
        let mut store = Store::new();
        let mut event = fetched_event("transparent-capture", "Available", None, 60, 120);
        event.transparent = true;

        let report = import_from_calendar(
            &mut store,
            std::slice::from_ref(&event),
            at(0),
            Tier::UserShared,
            &BTreeMap::new(),
        );

        assert_eq!(report.captured, 1);
        let task_id = linked_task_id(&store, &event.id);
        assert!(store.tasks[&task_id].transparent);
    }

    #[test]
    fn default_color_non_owned_event_captures_a_linked_dynamic_task() {
        let mut store = Store::new();
        let event = fetched_event("new-dynamic", "Inbox item", None, 60, 90);

        let report = import_from_calendar(
            &mut store,
            std::slice::from_ref(&event),
            at(0),
            Tier::UserShared,
            &BTreeMap::new(),
        );

        assert_eq!(
            report,
            ImportReport {
                captured: 1,
                completed: 0,
                moved: 0,
                resized: 0,
            }
        );
        let task_id = linked_task_id(&store, &event.id);
        let captured = &store.tasks[&task_id];
        assert_eq!(captured.title, event.summary);
        assert_eq!(captured.status, TaskStatus::Backlog);
        assert_eq!(captured.pinned, None);
        assert_eq!(captured.category, None);
        assert_eq!(captured.est_duration, Duration::minutes(30));
        assert_eq!(store.log.len(), 1);
    }

    #[test]
    fn colored_non_owned_event_captures_a_categorized_pinned_commitment() {
        let mut store = Store::new();
        let event = fetched_event("new-commitment", "Dinner", Some("5"), 120, 180);
        let colors = BTreeMap::from([("5".to_string(), "relationship".to_string())]);

        let report = import_from_calendar(
            &mut store,
            std::slice::from_ref(&event),
            at(0),
            Tier::UserShared,
            &colors,
        );

        assert_eq!(report.captured, 1);
        let task_id = linked_task_id(&store, &event.id);
        let captured = &store.tasks[&task_id];
        assert_eq!(captured.status, TaskStatus::Scheduled);
        assert_eq!(
            captured.pinned,
            Some(TimeWindow {
                start: event.start,
                end: event.end,
            })
        );
        assert_eq!(captured.category.as_deref(), Some("relationship"));
    }

    #[test]
    fn colored_owned_dynamic_event_marks_the_task_done() {
        let mut store = Store::new();
        let mut dynamic = task(1, "Dynamic", Tier::UserShared, false, None);
        dynamic.status = TaskStatus::Backlog;
        let task_id = dynamic.id;
        store.upsert_task(dynamic);
        store.upsert_calendar_link(task_id, "owned-dynamic".to_string());
        let event = fetched_event("owned-dynamic", "Dynamic", Some("8"), 30, 60);

        let report = import_from_calendar(
            &mut store,
            &[event],
            at(0),
            Tier::UserShared,
            &BTreeMap::new(),
        );

        assert_eq!(report.completed, 1);
        assert_eq!(store.tasks[&task_id].status, TaskStatus::Done);
        assert_eq!(store.log.len(), 1);
    }

    #[test]
    fn resized_owned_dynamic_event_updates_duration_and_replanning_uses_it() {
        let mut store = Store::new();
        let mut dynamic = task(1, "Dynamic", Tier::UserShared, false, None);
        dynamic.status = TaskStatus::Backlog;
        let task_id = dynamic.id;
        store.upsert_task(dynamic);
        store.upsert_calendar_link(task_id, "owned-dynamic".to_string());
        let event = fetched_event("owned-dynamic", "Dynamic", None, 60, 150);

        let report = import_from_calendar(
            &mut store,
            &[event],
            at(0),
            Tier::UserShared,
            &BTreeMap::new(),
        );

        assert_eq!(report.resized, 1);
        assert_eq!(store.tasks[&task_id].est_duration, Duration::minutes(90));
        assert!(matches!(
            &store.log[0].kind,
            ubu_core::LogEntryKind::Command(ubu_core::CommandKind::EditDuration {
                task_id: logged_id,
                est_duration,
            }) if *logged_id == task_id && *est_duration == Duration::minutes(90)
        ));

        let plan = re_plan(
            &store,
            ComputeTarget::DesktopOllama,
            at(0),
            at(0),
            &[],
            &AffectBudget { cap: 10 },
            &DeterministicPlacer,
        )
        .unwrap();
        let planned = plan
            .entries
            .iter()
            .find(|entry| entry.item == task_id)
            .expect("resized task is planned");
        assert_eq!(
            planned.window.end - planned.window.start,
            Duration::minutes(90)
        );
    }

    #[test]
    fn done_takes_precedence_over_a_dynamic_event_resize() {
        let mut store = Store::new();
        let mut dynamic = task(1, "Dynamic", Tier::UserShared, false, None);
        dynamic.status = TaskStatus::Backlog;
        let task_id = dynamic.id;
        store.upsert_task(dynamic);
        store.upsert_calendar_link(task_id, "owned-dynamic".to_string());
        let event = fetched_event("owned-dynamic", "Dynamic", Some("8"), 60, 150);

        let report = import_from_calendar(
            &mut store,
            &[event],
            at(0),
            Tier::UserShared,
            &BTreeMap::new(),
        );

        assert_eq!(report.completed, 1);
        assert_eq!(report.resized, 0);
        assert_eq!(store.tasks[&task_id].status, TaskStatus::Done);
        assert_eq!(store.tasks[&task_id].est_duration, Duration::minutes(30));
        assert!(store.log.iter().all(|entry| !matches!(
            entry.kind,
            ubu_core::LogEntryKind::Command(ubu_core::CommandKind::EditDuration { .. })
        )));
    }

    #[test]
    fn non_positive_dynamic_event_length_does_not_edit_duration() {
        let mut store = Store::new();
        let mut dynamic = task(1, "Dynamic", Tier::UserShared, false, None);
        dynamic.status = TaskStatus::Backlog;
        let task_id = dynamic.id;
        store.upsert_task(dynamic);
        store.upsert_calendar_link(task_id, "owned-dynamic".to_string());
        for end_minutes in [60, 30] {
            let event = fetched_event(
                "owned-dynamic",
                "Dynamic",
                None,
                60,
                end_minutes,
            );
            let report = import_from_calendar(
                &mut store,
                &[event],
                at(0),
                Tier::UserShared,
                &BTreeMap::new(),
            );
            assert_eq!(report.resized, 0);
        }
        assert_eq!(store.tasks[&task_id].est_duration, Duration::minutes(30));
        assert!(store.log.is_empty());
    }

    #[test]
    fn moving_a_same_length_dynamic_event_does_not_edit_duration() {
        let mut store = Store::new();
        let mut dynamic = task(1, "Dynamic", Tier::UserShared, false, None);
        dynamic.status = TaskStatus::Backlog;
        let task_id = dynamic.id;
        store.upsert_task(dynamic);
        store.upsert_calendar_link(task_id, "owned-dynamic".to_string());
        let event = fetched_event("owned-dynamic", "Dynamic", None, 300, 330);

        let report = import_from_calendar(
            &mut store,
            &[event],
            at(0),
            Tier::UserShared,
            &BTreeMap::new(),
        );

        assert_eq!(report.resized, 0);
        assert_eq!(store.tasks[&task_id].est_duration, Duration::minutes(30));
        assert!(store.log.is_empty());
    }

    #[test]
    fn moved_owned_commitment_updates_its_pin_through_edit_pin() {
        let mut store = Store::new();
        let commitment = task(1, "Commitment", Tier::UserShared, true, None);
        let task_id = commitment.id;
        store.upsert_task(commitment);
        store.upsert_calendar_link(task_id, "owned-commitment".to_string());
        let event = fetched_event("owned-commitment", "Commitment", Some("5"), 300, 360);
        let expected = TimeWindow {
            start: event.start,
            end: event.end,
        };

        let report = import_from_calendar(
            &mut store,
            &[event],
            at(0),
            Tier::UserShared,
            &BTreeMap::new(),
        );

        assert_eq!(report.moved, 1);
        assert_eq!(store.tasks[&task_id].pinned, Some(expected.clone()));
        assert!(matches!(
            &store.log[0].kind,
            ubu_core::LogEntryKind::Command(ubu_core::CommandKind::EditPin {
                task_id: logged_id,
                pinned: Some(logged_window),
            }) if *logged_id == task_id && logged_window == &expected
        ));
    }

    #[test]
    fn importing_the_same_events_twice_makes_no_second_pass_changes() {
        let mut store = Store::new();
        let mut dynamic = task(1, "Existing dynamic", Tier::UserShared, false, None);
        dynamic.status = TaskStatus::Backlog;
        store.upsert_task(dynamic);
        store.upsert_calendar_link(id(1), "owned-dynamic".to_string());
        store.upsert_task(task(2, "Existing commitment", Tier::UserShared, true, None));
        store.upsert_calendar_link(id(2), "owned-commitment".to_string());
        let events = vec![
            fetched_event("new-dynamic", "New dynamic", None, 0, 30),
            fetched_event("new-commitment", "New commitment", Some("5"), 30, 60),
            fetched_event("owned-dynamic", "Existing dynamic", Some("8"), 60, 90),
            fetched_event(
                "owned-commitment",
                "Existing commitment",
                Some("5"),
                90,
                120,
            ),
        ];
        let colors = BTreeMap::from([("5".to_string(), "personal".to_string())]);

        let first = import_from_calendar(&mut store, &events, at(0), Tier::UserShared, &colors);
        assert_eq!(
            first,
            ImportReport {
                captured: 2,
                completed: 1,
                moved: 1,
                resized: 0,
            }
        );
        let after_first = store.clone();

        let second = import_from_calendar(&mut store, &events, at(1), Tier::UserShared, &colors);

        assert_eq!(
            second,
            ImportReport {
                captured: 0,
                completed: 0,
                moved: 0,
                resized: 0,
            }
        );
        assert_eq!(store, after_first);
    }
}
