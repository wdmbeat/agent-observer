use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "agent-observer",
    version,
    about = "Rust port of the agent-observer telescope-survey competition kit"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Run an agent subprocess against a scenario (JSON-Lines transport)
    Run {
        /// Scenario directory containing config/ and outputs/reference/
        #[arg(long)]
        scenario: PathBuf,
        /// Agent command string, spawned via `sh -c` (e.g. "python3 /path/minimal_agent.py"),
        /// or the keyword `builtin` for the native deterministic agent (no subprocess)
        #[arg(long)]
        agent: String,
        /// Working directory for the agent and source of its .env (default:
        /// directory of the first path-like token in --agent, else cwd)
        #[arg(long)]
        agent_dir: Option<PathBuf>,
        /// Output directory for decisions.csv, workflow_result.json, score_report.json, agent.log
        #[arg(long)]
        out: PathBuf,
        /// Global wall-clock budget in seconds (default: workflow_config.json value)
        #[arg(long)]
        wallclock: Option<f64>,
        /// Seconds allowed for the agent to accept `initialize`
        #[arg(long, default_value = "30.0")]
        init_timeout: f64,
        /// Keep the full initialize payload inside workflow_result.json
        #[arg(long)]
        keep_initial_publication: bool,
        /// Print only the final JSON summary
        #[arg(long)]
        quiet: bool,
    },
    /// Score a decisions CSV against a scenario
    Score {
        /// Scenario directory containing config/ and outputs/reference/
        #[arg(long)]
        scenario: PathBuf,
        /// decisions.csv to score
        #[arg(long)]
        decisions: PathBuf,
        /// Write the score report here; stdout when omitted
        #[arg(long)]
        out: Option<PathBuf>,
        #[arg(long, default_value = "trace_complete")]
        termination_reason: String,
    },
}

/// Directory of the first token in the command that names an existing file.
fn default_agent_dir(command: &str) -> PathBuf {
    for token in command.split_whitespace() {
        let path = PathBuf::from(token);
        if path.is_file() {
            if let Some(parent) = path.parent() {
                return parent.to_path_buf();
            }
        }
    }
    PathBuf::from(".")
}

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run {
            scenario,
            agent,
            agent_dir,
            out,
            wallclock,
            init_timeout,
            keep_initial_publication,
            quiet,
        } => {
            let options = agent_observer::workflow::RunOptions {
                scenario,
                agent_dir: agent_dir.unwrap_or_else(|| default_agent_dir(&agent)),
                agent_command: agent,
                out_dir: out,
                wallclock,
                init_timeout,
                keep_initial_publication,
                quiet,
            };
            let outcome = agent_observer::workflow::run_local(&options)?;
            println!(
                "{}",
                agent_observer::scoring::dumps_report(&outcome.summary)
            );
            std::process::exit(outcome.exit_code);
        }
        Commands::Score {
            scenario,
            decisions,
            out,
            termination_reason,
        } => {
            let report = agent_observer::scoring::score_files(
                &scenario,
                &decisions,
                out.as_deref(),
                &termination_reason,
            )?;
            if out.is_none() {
                println!("{}", agent_observer::scoring::dumps_report(&report));
            }
        }
    }
    Ok(())
}
