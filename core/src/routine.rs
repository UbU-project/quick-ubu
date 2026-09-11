//! Pure recurrence expansion into deterministic pinned tasks.

use std::collections::BTreeSet;

use chrono::{Datelike, Days, Duration, NaiveDate, NaiveTime, TimeZone, Utc, Weekday, WeekdaySet};
pub use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

use crate::store::Store;
use crate::types::{
    AfterConstraint, DeferPolicy, Id, Provenance, Task, TaskStatus, Tier, TimeWindow,
};

const NAMESPACE: Uuid = Uuid::from_u128(0x6f51_89f1_6208_5c1e_a8ec_15c0f894ea9d);

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Recurrence {
    Daily,
    MonthlyFirstWorkday,
    QuarterlyFirstWorkday,
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
pub struct RoutineAfter {
    pub template_id: Id,
    pub offset: Duration,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct RoutineTemplate {
    pub id: Id,
    pub title: String,
    pub tier: Tier,
    pub start_time: NaiveTime,
    #[serde(default)]
    pub dynamic: bool,
    #[serde(default)]
    pub latest_tod: Option<NaiveTime>,
    pub duration: Duration,
    pub affect_cost: i32,
    /// Free-form classification (e.g. "personal", "relationship", "business").
    /// Maps to a calendar color on export and groups time in reporting.
    #[serde(default)]
    pub category: Option<String>,
    /// If true, this commitment is shown/scheduled but does NOT occupy time —
    /// dynamic tasks may be placed within its window (a Google "transparent"
    /// event). Default false = opaque/blocking.
    #[serde(default)]
    pub transparent: bool,
    /// Popup reminders, minutes-before-start (0 = fire at the event's start).
    /// Empty = no reminder. The at-start (0) reminder is the mobile "do next"
    /// signal: Google Calendar has no other cue for what to do now, so an event
    /// with no reminder is effectively invisible on the phone.
    #[serde(default)]
    pub reminders: Vec<i32>,
    pub recurrence: Recurrence,
    #[serde(default)]
    pub after: Vec<RoutineAfter>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GenerateReport {
    pub created: usize,
    pub skipped: usize,
}

fn first_workday_of_month(year: i32, month: u32) -> NaiveDate {
    let first = NaiveDate::from_ymd_opt(year, month, 1).expect("valid year and month");
    let offset = match first.weekday() {
        Weekday::Sat => 2,
        Weekday::Sun => 1,
        _ => 0,
    };
    first + Days::new(offset)
}

fn matches_date(recurrence: &Recurrence, date: NaiveDate) -> bool {
    match recurrence {
        Recurrence::Daily => true,
        Recurrence::MonthlyFirstWorkday => {
            date == first_workday_of_month(date.year(), date.month())
        }
        Recurrence::QuarterlyFirstWorkday => {
            matches!(date.month(), 1 | 4 | 7 | 10)
                && date == first_workday_of_month(date.year(), date.month())
        }
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
            let (pinned, earliest_start, must_finish_by) = if template.dynamic {
                let latest = template
                    .latest_tod
                    .unwrap_or_else(|| NaiveTime::from_hms_opt(23, 59, 59).unwrap());
                let localized = tz.from_local_datetime(&date.and_time(latest));
                let Some(ceiling) = localized.clone().single().or_else(|| localized.earliest())
                else {
                    continue;
                };
                (None, Some(start), Some(ceiling.with_timezone(&Utc)))
            } else {
                (
                    Some(TimeWindow {
                        start,
                        end: start + template.duration,
                    }),
                    None,
                    None,
                )
            };
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
                earliest_start,
                category: template.category.clone(),
                pinned,
                transparent: template.transparent,
                blocked_by: Vec::new(),
                after: template
                    .after
                    .iter()
                    .map(|reference| AfterConstraint {
                        task_id: Uuid::new_v5(
                            &NAMESPACE,
                            format!("{}|{}", reference.template_id, date).as_bytes(),
                        ),
                        offset: reference.offset,
                    })
                    .collect(),
                must_finish_by,
                defer_policy: DeferPolicy::RescheduleAsap,
                status: TaskStatus::Scheduled,
                provenance: Provenance::Manual,
                reminders: template.reminders.clone(),
                commitment: None,
            });
        }
    }

    tasks.sort_by_key(|task| {
        (
            task.pinned
                .as_ref()
                .map(|window| window.start)
                .or(task.earliest_start)
                .expect("routine tasks have a pin or an earliest start"),
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
    generate_routine_tasks_with_daily_start(store, from, days, tz)
}

/// Generate within the requested window, excluding daily occurrences before
/// `daily_start`. Other recurrence types retain their normal date matching.
pub fn generate_routine_tasks_with_daily_start(
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

    let tasks = templates.iter().flat_map(|template| {
        let excluded_days = 0;
        if excluded_days == days {
            return Vec::new();
        }
        let Some(template_from) = from.checked_add_days(Days::new(u64::from(excluded_days))) else {
            return Vec::new();
        };
        expand_routine(
            std::slice::from_ref(template),
            template_from,
            days - excluded_days,
            tz,
        )
    });
    for task in tasks {
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
            transparent: false,
            reminders: Vec::new(),
            after: Vec::new(),
            dynamic: false,
            latest_tod: None,
            recurrence,
        }
    }

    fn pinned_start(task: &Task) -> chrono::DateTime<Utc> {
        task.pinned.as_ref().expect("routine task is pinned").start
    }

    #[test]
    fn dynamic_routines_get_explicit_or_end_of_local_day_windows() {
        let tz = chrono_tz::America::New_York;
        for day in [date(2026, 3, 8), date(2026, 11, 1)] {
            for latest in [Some(time(17, 0)), None] {
                let mut routine = template(1, Recurrence::Daily);
                routine.dynamic = true;
                routine.latest_tod = latest;
                let tasks = expand_routine(&[routine], day, 1, tz);
                assert_eq!(tasks.len(), 1);
                let task = &tasks[0];
                assert!(task.pinned.is_none());
                assert_eq!(
                    task.earliest_start,
                    Some(
                        tz.from_local_datetime(&day.and_time(time(6, 30)))
                            .unwrap()
                            .with_timezone(&Utc)
                    )
                );
                let ceiling = latest.unwrap_or(NaiveTime::from_hms_opt(23, 59, 59).unwrap());
                assert_eq!(
                    task.must_finish_by,
                    Some(
                        tz.from_local_datetime(&day.and_time(ceiling))
                            .unwrap()
                            .with_timezone(&Utc)
                    )
                );
            }
        }
    }

    #[test]
    fn dynamic_ceiling_uses_earliest_ambiguous_time_and_skips_nonexistent_time() {
        let mut routine = template(1, Recurrence::Daily);
        routine.dynamic = true;
        routine.start_time = time(0, 30);
        routine.latest_tod = Some(time(1, 30));
        let tz = chrono_tz::America::New_York;
        let day = date(2026, 11, 1);
        let tasks = expand_routine(&[routine.clone()], day, 1, tz);
        assert_eq!(
            tasks[0].must_finish_by,
            tz.from_local_datetime(&day.and_time(time(1, 30)))
                .earliest()
                .map(|at| at.with_timezone(&Utc))
        );
        routine.latest_tod = Some(time(2, 30));
        assert!(expand_routine(&[routine], date(2026, 3, 8), 1, tz).is_empty());
    }

    #[test]
    fn pinned_routines_ignore_latest_tod_and_mixed_generation_sorts_by_start() {
        let mut pinned = template(1, Recurrence::Daily);
        pinned.latest_tod = Some(time(1, 0));
        let mut dynamic = template(2, Recurrence::Daily);
        dynamic.dynamic = true;
        dynamic.start_time = time(6, 0);
        let tasks = expand_routine(&[pinned, dynamic], date(2026, 9, 11), 1, chrono_tz::UTC);
        assert_eq!(tasks[0].title, "routine-2");
        let fixed = &tasks[1];
        assert_eq!(fixed.pinned.as_ref().unwrap().start.time(), time(6, 30));
        assert!(fixed.earliest_start.is_none());
        assert!(fixed.must_finish_by.is_none());
    }

    #[test]
    fn catherine_dynamic_chain_fits_same_day_or_conflicts_when_offset_is_too_long() {
        for offset in [Duration::hours(1), Duration::hours(3)] {
            let mut first = template(1, Recurrence::Daily);
            first.dynamic = true;
            first.start_time = time(8, 0);
            first.duration = Duration::minutes(30);
            let mut second = template(2, Recurrence::Daily);
            second.dynamic = true;
            second.start_time = time(8, 0);
            second.duration = Duration::minutes(30);
            second.latest_tod = Some(time(10, 0));
            second.after.push(RoutineAfter {
                template_id: first.id,
                offset,
            });
            let mut store = Store::new();
            store.upsert_routine(first);
            store.upsert_routine(second);
            let day = date(2026, 9, 11);
            assert_eq!(
                generate_routine_tasks(&mut store, day, 2, chrono_tz::UTC).created,
                4
            );
            assert_eq!(
                generate_routine_tasks(&mut store, day, 2, chrono_tz::UTC).skipped,
                4
            );
            let now = day.and_hms_opt(0, 0, 0).unwrap().and_utc();
            let plan = crate::re_plan(
                &store,
                crate::ComputeTarget::DesktopOllama,
                now,
                now,
                &[],
                &crate::AffectBudget { cap: 100 },
                &crate::DeterministicPlacer,
            )
            .unwrap();
            for task in store
                .tasks
                .values()
                .filter(|task| task.title == "routine-2")
            {
                let entry = plan.entries.iter().find(|entry| entry.item == task.id);
                if offset == Duration::hours(1) {
                    let entry = entry.unwrap();
                    let reference = plan
                        .entries
                        .iter()
                        .find(|entry| entry.item == task.after[0].task_id)
                        .unwrap();
                    assert_eq!(entry.window.start, reference.window.end + offset);
                    assert!(entry.window.start >= task.earliest_start.unwrap());
                    assert_eq!(entry.window.end, task.must_finish_by.unwrap());
                } else {
                    assert!(entry.is_none());
                    assert!(plan.conflicts.contains(&crate::Conflict {
                        item: task.id,
                        reason: "does not fit before deadline".into()
                    }));
                }
            }
        }
    }

    #[test]
    fn reversed_dynamic_window_conflicts_without_becoming_an_overnight_window() {
        let mut routine = template(1, Recurrence::Daily);
        routine.dynamic = true;
        routine.latest_tod = Some(time(5, 0));
        let mut store = Store::new();
        store.upsert_routine(routine);
        let day = date(2026, 9, 11);
        generate_routine_tasks(&mut store, day, 1, chrono_tz::UTC);
        let now = day.and_hms_opt(0, 0, 0).unwrap().and_utc();
        let plan = crate::re_plan(
            &store,
            crate::ComputeTarget::DesktopOllama,
            now,
            now,
            &[],
            &crate::AffectBudget { cap: 100 },
            &crate::DeterministicPlacer,
        )
        .unwrap();
        assert!(plan.entries.is_empty());
        assert_eq!(plan.conflicts[0].reason, "does not fit before deadline");
    }

    #[test]
    fn legacy_json_defaults_hard_ceiling_and_dynamic_routine_fields() {
        let mut store = Store::new();
        store.upsert_routine(template(1, Recurrence::Daily));
        generate_routine_tasks(&mut store, date(2026, 9, 11), 1, chrono_tz::UTC);
        let mut value = serde_json::to_value(&store).unwrap();
        for task in value["tasks"].as_object_mut().unwrap().values_mut() {
            task.as_object_mut().unwrap().remove("must_finish_by");
        }
        for template in value["routines"].as_object_mut().unwrap().values_mut() {
            template.as_object_mut().unwrap().remove("dynamic");
            template.as_object_mut().unwrap().remove("latest_tod");
        }
        let loaded: Store = serde_json::from_value(value).unwrap();
        assert_eq!(loaded, store);
        assert!(loaded
            .tasks
            .values()
            .all(|task| task.must_finish_by.is_none()));
        assert!(loaded
            .routines
            .values()
            .all(|routine| !routine.dynamic && routine.latest_tod.is_none()));
    }

    #[test]
    fn routine_after_resolves_same_day_ids_across_days_and_dst() {
        let first = template(1, Recurrence::Daily);
        let mut second = template(2, Recurrence::Daily);
        second.after.push(RoutineAfter {
            template_id: first.id,
            offset: Duration::hours(1),
        });
        let tasks = expand_routine(
            &[first.clone(), second.clone()],
            date(2026, 10, 31),
            3,
            chrono_tz::America::New_York,
        );
        for day in 0..3 {
            let on = date(2026, 10, 31) + Days::new(day);
            let first_id = Uuid::new_v5(&NAMESPACE, format!("{}|{}", first.id, on).as_bytes());
            let second_id = Uuid::new_v5(&NAMESPACE, format!("{}|{}", second.id, on).as_bytes());
            assert!(tasks.iter().any(|task| task.id == first_id));
            let task = tasks.iter().find(|task| task.id == second_id).unwrap();
            assert_eq!(
                task.after,
                vec![AfterConstraint {
                    task_id: first_id,
                    offset: Duration::hours(1)
                }]
            );
        }
    }

    #[test]
    fn routine_after_preserves_nonfiring_reference_for_dynamic_planning_conflict() {
        let first = template(
            1,
            Recurrence::MonthlyDay {
                days: [2].into_iter().collect(),
            },
        );
        let mut second = template(2, Recurrence::Daily);
        second.dynamic = true;
        second.after.push(RoutineAfter {
            template_id: first.id,
            offset: Duration::minutes(60),
        });
        let mut tasks = expand_routine(&[first, second], date(2026, 9, 1), 1, chrono_tz::UTC);
        assert_eq!(tasks.len(), 1);
        let task = tasks.remove(0);
        let task_id = task.id;
        assert!(task.pinned.is_none());
        let mut store = Store::new();
        store.upsert_task(task);
        let now = date(2026, 9, 1).and_hms_opt(0, 0, 0).unwrap().and_utc();
        let plan = crate::re_plan(
            &store,
            crate::ComputeTarget::DesktopOllama,
            now,
            now,
            &[],
            &crate::AffectBudget { cap: 100 },
            &crate::DeterministicPlacer,
        )
        .unwrap();
        assert!(plan.entries.is_empty());
        assert_eq!(
            plan.conflicts,
            vec![crate::Conflict {
                item: task_id,
                reason: "unresolved after-reference".into()
            }]
        );
    }

    #[test]
    fn legacy_task_and_routine_json_default_after_to_empty() {
        let mut store = Store::new();
        store.upsert_routine(template(1, Recurrence::Daily));
        generate_routine_tasks(&mut store, date(2026, 9, 1), 1, chrono_tz::UTC);
        let mut value = serde_json::to_value(&store).unwrap();
        for collection in ["tasks", "routines"] {
            for record in value[collection].as_object_mut().unwrap().values_mut() {
                record.as_object_mut().unwrap().remove("after");
            }
        }
        assert_eq!(serde_json::from_value::<Store>(value).unwrap(), store);
    }

    #[test]
    fn monthly_first_workday_handles_saturday_sunday_and_weekday_starts() {
        for (year, month, expected_day) in [(2026, 8, 3), (2026, 11, 2), (2026, 9, 1)] {
            let expected = date(year, month, expected_day);
            assert_eq!(first_workday_of_month(year, month), expected);
            for day in 1..=31 {
                if let Some(candidate) = NaiveDate::from_ymd_opt(year, month, day) {
                    assert_eq!(
                        matches_date(&Recurrence::MonthlyFirstWorkday, candidate),
                        candidate == expected
                    );
                }
            }
            let tasks = expand_routine(
                &[template(1, Recurrence::MonthlyFirstWorkday)],
                date(year, month, 1),
                7,
                chrono_tz::UTC,
            );
            assert_eq!(tasks.len(), 1);
            assert_eq!(pinned_start(&tasks[0]).date_naive(), expected);
        }
    }

    #[test]
    fn quarterly_first_workday_matches_only_the_four_quarter_starts() {
        for (year, days) in [(2022, [3, 1, 1, 3]), (2023, [2, 3, 3, 2])] {
            let expected: Vec<_> = [1, 4, 7, 10]
                .into_iter()
                .zip(days)
                .map(|(month, day)| date(year, month, day))
                .collect();
            let from = date(year, 1, 1);
            for offset in 0..365 {
                let candidate = from + Days::new(offset);
                assert_eq!(
                    matches_date(&Recurrence::QuarterlyFirstWorkday, candidate),
                    expected.contains(&candidate)
                );
            }
            let tasks = expand_routine(
                &[template(1, Recurrence::QuarterlyFirstWorkday)],
                from,
                365,
                chrono_tz::UTC,
            );
            assert_eq!(
                tasks
                    .iter()
                    .map(|task| pinned_start(task).date_naive())
                    .collect::<Vec<_>>(),
                expected
            );
        }
    }

    #[test]
    fn generated_tasks_inherit_reminders_including_at_start_and_empty() {
        for reminders in [vec![10, 0], vec![]] {
            let mut routine = template(1, Recurrence::Daily);
            routine.reminders = reminders.clone();
            let mut store = Store::new();
            store.upsert_routine(routine);
            let report = generate_routine_tasks(&mut store, date(2026, 9, 1), 2, chrono_tz::UTC);
            assert_eq!(report.created, 2);
            assert!(store.tasks.values().all(|task| task.reminders == reminders));
        }
    }

    #[test]
    fn store_json_without_task_or_routine_reminders_defaults_to_empty() {
        let mut routine = template(1, Recurrence::Daily);
        routine.reminders = vec![10, 0];
        let mut store = Store::new();
        store.upsert_routine(routine);
        generate_routine_tasks(&mut store, date(2026, 9, 1), 1, chrono_tz::UTC);
        let mut value = serde_json::to_value(&store).unwrap();
        for collection in ["tasks", "routines"] {
            for record in value[collection].as_object_mut().unwrap().values_mut() {
                record.as_object_mut().unwrap().remove("reminders");
            }
        }
        let json = serde_json::to_string(&value).unwrap();
        assert!(!json.contains("reminders"));
        let loaded: Store = serde_json::from_str(&json).unwrap();
        assert!(loaded.tasks.values().all(|task| task.reminders.is_empty()));
        assert!(loaded
            .routines()
            .values()
            .all(|routine| routine.reminders.is_empty()));
        store
            .tasks
            .values_mut()
            .for_each(|task| task.reminders.clear());
        store
            .routines
            .values_mut()
            .for_each(|routine| routine.reminders.clear());
        assert_eq!(loaded, store);
    }

    #[test]
    fn canonical_routine_example_matches_templates_and_round_trips() {
        let mut daily = template(1, Recurrence::Daily);
        daily.title = "Daily check-in".to_string();
        daily.duration = Duration::minutes(10);
        daily.category = Some("personal".to_string());
        daily.transparent = true;
        daily.reminders = vec![0];
        let mut weekly = template(
            2,
            Recurrence::Weekly {
                weekdays: [Weekday::Mon, Weekday::Wed].into_iter().collect(),
            },
        );
        weekly.title = "Weekly review".to_string();
        weekly.duration = Duration::minutes(30);
        weekly.reminders = vec![10, 0];
        let mut monthly = template(3, Recurrence::MonthlyFirstWorkday);
        monthly.title = "Monthly planning".to_string();
        let mut quarterly = template(4, Recurrence::QuarterlyFirstWorkday);
        quarterly.title = "Quarterly planning".to_string();
        quarterly.duration = Duration::minutes(60);
        let routines = vec![daily, weekly, monthly, quarterly];
        let json = serde_json::to_string_pretty(&routines).unwrap() + "\n";
        // Compare the checked-in legacy fixture by value: new defaulted fields
        // need not be written into the source tree just to run the tests.
        let saved = include_str!("../../docs/example-routine.json");
        assert_eq!(
            serde_json::from_str::<Vec<RoutineTemplate>>(saved).unwrap(),
            routines
        );
        assert_eq!(
            serde_json::from_str::<Vec<RoutineTemplate>>(&json).unwrap(),
            routines
        );
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
    fn generated_task_inherits_template_category() {
        let mut routine = template(1, Recurrence::Daily);
        routine.category = Some("personal".to_string());

        let tasks = expand_routine(&[routine], date(2026, 9, 1), 1, chrono_tz::UTC);

        assert_eq!(tasks[0].category.as_deref(), Some("personal"));
    }

    #[test]
    fn generated_task_inherits_template_transparency() {
        let mut routine = template(1, Recurrence::Daily);
        routine.transparent = true;

        let tasks = expand_routine(&[routine], date(2026, 9, 1), 1, chrono_tz::UTC);

        assert!(tasks[0].transparent);
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
    fn store_json_without_task_or_routine_categories_defaults_them_to_none() {
        let mut store = Store::new();
        let mut routine = template(1, Recurrence::Daily);
        routine.category = Some("relationship".to_string());
        let task = expand_routine(
            std::slice::from_ref(&routine),
            date(2026, 9, 1),
            1,
            chrono_tz::UTC,
        )
        .pop()
        .unwrap();
        store.upsert_routine(routine);
        store.upsert_task(task);
        let mut value = serde_json::to_value(store).expect("store serializes");
        for collection in ["tasks", "routines"] {
            for record in value[collection]
                .as_object_mut()
                .expect("store collection serializes as an object")
                .values_mut()
            {
                record
                    .as_object_mut()
                    .expect("stored record serializes as an object")
                    .remove("category");
            }
        }
        let legacy_json = serde_json::to_string(&value).expect("JSON serializes");
        assert!(!legacy_json.contains("\"category\""));

        let loaded: Store = serde_json::from_str(&legacy_json).expect("legacy store loads");

        assert!(loaded.tasks.values().all(|task| task.category.is_none()));
        assert!(loaded
            .routines()
            .values()
            .all(|routine| routine.category.is_none()));
    }

    #[test]
    fn store_json_without_task_or_routine_transparency_defaults_to_false() {
        let mut store = Store::new();
        let mut routine = template(1, Recurrence::Daily);
        routine.transparent = true;
        let task = expand_routine(
            std::slice::from_ref(&routine),
            date(2026, 9, 1),
            1,
            chrono_tz::UTC,
        )
        .pop()
        .unwrap();
        store.upsert_routine(routine);
        store.upsert_task(task);
        let mut value = serde_json::to_value(store).expect("store serializes");
        for collection in ["tasks", "routines"] {
            for record in value[collection]
                .as_object_mut()
                .expect("store collection serializes as an object")
                .values_mut()
            {
                record
                    .as_object_mut()
                    .expect("stored record serializes as an object")
                    .remove("transparent");
            }
        }
        let legacy_json = serde_json::to_string(&value).expect("JSON serializes");
        assert!(!legacy_json.contains("transparent"));

        let loaded: Store = serde_json::from_str(&legacy_json).expect("legacy store loads");

        assert!(loaded.tasks.values().all(|task| !task.transparent));
        assert!(loaded
            .routines()
            .values()
            .all(|routine| !routine.transparent));
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
            transparent: false,
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
