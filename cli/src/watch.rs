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
    events
        .iter()
        .map(|event| (event.id.clone(), event_fingerprint(event)))
        .collect()
}

pub fn has_changes(
    current: &BTreeMap<String, String>,
    previous: &BTreeMap<String, String>,
) -> bool {
    current != previous
}

use chrono::{DateTime, Utc};
use gcal::{
    export_plan, fetch_import_events, import_from_calendar, CalendarTransport, ExportReport,
    ImportReport,
};
use ubu_core::{
    re_plan, AffectBudget, ComputeTarget, DeterministicPlacer, Store, Tier, TimeWindow,
};

use crate::persist::StorageBackend;

pub struct WatchConfig {
    pub window: TimeWindow,
    pub color_map: BTreeMap<String, String>,
    pub color_to_category: BTreeMap<String, String>,
    pub budget: AffectBudget,
}

impl WatchConfig {
    pub fn new(window: TimeWindow, color_map: BTreeMap<String, String>) -> Self {
        let color_to_category = color_map
            .iter()
            .map(|(category, color)| (color.clone(), category.clone()))
            .collect();
        Self {
            window,
            color_map,
            color_to_category,
            budget: AffectBudget { cap: 100 },
        }
    }
}

#[derive(Debug)]
pub struct CycleReport {
    pub import: ImportReport,
    pub planned: usize,
    pub conflicts: usize,
    pub export: ExportReport,
}

pub struct WatchState {
    initialized: bool,
    retry_cycle: bool,
}

impl WatchState {
    pub fn new(store: &Store) -> Self {
        Self {
            initialized: !store.poll_snapshot.is_empty(),
            retry_cycle: false,
        }
    }

    pub async fn poll<T: CalendarTransport>(
        &mut self,
        store: &mut Store,
        backend: &dyn StorageBackend,
        transport: &T,
        config: &WatchConfig,
        now: DateTime<Utc>,
    ) -> Result<Option<CycleReport>, String> {
        let result = self
            .poll_inner(store, backend, transport, config, now)
            .await;
        // A partial export or failed save must be retried even if the next fetch
        // happens to match the old snapshot. Keep successful in-memory links.
        self.retry_cycle = result.is_err();
        result
    }

    async fn poll_inner<T: CalendarTransport>(
        &mut self,
        store: &mut Store,
        backend: &dyn StorageBackend,
        transport: &T,
        config: &WatchConfig,
        now: DateTime<Utc>,
    ) -> Result<Option<CycleReport>, String> {
        let fetched = fetch_import_events(store, transport, &config.window)
            .await
            .map_err(|error| format!("poll fetch failed: {error}"))?;
        let current = snapshot_of(&fetched.events);
        if self.initialized && !self.retry_cycle && !has_changes(&current, &store.poll_snapshot) {
            return Ok(None);
        }
        let import = import_from_calendar(
            store,
            &fetched.events,
            &fetched.deleted,
            now,
            Tier::UserShared,
            &config.color_to_category,
        );
        let plan = re_plan(
            store,
            ComputeTarget::DesktopOllama,
            now,
            now,
            &[],
            &config.budget,
            &DeterministicPlacer,
        )
        .map_err(|error| format!("watch planning failed: {error:?}"))?;
        let export = export_plan(store, &plan, transport, &config.color_map, Tier::UserShared)
            .await
            .map_err(|error| format!("watch export failed: {error}"))?;
        let after = fetch_import_events(store, transport, &config.window)
            .await
            .map_err(|error| format!("post-export fetch failed: {error}"))?;
        let previous = std::mem::replace(&mut store.poll_snapshot, snapshot_of(&after.events));
        if let Err(error) = backend.save(store) {
            store.poll_snapshot = previous;
            return Err(format!("watch save failed: {error}"));
        }
        // An empty calendar is still initialized after a successful cycle. This
        // avoids re-firing forever when both pre/post-export snapshots are empty.
        self.initialized = true;
        Ok(Some(CycleReport {
            import,
            planned: plan.entries.len(),
            conflicts: plan.conflicts.len(),
            export,
        }))
    }
}

pub async fn run<T: CalendarTransport>(
    store: &mut Store,
    backend: &dyn StorageBackend,
    transport: &T,
    config: &WatchConfig,
    interval: std::time::Duration,
) {
    let mut state = WatchState::new(store);
    loop {
        match state.poll(store, backend, transport, config, Utc::now()).await {
            Ok(Some(report)) => println!(
                "watch: captured {}, completed {}, reopened {}, moved {}, resized {}, removed {}; planned {}, conflicts {}; created {}, updated {}, skipped {}",
                report.import.captured, report.import.completed, report.import.reopened,
                report.import.moved, report.import.resized, report.import.removed,
                report.planned, report.conflicts, report.export.created, report.export.updated, report.export.skipped
            ),
            Ok(None) => {},
            Err(error) => eprintln!("quick-ubu watch: {error}"),
        }
        tokio::time::sleep(interval).await;
    }
}
