use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::str::FromStr;

use chrono::{DateTime, Duration, NaiveDate, Utc, Weekday};
use clap::{Args, Parser, Subcommand, ValueEnum};
use gcal::{export_plan, import_from_calendar, CalendarTransport, GoogleCalendarTransport};
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
    #[arg(long, required_if_eq("planner", "ollama"))]
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
                    let model = args
                        .model
                        .expect("clap requires --model when --planner ollama");
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
                    "{}  {}  {}  {}  {}  {}s  {}",
                    short_id(routine.id),
                    routine.title,
                    routine.category.as_deref().unwrap_or(""),
                    tier_name(routine.tier),
                    routine.start_time,
                    routine.duration.num_seconds(),
                    recurrence_summary(&routine.recurrence)
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
            let color_map = load_color_map(args.color_config.as_deref())?;
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
            let color_to_category = load_color_map(args.color_config.as_deref())?
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

fn print_replan(output: logic::ReplanOutput) {
    println!("Schedule:");
    for entry in output.schedule {
        println!(
            "{}–{}  {}  {} ({})",
            entry.window.start.format("%H:%M"),
            entry.window.end.format("%H:%M"),
            entry.title,
            entry.category.as_deref().unwrap_or(""),
            short_id(entry.id)
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
