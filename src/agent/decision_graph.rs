//! Port of `agent/decision_graph.py` — the deterministic decision pipeline
//! plus `minimal_agent.py`'s `MinimalDecisionAgent` facade and the optional
//! LLM selector (no LangGraph).
//!
//! The pipeline is fixed (detector.process_snapshot → fault-scope filter →
//! preview_actions → selector → detector suspect override → note_observation →
//! protocol envelope); only the candidate-selection step differs between
//! strategies. The `Selector` trait plays the role of the `choose_action`
//! seam: `my_strategy.rs` holds the baseline, `reference_strategy.rs` the
//! worked teaching example, and `LlmSelector` ports `_model_node`/`_finalize`'s
//! model branch around any inner selector.

use std::time::Instant;

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::workflow::DecisionProvider;

use super::model_factory::ChatModel;
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

/// Lets `DeterministicAgent<Box<dyn Selector>>` cover every runtime
/// combination (baseline/reference, with or without the LLM wrapper).
impl Selector for Box<dyn Selector> {
    fn select(
        &mut self,
        previews: &[CandidatePreview],
        snapshot: &Value,
        publication: &Value,
    ) -> Selection {
        (**self).select(previews, snapshot, publication)
    }
}

/// `decision_graph.py::SYSTEM_PROMPT`.
pub const SYSTEM_PROMPT: &str = "You choose one telescope action for the current decision only.\nEvery listed candidate is a legal current start and already uses the public scoring\nformula. Return exactly one JSON object with action, tile_id, program, request_id,\nand reason. Select only a listed (tile_id, program, request_id) tuple. Do not plan\nfuture slots and do not invent simulator calls or fields.";

/// Optional LLM candidate selection (`decision_graph.py::_model_node` +
/// `_finalize`'s model branch): the model picks among the top-K previews; any
/// error or invalid answer falls back to the inner selector.
pub struct LlmSelector<S: Selector> {
    model: ChatModel,
    top_k: usize,
    inner: S,
}

impl<S: Selector> LlmSelector<S> {
    pub fn new(model: ChatModel, top_k: usize, inner: S) -> Self {
        Self { model, top_k, inner }
    }
}

impl<S: Selector> Selector for LlmSelector<S> {
    fn select(
        &mut self,
        previews: &[CandidatePreview],
        snapshot: &Value,
        publication: &Value,
    ) -> Selection {
        let candidates: Vec<Value> = previews
            .iter()
            .take(self.top_k)
            .enumerate()
            .map(|(index, preview)| compact_candidate(preview, index + 1))
            .collect();
        if !candidates.is_empty() {
            let prompt = json!({
                "decision_sequence": snapshot["decision_sequence"],
                "cursor": snapshot["cursor"],
                "candidates": candidates,
                "output_schema": {
                    "action": "observe",
                    "tile_id": "one listed tile_id",
                    "program": "the listed program",
                    "request_id": "the listed request_id, possibly empty",
                    "reason": "one short sentence",
                },
            });
            let prompt = serde_json::to_string(&prompt).expect("prompt serialization");
            let outcome = self
                .model
                .invoke(SYSTEM_PROMPT, &prompt)
                .and_then(|text| decide_from_model_text(&text, previews, self.top_k));
            match outcome {
                Ok(selection) => return selection,
                Err(error) => eprintln!("sac-agent model fallback: {error}"),
            }
        }
        self.inner.select(previews, snapshot, publication)
    }
}

/// `decision_graph.py::_compact`: the per-candidate JSON the model sees.
fn compact_candidate(preview: &CandidatePreview, rank: usize) -> Value {
    json!({
        "rank": rank,
        "tile_id": preview.tile_id,
        "program": preview.program,
        "request_id": preview.request_id,
        "region_id": preview.region_id,
        "scheduling_class": preview.scheduling_class,
        "nominal_exptime_seconds": preview.nominal_exptime_seconds,
        "combined_quality": preview.combined_quality,
        "estimated_science_score": preview.estimated_science_score,
        "terminal_penalty_avoidance": preview.terminal_penalty_avoidance,
        "request_policy_value": preview.request_policy_value,
        "estimated_total_gain": preview.estimated_total_gain,
        "estimated_gain_per_second": preview.estimated_gain_per_second,
    })
}

/// `decision_graph.py::_parse_object`: strip ``` fences, else slice the first
/// `{` .. last `}` span; the result must be a JSON object.
fn parse_model_object(text: &str) -> Result<Value> {
    let mut stripped = text.trim().to_string();
    if stripped.starts_with("```") {
        let mut lines: Vec<&str> = stripped.lines().collect();
        if !lines.is_empty() && lines[0].starts_with("```") {
            lines.remove(0);
        }
        if !lines.is_empty() && lines.last().unwrap().trim() == "```" {
            lines.pop();
        }
        stripped = lines.join("\n").trim().to_string();
    }
    let value: Value = match serde_json::from_str(&stripped) {
        Ok(value) => value,
        Err(_) => {
            let start = stripped.find('{');
            let end = stripped.rfind('}');
            match (start, end) {
                (Some(start), Some(end)) if end > start => {
                    serde_json::from_str(&stripped[start..=end])?
                }
                _ => bail!("model output contains no JSON object"),
            }
        }
    };
    if !value.is_object() {
        bail!("model output must be a JSON object");
    }
    Ok(value)
}

/// `decision_graph.py::_validated_model_decision`: accept only an `observe`
/// action naming one of the top-K (tile_id, program, request_id) triples.
fn validated_model_decision(
    selection: &Value,
    previews: &[CandidatePreview],
    top_k: usize,
) -> Option<Selection> {
    if selection.get("action").and_then(Value::as_str) != Some("observe") {
        return None;
    }
    let key = (
        selection.get("tile_id").and_then(Value::as_str).unwrap_or(""),
        selection.get("program").and_then(Value::as_str).unwrap_or(""),
        selection.get("request_id").and_then(Value::as_str).unwrap_or(""),
    );
    let listed = previews.iter().take(top_k).any(|preview| {
        (preview.tile_id.as_str(), preview.program.as_str(), preview.request_id.as_str()) == key
    });
    if !listed {
        return None;
    }
    let reason = selection
        .get("reason")
        .and_then(Value::as_str)
        .unwrap_or("model selection")
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    let reason: String = reason.chars().take(240).collect();
    Some(Selection::Observe {
        tile_id: key.0.to_string(),
        program: key.1.to_string(),
        request_id: key.2.to_string(),
        reason: if reason.is_empty() { "model selection".to_string() } else { reason },
        source: "model",
    })
}

/// `_parse_object` + `_validated_model_decision`: turn raw model output into a
/// validated selection, erroring (→ inner-selector fallback) on anything invalid.
fn decide_from_model_text(
    text: &str,
    previews: &[CandidatePreview],
    top_k: usize,
) -> Result<Selection> {
    let selection = parse_model_object(text)?;
    validated_model_decision(&selection, previews, top_k).ok_or_else(|| {
        anyhow::anyhow!("model selection is not a listed top-{top_k} observe action")
    })
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


#[cfg(test)]
mod tests {
    use super::*;

    fn preview(tile_id: &str, program: &str, request_id: &str) -> CandidatePreview {
        CandidatePreview {
            tile_id: tile_id.to_string(),
            program: program.to_string(),
            request_id: request_id.to_string(),
            region_id: "R1".to_string(),
            scheduling_class: "STANDARD".to_string(),
            nominal_exptime_seconds: 300,
            atmospheric_quality: 0.8,
            lunar_quality_factor: 1.0,
            combined_quality: 0.8,
            quality_band: "DARK".to_string(),
            tile_science_value: 100.0,
            estimated_science_score: 80.0,
            terminal_penalty_avoidance: 0.0,
            request_policy_value: 5.0,
            estimated_total_gain: 85.0,
            estimated_gain_per_second: 0.28,
            estimate_semantics: "transparent".to_string(),
        }
    }

    #[test]
    fn parse_model_object_handles_fences_and_prose() {
        let plain = parse_model_object(r#"{"action": "observe"}"#).unwrap();
        assert_eq!(plain["action"], "observe");
        let fenced = parse_model_object("```json\n{\"action\": \"observe\"}\n```").unwrap();
        assert_eq!(fenced["action"], "observe");
        let prose =
            parse_model_object("Here is my pick: {\"action\": \"observe\"} — done.").unwrap();
        assert_eq!(prose["action"], "observe");
        assert!(parse_model_object("[1, 2]").is_err());
        let error = parse_model_object("no object here").unwrap_err();
        assert!(error.to_string().contains("no JSON object"));
    }

    #[test]
    fn model_decision_requires_a_listed_observe_action() {
        let previews = vec![
            preview("TILE-1", "DARK", ""),
            preview("TILE-2", "BRIGHT", "REQ-1"),
        ];
        let valid = decide_from_model_text(
            r#"{"action":"observe","tile_id":"TILE-2","program":"BRIGHT","request_id":"REQ-1","reason":"  best   gain  "}"#,
            &previews,
            2,
        )
        .unwrap();
        assert_eq!(
            valid,
            Selection::Observe {
                tile_id: "TILE-2".to_string(),
                program: "BRIGHT".to_string(),
                request_id: "REQ-1".to_string(),
                reason: "best gain".to_string(),
                source: "model",
            }
        );
        // The same pick is rejected once it falls outside the top-K window.
        assert!(decide_from_model_text(
            r#"{"action":"observe","tile_id":"TILE-2","program":"BRIGHT","request_id":"REQ-1"}"#,
            &previews,
            1,
        )
        .is_err());
        // Non-observe actions and unlisted triples are rejected.
        assert!(decide_from_model_text(r#"{"action":"wait"}"#, &previews, 2).is_err());
        assert!(decide_from_model_text(
            r#"{"action":"observe","tile_id":"TILE-9","program":"DARK","request_id":""}"#,
            &previews,
            2,
        )
        .is_err());
    }

    #[test]
    fn model_reason_defaults_and_truncates() {
        let previews = vec![preview("TILE-1", "DARK", "")];
        let missing = decide_from_model_text(
            r#"{"action":"observe","tile_id":"TILE-1","program":"DARK","request_id":""}"#,
            &previews,
            1,
        )
        .unwrap();
        let Selection::Observe { reason, .. } = missing else { panic!("observe expected") };
        assert_eq!(reason, "model selection");
        let long = format!(
            r#"{{"action":"observe","tile_id":"TILE-1","program":"DARK","request_id":"","reason":"{}"}}"#,
            "x".repeat(300)
        );
        let truncated = decide_from_model_text(&long, &previews, 1).unwrap();
        let Selection::Observe { reason, .. } = truncated else { panic!("observe expected") };
        assert_eq!(reason.chars().count(), 240);
    }
}
