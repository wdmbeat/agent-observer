//! In-process deterministic agent — port of `agent/decision_graph.py`
//! (deterministic path only: no LLM, no LangGraph) plus the
//! `minimal_agent.py`/`protocol.py` response envelope.
//!
//! The agent receives the exact snapshot `serde_json::Value` the workflow would
//! send over the wire, and returns the exact response envelope the Python agent
//! would print — so behavior is provably identical to the subprocess path.

use std::time::Instant;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::contracts::PARTICIPANT_PROTOCOL_VERSION;
use crate::workflow::DecisionProvider;

use super::anomaly::AnomalyDetector;
use super::preview::{preview_actions, CandidatePreview};

/// The shipped `my_strategy.choose_action` returns `candidates[0]` with no
/// reason, which the graph treats as agreement with the default ranking — so
/// the deterministic path below is exactly what the shipped agent does.
#[derive(Default)]
pub struct BuiltinAgent {
    initial_publication: Option<Value>,
    detector: Option<AnomalyDetector>,
}

impl BuiltinAgent {
    pub fn new() -> Self {
        Self::default()
    }

    /// `MinimalDecisionAgent.decide`: one decision for one snapshot.
    pub fn decide(&mut self, snapshot: &Value) -> Result<Value> {
        let Self { initial_publication, detector } = self;
        let publication = initial_publication
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("decision_request received before initialize"))?;
        let detector = detector
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
        let mut decision = if previews.is_empty() {
            json!({
                "action": "wait",
                "tile_id": "",
                "program": "",
                "request_id": "",
                "reason": "no legal observable candidate can finish in its known window",
                "decision_source": "deterministic",
            })
        } else {
            let best = &previews[0];
            json!({
                "action": "observe",
                "tile_id": best.tile_id,
                "program": best.program,
                "request_id": best.request_id,
                "reason": "highest public current-snapshot estimate",
                "decision_source": "deterministic",
            })
        };
        // When nothing on the board gains anything, spend the slot confirming a
        // suspect tile: a second read separates permanent tags from weather edges.
        if mechanics {
            if let Some(suspect) = detector.top_suspect(&previews) {
                if previews.is_empty()
                    || previews[0].estimated_gain_per_second <= 0.0
                {
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
        // protocol.py::decision_response (always speaks v2, both are accepted).
        let mut envelope = json!({
            "protocol_version": PARTICIPANT_PROTOCOL_VERSION,
            "message_type": "decision_response",
            "decision_sequence": snapshot["decision_sequence"],
            "action": decision["action"],
            "tile_id": decision["tile_id"],
            "program": decision["program"],
            "request_id": decision["request_id"],
            "reason": decision["reason"],
            "decision_source": decision["decision_source"],
        });
        if !reports.is_empty() {
            envelope["reports"] = Value::Array(reports);
        }
        Ok(envelope)
    }
}

impl DecisionProvider for BuiltinAgent {
    fn publish_initial(&mut self, publication: &Value) -> Result<()> {
        if publication["schema_version"].as_str()
            != Some(crate::contracts::INITIAL_PUBLICATION_VERSION)
        {
            bail!("unsupported initial publication schema_version");
        }
        self.detector = Some(AnomalyDetector::new(publication));
        self.initial_publication = Some(publication.clone());
        Ok(())
    }

    fn call(&mut self, snapshot: &Value, _deadline: Instant) -> Result<Value> {
        self.decide(snapshot)
    }
}
