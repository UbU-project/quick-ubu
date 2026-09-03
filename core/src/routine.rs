//! Pure recurrence expansion into deterministic pinned tasks.

use std::collections::BTreeSet;

use chrono::{Datelike, Days, Duration, NaiveDate, NaiveTime, TimeZone, Utc, Weekday, WeekdaySet};
pub use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::store::Store;
use crate::types::{DeferPolicy, Id, Provenance, Task, TaskStatus, Tier, TimeWindow};

const NAMESPACE: Uuid = Uuid::from_u128(0x6f51_89f1_6208_5c1e_a8ec_15c0f894ea9d);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recurrence {
    Daily,
    Weekly {
        #[serde(
            serialize_with = "serialize_weekdays",
            deserialize_with = "deserialize_weekdays"
        )]
        weekdays: WeekdaySet,
    },
    MonthlyDay {
        days: BTreeSet<u32>,
    },
}

fn serialize_weekdays<S>(weekdays: &WeekdaySet, serializer: S) -> Result<S::Ok, S::Error>
where
    S: Serializer,
{
    weekdays
        .iter(Weekday::Mon)
        .collect::<Vec<_>>()
        .serialize(serializer)
}

fn deserialize_weekdays<'de, D>(deserializer: D) -> Result<WeekdaySet, D::Error>
where
    D: Deserializer<'de>,
{
    Ok(Vec::<Weekday>::deserialize(deserializer)?
        .into_iter()
        .collect())
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutineTemplate {
    pub id: Id,
    pub title: String,
    pub tier: Tier,
    pub start_time: NaiveTime,
    pub duration: Duration,
    pub affect_cost: i32,
    /// Free-form classification (e.g. "personal", "relationship", "business").
    /// Maps to a calendar color on export and groups time in reporting.
    #[serde(default)]
    pub category: Option<String>,
    pub recurrence: Recurrence,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerateReport {
    pub created: usize,
    pub skipped: usize,
}

fn matches_date(recurrence: &Recurrence, date: NaiveDate) -> bool {
    match recurrence {
        Recurrence::Daily => true,
        Recurrence::Weekly { weekdays } => weekdays.contains(date.weekday()),
        Recurrence::MonthlyDay { days } => days.contains(&date.day()),
    }
}

pub fn expand_routine(
    templates: &[RoutineTemplate],
    from: NaiveDate,
    days: u32,
    tz: Tz,
) -> Vec<Task> {
    let mut tasks = Vec::new();

    for offset in 0..days {
        let Some(date) = from.checked_add_days(Days::new(u64::from(offset))) else {
            break;
        };
        for template in templates {
            if !matches_date(&template.recurrence, date) {
                continue;
            }

            let local_start = date.and_time(template.start_time);
            let localized = tz.from_local_datetime(&local_start);
            let Some(localized_start) = localized.clone().single().or_else(|| localized.earliest())
            else {
                continue;
            };
            let start = localized_start.with_timezone(&Utc);
            let end = start + template.duration;
            let id = Uuid::new_v5(&NAMESPACE, format!("{}|{}", template.id, date).as_bytes());

            tasks.push(Task {
                id,
                tier: template.tier,
                title: template.title.clone(),
                detail: None,
                objective_ids: Vec::new(),
                skills: Vec::new(),
                affect_cost: template.affect_cost,
                est_duration: template.duration,
                due: None,
                earliest_start: None,
                category: template.category.clone(),
                pinned: Some(TimeWindow { start, end }),
                blocked_by: Vec::new(),
                defer_policy: DeferPolicy::RescheduleAsap,
                status: TaskStatus::Scheduled,
                provenance: Provenance::Manual,
                commitment: None,
            });
        }
    }

    tasks.sort_by_key(|task| {
        (
            task.pinned
                .as_ref()
                .expect("routine tasks are pinned")
                .start,
            task.id,
        )
    });
    tasks
}

pub fn generate_routine_tasks(
    store: &mut Store,
    from: NaiveDate,
    days: u32,
    tz: Tz,
) -> GenerateReport {
    let templates: Vec<RoutineTemplate> = store.routines().values().cloned().collect();
    let mut report = GenerateReport {
        created: 0,
        skipped: 0,
    };

    for task in expand_routine(&templates, from, days, tz) {
        if store.tasks.contains_key(&task.id) {
            report.skipped += 1;
        } else {
            store.upsert_task(task);
            report.created += 1;
        }
    }

    report
}

#[cfg(test)]
mod tests {
    use chrono::{NaiveDate, NaiveTime, Timelike};

    use super::*;

    fn id(value: u128) -> Id {
        Uuid::from_u128(value)
    }

    fn date(year: i32, month: u32, day: u32) -> NaiveDate {
        NaiveDate::from_ymd_opt(year, month, day).unwrap()
    }

    fn time(hour: u32, minute: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(hour, minute, 0).unwrap()
    }

    fn template(value: u128, recurrence: Recurrence) -> RoutineTemplate {
        RoutineTemplate {
            id: id(value),
            title: format!("routine-{value}"),
            tier: Tier::UserShared,
            start_time: time(6, 30),
            duration: Duration::minutes(45),
            affect_cost: 3,
            category: None,
            recurrence,
        }
    }

    fn pinned_start(task: &Task) -> chrono::DateTime<Utc> {
        task.pinned.as_ref().expect("routine task is pinned").start
    }

    #[test]
    fn daily_expands_once_per_day_across_the_range() {
        let tasks = expand_routine(
            &[template(1, Recurrence::Daily)],
            date(2026, 9, 1),
            4,
            chrono_tz::UTC,
        );

        assert_eq!(tasks.len(), 4);
        assert_eq!(
            tasks
                .iter()
                .map(|task| pinned_start(task).date_naive())
                .collect::<Vec<_>>(),
            (1..=4).map(|day| date(2026, 9, day)).collect::<Vec<_>>()
        );
        assert!(tasks
            .iter()
            .all(|task| task.status == TaskStatus::Scheduled));
    }

    #[test]
    fn weekly_expands_only_on_mondays_and_wednesdays() {
        let weekdays = [Weekday::Mon, Weekday::Wed].into_iter().collect();
        let tasks = expand_routine(
            &[template(1, Recurrence::Weekly { weekdays })],
            date(2024, 1, 1),
            7,
            chrono_tz::UTC,
        );

        assert_eq!(tasks.len(), 2);
        assert_eq!(pinned_start(&tasks[0]).date_naive(), date(2024, 1, 1));
        assert_eq!(pinned_start(&tasks[1]).date_naive(), date(2024, 1, 3));
        assert!(tasks
            .iter()
            .all(|task| matches!(pinned_start(task).weekday(), Weekday::Mon | Weekday::Wed)));
    }

    #[test]
    fn monthly_expands_only_on_the_first_and_fifteenth() {
        let tasks = expand_routine(
            &[template(
                1,
                Recurrence::MonthlyDay {
                    days: [1, 15].into_iter().collect(),
                },
            )],
            date(2024, 1, 1),
            46,
            chrono_tz::UTC,
        );

        assert_eq!(tasks.len(), 4);
        assert_eq!(
            tasks
                .iter()
                .map(|task| pinned_start(task).date_naive())
                .collect::<Vec<_>>(),
            vec![
                date(2024, 1, 1),
                date(2024, 1, 15),
                date(2024, 2, 1),
                date(2024, 2, 15),
            ]
        );
    }

    #[test]
    fn new_york_wall_time_tracks_standard_and_daylight_offsets() {
        let routine = template(1, Recurrence::Daily);
        let standard = expand_routine(
            std::slice::from_ref(&routine),
            date(2026, 1, 15),
            1,
            chrono_tz::America::New_York,
        );
        let daylight = expand_routine(
            &[routine],
            date(2026, 7, 15),
            1,
            chrono_tz::America::New_York,
        );

        assert_eq!(
            pinned_start(&standard[0]),
            date(2026, 1, 15).and_hms_opt(11, 30, 0).unwrap().and_utc()
        );
        assert_eq!(
            pinned_start(&daylight[0]),
            date(2026, 7, 15).and_hms_opt(10, 30, 0).unwrap().and_utc()
        );
    }

    #[test]
    fn identical_expansions_have_identical_ids_and_output() {
        let templates = vec![
            template(2, Recurrence::Daily),
            template(1, Recurrence::Daily),
        ];

        let first = expand_routine(&templates, date(2026, 9, 1), 3, chrono_tz::UTC);
        let second = expand_routine(&templates, date(2026, 9, 1), 3, chrono_tz::UTC);

        assert_eq!(first, second);
        assert_eq!(
            first.iter().map(|task| task.id).collect::<Vec<_>>(),
            second.iter().map(|task| task.id).collect::<Vec<_>>()
        );
        assert!(first.windows(2).all(|pair| {
            let left = (pinned_start(&pair[0]), pair[0].id);
            let right = (pinned_start(&pair[1]), pair[1].id);
            left <= right
        }));
    }

    #[test]
    fn pinned_window_duration_equals_template_duration() {
        let routine = template(1, Recurrence::Daily);
        let tasks = expand_routine(
            std::slice::from_ref(&routine),
            date(2026, 9, 1),
            1,
            chrono_tz::UTC,
        );
        let window = tasks[0].pinned.as_ref().unwrap();

        assert_eq!(window.end - window.start, routine.duration);
        assert_eq!(tasks[0].est_duration, routine.duration);
    }

    #[test]
    fn weekly_recurrence_serde_round_trips() {
        let recurrence = Recurrence::Weekly {
            weekdays: [Weekday::Mon, Weekday::Wed].into_iter().collect(),
        };

        let json = serde_json::to_string(&recurrence).expect("recurrence serializes");
        let restored: Recurrence = serde_json::from_str(&json).expect("recurrence deserializes");

        assert_eq!(restored, recurrence);
    }

    #[test]
    fn nonexistent_spring_forward_time_skips_only_that_occurrence() {
        let mut routine = template(1, Recurrence::Daily);
        routine.start_time = time(2, 30);

        let tasks = expand_routine(
            &[routine],
            date(2026, 3, 7),
            3,
            chrono_tz::America::New_York,
        );

        assert_eq!(tasks.len(), 2);
        assert_eq!(
            tasks
                .iter()
                .map(|task| {
                    pinned_start(task)
                        .with_timezone(&chrono_tz::America::New_York)
                        .date_naive()
                })
                .collect::<Vec<_>>(),
            vec![date(2026, 3, 7), date(2026, 3, 9)]
        );
    }

    #[test]
    fn ambiguous_fall_back_time_uses_the_earliest_instant() {
        let mut routine = template(1, Recurrence::Daily);
        routine.start_time = time(1, 30);

        let tasks = expand_routine(
            &[routine],
            date(2026, 11, 1),
            1,
            chrono_tz::America::New_York,
        );

        assert_eq!(pinned_start(&tasks[0]).hour(), 5);
        assert_eq!(pinned_start(&tasks[0]).minute(), 30);
    }

    #[test]
    fn generation_creates_expected_tasks_then_skips_every_duplicate() {
        let mut store = Store::new();
        store.upsert_routine(template(1, Recurrence::Daily));

        let first = generate_routine_tasks(&mut store, date(2026, 9, 1), 3, chrono_tz::UTC);
        assert_eq!(
            first,
            GenerateReport {
                created: 3,
                skipped: 0,
            }
        );
        assert_eq!(store.tasks.len(), 3);
        assert!(store.tasks.values().all(|task| {
            task.status == TaskStatus::Scheduled
                && task.pinned.is_some()
                && task.title == "routine-1"
        }));
        let after_first = store.tasks.clone();

        let second = generate_routine_tasks(&mut store, date(2026, 9, 1), 3, chrono_tz::UTC);
        assert_eq!(
            second,
            GenerateReport {
                created: 0,
                skipped: 3,
            }
        );
        assert_eq!(store.tasks, after_first);
        assert_eq!(
            store.tasks.keys().copied().collect::<BTreeSet<_>>().len(),
            3
        );
    }

    #[test]
    fn regeneration_never_clobbers_a_moved_or_completed_task() {
        let mut store = Store::new();
        store.upsert_routine(template(1, Recurrence::Daily));
        generate_routine_tasks(&mut store, date(2026, 9, 1), 1, chrono_tz::UTC);
        let task_id = *store.tasks.keys().next().unwrap();
        let task = store.tasks.get_mut(&task_id).unwrap();
        let original = task.pinned.as_ref().unwrap().clone();
        task.pinned = Some(TimeWindow {
            start: original.start + Duration::hours(2),
            end: original.end + Duration::hours(2),
        });
        task.status = TaskStatus::Done;
        let changed = task.clone();

        let report = generate_routine_tasks(&mut store, date(2026, 9, 1), 1, chrono_tz::UTC);

        assert_eq!(
            report,
            GenerateReport {
                created: 0,
                skipped: 1,
            }
        );
        assert_eq!(store.tasks[&task_id], changed);
    }

    #[test]
    fn adding_template_fills_only_its_tasks_for_covered_dates() {
        let mut store = Store::new();
        store.upsert_routine(template(1, Recurrence::Daily));
        generate_routine_tasks(&mut store, date(2026, 9, 1), 2, chrono_tz::UTC);
        let original_ids: BTreeSet<Id> = store.tasks.keys().copied().collect();

        let added = template(2, Recurrence::Daily);
        let expected_new_ids: BTreeSet<Id> = expand_routine(
            std::slice::from_ref(&added),
            date(2026, 9, 1),
            2,
            chrono_tz::UTC,
        )
        .into_iter()
        .map(|task| task.id)
        .collect();
        store.upsert_routine(added);
        let report = generate_routine_tasks(&mut store, date(2026, 9, 1), 2, chrono_tz::UTC);

        assert_eq!(
            report,
            GenerateReport {
                created: 2,
                skipped: 2,
            }
        );
        assert!(original_ids.iter().all(|id| store.tasks.contains_key(id)));
        assert!(expected_new_ids
            .iter()
            .all(|id| store.tasks.contains_key(id)));
        assert_eq!(store.tasks.len(), 4);
    }

    #[test]
    fn store_json_without_routines_defaults_to_an_empty_map() {
        let mut store = Store::new();
        store.upsert_routine(template(1, Recurrence::Daily));
        let mut value = serde_json::to_value(store).expect("store serializes");
        value
            .as_object_mut()
            .expect("store serializes as an object")
            .remove("routines");
        let legacy_json = serde_json::to_string(&value).expect("JSON serializes");
        assert!(!legacy_json.contains("routines"));

        let loaded: Store = serde_json::from_str(&legacy_json).expect("legacy store loads");

        assert!(loaded.routines().is_empty());
    }

    #[test]
    fn routine_store_methods_upsert_list_and_remove_by_id() {
        let mut store = Store::new();
        let first = template(1, Recurrence::Daily);
        assert_eq!(store.upsert_routine(first.clone()), None);
        assert_eq!(store.routines().get(&first.id), Some(&first));

        let replacement = RoutineTemplate {
            title: "replacement".to_string(),
            category: None,
            ..first.clone()
        };
        assert_eq!(
            store.upsert_routine(replacement.clone()),
            Some(first.clone())
        );
        assert_eq!(store.remove_routine(first.id), Some(replacement));
        assert!(store.routines().is_empty());
    }
}
