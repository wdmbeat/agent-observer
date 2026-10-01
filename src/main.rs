use std::path::PathBuf;

use anyhow::Result;
use clap::{Args, Parser, Subcommand};

use agent_observer::agent::{AgentSpec, Strategy};

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
    /// Run an agent against a scenario and score the trace
    #[command(after_long_help = RUN_EXAMPLES)]
    Run {
        /// Scenario directory containing config/ and outputs/reference/
        #[arg(long)]
        scenario: PathBuf,
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
        /// Which agent to drive
        #[command(subcommand)]
        agent: AgentCommand,
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

#[derive(Subcommand)]
enum AgentCommand {
    /// Native in-process Rust strategy (no subprocess)
    Rust(RustArgs),
    /// Run a Python agent script via `python3 -B <script>`
    Python(PythonArgs),
    /// Any shell command, spawned via `sh -c`
    External(ExternalArgs),
}

#[derive(Args)]
struct RustArgs {
    #[arg(value_enum)]
    strategy: Strategy,
}

#[derive(Args)]
struct PythonArgs {
    /// Path to the agent entry script (e.g. minimal_agent.py)
    script: PathBuf,
    /// Working directory for the agent and source of its .env
    /// (default: the script's parent directory)
    #[arg(long)]
    agent_dir: Option<PathBuf>,
}

#[derive(Args)]
struct ExternalArgs {
    /// Shell command string (e.g. "python3 /path/agent.py")
    command: String,
    /// Working directory for the agent and source of its .env (default:
    /// directory of the first path-like token in the command, else cwd)
    #[arg(long)]
    agent_dir: Option<PathBuf>,
}

impl AgentCommand {
    fn into_spec(self) -> AgentSpec {
        match self {
            AgentCommand::Rust(args) => AgentSpec::Builtin(args.strategy),
            AgentCommand::Python(args) => AgentSpec::Python(args.script, args.agent_dir),
            AgentCommand::External(args) => AgentSpec::External(args.command, args.agent_dir),
        }
    }
}

const RUN_EXAMPLES: &str = "\
Examples:
  agent-observer run --scenario <dir> --out out rust baseline
  agent-observer run --scenario <dir> --out out python tmp/agent-observer-starter-kit/agent/minimal_agent.py
  agent-observer run --scenario <dir> --out out external \"python3 /path/to/agent.py\" --agent-dir /path/to";

fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Commands::Run {
            scenario,
            out,
            wallclock,
            init_timeout,
            keep_initial_publication,
            quiet,
            agent,
        } => {
            let options = agent_observer::workflow::RunOptions {
                scenario,
                agent: agent.into_spec(),
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
