//! Sequential, pass-capped classifier operations with durable chunk boundaries.

use std::sync::atomic::{AtomicBool, Ordering};

use ollama_planner::LlmTransport;
use ubu_core::{recent_completed_examples, CompletedExample, Id, Store};

use crate::logic::{self, AdviseReport, Proposed, TaskFilter};

#[derive(Debug, PartialEq, Eq)]
pub enum BatchOutcome {
    Completed,
    Interrupted,
    Failed(String),
}

enum Proposals {
    Tags(Vec<(Id, String)>),
    Advice(Proposed),
}

struct Operation {
    name: &'static str,
    prompt_builder: fn(&Store, &[Id], &[CompletedExample]) -> (String, Vec<Id>),
    parser: fn(&str, &[Id]) -> Result<Proposals, String>,
    enqueuer: fn(&mut Store, Proposals) -> AdviseReport,
}

const OPERATIONS: &[Operation] = &[
    Operation {
        name: "tags",
        prompt_builder: logic::build_tag_prompt,
        parser: |text, ids| logic::parse_tag_proposals(text, ids).map(Proposals::Tags),
        enqueuer: |store, proposals| {
            let Proposals::Tags(tags) = proposals else {
                unreachable!("tags parser payload")
            };
            logic::filter_and_enqueue_tags(store, tags)
        },
    },
    Operation {
        name: "advise",
        prompt_builder: logic::build_advisor_prompt,
        parser: |text, ids| logic::parse_proposals(text, ids).map(Proposals::Advice),
        enqueuer: |store, proposals| {
            let Proposals::Advice(advice) = proposals else {
                unreachable!("advisor parser payload")
            };
            logic::filter_and_enqueue(store, advice)
        },
    },
];

#[derive(Default)]
struct Summary {
    tasks: usize,
    queued: usize,
    errors: usize,
}

fn save_progress(
    store: &Store,
    save: &mut dyn FnMut(&Store) -> Result<(), String>,
) -> Result<(), BatchOutcome> {
    save(store)
        .map_err(|error| BatchOutcome::Failed(format!("failed to save batch progress: {error}")))
}

fn check_interrupt(
    store: &Store,
    interrupted: &AtomicBool,
    save: &mut dyn FnMut(&Store) -> Result<(), String>,
) -> Result<(), BatchOutcome> {
    if interrupted.load(Ordering::SeqCst) {
        save_progress(store, save)?;
        return Err(BatchOutcome::Interrupted);
    }
    Ok(())
}

/// Make one pass per requested operation, up to each task's persisted pass cap.
/// Handled model errors count as attempts; storage/setup errors are fatal.
pub fn run_batch<T: LlmTransport>(
    store: &mut Store,
    transport: &T,
    ops: &[&str],
    pass_cap: u32,
    batch_size: usize,
    history_n: usize,
    interrupted: &AtomicBool,
    save: &mut dyn FnMut(&Store) -> Result<(), String>,
) -> BatchOutcome {
    if batch_size == 0 {
        return BatchOutcome::Failed("batch size must be greater than zero".into());
    }
    // Validate the complete operation list before making any changes or calls.
    let operations = ops
        .iter()
        .map(|name| {
            OPERATIONS
                .iter()
                .find(|op| op.name == *name)
                .ok_or_else(|| BatchOutcome::Failed(format!("unknown batch operation: {name}")))
        })
        .collect::<Result<Vec<_>, _>>();
    let operations = match operations {
        Ok(operations) => operations,
        Err(outcome) => return outcome,
    };
    if let Err(outcome) = check_interrupt(store, interrupted, save) {
        return outcome;
    }
    for op in operations {
        let mut summary = Summary::default();
        let outcome = (|| -> Result<(), BatchOutcome> {
            let eligible: Vec<_> = logic::select_active_tasks(store, &TaskFilter::default())
                .into_iter()
                .filter(|id| {
                    store
                        .batch_passes
                        .get(&format!("{id}|{}", op.name))
                        .copied()
                        .unwrap_or(0)
                        < pass_cap
                })
                .collect();
            for chunk in eligible.chunks(batch_size) {
                check_interrupt(store, interrupted, save)?;
                let history = recent_completed_examples(store, history_n);
                let (prompt, ids) = (op.prompt_builder)(store, chunk, &history);
                match transport
                    .generate(&prompt)
                    .and_then(|text| (op.parser)(&text, &ids))
                {
                    Ok(proposals) => summary.queued += (op.enqueuer)(store, proposals).enqueued,
                    Err(error) => {
                        summary.errors += 1;
                        eprintln!(
                            "batch {} failed for {} tasks: {error}; pass counted, continuing",
                            op.name,
                            chunk.len()
                        );
                    }
                }
                for id in chunk {
                    // Eligibility ensures the previous count is below pass_cap,
                    // including when pass_cap is u32::MAX.
                    *store
                        .batch_passes
                        .entry(format!("{id}|{}", op.name))
                        .or_default() += 1;
                }
                summary.tasks += chunk.len();
                save_progress(store, save)?;
            }
            // Also honor Ctrl-C during the last request or its save, even if no
            // further chunks remain. Never report completion with a set flag.
            check_interrupt(store, interrupted, save)
        })();
        println!(
            "batch {}: tasks processed {}, proposals queued {}, errors {}",
            op.name, summary.tasks, summary.queued, summary.errors
        );
        if let Err(outcome) = outcome {
            return outcome;
        }
    }
    match check_interrupt(store, interrupted, save) {
        Ok(()) => BatchOutcome::Completed,
        Err(outcome) => outcome,
    }
}

impl BatchOutcome {
    pub fn exit_code(&self) -> u8 {
        match self {
            Self::Completed => 0,
            Self::Interrupted => 130,
            Self::Failed(_) => 1,
        }
    }
}

#[cfg(test)]
#[path = "batch_tests.rs"]
mod tests;
