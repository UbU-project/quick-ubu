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

#[allow(async_fn_in_trait)]
pub trait CalendarTransport {
    async fn create_event(&self, event: &CalendarEvent) -> Result<String, String>;

    async fn update_event(&self, event_id: &str, event: &CalendarEvent) -> Result<(), String>;
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

#[derive(Serialize)]
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
}

#[cfg(test)]
mod stub_tests {
    use super::*;

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
}
