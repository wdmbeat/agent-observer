//! Port of `agent/decision_graph.py` — the deterministic decision pipeline
//! (no LLM, no LangGraph) plus `minimal_agent.py`'s `MinimalDecisionAgent`
//! facade.
//!
//! The pipeline is fixed (detector.process_snapshot → fault-scope filter →
//! preview_actions → selector → detector suspect override → note_observation →
//! protocol envelope); only the candidate-selection step differs between
//! strategies. The `Selector` trait plays the role of the `choose_action`
//! seam: `my_strategy.rs` holds the baseline, `reference_strategy.rs` the
//! worked teaching example.

use std::time::Instant;

use anyhow::Result;
use serde_json::{json, Value};

use crate::workflow::DecisionProvider;

use super::my_strategy::BaselineSelector;
use super::protocol;
use super::scoring_preview::{preview_actions, CandidatePreview};
use super::state::RunState;

/// One picked action, mirroring the decision dict the graph's finalize stage
/// produces.
#[derive(Debug, Clone, PartialEq)]
pub enum Selection {
    Wait {
        reason: String,
        source: &'static str,
    },
    Observe {
        tile_id: String,
        program: String,
        request_id: String,
        reason: String,
        source: &'static str,
    },
}

/// The candidate-selection step of the deterministic pipeline. Called with the
/// ranked previews (possibly empty) and the (fault-filtered) snapshot.
pub trait Selector {
    fn select(
        &mut self,
        previews: &[CandidatePreview],
        snapshot: &Value,
        publication: &Value,
    ) -> Selection;
}

/// The deterministic agent facade: anomaly tracking plus one selection per
/// snapshot. `S` is the selection strategy.
pub struct DeterministicAgent<S> {
    state: RunState,
    selector: S,
}

impl<S: Default> Default for DeterministicAgent<S> {
    fn default() -> Self {
        Self { state: RunState::default(), selector: S::default() }
    }
}

/// The baseline in-process strategy (`rust baseline`).
pub type BuiltinAgent = DeterministicAgent<BaselineSelector>;

impl BuiltinAgent {
    pub fn new() -> Self {
        Self::default()
    }
}

impl<S> DeterministicAgent<S> {
    pub fn with_selector(selector: S) -> Self {
        Self { state: RunState::default(), selector }
    }
}

impl<S: Selector> DeterministicAgent<S> {
    /// `MinimalDecisionAgent.decide`: one decision for one snapshot.
    pub fn decide(&mut self, snapshot: &Value) -> Result<Value> {
        let Self { state, selector } = self;
        let publication = state
            .initial_publication
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("decision_request received before initialize"))?;
        let detector = state
            .detector
            .as_mut()
            .ok_or_else(|| anyhow::anyhow!("decision_request received before initialize"))?;
        // Practice scenarios speak the pre-anomaly snapshot: no score feedback,
        // no reports, and a repeat observation would be an invalid duplicate there.
        let mechanics = snapshot["schema_version"].as_str() == Some("decision-snapshot-v3");
        let reports = if mechanics {
            detector.process_snapshot(snapshot)
        } else {
            Vec::new()
        };
        let filtered = if mechanics {
            detector.filter_fault_scope(snapshot)
        } else {
            std::borrow::Cow::Borrowed(snapshot)
        };
        let previews: Vec<CandidatePreview> = preview_actions(
            filtered.as_ref(),
            &publication["scoring_contract"],
            if mechanics { Some(&detector.bests) } else { None },
        )?;
        let mut decision = match selector.select(&previews, filtered.as_ref(), publication) {
            Selection::Wait { reason, source } => json!({
                "action": "wait",
                "tile_id": "",
                "program": "",
                "request_id": "",
                "reason": reason,
                "decision_source": source,
            }),
            Selection::Observe { tile_id, program, request_id, reason, source } => json!({
                "action": "observe",
                "tile_id": tile_id,
                "program": program,
                "request_id": request_id,
                "reason": reason,
                "decision_source": source,
            }),
        };
        // When nothing on the board gains anything, spend the slot confirming a
        // suspect tile: a second read separates permanent tags from weather edges.
        if mechanics {
            if let Some(suspect) = detector.top_suspect(&previews) {
                if previews.is_empty() || previews[0].estimated_gain_per_second <= 0.0 {
                    decision = json!({
                        "action": "observe",
                        "tile_id": suspect.tile_id,
                        "program": suspect.program,
                        "request_id": suspect.request_id,
                        "reason": "repeat observation to confirm an anomalous realized-score deviation",
                        "decision_source": "detector",
                    });
                }
            }
        }
        if mechanics && decision["action"].as_str() == Some("observe") {
            let triple = (
                decision["tile_id"].as_str().unwrap_or(""),
                decision["program"].as_str().unwrap_or(""),
                decision["request_id"].as_str().unwrap_or(""),
            );
            let row = previews.iter().find(|row| {
                (row.tile_id.as_str(), row.program.as_str(), row.request_id.as_str()) == triple
            });
            let expected = row.map(|row| detector.potential_of(row));
            let under_cold_wave = detector.under_cold_wave(filtered.as_ref());
            detector.note_observation(Some(triple.0), expected, under_cold_wave);
        } else if mechanics {
            detector.note_observation(None, None, false);
        }
        Ok(protocol::decision_response(
            &snapshot["decision_sequence"],
            &decision,
            reports,
        ))
    }
}

impl<S: Selector> DecisionProvider for DeterministicAgent<S> {
    fn publish_initial(&mut self, publication: &Value) -> Result<()> {
        self.state.publish_initial(publication)
    }

    fn call(&mut self, snapshot: &Value, _deadline: Instant) -> Result<Value> {
        self.decide(snapshot)
    }
}
