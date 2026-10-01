//! Port of `agent/minimal_agent.py::run` — the standalone agent's persistent
//! JSON-Lines stdin/stdout loop. One platform envelope per line in, one
//! compact `decision_response` per line out; status goes to stderr. The
//! `sac-agent` binary (`src/bin/sac_agent.rs`) is this loop's `main`.

use std::io::{BufRead, Write};
use std::path::Path;

use anyhow::{Context, Result};
use serde_json::Value;

use crate::transport::load_dotenv;
use crate::workflow::DecisionProvider;

use super::decision_graph::{DeterministicAgent, LlmSelector, Selector};
use super::model_factory::{build_chat_model, ModelSettings};
use super::protocol::{self, MessageType};
use super::strategy;

/// JSON value rendering for the finish summary (Python `print` of the raw
/// payload values: strings bare, numbers as-is).
fn display(value: &Value) -> String {
    value.as_str().map(str::to_string).unwrap_or_else(|| value.to_string())
}

/// Consume platform envelopes and emit one response per decision request.
pub fn run() -> Result<()> {
    // .env in the working directory, without overriding real env vars
    // (Python: load_dotenv(override=False)).
    for (key, value) in load_dotenv(Path::new(".env")) {
        if std::env::var_os(&key).is_none() {
            std::env::set_var(&key, value);
        }
    }
    let settings = ModelSettings::from_environment()?;
    let (model, provider_status) = match build_chat_model(&settings) {
        Ok(Some(model)) => (Some(model), settings.provider.clone()),
        Ok(None) => (None, "deterministic".to_string()),
        Err(_) => (None, "deterministic fallback (ModelConfigurationError)".to_string()),
    };
    eprintln!("sac-agent provider={provider_status}");
    let stdin = std::io::stdin();
    let mut stdout = std::io::stdout().lock();
    let mut agent: Option<DeterministicAgent<Box<dyn Selector>>> = None;
    let mut decisions = 0u64;
    for line in stdin.lock().lines() {
        let line = line.context("reading platform message")?;
        if line.trim().is_empty() {
            continue;
        }
        let message: Value =
            serde_json::from_str(&line).context("platform message is not valid JSON")?;
        let (message_type, payload) = protocol::parse_platform_message(&message)?;
        match message_type {
            MessageType::Initialize => {
                let inner = strategy::standalone_selector_from_env();
                let selector: Box<dyn Selector> = match &model {
                    Some(model) => Box::new(LlmSelector::new(
                        model.clone(),
                        settings.top_k_candidates as usize,
                        inner,
                    )),
                    None => inner,
                };
                let mut next = DeterministicAgent::with_selector(selector);
                next.publish_initial(&payload)?;
                agent = Some(next);
            }
            MessageType::DecisionRequest => {
                let agent = agent
                    .as_mut()
                    .ok_or_else(|| anyhow::anyhow!("decision_request received before initialize"))?;
                let response = agent.decide(&payload)?;
                decisions += 1;
                serde_json::to_writer(&mut stdout, &response)?;
                stdout.write_all(b"\n")?;
                stdout.flush()?;
            }
            MessageType::Finish => {
                eprintln!(
                    "sac-agent finished: termination_reason={} decisions={decisions} last_decision_sequence={}",
                    display(&payload["termination_reason"]),
                    display(&payload["last_decision_sequence"]),
                );
                return Ok(());
            }
        }
    }
    Ok(())
}
