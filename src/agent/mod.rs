//! Native deterministic agent — port of the `agent/` Python package
//! (deterministic path; no LLM, no LangGraph).

pub mod anomaly;
pub mod builtin;
pub mod preview;
pub mod strategy;

use std::path::PathBuf;

use anyhow::{bail, Context, Result};

pub use strategy::Strategy;

/// Which agent the workflow drives — the typed replacement for the old
/// stringly `--agent` dispatch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentSpec {
    /// In-process native strategy (no subprocess).
    Builtin(Strategy),
    /// `python3 -B <script>`; agent dir defaults to the script's parent.
    Python(PathBuf, Option<PathBuf>),
    /// Any shell command via `sh -c`; agent dir defaults to the directory of
    /// the first path-like token, else the cwd.
    External(String, Option<PathBuf>),
}

/// A resolved agent: either a native strategy or a subprocess command plus
/// its working directory (also the `.env` source).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResolvedAgent {
    Builtin(Strategy),
    Subprocess { command: String, agent_dir: PathBuf },
}

impl ResolvedAgent {
    pub fn describe(&self) -> String {
        match self {
            ResolvedAgent::Builtin(strategy) => format!("builtin strategy={strategy:?}"),
            ResolvedAgent::Subprocess { command, agent_dir } => {
                format!("command={command:?} cwd={}", agent_dir.display())
            }
        }
    }
}

/// Single-quote a path for `sh -c`.
fn shell_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

impl AgentSpec {
    pub fn resolve(&self) -> Result<ResolvedAgent> {
        match self {
            AgentSpec::Builtin(strategy) => Ok(ResolvedAgent::Builtin(*strategy)),
            AgentSpec::Python(script, agent_dir) => {
                let script = script
                    .canonicalize()
                    .with_context(|| format!("agent script not found: {}", script.display()))?;
                if !script.is_file() {
                    bail!("agent script not found: {}", script.display());
                }
                let agent_dir = match agent_dir {
                    Some(dir) => dir.clone(),
                    None => script.parent().unwrap().to_path_buf(),
                };
                Ok(ResolvedAgent::Subprocess {
                    command: format!("python3 -B {}", shell_quote(&script.to_string_lossy())),
                    agent_dir,
                })
            }
            AgentSpec::External(command, agent_dir) => {
                if command.trim().is_empty() {
                    bail!("agent command cannot be empty");
                }
                let agent_dir = agent_dir
                    .clone()
                    .unwrap_or_else(|| default_external_agent_dir(command));
                Ok(ResolvedAgent::Subprocess {
                    command: command.clone(),
                    agent_dir,
                })
            }
        }
    }
}

/// Directory of the first token in the command that names an existing file.
fn default_external_agent_dir(command: &str) -> PathBuf {
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
