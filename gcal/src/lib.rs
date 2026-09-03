//! Google Calendar export behind a transport boundary.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use ubu_core::{visible_as_content, Plan, Store, Tier};

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
