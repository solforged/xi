//! Benchmarks coding-agent harnesses on a task suite.

mod config;
mod metrics;
mod report;
mod runner;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

type Result<T, E = Box<dyn std::error::Error + Send + Sync>> = std::result::Result<T, E>;

#[derive(Parser)]
#[command(about = "Compare coding-agent harnesses on verified tasks")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Prove that checks fail before and pass after each reference patch.
    Verify {
        tasks: Vec<String>,
        #[arg(long, default_value = "bench/tasks")]
        tasks_dir: PathBuf,
    },
    /// Run each arm against fresh task copies.
    Run {
        #[arg(long, default_value = "bench/arms.toml")]
        arms: PathBuf,
        #[arg(long = "arm", num_args = 1..)]
        arm_names: Vec<String>,
        #[arg(long = "task", num_args = 1..)]
        task_names: Vec<String>,
        #[arg(long, default_value = "bench/tasks")]
        tasks_dir: PathBuf,
        #[arg(long)]
        model: String,
        #[arg(long, default_value_t = 1, value_parser = clap::value_parser!(u32).range(1..))]
        trials: u32,
        #[arg(long)]
        out: PathBuf,
    },
    /// Print aggregate and per-task Markdown tables.
    Report { dir: PathBuf },
}

#[tokio::main]
async fn main() {
    if let Err(error) = execute(Cli::parse()).await {
        eprintln!("xi-bench: {error}");
        std::process::exit(1);
    }
}

async fn execute(cli: Cli) -> Result<()> {
    match cli.command {
        Command::Verify { tasks, tasks_dir } => {
            runner::verify(&config::load_tasks(&tasks_dir, &tasks)?).await
        }
        Command::Run {
            arms,
            arm_names,
            task_names,
            tasks_dir,
            model,
            trials,
            out,
        } => {
            let tasks = config::load_tasks(&tasks_dir, &task_names)?;
            let arms = config::load_arms(&arms, &arm_names)?;
            runner::run(&tasks, &arms, &model, trials, &out).await
        }
        Command::Report { dir } => {
            print!("{}", report::render(&report::load(&dir)?));
            Ok(())
        }
    }
}
