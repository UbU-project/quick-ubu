//! Google Calendar export behind a transport boundary.

use chrono::{DateTime, Utc};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CalendarEvent {
    pub summary: String,
    pub start: DateTime<Utc>,
    pub end: DateTime<Utc>,
    pub color_id: Option<String>,
}

pub trait CalendarTransport {
    async fn create_event(&self, event: &CalendarEvent) -> Result<String, String>;

    async fn update_event(&self, event_id: &str, event: &CalendarEvent) -> Result<(), String>;
}
