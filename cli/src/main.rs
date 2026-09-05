use std::collections::BTreeMap;
use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

use chrono::{DateTime, Duration, NaiveDate, Utc, Weekday};
use clap::{Args, Parser, Subcommand, ValueEnum};
use gcal::{
    default_category_colors, export_plan, import_from_calendar, CalendarTransport,
    GoogleCalendarTransport,
};
use ollama_planner::{OllamaHttpTransport, OllamaPlanner};
use ubu_core::{
    generate_routine_tasks, re_plan, AffectBudget, ComputeTarget, DeterministicPlacer, Planner,
    Recurrence, RoutineTemplate, TaskStatus, Tier, Tz,
};

mod logic;
mod persist;

#[derive(Debug, Parser)]
#[command(name = "quick-ubu")]
struct Cli {
    #[arg(long, default_value = "quick-ubu-store.json", global = true)]
    store: PathBuf,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Add(AddArgs),
    List,
    Done { prefix: String },
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
    Advise {
        #[arg(long)]
        model: Option<String>,
        #[arg(long, default_value = "http://localhost:11434")]
        ollama_url: String,
        #[arg(long, default_value_t = 30)]
        ollama_timeout: u64,
    },
    ObjectiveAdd(ObjectiveAddArgs),
    Replan(ReplanArgs),
    Next(NextArgs),
    RoutineImport { path: PathBuf },
    RoutineList,
    Generate(GenerateArgs),
    Export(ExportArgs),
    Import(ImportArgs),
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
    #[arg(long, default_value_t = 30)]
    ollama_timeout: u64,
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
struct ImportArgs {
    #[arg(long, default_value = "primary")]
    calendar_id: String,
    #[arg(long, default_value = "credentials.json")]
    credentials: PathBuf,
    #[arg(long, default_value = "token-cache.json")]
    token_cache: PathBuf,
    #[arg(long)]
    from: Option<String>,
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
    let mut store = persist::load(&cli.store)?;

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
                    pin: parse_optional_datetime(args.pin)?,
                    category: args.category,
                    transparent: args.transparent,
                    reminders: args.reminders,
                    objective_prefixes: args.objective,
                    blocked_by_prefixes: args.blocked_by,
                },
            )?;
            persist::save(&cli.store, &store)?;
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
            logic::done(&mut store, &prefix)?;
            persist::save(&cli.store, &store)?;
        }
        Command::Defer { prefix } => {
            logic::defer(&mut store, &prefix)?;
            persist::save(&cli.store, &store)?;
        }
        Command::DepAdd { task, blocker } => {
            logic::dep_add(&mut store, &task, &blocker)?;
            persist::save(&cli.store, &store)?;
        }
        Command::DepRm { task, blocker } => {
            logic::dep_rm(&mut store, &task, &blocker)?;
            persist::save(&cli.store, &store)?;
        }
        Command::DepSet { task, blockers } => {
            logic::dep_set(&mut store, &task, blockers)?;
            persist::save(&cli.store, &store)?;
        }
        Command::DepList { task } => {
            for (task_id, title, blockers) in logic::dep_list(&store, task)? {
                println!("{task_id}  {title}  [{}]", blockers.join(", "));
            }
        }
        Command::PrefAdd { a, b, eq } => {
            logic::pref_add(&mut store, &a, &b, eq)?;
            persist::save(&cli.store, &store)?;
        }
        Command::PrefRm { a, b } => {
            logic::pref_rm(&mut store, &a, &b)?;
            persist::save(&cli.store, &store)?;
        }
        Command::PrefList => {
            for line in logic::pref_list(&store) {
                println!("{line}");
            }
        }
        Command::Review => {
            review_decisions(&mut store)?;
            persist::save(&cli.store, &store)?;
        }
        Command::Prioritize => {
            let added = logic::enqueue_incomparable_pairs(&mut store);
            println!("enqueued {added}");
            review_decisions(&mut store)?;
            persist::save(&cli.store, &store)?;
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
            persist::save(&cli.store, &store)?;
            println!("{id}");
        }
        Command::SetModel { name } => {
            logic::set_model(&mut store, name);
            persist::save(&cli.store, &store)?;
        }
        Command::SetColor { category, color_id } => {
            store.set_category_color(category, color_id);
            persist::save(&cli.store, &store)?;
        }
        Command::ColorList => {
            for (category, color_id) in effective_color_map(&store, None)? {
                println!("{category}  {color_id}");
            }
        }
        Command::Advise {
            model,
            ollama_url,
            ollama_timeout,
        } => {
            let resolved_model = logic::resolve_model(&store, model)?;
            let transport = OllamaHttpTransport {
                base_url: ollama_url,
                model: resolved_model.clone(),
                timeout_secs: ollama_timeout,
            };
            let report = logic::advise(&mut store, &transport, Some(resolved_model))?;
            persist::save(&cli.store, &store)?;
            println!(
                "enqueued {}, dropped_known {}, dropped_cycle {}",
                report.enqueued, report.dropped_known, report.dropped_cycle
            );
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
            persist::save(&cli.store, &store)?;
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
            let report = generate_routine_tasks(&mut store, from, args.days, tz);
            persist::save(&cli.store, &store)?;
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
            persist::save(&cli.store, &store)?;
            println!("created {}, updated {}", report.created, report.updated);
        }
        Command::Import(args) => {
            let now = Utc::now();
            let from = args
                .from
                .as_deref()
                .map(logic::parse_datetime)
                .transpose()?
                .unwrap_or(now);
            let to = args
                .to
                .as_deref()
                .map(logic::parse_datetime)
                .transpose()?
                .unwrap_or(now + Duration::days(7));
            // Duplicate colors resolve to the alphabetically last category.
            let color_to_category = effective_color_map(&store, args.color_config.as_deref())?
                .into_iter()
                .map(|(category, color_id)| (color_id, category))
                .collect();
            let transport =
                GoogleCalendarTransport::new(args.credentials, args.token_cache, args.calendar_id);
            let runtime = tokio::runtime::Runtime::new()
                .map_err(|error| format!("failed to start async runtime: {error}"))?;
            let events = runtime.block_on(transport.list_events(from, to))?;
            let report = import_from_calendar(
                &mut store,
                &events,
                now,
                Tier::UserShared,
                &color_to_category,
            );
            persist::save(&cli.store, &store)?;
            println!(
                "captured {}, completed {}, moved {}",
                report.captured, report.completed, report.moved
            );
        }
    }

    Ok(())
}

fn review_decisions(store: &mut ubu_core::Store) -> Result<(), String> {
    let decision_ids = store
        .pending_decisions
        .iter()
        .map(|decision| decision.id)
        .collect::<Vec<_>>();
    let stdin = io::stdin();

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
                ubu_core::Proposal::Dependency { .. } => "[c] confirm, [r] reject, [q] quit: ",
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
                (ubu_core::Proposal::Dependency { .. }, "c") => Some(logic::Answer::Confirm),
                (ubu_core::Proposal::Dependency { .. }, "r") => Some(logic::Answer::Reject),
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

        let path = std::env::temp_dir().join(format!("gc-4-colors-{}.json", uuid::Uuid::new_v4()));
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
        let path = std::env::temp_dir().join(format!("gc-4-legacy-{}.json", uuid::Uuid::new_v4()));
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
            end: now + Duration::minutes(30),
            transparent: false,
        };
        let path = std::env::temp_dir().join(format!("gc-4-import-{}.json", uuid::Uuid::new_v4()));
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
            } => {
                assert_eq!(model, None);
                assert_eq!(ollama_url, "http://localhost:11434");
                assert_eq!(ollama_timeout, 30);
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
        ])
        .unwrap();
        match cli.command {
            Command::Advise {
                model,
                ollama_url,
                ollama_timeout,
            } => {
                assert_eq!(model.as_deref(), Some("override"));
                assert_eq!(ollama_url, "http://unused.invalid");
                assert_eq!(ollama_timeout, 9);
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
                }
                _ => panic!("expected replan"),
            }
        }
    }
}
