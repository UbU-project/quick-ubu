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
        if result.is_ok() {
            self.retry_cycle = false;
        }
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
        // Retry only once a cycle starts. A transient poll-fetch failure alone
        // must not turn an unchanged calendar into a new import/export cycle.
        self.retry_cycle = true;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::persist::SqliteBackend;
    use chrono::{Duration, TimeZone};
    use gcal::CalendarEvent;
    use std::cell::{Cell, RefCell};

    fn now() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 11, 12, 0, 0).unwrap()
    }
    fn event(id: &str) -> FetchedEvent {
        FetchedEvent {
            id: id.into(),
            summary: "Task | \"quoted\"\n雪".into(),
            color_id: None,
            start: now() + Duration::hours(1),
            end: now() + Duration::hours(2),
            transparent: false,
        }
    }
    fn config() -> WatchConfig {
        WatchConfig::new(
            gcal::calendar_import_window(now(), None, None).unwrap(),
            gcal::default_category_colors(),
        )
    }
    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Runtime::new().unwrap()
    }

    #[test]
    fn unchanged_poll_after_transient_fetch_error_does_not_cycle() {
        runtime().block_on(async {
            let remote = Calendar::with_event(event("a"));
            let backend = Backend::new();
            let mut store = Store::new();
            let mut state = WatchState::new(&store);
            state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap();
            remote.fail_list.set(remote.lists.get() + 1);
            assert!(state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .is_err());
            assert!(state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap()
                .is_none());
            assert_eq!(backend.saves.get(), 1);
            assert_eq!(remote.writes.get(), 1);
        });
    }

    #[test]
    fn failed_planning_retries_even_if_calendar_returns_to_previous_snapshot() {
        runtime().block_on(async {
            let remote = Calendar::with_event(event("a"));
            let backend = Backend::new();
            let mut store = Store::new();
            let mut state = WatchState::new(&store);
            state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap();
            let previous = store.poll_snapshot.clone();
            let original_event = remote.events.borrow()["a"].clone();
            let task = store.tasks.values_mut().next().unwrap();
            task.after.push(ubu_core::AfterConstraint {
                task_id: task.id,
                offset: Duration::zero(),
            });
            remote
                .events
                .borrow_mut()
                .get_mut("a")
                .unwrap()
                .summary
                .push('!');
            let error = state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap_err();
            assert!(error.contains("watch planning failed"));
            assert_eq!(store.poll_snapshot, previous);
            store.tasks.values_mut().next().unwrap().after.clear();
            remote
                .events
                .borrow_mut()
                .insert("a".into(), original_event);
            assert!(state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap()
                .is_some());
            assert_eq!(backend.saves.get(), 2);
            assert_eq!(backend.load().unwrap(), store);
        });
    }

    #[test]
    fn fingerprints_exclude_ids_and_snapshots_are_deterministic() {
        let a = event("a");
        let b = event("b");
        assert_eq!(event_fingerprint(&a), event_fingerprint(&b));
        let expected = snapshot_of(&[a.clone(), b.clone()]);
        assert_eq!(expected.len(), 2);
        assert_eq!(expected, snapshot_of(&[b, a]));
        assert!(!has_changes(&expected, &expected));
        let mut different = event("a");
        different.color_id = Some(String::new());
        assert_ne!(event_fingerprint(&different), expected["a"]);
        let mut left = event("a");
        left.summary = "a|b".into();
        left.color_id = Some("c".into());
        let mut right = event("a");
        right.summary = "a".into();
        right.color_id = Some("b|c".into());
        assert_ne!(event_fingerprint(&left), event_fingerprint(&right));
    }

    #[test]
    fn changes_detect_added_removed_and_each_fingerprinted_field() {
        let original = event("a");
        let previous = snapshot_of(&[original.clone()]);
        assert!(has_changes(
            &snapshot_of(&[original.clone(), event("b")]),
            &previous
        ));
        assert!(has_changes(&BTreeMap::new(), &previous));
        for field in 0..5 {
            let mut changed = original.clone();
            match field {
                0 => changed.summary.push('!'),
                1 => changed.color_id = Some("9".into()),
                2 => changed.start += Duration::minutes(1),
                3 => changed.end += Duration::minutes(1),
                _ => changed.transparent = true,
            }
            assert!(has_changes(&snapshot_of(&[changed]), &previous));
        }
        assert!(!has_changes(&BTreeMap::new(), &BTreeMap::new()));
    }

    #[derive(Default)]
    struct Calendar {
        events: RefCell<BTreeMap<String, FetchedEvent>>,
        lists: Cell<usize>,
        writes: Cell<usize>,
        fail_list: Cell<usize>,
        fail_export: Cell<bool>,
    }
    impl Calendar {
        fn with_event(event: FetchedEvent) -> Self {
            Self {
                events: RefCell::new([(event.id.clone(), event)].into_iter().collect()),
                ..Self::default()
            }
        }
        fn write(&self, id: &str, event: &CalendarEvent) -> Result<(), String> {
            if self.fail_export.replace(false) {
                return Err("injected export failure".into());
            }
            self.writes.set(self.writes.get() + 1);
            self.events.borrow_mut().insert(
                id.into(),
                FetchedEvent {
                    id: id.into(),
                    summary: event.summary.clone(),
                    color_id: event.color_id.clone(),
                    start: event.start,
                    end: event.end,
                    transparent: event.transparent,
                },
            );
            Ok(())
        }
    }
    impl CalendarTransport for Calendar {
        async fn create_event(&self, event: &CalendarEvent) -> Result<String, String> {
            let id = format!("created-{}", self.writes.get());
            self.write(&id, event)?;
            Ok(id)
        }
        async fn update_event(&self, id: &str, event: &CalendarEvent) -> Result<(), String> {
            self.write(id, event)
        }
        async fn list_events(
            &self,
            _: DateTime<Utc>,
            _: DateTime<Utc>,
        ) -> Result<Vec<FetchedEvent>, String> {
            self.lists.set(self.lists.get() + 1);
            if self.lists.get() == self.fail_list.get() {
                return Err("injected fetch failure".into());
            }
            Ok(self.events.borrow().values().cloned().collect())
        }
        async fn get_event(&self, id: &str) -> Result<Option<FetchedEvent>, String> {
            Ok(self.events.borrow().get(id).cloned())
        }
    }
    struct Backend {
        sqlite: SqliteBackend,
        saves: Cell<usize>,
        fail: Cell<bool>,
    }
    impl Backend {
        fn new() -> Self {
            Self {
                sqlite: SqliteBackend::in_memory().unwrap(),
                saves: Cell::new(0),
                fail: Cell::new(false),
            }
        }
    }
    impl StorageBackend for Backend {
        fn load(&self) -> Result<Store, String> {
            self.sqlite.load()
        }
        fn save(&self, store: &Store) -> Result<(), String> {
            self.saves.set(self.saves.get() + 1);
            if self.fail.replace(false) {
                return Err("injected save failure".into());
            }
            self.sqlite.save(store)
        }
    }

    #[test]
    fn cycle_absorbs_export_and_subsequent_poll_and_restart_do_not_refire() {
        runtime().block_on(async {
            let remote = Calendar::with_event(event("a"));
            let before_export = snapshot_of(&[event("a")]);
            let backend = Backend::new();
            let mut store = Store::new();
            let mut state = WatchState::new(&store);
            let report = state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(report.import.captured, 1);
            assert_eq!(report.planned, 1);
            assert_eq!(report.export.updated, 1);
            assert_ne!(store.poll_snapshot, before_export);
            assert_eq!(backend.load().unwrap(), store);
            let exported =
                snapshot_of(&remote.events.borrow().values().cloned().collect::<Vec<_>>());
            assert!(!has_changes(&exported, &store.poll_snapshot));
            assert!(state
                .poll(
                    &mut store,
                    &backend,
                    &remote,
                    &config(),
                    now() + Duration::minutes(1)
                )
                .await
                .unwrap()
                .is_none());
            let mut restarted = WatchState::new(&store);
            assert!(restarted
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap()
                .is_none());
            assert_eq!(backend.saves.get(), 1);
            assert_eq!(remote.writes.get(), 1);
        });
    }

    #[test]
    fn unchanged_empty_calendar_cycles_once_per_process() {
        runtime().block_on(async {
            let remote = Calendar::default();
            let backend = Backend::new();
            let mut store = Store::new();
            let mut state = WatchState::new(&store);
            assert!(state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap()
                .is_some());
            assert!(state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap()
                .is_none());
            assert_eq!(backend.saves.get(), 1);
            assert_eq!(remote.writes.get(), 0);
        });
    }

    #[test]
    fn operator_recolor_and_time_change_complete_task_and_are_absorbed() {
        runtime().block_on(async {
            let remote = Calendar::with_event(event("a"));
            let backend = Backend::new();
            let mut store = Store::new();
            let mut state = WatchState::new(&store);
            state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap();
            {
                let mut events = remote.events.borrow_mut();
                let edited = events.get_mut("a").unwrap();
                edited.color_id = Some("9".into());
                edited.start = now() - Duration::minutes(30);
                edited.end = now();
            }
            let report = state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(report.import.completed, 1);
            assert_eq!(report.planned, 0);
            assert!(store
                .tasks
                .values()
                .all(|task| task.status == ubu_core::TaskStatus::Done));
            assert!(state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap()
                .is_none());
            assert_eq!(backend.load().unwrap(), store);
        });
    }

    #[test]
    fn confirmed_deletion_runs_import_and_removes_pinned_task() {
        runtime().block_on(async {
            let mut fixed = event("fixed");
            fixed.color_id = Some("9".into());
            let remote = Calendar::with_event(fixed);
            let backend = Backend::new();
            let mut store = Store::new();
            let mut state = WatchState::new(&store);
            state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap();
            remote.events.borrow_mut().clear();
            let report = state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap()
                .unwrap();
            assert_eq!(report.import.removed, 1);
            assert!(store.tasks.is_empty());
            assert!(store.poll_snapshot.is_empty());
            assert!(state
                .poll(&mut store, &backend, &remote, &config(), now())
                .await
                .unwrap()
                .is_none());
        });
    }

    #[test]
    fn fetch_export_post_fetch_and_save_errors_retry_without_advancing_snapshot() {
        runtime().block_on(async {
            for failure in ["fetch", "export", "post-fetch", "save"] {
                let remote = Calendar::with_event(event("a"));
                let backend = Backend::new();
                let mut store = Store::new();
                match failure {
                    "fetch" => remote.fail_list.set(1),
                    "export" => remote.fail_export.set(true),
                    "post-fetch" => remote.fail_list.set(2),
                    _ => backend.fail.set(true),
                }
                let mut state = WatchState::new(&store);
                assert!(
                    state
                        .poll(&mut store, &backend, &remote, &config(), now())
                        .await
                        .is_err(),
                    "{failure}"
                );
                assert!(store.poll_snapshot.is_empty());
                assert_eq!(backend.load().unwrap(), Store::new());
                assert!(
                    state
                        .poll(&mut store, &backend, &remote, &config(), now())
                        .await
                        .unwrap()
                        .is_some(),
                    "{failure}"
                );
                assert_eq!(store.tasks.len(), 1);
                assert_eq!(backend.load().unwrap(), store);
                assert!(state
                    .poll(&mut store, &backend, &remote, &config(), now())
                    .await
                    .unwrap()
                    .is_none());
            }
        });
    }
}
