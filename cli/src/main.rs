// Only test builds replace terminal and file I/O with the memory harness.
#[cfg(test)]
macro_rules! print {
    ($($arg:tt)*) => { crate::test_support::print(format_args!($($arg)*)) };
}
#[cfg(test)]
macro_rules! println {
    () => { crate::test_support::print(format_args!("\n")) };
    ($($arg:tt)*) => { crate::test_support::print(format_args!("{}\n", format_args!($($arg)*))) };
}
#[cfg(test)]
macro_rules! eprintln {
    ($($arg:tt)*) => { crate::test_support::eprint(format_args!("{}\n", format_args!($($arg)*))) };
}
#[cfg(test)]
mod test_support;
#[cfg(test)]
#[path = "../tests/commands.rs"]
mod command_tests;

use std::collections::BTreeMap;
#[cfg(not(test))]
use std::fs;
#[cfg(test)]
use test_support::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use chrono::{DateTime, NaiveDate, Utc, Weekday};
use clap::{Args, Parser, Subcommand, ValueEnum};
use gcal::{
    calendar_import_window, default_category_colors, export_plan, fetch_import_events,
    import_from_calendar, GoogleCalendarTransport,
};
use ollama_planner::{OllamaHttpTransport, OllamaPlanner};
use ubu_core::{
    generate_routine_tasks_with_daily_start, re_plan, AffectBudget, ComputeTarget,
    DeterministicPlacer, Planner, Recurrence, RoutineTemplate, TaskStatus, Tier, Tz,
};

mod clarify;
mod logic;
mod persist;
mod watch;
use persist::{SqliteBackend, StorageBackend};

#[derive(Debug, Parser)]
#[command(name = "quick-ubu")]
struct Cli {
    #[arg(long, default_value = "quick-ubu-store.db", global = true)]
    store: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Add(AddArgs),
    List,
    Done { prefix: String },
    /// Report stored time by category, including transparent tasks.
    Report(ReportArgs),
    Defer { prefix: String },
    DepAdd {
        task: String,
        blocker: String,
    },
    DepRm {
        task: String,
        blocker: String,
    },
    DepSet {
        task: String,
        blockers: Vec<String>,
    },
    DepList {
        task: Option<String>,
    },
    /// Start a dynamic task after another task's scheduled end plus an offset.
    AfterAdd {
        task: String,
        ref_task: String,
        #[arg(allow_negative_numbers = true)]
        offset_minutes: i64,
    },
    AfterRm {
        task: String,
        ref_task: String,
    },
    AfterList {
        task: Option<String>,
    },
    PrefAdd {
        a: String,
        b: String,
        #[arg(long)]
        eq: bool,
    },
    PrefRm {
        a: String,
        b: String,
    },
    PrefList,
    Review,
    Prioritize,
    SetModel { name: String },
    /// Persist a category's Google Calendar event colorId.
    SetColor { category: String, color_id: String },
    /// List category colors with persisted overrides applied.
    ColorList,
    /// Interview the operator about a task using EDITOR (or VISUAL).
    Clarify {
        prefix: String,
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value_t = 5)]
        max_rounds: usize,
        #[arg(long, default_value_t = 20)]
        history: usize,
    },
    SuggestTags {
        #[arg(long)]
        model: Option<String>,
        /// Number of recent tagged completions to include as context (0 disables).
        #[arg(long, default_value_t = 20)]
        history: usize,
        /// Maximum active tasks per classifier call.
        #[arg(long, default_value = "25")]
        batch_size: std::num::NonZeroUsize,
        /// Select tasks with this exact category.
        #[arg(long)]
        category: Option<String>,
        /// Maximum tasks to select, after filtering and category ordering.
        #[arg(long)]
        limit: Option<usize>,
        /// Select only tasks without tags.
        #[arg(long)]
        untagged: bool,
    },
    Advise {
        #[arg(long)]
        model: Option<String>,
        /// Number of recent tagged completions to include as context (0 disables).
        #[arg(long, default_value_t = 20)]
        history: usize,
        /// Maximum active tasks per classifier call.
        #[arg(long, default_value = "25")]
        batch_size: std::num::NonZeroUsize,
        /// Select tasks with this exact category.
        #[arg(long)]
        category: Option<String>,
        /// Maximum tasks to select, after filtering and category ordering.
        #[arg(long)]
        limit: Option<usize>,
        /// Select tasks carrying this exact tag; advice relates tasks within each batch only.
        #[arg(long)]
        tag: Option<String>,
        #[arg(long, default_value = "http://localhost:11434")]
        ollama_url: String,
        /// Seconds from request start to the first complete Ollama stream message.
        #[arg(long, default_value_t = 300)]
        ollama_timeout: u64,
        /// Maximum seconds for the entire Ollama generation, including startup.
        #[arg(long, default_value_t = 900)]
        ollama_total_timeout: u64,
    },
    ObjectiveAdd(ObjectiveAddArgs),
    Replan(ReplanArgs),
    Next(NextArgs),
    RoutineImport { path: PathBuf },
    RoutineList,
    Generate(GenerateArgs),
    Export(ExportArgs),
    Import(ImportArgs),
    /// Poll Calendar and import, replan, and export when events change.
    Watch(WatchArgs),
}

#[derive(Debug, Args)]
struct AddArgs {
    #[arg(long)]
    title: String,
    #[arg(long)]
    duration: i64,
    #[arg(long, default_value = "user-shared")]
    tier: String,
    #[arg(long, default_value_t = 0)]
    affect: i32,
    #[arg(long)]
    due: Option<String>,
    #[arg(long)]
    earliest_start: Option<String>,
    /// Hard completion ceiling for a dynamic task (RFC3339).
    #[arg(long)]
    must_finish_by: Option<String>,
    #[arg(long)]
    pin: Option<String>,
    #[arg(long)]
    category: Option<String>,
    #[arg(long)]
    transparent: bool,
    /// Popup reminder minutes before start (0 = at start); repeat for multiple reminders.
    #[arg(long = "reminder", allow_negative_numbers = true)]
    reminders: Vec<i32>,
    #[arg(long)]
    objective: Vec<String>,
    #[arg(long)]
    blocked_by: Vec<String>,
}

#[derive(Debug, Args)]
struct ObjectiveAddArgs {
    #[arg(long)]
    title: String,
    #[arg(long, default_value = "user-shared")]
    tier: String,
    #[arg(long)]
    target_date: Option<String>,
}

#[derive(Debug, Args)]
struct ReplanArgs {
    #[arg(long)]
    horizon: Option<String>,
    #[arg(long, default_value_t = 100)]
    affect_cap: i32,
    #[arg(long, value_enum, default_value_t = PlannerChoice::Deterministic)]
    planner: PlannerChoice,
    #[arg(long, default_value = "http://localhost:11434")]
    ollama_url: String,
    #[arg(long)]
    model: Option<String>,
    /// Seconds from request start to the first complete Ollama stream message.
    #[arg(long, default_value_t = 300)]
    ollama_timeout: u64,
    /// Maximum seconds for the entire Ollama generation, including startup.
    #[arg(long, default_value_t = 900)]
    ollama_total_timeout: u64,
}

#[derive(Debug, Args)]
struct ReportArgs {
    /// Inclusive start date at midnight UTC (YYYY-MM-DD).
    #[arg(long)]
    from: Option<String>,
    /// Inclusive end date at midnight UTC (YYYY-MM-DD).
    #[arg(long)]
    to: Option<String>,
    /// Default lookback from now; --from overrides the start.
    #[arg(long, default_value_t = 7)]
    days: u64,
}

#[derive(Debug, Args)]
struct NextArgs {
    #[arg(long, default_value_t = 100)]
    affect_cap: i32,
}

#[derive(Debug, Args)]
struct GenerateArgs {
    #[arg(long)]
    from: Option<String>,
    #[arg(long, default_value_t = 7)]
    days: u32,
    #[arg(long, default_value = "America/New_York")]
    tz: String,
}

#[derive(Debug, Args)]
struct ExportArgs {
    #[arg(long, default_value = "primary")]
    calendar_id: String,
    #[arg(long, default_value = "credentials.json")]
    credentials: PathBuf,
    #[arg(long, default_value = "token-cache.json")]
    token_cache: PathBuf,
    #[arg(long)]
    color_config: Option<PathBuf>,
}

#[derive(Debug, Args)]
struct WatchArgs {
    #[arg(long, default_value_t = 60, value_parser = clap::value_parser!(u64).range(1..))]
    interval: u64,
    #[command(flatten)]
    calendar: ExportArgs,
}

#[derive(Debug, Args)]
struct ImportArgs {
    #[arg(long, default_value = "primary")]
    calendar_id: String,
    #[arg(long, default_value = "credentials.json")]
    credentials: PathBuf,
    #[arg(long, default_value = "token-cache.json")]
    token_cache: PathBuf,
    /// RFC3339 lower bound; import always looks back at least 24 hours.
    #[arg(long)]
    from: Option<String>,
    /// RFC3339 upper bound; import always looks ahead at least one calendar month.
    #[arg(long)]
    to: Option<String>,
    #[arg(long)]
    color_config: Option<PathBuf>,
}

#[derive(Clone, Debug, ValueEnum)]
enum PlannerChoice {
    Deterministic,
    Ollama,
}

fn main() -> ExitCode {
    match run(Cli::parse()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("quick-ubu: {error}");
            ExitCode::FAILURE
        }
    }
}

fn run(cli: Cli) -> Result<(), String> {
    let backend = SqliteBackend::open(&cli.store)?;
    run_with_backend(cli, &backend)
}

fn print_classifier_report(report: &logic::BatchReport) {
    println!(
        "batches run {}, failed {}, enqueued {}, dropped_known {}, dropped_cycle {}",
        report.batches_run,
        report.failed_batches,
        report.totals.enqueued,
        report.totals.dropped_known,
        report.totals.dropped_cycle
    );
}

fn run_with_backend(cli: Cli, backend: &dyn StorageBackend) -> Result<(), String> {
    let mut store = backend.load()?;

    match cli.command {
        Command::Add(args) => {
            let id = logic::add(
                &mut store,
                logic::AddInput {
                    title: args.title,
                    duration_minutes: args.duration,
                    tier: logic::parse_tier(&args.tier)?,
                    affect_cost: args.affect,
                    due: parse_optional_datetime(args.due)?,
                    earliest_start: parse_optional_datetime(args.earliest_start)?,
                    must_finish_by: parse_optional_datetime(args.must_finish_by)?,
                    pin: parse_optional_datetime(args.pin)?,
                    category: args.category,
                    transparent: args.transparent,
                    reminders: args.reminders,
                    objective_prefixes: args.objective,
                    blocked_by_prefixes: args.blocked_by,
                },
            )?;
            backend.save(&store)?;
            println!("{id}");
        }
        Command::List => {
            for row in logic::list(&store) {
                let due = row
                    .due
                    .map(|date| format!("  due {}", date.to_rfc3339()))
                    .unwrap_or_default();
                println!(
                    "{}  {}  {}  {}m  aff:{}{}  {}",
                    short_id(row.id),
                    task_status_name(&row.status),
                    tier_name(row.tier),
                    row.duration_minutes,
                    row.affect_cost,
                    due,
                    row.title
                );
            }
        }
        Command::Done { prefix } => {
            logic::done(&mut store, &prefix, Utc::now())?;
            backend.save(&store)?;
        }
        Command::Report(args) => {
            let window = logic::report_window(
                Utc::now(),
                args.from.as_deref(),
                args.to.as_deref(),
                args.days,
            )?;
            let totals = ubu_core::report_by_category(&store, window.start, window.end);
            print!("{}", logic::format_category_report(&totals));
        }
        Command::Defer { prefix } => {
            logic::defer(&mut store, &prefix)?;
            backend.save(&store)?;
        }
        Command::DepAdd { task, blocker } => {
            logic::dep_add(&mut store, &task, &blocker)?;
            backend.save(&store)?;
        }
        Command::DepRm { task, blocker } => {
            logic::dep_rm(&mut store, &task, &blocker)?;
            backend.save(&store)?;
        }
        Command::DepSet { task, blockers } => {
            logic::dep_set(&mut store, &task, blockers)?;
            backend.save(&store)?;
        }
        Command::DepList { task } => {
            for (task_id, title, blockers) in logic::dep_list(&store, task)? {
                println!("{task_id}  {title}  [{}]", blockers.join(", "));
            }
        }
        Command::AfterAdd { task, ref_task, offset_minutes } => {
            logic::after_add(&mut store, &task, &ref_task, offset_minutes)?;
            backend.save(&store)?;
        }
        Command::AfterRm { task, ref_task } => {
            logic::after_rm(&mut store, &task, &ref_task)?;
            backend.save(&store)?;
        }
        Command::AfterList { task } => {
            for line in logic::after_list(&store, task)? {
                println!("{line}");
            }
        }
        Command::PrefAdd { a, b, eq } => {
            logic::pref_add(&mut store, &a, &b, eq)?;
            backend.save(&store)?;
        }
        Command::PrefRm { a, b } => {
            logic::pref_rm(&mut store, &a, &b)?;
            backend.save(&store)?;
        }
        Command::PrefList => {
            for line in logic::pref_list(&store) {
                println!("{line}");
            }
        }
        Command::Review => {
            review_decisions(&mut store)?;
            backend.save(&store)?;
        }
        Command::Prioritize => {
            let added = logic::enqueue_incomparable_pairs(&mut store);
            println!("enqueued {added}");
            review_decisions(&mut store)?;
            backend.save(&store)?;
        }
        Command::ObjectiveAdd(args) => {
            let id = logic::objective_add(
                &mut store,
                logic::ObjectiveAddInput {
                    title: args.title,
                    tier: logic::parse_tier(&args.tier)?,
                    target_date: parse_optional_datetime(args.target_date)?,
                },
            );
            backend.save(&store)?;
            println!("{id}");
        }
        Command::SetModel { name } => {
            logic::set_model(&mut store, name);
            backend.save(&store)?;
        }
        Command::SetColor { category, color_id } => {
            store.set_category_color(category, color_id);
            backend.save(&store)?;
        }
        Command::ColorList => {
            for (category, color_id) in effective_color_map(&store, None)? {
                println!("{category}  {color_id}");
            }
        }
        Command::Clarify {
            prefix,
            model,
            max_rounds,
            history,
        } => {
            let task_id = persist::resolve_task_id(&store, &prefix)?;
            let model = logic::resolve_model(&store, model)?;
            let transport = OllamaHttpTransport {
                base_url: "http://localhost:11434".into(),
                model,
                timeout_secs: 300,
                total_timeout_secs: 900,
            };
            let history = ubu_core::recent_completed_examples(&store, history);
            let report = clarify::clarify_task(
                &mut store,
                task_id,
                &transport,
                &mut clarify::EditorCollector,
                &history,
                max_rounds,
            )?;
            backend.save(&store)?;
            println!(
                "clarified {task_id}; enqueued {}, dropped_known {}, dropped_cycle {}",
                report.enqueued, report.dropped_known, report.dropped_cycle
            );
        }
        Command::SuggestTags {
            model,
            history,
            batch_size,
            category,
            limit,
            untagged,
        } => {
            let resolved_model = logic::resolve_model(&store, model)?;
            let transport = OllamaHttpTransport {
                base_url: "http://localhost:11434".into(),
                model: resolved_model.clone(),
                timeout_secs: 300,
                total_timeout_secs: 900,
            };
            let history = ubu_core::recent_completed_examples(&store, history);
            let filter = logic::TaskFilter {
                untagged,
                category,
                limit,
                tag: None,
            };
            let selected = logic::select_active_tasks(&store, &filter);
            let report = logic::classify_batches(
                &mut store,
                &selected,
                batch_size.get(),
                |store, chunk| {
                    logic::suggest_tags(
                        store,
                        chunk,
                        &transport,
                        Some(resolved_model.clone()),
                        &history,
                    )
                },
            )?;
            backend.save(&store)?;
            print_classifier_report(&report);
        }
        Command::Advise {
            model,
            history,
            batch_size,
            category,
            limit,
            tag,
            ollama_url,
            ollama_timeout,
            ollama_total_timeout,
        } => {
            let resolved_model = logic::resolve_model(&store, model)?;
            let transport = OllamaHttpTransport {
                base_url: ollama_url,
                model: resolved_model.clone(),
                timeout_secs: ollama_timeout,
                total_timeout_secs: ollama_total_timeout,
            };
            let history = ubu_core::recent_completed_examples(&store, history);
            let filter = logic::TaskFilter {
                category,
                tag,
                limit,
                untagged: false,
            };
            let selected = logic::select_active_tasks(&store, &filter);
            let report = logic::classify_batches(
                &mut store,
                &selected,
                batch_size.get(),
                |store, chunk| {
                    logic::advise(
                        store,
                        chunk,
                        &transport,
                        Some(resolved_model.clone()),
                        &history,
                    )
                },
            )?;
            backend.save(&store)?;
            print_classifier_report(&report);
        }
        Command::Replan(args) => {
            let now = Utc::now();
            let horizon = match args.horizon {
                Some(value) => logic::parse_datetime(&value)?,
                None => now,
            };
            let output = match args.planner {
                PlannerChoice::Deterministic => {
                    logic::replan(&store, now, horizon, args.affect_cap)
                }
                PlannerChoice::Ollama => {
                    let model = logic::resolve_model(&store, args.model)?;
                    let planner = OllamaPlanner::new(OllamaHttpTransport {
                        base_url: args.ollama_url,
                        model,
                        timeout_secs: args.ollama_timeout,
                        total_timeout_secs: args.ollama_total_timeout,
                    });
                    logic::replan_with_planner(
                        &store,
                        now,
                        horizon,
                        args.affect_cap,
                        &planner as &dyn Planner,
                    )
                }
            }
            .map_err(|error| format!("replan failed: {error:?}"))?;
            print_replan(output);
        }
        Command::Next(args) => {
            let now = Utc::now();
            let output = logic::next(&store, now, args.affect_cap)
                .map_err(|error| format!("next failed: {error:?}"))?;
            print_next(output);
        }
        Command::RoutineImport { path } => {
            let contents = fs::read_to_string(&path)
                .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
            let routines: Vec<RoutineTemplate> = serde_json::from_str(&contents)
                .map_err(|error| format!("failed to parse {}: {error}", path.display()))?;
            let imported = routines.len();
            for routine in routines {
                store.upsert_routine(routine);
            }
            backend.save(&store)?;
            println!("imported {imported}");
        }
        Command::RoutineList => {
            for routine in store.routines().values() {
                println!(
                    "{}  {}  {}  {}  {}  {}  {}s  {}{}",
                    short_id(routine.id),
                    routine.title,
                    routine.category.as_deref().unwrap_or(""),
                    transparency_marker(routine.transparent),
                    tier_name(routine.tier),
                    routine.start_time,
                    routine.duration.num_seconds(),
                    recurrence_summary(&routine.recurrence),
                    reminder_marker(&routine.reminders)
                );
            }
        }
        Command::Generate(args) => {
            let tz = Tz::from_str(&args.tz).map_err(|_| format!("invalid timezone {}", args.tz))?;
            let from = match args.from {
                Some(value) => NaiveDate::parse_from_str(&value, "%Y-%m-%d")
                    .map_err(|error| format!("invalid date {value}: {error}"))?,
                None => Utc::now().with_timezone(&tz).date_naive(),
            };
            let report =
                generate_routine_tasks_with_daily_start(&mut store, from, args.days, tz);
            backend.save(&store)?;
            println!("created {}, skipped {}", report.created, report.skipped);
        }
        Command::Export(args) => {
            let now = Utc::now();
            let plan = re_plan(
                &store,
                ComputeTarget::DesktopOllama,
                now,
                now,
                &[],
                &AffectBudget { cap: 100 },
                &DeterministicPlacer,
            )
            .map_err(|error| format!("export planning failed: {error:?}"))?;
            let color_map = effective_color_map(&store, args.color_config.as_deref())?;
            let transport =
                GoogleCalendarTransport::new(args.credentials, args.token_cache, args.calendar_id);
            let runtime = tokio::runtime::Runtime::new()
                .map_err(|error| format!("failed to start async runtime: {error}"))?;
            let report = runtime.block_on(export_plan(
                &mut store,
                &plan,
                &transport,
                &color_map,
                Tier::UserShared,
            ))?;
            backend.save(&store)?;
            println!("created {}, updated {}", report.created, report.updated);
        }
        Command::Watch(args) => {
            let now = Utc::now();
            let window = calendar_import_window(now, None, None)?;
            let color_map = effective_color_map(&store, args.calendar.color_config.as_deref())?;
            let config = watch::WatchConfig::new(window, color_map);
            let transport = GoogleCalendarTransport::new(
                args.calendar.credentials, args.calendar.token_cache, args.calendar.calendar_id,
            );
            let runtime = tokio::runtime::Runtime::new()
                .map_err(|error| format!("failed to start async runtime: {error}"))?;
            runtime.block_on(watch::run(&mut store, backend, &transport, &config,
                std::time::Duration::from_secs(args.interval)));
        }
        Command::Import(args) => {
            let now = Utc::now();
            let from = args
                .from
                .as_deref()
                .map(logic::parse_datetime)
                .transpose()?;
            let to = args.to.as_deref().map(logic::parse_datetime).transpose()?;
            let window = calendar_import_window(now, from, to)?;
            // Duplicate colors resolve to the alphabetically last category.
            let color_to_category = effective_color_map(&store, args.color_config.as_deref())?
                .into_iter()
                .map(|(category, color_id)| (color_id, category))
                .collect();
            let transport =
                GoogleCalendarTransport::new(args.credentials, args.token_cache, args.calendar_id);
            let runtime = tokio::runtime::Runtime::new()
                .map_err(|error| format!("failed to start async runtime: {error}"))?;
            let fetched = runtime.block_on(fetch_import_events(&store, &transport, &window))?;
            let report = import_from_calendar(
                &mut store,
                &fetched.events,
                &fetched.deleted,
                now,
                Tier::UserShared,
                &color_to_category,
            );
            backend.save(&store)?;
            println!(
                "captured {}, completed {}, reopened {}, moved {}, resized {}, removed {}",
                report.captured, report.completed, report.reopened, report.moved, report.resized, report.removed
            );
        }
    }

    Ok(())
}

fn review_decisions(store: &mut ubu_core::Store) -> Result<(), String> {
    if store.pending_decisions.is_empty() {
        return Ok(());
    }
    let now = Utc::now();
    let plan = match re_plan(
        store,
        ComputeTarget::DesktopOllama,
        now,
        now,
        &[],
        &AffectBudget { cap: 100 },
        &DeterministicPlacer,
    ) {
        Ok(plan) => Some(plan),
        Err(error) => {
            eprintln!("review planning failed: {error:?}; using uniform review order");
            None
        }
    };
    let seed = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;
    let decision_ids = logic::shuffled_pending_ids(store, seed, plan.as_ref(), now);
    #[cfg(not(test))]
    let stdin = io::stdin();
    #[cfg(test)]
    let stdin = test_support::stdin();

    for decision_id in decision_ids {
        let Some(decision) = store
            .pending_decisions
            .iter()
            .find(|decision| decision.id == decision_id)
            .cloned()
        else {
            continue;
        };
        print_decision(store, &decision);

        loop {
            let prompt = match &decision.proposal {
                ubu_core::Proposal::Preference { .. } => {
                    "[a] A ≻ B, [b] B ≻ A, [e] indifferent, [s] skip, [q] quit: "
                }
                ubu_core::Proposal::Dependency { .. } | ubu_core::Proposal::Tag { .. } => "[c] confirm, [r] reject, [q] quit: ",
            };
            print!("{prompt}");
            io::stdout()
                .flush()
                .map_err(|error| format!("failed to flush review prompt: {error}"))?;
            let mut input = String::new();
            let bytes_read = stdin
                .read_line(&mut input)
                .map_err(|error| format!("failed to read review answer: {error}"))?;
            if bytes_read == 0 {
                return Ok(());
            }
            let answer = input.trim().to_ascii_lowercase();
            if answer == "q" {
                return Ok(());
            }
            let parsed = match (&decision.proposal, answer.as_str()) {
                (ubu_core::Proposal::Preference { .. }, "a") => Some(logic::Answer::AStrictB),
                (ubu_core::Proposal::Preference { .. }, "b") => Some(logic::Answer::BStrictA),
                (ubu_core::Proposal::Preference { .. }, "e") => Some(logic::Answer::Indifferent),
                (ubu_core::Proposal::Preference { .. }, "s") => Some(logic::Answer::Skip),
                (ubu_core::Proposal::Dependency { .. } | ubu_core::Proposal::Tag { .. }, "c") => Some(logic::Answer::Confirm),
                (ubu_core::Proposal::Dependency { .. } | ubu_core::Proposal::Tag { .. }, "r") => Some(logic::Answer::Reject),
                _ => None,
            };
            let Some(answer) = parsed else {
                println!("invalid answer");
                continue;
            };

            match logic::resolve_decision(store, decision_id, answer) {
                Ok(resolution) => println!("{resolution:?}"),
                Err(error) => println!("error: {error}"),
            }
            break;
        }
    }

    Ok(())
}

fn print_decision(store: &ubu_core::Store, decision: &ubu_core::PendingDecision) {
    match &decision.proposal {
        ubu_core::Proposal::Tag { task_id, tag } => println!(
            "Tag: {} → {:?}", decision_task_label(store, *task_id), tag
        ),
        ubu_core::Proposal::Preference { a, b, suggested } => {
            println!(
                "Preference: {} vs {}",
                decision_task_label(store, *a),
                decision_task_label(store, *b)
            );
            if let Some(suggested) = suggested {
                let suggestion = match suggested {
                    ubu_core::PrefSuggestion::AStrictB => "A ≻ B",
                    ubu_core::PrefSuggestion::BStrictA => "B ≻ A",
                    ubu_core::PrefSuggestion::Indifferent => "indifferent",
                };
                println!("Suggestion: {suggestion}");
            }
        }
        ubu_core::Proposal::Dependency { blocked, blocker } => println!(
            "Dependency: {} blocked by {}",
            decision_task_label(store, *blocked),
            decision_task_label(store, *blocker)
        ),
    }
}

fn decision_task_label(store: &ubu_core::Store, task_id: ubu_core::Id) -> String {
    let title = store
        .tasks
        .get(&task_id)
        .map(|task| task.title.as_str())
        .unwrap_or("<unknown>");
    format!("{title} ({})", short_id(task_id))
}

fn parse_optional_datetime(value: Option<String>) -> Result<Option<DateTime<Utc>>, String> {
    value.as_deref().map(logic::parse_datetime).transpose()
}

fn load_color_map(path: Option<&Path>) -> Result<BTreeMap<String, String>, String> {
    let Some(path) = path else {
        return Ok(BTreeMap::new());
    };
    let contents = fs::read_to_string(path)
        .map_err(|error| format!("failed to read {}: {error}", path.display()))?;
    serde_json::from_str(&contents)
        .map_err(|error| format!("failed to parse {}: {error}", path.display()))
}

fn effective_color_map(
    store: &ubu_core::Store,
    color_config: Option<&Path>,
) -> Result<BTreeMap<String, String>, String> {
    let mut colors = default_category_colors();
    colors.extend(store.category_colors.clone());
    colors.extend(load_color_map(color_config)?);
    Ok(colors)
}

fn print_replan(output: logic::ReplanOutput) {
    println!("Schedule:");
    for entry in output.schedule {
        println!(
            "{}–{}  {}  {}  {} ({}){}",
            entry.window.start.format("%H:%M"),
            entry.window.end.format("%H:%M"),
            entry.title,
            entry.category.as_deref().unwrap_or(""),
            transparency_marker(entry.transparent),
            short_id(entry.id),
            reminder_marker(&entry.reminders)
        );
    }

    println!("Objective ETAs:");
    for objective in output.objective_etas {
        let eta = objective
            .eta
            .map(|datetime| datetime.to_rfc3339())
            .unwrap_or_else(|| "unscheduled".to_string());
        println!("{} → {eta}", objective.title);
    }

    println!("Conflicts:");
    for conflict in output.conflicts {
        println!(
            "{} ({}): {}",
            conflict.title,
            short_id(conflict.id),
            conflict.reason
        );
    }
}

fn print_next(output: Option<logic::ScheduleRow>) {
    match output {
        Some(entry) => println!(
            "{}–{}  {} ({})",
            entry.window.start.to_rfc3339(),
            entry.window.end.to_rfc3339(),
            entry.title,
            short_id(entry.id)
        ),
        None => println!("nothing ready"),
    }
}

fn recurrence_summary(recurrence: &Recurrence) -> String {
    match recurrence {
        Recurrence::Daily => "Daily".to_string(),
        Recurrence::MonthlyFirstWorkday => "MonthlyFirstWorkday".to_string(),
        Recurrence::QuarterlyFirstWorkday => "QuarterlyFirstWorkday".to_string(),
        Recurrence::Weekly { weekdays } => format!(
            "Weekly[{}]",
            weekdays
                .iter(Weekday::Mon)
                .map(|weekday| format!("{weekday:?}"))
                .collect::<Vec<_>>()
                .join(",")
        ),
        Recurrence::MonthlyDay { days } => format!(
            "MonthlyDay[{}]",
            days.iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(",")
        ),
    }
}

fn reminder_marker(reminders: &[i32]) -> String {
    if reminders.is_empty() {
        String::new()
    } else {
        format!(
            "  reminders:[{}]m",
            reminders.iter().map(i32::to_string).collect::<Vec<_>>().join(",")
        )
    }
}

fn short_id(id: ubu_core::Id) -> String {
    id.simple().to_string()[..8].to_string()
}

fn tier_name(tier: Tier) -> &'static str {
    match tier {
        Tier::SemiPublic => "semi-public",
        Tier::UserShared => "user-shared",
        Tier::TopSecret => "top-secret",
    }
}

fn task_status_name(status: &TaskStatus) -> &'static str {
    match status {
        TaskStatus::Backlog => "backlog",
        TaskStatus::Scheduled => "scheduled",
        TaskStatus::Active => "active",
        TaskStatus::Done => "done",
        TaskStatus::Deferred => "deferred",
    }
}

fn transparency_marker(transparent: bool) -> &'static str {
    if transparent {
        "transparent"
    } else {
        ""
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn watch_parses_default_and_custom_calendar_options_without_io() {
        let cli = Cli::try_parse_from(["quick-ubu", "watch"]).unwrap();
        let Command::Watch(args) = cli.command else {
            panic!("expected watch");
        };
        assert_eq!(args.interval, 60);
        assert_eq!(args.calendar.calendar_id, "primary");
        assert_eq!(args.calendar.credentials, PathBuf::from("credentials.json"));
        assert_eq!(args.calendar.token_cache, PathBuf::from("token-cache.json"));
        assert!(args.calendar.color_config.is_none());
        let cli = Cli::try_parse_from([
            "quick-ubu",
            "watch",
            "--interval",
            "5",
            "--credentials",
            "auth.json",
            "--token-cache",
            "cache.json",
            "--calendar-id",
            "test",
            "--color-config",
            "colors.json",
        ])
        .unwrap();
        let Command::Watch(args) = cli.command else {
            panic!("expected watch");
        };
        assert_eq!(args.interval, 5);
        assert_eq!(args.calendar.calendar_id, "test");
        assert_eq!(args.calendar.credentials, PathBuf::from("auth.json"));
        assert_eq!(args.calendar.token_cache, PathBuf::from("cache.json"));
        assert_eq!(
            args.calendar.color_config,
            Some(PathBuf::from("colors.json"))
        );
        for invalid in ["0", "-1", "invalid"] {
            assert!(Cli::try_parse_from(["quick-ubu", "watch", "--interval", invalid]).is_err());
        }
    }

    #[test]
    fn store_defaults_to_sqlite_and_accepts_an_explicit_path() {
        assert_eq!(
            Cli::try_parse_from(["quick-ubu", "list"]).unwrap().store,
            PathBuf::from("quick-ubu-store.db")
        );
        assert_eq!(
            Cli::try_parse_from(["quick-ubu", "--store", "custom.db", "list"])
                .unwrap().store,
            PathBuf::from("custom.db")
        );
    }

    #[test]
    fn report_command_parses_defaults_and_overrides_without_io() {
        let cli = Cli::try_parse_from(["quick-ubu", "report"]).unwrap();
        let Command::Report(args) = cli.command else {
            panic!("expected report")
        };
        assert_eq!(args.days, 7);
        assert_eq!(args.from, None);
        assert_eq!(args.to, None);
        let cli = Cli::try_parse_from([
            "quick-ubu",
            "report",
            "--days",
            "14",
            "--from",
            "2026-08-01",
            "--to",
            "2026-09-01",
        ])
        .unwrap();
        let Command::Report(args) = cli.command else {
            panic!("expected report")
        };
        assert_eq!(args.days, 14);
        assert_eq!(args.from.as_deref(), Some("2026-08-01"));
        assert_eq!(args.to.as_deref(), Some("2026-09-01"));
        assert!(Cli::try_parse_from(["quick-ubu", "report", "--days", "-1"]).is_err());
    }

    #[test]
    fn effective_colors_include_every_legacy_default() {
        let colors = effective_color_map(&ubu_core::Store::new(), None).unwrap();
        let expected = [
            ("personal", "3"),
            ("relationship", "5"),
            ("business", "6"),
            ("committed", "11"),
            ("location", "8"),
            ("entertainment", "1"),
            ("grocery", "2"),
            ("commute", "7"),
            ("undefined", "4"),
            ("education_house", "10"),
            ("work", "9"),
        ];
        assert_eq!(colors.len(), expected.len());
        for (category, color) in expected {
            assert_eq!(colors[category], color);
        }
    }

    #[test]
    fn effective_colors_overlay_store_then_file_per_category() {
        let mut store = ubu_core::Store::new();
        store.set_category_color("personal".into(), "5".into());
        store.set_category_color("custom".into(), "8".into());
        let persisted = effective_color_map(&store, None).unwrap();
        assert_eq!(persisted["personal"], "5");
        assert_eq!(persisted["work"], "9");
        assert_eq!(persisted["custom"], "8");

        let path = PathBuf::from("memory").join(format!("gc-4-colors-{}.json", uuid::Uuid::new_v4()));
        fs::write(&path, r#"{"personal":"7","file_only":"2"}"#).unwrap();
        let colors = effective_color_map(&store, Some(&path)).unwrap();
        assert_eq!(colors["personal"], "7");
        assert_eq!(colors["work"], "9");
        assert_eq!(colors["custom"], "8");
        assert_eq!(colors["file_only"], "2");
        assert_eq!(store.category_colors["personal"], "5");

        fs::write(&path, "invalid JSON").unwrap();
        assert!(effective_color_map(&store, Some(&path))
            .unwrap_err()
            .contains("failed to parse"));
        fs::remove_file(&path).unwrap();
        assert!(effective_color_map(&store, Some(&path))
            .unwrap_err()
            .contains("failed to read"));
    }

    #[test]
    fn legacy_store_without_category_colors_loads_empty_and_uses_defaults() {
        let path = PathBuf::from("memory").join(format!("gc-4-legacy-{}.json", uuid::Uuid::new_v4()));
        fs::write(
            &path,
            r#"{"objectives":{},"tasks":{},"bundles":{},"preferences":[],"log":[]}"#,
        )
        .unwrap();
        let mut store = persist::load(&path).unwrap();
        assert!(store.category_colors.is_empty());
        assert_eq!(
            effective_color_map(&store, None).unwrap(),
            default_category_colors()
        );

        store.set_category_color("personal".into(), "7".into());
        persist::save(&path, &store).unwrap();
        assert_eq!(
            persist::load(&path).unwrap().category_colors["personal"],
            "7"
        );
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn import_captures_inverse_categories_from_effective_colors_without_http() {
        let now = DateTime::from_timestamp(0, 0).unwrap();
        let event = gcal::FetchedEvent {
            id: "colored-event".into(),
            summary: "Routine".into(),
            color_id: Some("3".into()),
            start: now,
            end: now + chrono::Duration::minutes(30),
            transparent: false,
        };
        let path = PathBuf::from("memory").join(format!("gc-4-import-{}.json", uuid::Uuid::new_v4()));
        fs::write(&path, r#"{"work":"9","relationship":"3"}"#).unwrap();
        for (layer, expected) in [(0, "personal"), (1, "work"), (2, "relationship")] {
            let mut store = ubu_core::Store::new();
            if layer > 0 {
                store.set_category_color("work".into(), "3".into());
            }
            let inverse = effective_color_map(&store, (layer == 2).then_some(path.as_path()))
                .unwrap()
                .into_iter()
                .map(|(category, color)| (color, category))
                .collect();
            let report = import_from_calendar(
                &mut store,
                std::slice::from_ref(&event),
                &[],
                now,
                Tier::UserShared,
                &inverse,
            );
            assert_eq!(report.captured, 1);
            let captured = store.tasks.values().next().unwrap();
            assert_eq!(captured.category.as_deref(), Some(expected));
            assert!(captured.pinned.is_some());
        }
        fs::remove_file(path).unwrap();
    }

    #[test]
    fn advisor_and_replan_parse_optional_model_and_transport_settings_without_http() {
        let cli = Cli::try_parse_from(["quick-ubu", "advise"]).unwrap();
        match cli.command {
            Command::Advise {
                model,
                ollama_url,
                ollama_timeout,
                ollama_total_timeout,
                ..
            } => {
                assert_eq!(model, None);
                assert_eq!(ollama_url, "http://localhost:11434");
                assert_eq!(ollama_timeout, 300);
                assert_eq!(ollama_total_timeout, 900);
            }
            _ => panic!("expected advise"),
        }
        let cli = Cli::try_parse_from([
            "quick-ubu",
            "advise",
            "--model",
            "override",
            "--ollama-url",
            "http://unused.invalid",
            "--ollama-timeout",
            "9",
            "--ollama-total-timeout",
            "18",
        ])
        .unwrap();
        match cli.command {
            Command::Advise {
                model,
                ollama_url,
                ollama_timeout,
                ollama_total_timeout,
                ..
            } => {
                assert_eq!(model.as_deref(), Some("override"));
                assert_eq!(ollama_url, "http://unused.invalid");
                assert_eq!(ollama_timeout, 9);
                assert_eq!(ollama_total_timeout, 18);
            }
            _ => panic!("expected advise"),
        }
        for model_args in [vec![], vec!["--model", "override"]] {
            let mut args = vec!["quick-ubu", "replan", "--planner", "ollama"];
            args.extend(model_args.iter().copied());
            let cli = Cli::try_parse_from(args).unwrap();
            match cli.command {
                Command::Replan(args) => {
                    assert_eq!(args.model.as_deref(), model_args.get(1).copied());
                    assert_eq!(args.ollama_timeout, 300);
                    assert_eq!(args.ollama_total_timeout, 900);
                }
                _ => panic!("expected replan"),
            }
        }
    }
}
