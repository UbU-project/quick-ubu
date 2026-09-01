use std::path::PathBuf;

use clap::{Args, Parser, Subcommand};

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
}

fn main() {
    let _ = Cli::parse();
}
