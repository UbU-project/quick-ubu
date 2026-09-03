//! Pure recurrence expansion into deterministic pinned tasks.

use std::collections::BTreeSet;

use chrono::{Datelike, Days, Duration, NaiveDate, NaiveTime, TimeZone, Utc, Weekday, WeekdaySet};
use chrono_tz::Tz;
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use uuid::Uuid;

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
    pub recurrence: Recurrence,
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
