//! Google Calendar export behind a transport boundary.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use chrono::{DateTime, Duration, Months, Utc};
use serde::{Deserialize, Serialize};
use ubu_core::{
    log_actual, log_capture, log_edit_duration, log_edit_pin, log_remove_task, log_undo_completion, reconcile,
    visible_as_content, ActualStatus, DeferPolicy, FactKind, Id, LogEntryKind, Plan, Provenance,
    Store, Task, TaskStatus, Tier, TimeWindow,
};
use yup_oauth2::{InstalledFlowAuthenticator, InstalledFlowReturnMethod};

const CALENDAR_SCOPE: &str = "https://www.googleapis.com/auth/calendar";
const CALENDAR_API_BASE: &str = "https://www.googleapis.com/calendar/v3/calendars";
const CAPTURE_NAMESPACE: Id = Id::from_u128(0xfbb8_2411_158b_4a86_9f69_42d19fec7587);

/// category -> Google event colorId, matching the operator's legacy scheme.
pub fn default_category_colors() -> BTreeMap<String, String> {
    [
        ("personal", "3"),
        ("relationship", "5"),
        ("business", "6"),
        ("committed", "11"),
        ("location", "8"),
        ("entertainment", "1"),
        ("grocery", "2"),
        ("commute", "7"),
        ("undefined", "4"),
        ("education_house", "10"),
        ("work", "9"),
    ]
    .into_iter()
    .map(|(category, color_id)| (category.to_string(), color_id.to_string()))
    .collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    pub summary: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub color_id: Option<String>,
    pub transparent: bool,
    pub reminders: Vec<i32>,
}

fn event_signature(event: &CalendarEvent) -> String {
    #[derive(Serialize)]
    struct EventSignature<'a> {
        summary: &'a str,
        start: String,
        end: String,
        color_id: Option<&'a str>,
        transparent: bool,
        reminders: &'a [i32],
    }

    serde_json::to_string(&EventSignature {
        summary: &event.summary,
        start: event.start.to_rfc3339(),
        end: event.end.to_rfc3339(),
        color_id: event.color_id.as_deref(),
        transparent: event.transparent,
        reminders: &event.reminders,
    })
    .expect("event signature fields are JSON serializable")
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

    /// Fetch a linked event without a date filter. Deleted events and events
    /// without both start/end dateTime values return None.
    async fn get_event(&self, event_id: &str) -> Result<Option<FetchedEvent>, String>;
}

/// Real Google transport. HTTP handling is tested against a local server;
/// credentials and OAuth still require verification against Google Calendar.
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

    async fn parse_response<T: serde::de::DeserializeOwned>(
        response: reqwest::Response,
        resource: &str,
    ) -> Result<T, String> {
        let body = response.bytes().await.map_err(|error| {
            format!("failed to read Google Calendar {resource} response body: {error}")
        })?;
        serde_json::from_slice(&body).map_err(|error| {
            format!(
                "failed to parse Google Calendar {resource}: {error}\nResponse body:\n{}",
                String::from_utf8_lossy(&body)
            )
        })
    }

    async fn list_events_with_token(
        &self,
        url: reqwest::Url,
        token: &str,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<FetchedEvent>, String> {
        let query = [
            ("timeMin", from.to_rfc3339()),
            ("timeMax", to.to_rfc3339()),
            ("singleEvents", "true".to_string()),
        ];
        let mut events = Vec::new();
        let mut page_token = None;
        let mut seen_tokens = BTreeSet::new();
        loop {
            let mut request = self
                .client
                .get(url.clone())
                .bearer_auth(token)
                .query(&query);
            if let Some(page_token) = &page_token {
                request = request.query(&[("pageToken", page_token)]);
            }
            let response = request
                .send()
                .await
                .map_err(|error| format!("failed to list Google Calendar events: {error}"))?;
            if !response.status().is_success() {
                return Err(Self::response_error(response).await);
            }
            let page: ListedEvents = Self::parse_response(response, "events").await?;
            for event in page.items {
                if event.status.as_deref() != Some("cancelled") && event.has_date_times() {
                    events.push(FetchedEvent::try_from(event)?);
                }
            }
            page_token = page.next_page_token.filter(|token| !token.is_empty());
            let Some(next) = &page_token else {
                return Ok(events);
            };
            if !seen_tokens.insert(next.clone()) {
                return Err("Google Calendar returned a repeated nextPageToken".into());
            }
        }
    }

    async fn get_event_with_token(
        &self,
        url: reqwest::Url,
        token: &str,
    ) -> Result<Option<FetchedEvent>, String> {
        let response = self
            .client
            .get(url)
            .bearer_auth(token)
            .send()
            .await
            .map_err(|error| format!("failed to get Google Calendar event: {error}"))?;
        if matches!(
            response.status(),
            reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::GONE
        ) {
            return Ok(None);
        }
        if !response.status().is_success() {
            return Err(Self::response_error(response).await);
        }
        let event: ListedEvent = Self::parse_response(response, "event").await?;
        if event.status.as_deref() == Some("cancelled") || !event.has_date_times() {
            return Ok(None);
        }
        FetchedEvent::try_from(event).map(Some)
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
    reminders: GoogleReminders,
}

#[derive(Serialize)]
struct GoogleReminders {
    #[serde(rename = "useDefault")]
    use_default: bool,
    overrides: Vec<GoogleReminder>,
}

#[derive(Serialize)]
struct GoogleReminder {
    method: &'static str,
    minutes: i32,
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
            reminders: GoogleReminders {
                use_default: false,
                overrides: event
                    .reminders
                    .iter()
                    .map(|minutes| GoogleReminder {
                        method: "popup",
                        minutes: *minutes,
                    })
                    .collect(),
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
    #[serde(rename = "nextPageToken")]
    next_page_token: Option<String>,
}

#[derive(Deserialize)]
struct ListedEvent {
    id: String,
    #[serde(default)]
    summary: String,
    #[serde(rename = "colorId")]
    color_id: Option<String>,
    start: Option<ListedEventTime>,
    end: Option<ListedEventTime>,
    status: Option<String>,
    #[serde(default)]
    transparency: Option<String>,
}

#[derive(Deserialize)]
struct ListedEventTime {
    #[serde(rename = "dateTime")]
    date_time: Option<String>,
}

impl ListedEvent {
    fn has_date_times(&self) -> bool {
        // Date-only (single- or multi-day) events cannot become timed Tasks.
        self.start
            .as_ref()
            .and_then(|time| time.date_time.as_ref())
            .is_some()
            && self
                .end
                .as_ref()
                .and_then(|time| time.date_time.as_ref())
                .is_some()
    }
}

impl TryFrom<ListedEvent> for FetchedEvent {
    type Error = String;

    fn try_from(event: ListedEvent) -> Result<Self, Self::Error> {
        let start = event
            .start
            .and_then(|time| time.date_time)
            .ok_or("Google Calendar event has no start dateTime")?;
        let end = event
            .end
            .and_then(|time| time.date_time)
            .ok_or("Google Calendar event has no end dateTime")?;
        Ok(Self {
            id: event.id,
            summary: event.summary,
            color_id: event.color_id,
            start: DateTime::parse_from_rfc3339(&start)
                .map_err(|error| format!("invalid Google Calendar start dateTime: {error}"))?
                .with_timezone(&Utc),
            end: DateTime::parse_from_rfc3339(&end)
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

    async fn list_events(
        &self,
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<FetchedEvent>, String> {
        let token = self.access_token().await?;
        self.list_events_with_token(self.event_url(None), &token, from, to)
            .await
    }

    async fn get_event(&self, event_id: &str) -> Result<Option<FetchedEvent>, String> {
        let token = self.access_token().await?;
        self.get_event_with_token(self.event_url(Some(event_id)), &token)
            .await
    }
}

/// Always include the last 24 hours and the next calendar month. Explicit
/// bounds may widen this window, but cannot shorten either minimum.
pub fn calendar_import_window(
    now: DateTime<Utc>,
    from: Option<DateTime<Utc>>,
    to: Option<DateTime<Utc>>,
) -> Result<TimeWindow, String> {
    let start = from.unwrap_or(now).min(now - Duration::hours(24));
    let minimum_end = now
        .checked_add_months(Months::new(1))
        .ok_or("calendar import month look-ahead exceeds the supported date range")?;
    let end = to.unwrap_or(minimum_end);
    if end <= start {
        return Err("calendar import --to must be after the effective --from".into());
    }
    Ok(TimeWindow {
        start,
        end: end.max(minimum_end),
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FetchedImport {
    pub events: Vec<FetchedEvent>,
    pub deleted: Vec<Id>,
}

/// Discover events in the import window, then retrieve every missing linked,
/// unfinished task by ID, even if its event moved outside the window.
/// Only a per-ID None confirms deletion; list absence alone never does.
/// Fetching finishes before the caller mutates or saves the store.
pub async fn fetch_import_events<T: CalendarTransport>(
    store: &Store,
    transport: &T,
    window: &TimeWindow,
) -> Result<FetchedImport, String> {
    let mut events: BTreeMap<_, _> = transport
        .list_events(window.start, window.end)
        .await?
        .into_iter()
        .map(|event| (event.id.clone(), event))
        .collect();
    let mut deleted = Vec::new();
    for (task_id, event_id) in &store.calendar_links {
        let Some(task) = store.tasks.get(task_id) else {
            continue;
        };
        if task.status == TaskStatus::Done || events.contains_key(event_id) {
            continue;
        }
        if let Some(event) = transport.get_event(event_id).await? {
            events.insert(event.id.clone(), event);
        } else {
            deleted.push(*task_id);
        }
    }
    Ok(FetchedImport { events: events.into_values().collect(), deleted })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportReport {
    pub created: usize,
    pub updated: usize,
    pub skipped: usize,
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
        skipped: 0,
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
            reminders: task.reminders.clone(),
        };
        let sig = event_signature(&event);
        let existing_event_id = store.calendar_link(entry.item).cloned();

        if let Some(event_id) = existing_event_id {
            if store.export_signatures.get(&entry.item) == Some(&sig) {
                report.skipped += 1;
                continue;
            }
            transport.update_event(&event_id, &event).await?;
            store.export_signatures.insert(entry.item, sig);
            report.updated += 1;
        } else {
            let event_id = transport.create_event(&event).await?;
            store.upsert_calendar_link(entry.item, event_id);
            store.export_signatures.insert(entry.item, sig);
            report.created += 1;
        }
    }

    Ok(report)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImportReport {
    pub removed: usize,
    pub captured: usize,
    pub completed: usize,
    pub reopened: usize,
    pub moved: usize,
    pub resized: usize,
}

pub fn import_from_calendar(
    store: &mut Store,
    events: &[FetchedEvent],
    deleted: &[Id],
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
        removed: 0,
        captured: 0,
        completed: 0,
        reopened: 0,
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
                    if event.color_id.is_none()
                        && task.status == TaskStatus::Done
                        && event.end > now - Duration::hours(24)
                    {
                        // Only undo a Calendar completion (which records an
                        // actual window), not a CLI `done` fact with no window.
                        let latest_actual = store.log.iter().enumerate().filter(|(_, entry)| {
                            matches!(&entry.kind, LogEntryKind::Fact(FactKind::Actual { item_id, .. }) if *item_id == task_id)
                        }).max_by_key(|(index, entry)| (entry.at, *index)).map(|(_, entry)| entry);
                        if let Some(completion) = latest_actual.filter(|entry| {
                            matches!(
                                entry.kind,
                                LogEntryKind::Fact(FactKind::Actual {
                                    status: ActualStatus::Done,
                                    actual: Some(_),
                                    ..
                                })
                            )
                        }) {
                            entries.push(log_undo_completion(task_id, completion.id, now));
                            report.reopened += 1;
                        }
                    }
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
            reminders: Vec::new(),
            commitment: None,
        };
        entries.push(log_capture(task, now));
        captured_links.push((task_id, event.id.clone()));
        report.captured += 1;
    }

    for task_id in deleted.iter().copied().collect::<BTreeSet<_>>() {
        if store.tasks.contains_key(&task_id) {
            entries.push(log_remove_task(task_id, now));
            report.removed += 1;
        }
    }
    reconcile(store, &entries).expect("calendar import entries must reference known tasks");
    for entry in &entries {
        if let LogEntryKind::Command(ubu_core::CommandKind::UndoCompletion { task_id, .. }) =
            &entry.kind
        {
            // Replanning must refresh the Calendar event after undo; a cached
            // signature from before completion must not suppress that export.
            store.export_signatures.remove(task_id);
        }
    }
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
    filter_dates: bool,
    get_calls: std::cell::RefCell<Vec<String>>,
    get_error: Option<String>,
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
        from: DateTime<Utc>,
        to: DateTime<Utc>,
    ) -> Result<Vec<FetchedEvent>, String> {
        Ok(self
            .listed_events
            .iter()
            .filter(|event| !self.filter_dates || (event.end > from && event.start < to))
            .cloned()
            .collect())
    }

    async fn get_event(&self, event_id: &str) -> Result<Option<FetchedEvent>, String> {
        self.get_calls.borrow_mut().push(event_id.into());
        if let Some(error) = &self.get_error {
            return Err(error.clone());
        }
        Ok(self
            .listed_events
            .iter()
            .find(|event| event.id == event_id)
            .cloned())
    }
}

#[cfg(test)]
mod import_tests;

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
            reminders: Vec::new(),
            start: DateTime::from_timestamp(0, 0).unwrap(),
            end: DateTime::from_timestamp(60, 0).unwrap(),
            color_id: None,
            transparent: false,
        }
    }

    #[test]
    fn event_signatures_are_canonical_and_distinguish_every_sent_field() {
        let base = event();
        let signature = event_signature(&base);
        assert_eq!(signature, event_signature(&base.clone()));
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&signature).unwrap(),
            serde_json::json!({
                "summary": "test",
                "start": "1970-01-01T00:00:00+00:00",
                "end": "1970-01-01T00:01:00+00:00",
                "color_id": null,
                "transparent": false,
                "reminders": [],
            })
        );

        let mut variants = Vec::new();
        let mut changed = base.clone();
        changed.summary = "quotes \" and \\ and newline\n".into();
        variants.push(changed);
        let mut changed = base.clone();
        changed.start += Duration::nanoseconds(1);
        variants.push(changed);
        let mut changed = base.clone();
        changed.end += Duration::nanoseconds(1);
        variants.push(changed);
        for color in ["", "3"] {
            let mut changed = base.clone();
            changed.color_id = Some(color.into());
            variants.push(changed);
        }
        let mut changed = base.clone();
        changed.transparent = true;
        variants.push(changed);
        for reminders in [vec![0], vec![10, 0], vec![0, 10]] {
            let mut changed = base.clone();
            changed.reminders = reminders;
            variants.push(changed);
        }
        let mut signatures = std::collections::BTreeSet::from([signature]);
        for variant in variants {
            assert!(signatures.insert(event_signature(&variant)));
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
    fn google_event_body_writes_popup_overrides_and_disables_defaults_when_empty() {
        let mut calendar_event = event();
        calendar_event.reminders = vec![10, 0];
        let body = serde_json::to_value(GoogleEventBody::from(&calendar_event)).unwrap();
        assert_eq!(
            body["reminders"],
            serde_json::json!({
                "useDefault": false,
                "overrides": [{"method": "popup", "minutes": 10}, {"method": "popup", "minutes": 0}]
            })
        );
        calendar_event.reminders.clear();
        let body = serde_json::to_value(GoogleEventBody::from(&calendar_event)).unwrap();
        assert_eq!(
            body["reminders"],
            serde_json::json!({"useDefault": false, "overrides": []})
        );
    }

    #[tokio::test]
    async fn routine_reminders_flow_through_generation_and_export_create_update_and_clear() {
        let routine = ubu_core::RoutineTemplate {
            id: id(90),
            title: "Daily reminder".to_string(),
            tier: Tier::UserShared,
            start_time: chrono::NaiveTime::from_hms_opt(9, 0, 0).unwrap(),
            duration: Duration::minutes(30),
            affect_cost: 0,
            category: None,
            transparent: false,
            reminders: vec![10, 0],
            recurrence: ubu_core::Recurrence::Daily,
        };
        let mut store = Store::new();
        store.upsert_routine(routine);
        ubu_core::generate_routine_tasks(&mut store, at(0).date_naive(), 1, ubu_core::Tz::UTC);
        let task_id = *store.tasks.keys().next().unwrap();
        let plan = re_plan(
            &store,
            ComputeTarget::DesktopOllama,
            at(0),
            at(0),
            &[],
            &AffectBudget { cap: 100 },
            &DeterministicPlacer,
        )
        .unwrap();
        assert_eq!(plan.entries.len(), 1);
        let transport = StubTransport::default();
        for (index, reminders) in [vec![10, 0], vec![5, 0], vec![]].into_iter().enumerate() {
            if index > 0 {
                store.tasks.get_mut(&task_id).unwrap().reminders = reminders.clone();
            }
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
                    created: usize::from(index == 0),
                    updated: usize::from(index > 0),
                    skipped: 0,
                }
            );
            let calls = transport.calls.borrow();
            assert_eq!(calls.len(), 1);
            assert_eq!(call_event(&calls[0]).reminders, reminders);
            if index == 0 {
                assert!(matches!(&calls[0], StubCall::Create(_)));
            } else {
                assert!(
                    matches!(&calls[0], StubCall::Update { event_id, .. } if event_id == "event-1")
                );
            }
        }
    }

    #[tokio::test]
    async fn dynamic_task_export_preserves_reminders() {
        let mut store = Store::new();
        let mut dynamic = task(1, "Dynamic reminder", Tier::UserShared, false, None);
        dynamic.reminders = vec![0];
        store.upsert_task(dynamic);
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
        assert_eq!(call_event(&transport.calls.borrow()[0]).reminders, vec![0]);
    }

    #[test]
    fn listed_event_reads_transparent_and_defaults_other_values_to_opaque() {
        let listed = |transparency: Option<&str>| ListedEvent {
            id: "event".to_string(),
            summary: "Summary".to_string(),
            color_id: None,
            start: Some(ListedEventTime {
                date_time: Some(at(0).to_rfc3339()),
            }),
            end: Some(ListedEventTime {
                date_time: Some(at(30).to_rfc3339()),
            }),
            status: None,
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

    pub(super) fn id(value: u128) -> Id {
        Id::from_u128(value)
    }

    pub(super) fn at(minutes: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(minutes * 60, 0).unwrap()
    }

    pub(super) fn task(
        value: u128,
        title: &str,
        tier: Tier,
        pinned: bool,
        category: Option<&str>,
    ) -> Task {
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
            reminders: Vec::new(),
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

    pub(super) fn fetched_event(
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
    async fn default_colors_color_pinned_personal_but_leave_dynamic_default() {
        let mut store = Store::new();
        store.upsert_task(task(1, "Routine", Tier::UserShared, true, Some("personal")));
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
            &default_category_colors(),
            Tier::UserShared,
        )
        .await
        .unwrap();

        let calls = transport.calls.borrow();
        assert_eq!(calls.len(), 2);
        assert_eq!(call_event(&calls[0]).color_id.as_deref(), Some("3"));
        assert_eq!(call_event(&calls[1]).color_id, None);
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
                skipped: 0,
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
        assert_eq!(
            store.export_signatures,
            BTreeMap::from([
                (id(2), event_signature(call_event(&calls[0]))),
                (id(1), event_signature(call_event(&calls[1]))),
            ])
        );
    }

    #[tokio::test]
    async fn second_export_skips_every_unchanged_event_without_transport_calls() {
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
        let before = store.clone();

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
                updated: 0,
                skipped: 2,
            }
        );
        assert!(transport.calls.borrow().is_empty());
        assert_eq!(store, before);
    }

    #[tokio::test]
    async fn retitling_one_task_updates_only_its_event() {
        let mut store = Store::new();
        store.upsert_task(task(1, "First", Tier::UserShared, false, None));
        store.upsert_task(task(2, "Second", Tier::UserShared, false, None));
        let plan = plan(&[1, 2]);
        let transport = StubTransport::default();
        let colors = BTreeMap::new();
        export_plan(&mut store, &plan, &transport, &colors, Tier::UserShared)
            .await
            .unwrap();
        let old_signatures = store.export_signatures.clone();
        transport.calls.borrow_mut().clear();
        store.tasks.get_mut(&id(1)).unwrap().title = "Renamed".into();

        let report = export_plan(&mut store, &plan, &transport, &colors, Tier::UserShared)
            .await
            .unwrap();
        assert_eq!(
            report,
            ExportReport {
                created: 0,
                updated: 1,
                skipped: 1
            }
        );
        let calls = transport.calls.borrow();
        assert!(matches!(
            calls.as_slice(),
            [StubCall::Update { event_id, event }]
                if event_id == "event-1" && event.summary == "Renamed"
        ));
        assert_eq!(
            store.export_signatures[&id(1)],
            event_signature(call_event(&calls[0]))
        );
        assert_ne!(store.export_signatures[&id(1)], old_signatures[&id(1)]);
        assert_eq!(store.export_signatures[&id(2)], old_signatures[&id(2)]);
    }

    #[tokio::test]
    async fn missing_link_creates_an_event_even_with_a_matching_cached_signature() {
        let mut store = Store::new();
        store.upsert_task(task(1, "First", Tier::UserShared, false, None));
        let plan = plan(&[1]);
        let transport = StubTransport::default();
        let colors = BTreeMap::new();
        export_plan(&mut store, &plan, &transport, &colors, Tier::UserShared)
            .await
            .unwrap();
        store.calendar_links.remove(&id(1));
        transport.calls.borrow_mut().clear();

        let report = export_plan(&mut store, &plan, &transport, &colors, Tier::UserShared)
            .await
            .unwrap();
        assert_eq!(
            report,
            ExportReport {
                created: 1,
                updated: 0,
                skipped: 0
            }
        );
        let calls = transport.calls.borrow();
        assert!(matches!(calls.as_slice(), [StubCall::Create(_)]));
        assert_eq!(
            store.calendar_link(id(1)).map(String::as_str),
            Some("event-2")
        );
        assert_eq!(
            store.export_signatures[&id(1)],
            event_signature(call_event(&calls[0]))
        );
    }

    #[tokio::test]
    async fn legacy_links_without_signatures_update_once_then_skip_after_reload() {
        let mut store = Store::new();
        store.upsert_task(task(1, "First", Tier::UserShared, false, None));
        store.upsert_task(task(2, "Second", Tier::UserShared, false, None));
        store.upsert_calendar_link(id(1), "legacy-1".into());
        store.upsert_calendar_link(id(2), "legacy-2".into());
        let mut legacy = serde_json::to_value(&store).unwrap();
        legacy.as_object_mut().unwrap().remove("export_signatures");
        let mut store: Store =
            serde_json::from_str(&serde_json::to_string(&legacy).unwrap()).unwrap();
        assert!(store.export_signatures.is_empty());
        let plan = plan(&[1, 2]);
        let transport = StubTransport::default();
        let colors = BTreeMap::new();

        let first = export_plan(&mut store, &plan, &transport, &colors, Tier::UserShared)
            .await
            .unwrap();
        assert_eq!(
            first,
            ExportReport {
                created: 0,
                updated: 2,
                skipped: 0
            }
        );
        assert!(matches!(
            transport.calls.borrow().as_slice(),
            [StubCall::Update { event_id: first, .. }, StubCall::Update { event_id: second, .. }]
                if first == "legacy-1" && second == "legacy-2"
        ));
        assert_eq!(store.export_signatures.len(), 2);
        let reloaded: Store =
            serde_json::from_str(&serde_json::to_string(&store).unwrap()).unwrap();
        assert_eq!(reloaded, store);
        store = reloaded;
        transport.calls.borrow_mut().clear();

        let second = export_plan(&mut store, &plan, &transport, &colors, Tier::UserShared)
            .await
            .unwrap();
        assert_eq!(
            second,
            ExportReport {
                created: 0,
                updated: 0,
                skipped: 2
            }
        );
        assert!(transport.calls.borrow().is_empty());
    }

    #[tokio::test]
    async fn failed_transport_calls_do_not_cache_unsent_events_and_can_be_retried() {
        let mut store = Store::new();
        store.upsert_task(task(1, "First", Tier::UserShared, false, None));
        let plan = plan(&[1]);
        let colors = BTreeMap::new();
        let before = store.clone();
        let failed_create = StubTransport::with_create_error("create failed");
        assert_eq!(
            export_plan(&mut store, &plan, &failed_create, &colors, Tier::UserShared).await,
            Err("create failed".into())
        );
        assert_eq!(store, before);
        assert!(matches!(
            failed_create.calls.borrow().as_slice(),
            [StubCall::Create(_)]
        ));

        let transport = StubTransport::default();
        export_plan(&mut store, &plan, &transport, &colors, Tier::UserShared)
            .await
            .unwrap();
        store.tasks.get_mut(&id(1)).unwrap().title = "Renamed".into();
        let before = store.clone();
        let failed_update = StubTransport::with_update_error("update failed");
        assert_eq!(
            export_plan(&mut store, &plan, &failed_update, &colors, Tier::UserShared).await,
            Err("update failed".into())
        );
        assert_eq!(store, before);
        assert!(matches!(
            failed_update.calls.borrow().as_slice(),
            [StubCall::Update { .. }]
        ));
        transport.calls.borrow_mut().clear();

        let retry = export_plan(&mut store, &plan, &transport, &colors, Tier::UserShared)
            .await
            .unwrap();
        assert_eq!(
            retry,
            ExportReport {
                created: 0,
                updated: 1,
                skipped: 0
            }
        );
        assert!(matches!(
            transport.calls.borrow().as_slice(),
            [StubCall::Update { .. }]
        ));
        assert_ne!(store.export_signatures, before.export_signatures);
    }

    #[tokio::test]
    async fn edits_to_redacted_or_unsent_task_fields_do_not_trigger_updates() {
        let mut store = Store::new();
        store.upsert_task(task(1, "Secret", Tier::TopSecret, true, None));
        let plan = plan(&[1]);
        let transport = StubTransport::default();
        let colors = default_category_colors();
        export_plan(&mut store, &plan, &transport, &colors, Tier::UserShared)
            .await
            .unwrap();
        transport.calls.borrow_mut().clear();
        let secret = store.tasks.get_mut(&id(1)).unwrap();
        secret.title = "New secret".into();
        secret.category = Some("personal".into());
        secret.detail = Some("New detail".into());

        let report = export_plan(&mut store, &plan, &transport, &colors, Tier::UserShared)
            .await
            .unwrap();
        assert_eq!(
            report,
            ExportReport {
                created: 0,
                updated: 0,
                skipped: 1
            }
        );
        assert!(transport.calls.borrow().is_empty());
        assert!(!store.export_signatures[&id(1)].contains("secret"));
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
            &[],
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
            &[],
            at(0),
            Tier::UserShared,
            &BTreeMap::new(),
        );

        assert_eq!(
            report,
            ImportReport {
                removed: 0,
                captured: 1,
                completed: 0,
                reopened: 0,
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
            &[],
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
            &[],
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
            &[],
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
            &[],
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
            let event = fetched_event("owned-dynamic", "Dynamic", None, 60, end_minutes);
            let report = import_from_calendar(
                &mut store,
                &[event],
                &[],
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
            &[],
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
            &[],
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

        let first = import_from_calendar(&mut store, &events, &[], at(0), Tier::UserShared, &colors);
        assert_eq!(
            first,
            ImportReport {
                removed: 0,
                captured: 2,
                completed: 1,
                reopened: 0,
                moved: 1,
                resized: 0,
            }
        );
        let after_first = store.clone();

        let second = import_from_calendar(&mut store, &events, &[], at(1), Tier::UserShared, &colors);

        assert_eq!(
            second,
            ImportReport {
                removed: 0,
                captured: 0,
                completed: 0,
                reopened: 0,
                moved: 0,
                resized: 0,
            }
        );
        assert_eq!(store, after_first);
    }
}
