//! Deterministic soft placement behind the [`Planner`] boundary.

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, NaiveDate, Utc};

use crate::plan::{Conflict, ScheduleEntry};
use crate::precompute::AffectBudget;
use crate::types::{Id, TimeWindow};

#[derive(Debug, Clone, PartialEq)]
pub struct Placeable {
    pub task_id: Id,
    pub duration: Duration,
    pub affect_cost: i32,
    pub earliest_floor: DateTime<Utc>,
    pub due: Option<DateTime<Utc>>,
    pub sched_predecessors: Vec<Id>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlacementInput {
    pub items: Vec<Placeable>,
    pub fixed_occupied: Vec<TimeWindow>,
    pub budget: AffectBudget,
}

#[derive(Debug, Clone, PartialEq)]
pub struct PlacementOutput {
    pub entries: Vec<ScheduleEntry>,
    pub conflicts: Vec<Conflict>,
}

pub trait Planner {
    fn place(&self, input: &PlacementInput) -> PlacementOutput;
}

pub struct DeterministicPlacer;

impl Planner for DeterministicPlacer {
    fn place(&self, input: &PlacementInput) -> PlacementOutput {
        let mut occupied = input.fixed_occupied.clone();
        occupied.sort_by_key(|window| (window.start, window.end));

        let mut placed_end: BTreeMap<Id, DateTime<Utc>> = BTreeMap::new();
        let mut day_affect: BTreeMap<NaiveDate, i32> = BTreeMap::new();
        let mut entries = Vec::new();
        let mut conflicts = Vec::new();

        for item in &input.items {
            let mut start_floor = item.earliest_floor;
            let mut predecessor_unplaced = false;
            for predecessor in &item.sched_predecessors {
                if let Some(end) = placed_end.get(predecessor) {
                    start_floor = start_floor.max(*end);
                } else {
                    predecessor_unplaced = true;
                    break;
                }
            }
            if predecessor_unplaced {
                conflicts.push(Conflict {
                    item: item.task_id,
                    reason: "predecessor unplaced".to_string(),
                });
                continue;
            }

            if item.affect_cost > input.budget.cap {
                conflicts.push(Conflict {
                    item: item.task_id,
                    reason: "affect_cost exceeds daily budget".to_string(),
                });
                continue;
            }

            let mut start = start_floor;
            loop {
                start = earliest_gap(start, item.duration, &occupied);
                let day = start.date_naive();
                let load = day_affect.get(&day).copied().unwrap_or(0);
                if load + item.affect_cost <= input.budget.cap {
                    break;
                }
                start = next_day_start(day);
            }

            let end = start + item.duration;
            let window = TimeWindow { start, end };
            entries.push(ScheduleEntry {
                item: item.task_id,
                window: window.clone(),
                is_handle: false,
            });
            occupied.push(window);
            occupied.sort_by_key(|window| (window.start, window.end));
            placed_end.insert(item.task_id, end);
            // P-1 charges a task wholly to its start day, even across midnight.
            *day_affect.entry(start.date_naive()).or_insert(0) += item.affect_cost;

            if item.due.is_some_and(|due| end > due) {
                conflicts.push(Conflict {
                    item: item.task_id,
                    reason: "placed after due date".to_string(),
                });
            }
        }

        PlacementOutput { entries, conflicts }
    }
}

fn earliest_gap(
    mut candidate: DateTime<Utc>,
    duration: Duration,
    occupied: &[TimeWindow],
) -> DateTime<Utc> {
    loop {
        let end = candidate + duration;
        match occupied
            .iter()
            .find(|window| candidate < window.end && window.start < end)
        {
            Some(window) => candidate = candidate.max(window.end),
            None => return candidate,
        }
    }
}

fn next_day_start(day: NaiveDate) -> DateTime<Utc> {
    day.succ_opt()
        .and_then(|next| next.and_hms_opt(0, 0, 0))
        .expect("the placement horizon must fit chrono's date range")
        .and_utc()
}
