//! Shared deterministic decision pipeline — port of `agent/decision_graph.py`
//! (deterministic path only: no LLM, no LangGraph) plus the
//! `minimal_agent.py`/`protocol.py` response envelope.
//!
//! The pipeline is fixed (anomaly reports → fault-scope filter → previews →
//! selection → detector suspect override → note_observation → envelope); only
//! the candidate-selection step differs between strategies. `BuiltinAgent` is
//! the baseline selector; `reference.rs` plugs in the worked teaching example.

use std::time::Instant;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::contracts::PARTICIPANT_PROTOCOL_VERSION;
use crate::workflow::DecisionProvider;

use super::anomaly::AnomalyDetector;
use super::preview::{preview_actions, CandidatePreview};

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

/// Default policy: trust the platform ranking — observe previews[0].
///
/// The shipped `my_strategy.choose_action` returns `candidates[0]` with no
/// reason, which the Python graph treats as agreement with the default ranking
/// (returns None) — so this selector is exactly what the shipped agent does.
#[derive(Default)]
pub struct BaselineSelector;

impl Selector for BaselineSelector {
    fn select(&mut self, previews: &[CandidatePreview], _snapshot: &Value, _publication: &Value) -> Selection {
        match previews.first() {
            None => Selection::Wait {
                reason: "no legal observable candidate can finish in its known window".to_string(),
                source: "deterministic",
            },
            Some(best) => Selection::Observe {
                tile_id: best.tile_id.clone(),
                program: best.program.clone(),
                request_id: best.request_id.clone(),
                reason: "highest public current-snapshot estimate".to_string(),
                source: "deterministic",
            },
        }
    }
}

/// The deterministic agent facade: anomaly tracking plus one selection per
/// snapshot. `S` is the selection strategy.
pub struct DeterministicAgent<S> {
    initial_publication: Option<Value>,
    detector: Option<AnomalyDetector>,
    selector: S,
}

impl<S: Default> Default for DeterministicAgent<S> {
    fn default() -> Self {
        Self { initial_publication: None, detector: None, selector: S::default() }
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
        Self { initial_publication: None, detector: None, selector }
    }
}

impl<S: Selector> DeterministicAgent<S> {
    /// `MinimalDecisionAgent.decide`: one decision for one snapshot.
    pub fn decide(&mut self, snapshot: &Value) -> Result<Value> {
        let Self { initial_publication, detector, selector } = self;
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

impl<S: Selector> DecisionProvider for DeterministicAgent<S> {
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
