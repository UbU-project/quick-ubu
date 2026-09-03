//! Google Calendar export behind a transport boundary.

use std::collections::BTreeMap;
use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use ubu_core::{visible_as_content, Plan, Store, Tier};
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod};

const CALENDAR_SCOPE: &str = "https://www.googleapis.com/auth/calendar";
const CALENDAR_API_BASE: &str = "https://www.googleapis.com/calendar/v3/calendars";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    pub summary: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub color_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedEvent {
    pub id: String,
    pub summary: String,
    pub color_id: Option<String>,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
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
        DeferPolicy, Id, PlanAuthority, Provenance, ScheduleEntry, Task, TaskStatus, TimeWindow,
    };

    fn event() -> CalendarEvent {
        CalendarEvent {
            summary: "test".to_string(),
            start: DateTime::from_timestamp(0, 0).unwrap(),
            end: DateTime::from_timestamp(60, 0).unwrap(),
            color_id: None,
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
}
