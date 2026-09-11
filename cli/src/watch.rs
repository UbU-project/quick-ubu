//! Calendar polling and change detection; transport and persistence are injectable.
use std::collections::BTreeMap;

use gcal::FetchedEvent;

pub fn event_fingerprint(event: &FetchedEvent) -> String {
    // Encode the five fields as a JSON tuple so delimiters, quotes, and None
    // cannot collide. Event identity belongs to the snapshot key, not the value.
    serde_json::to_string(&(
        &event.summary,
        &event.color_id,
        event.start,
        event.end,
        event.transparent,
    ))
    .expect("calendar fingerprint fields serialize to JSON")
}

pub fn snapshot_of(events: &[FetchedEvent]) -> BTreeMap<String, String> {
    events.iter().map(|event| (event.id.clone(), event_fingerprint(event))).collect()
}

pub fn has_changes(current: &BTreeMap<String, String>, previous: &BTreeMap<String, String>) -> bool {
    current != previous
}
