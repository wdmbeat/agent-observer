//! Port of `agent/my_strategy.py` — the shipped user strategy.
//!
//! The shipped `choose_action` returns `candidates[0]` with no reason, which
//! the Python graph treats as agreement with the default ranking (returns
//! None) — so this selector is exactly what the shipped agent does: trust the
//! platform's gain-per-second ranking.

use super::decision_graph::{Selection, Selector};
use super::model::{DecisionSnapshot, InitialPublication};
use super::scoring_preview::CandidatePreview;

/// Default policy: trust the platform ranking — observe previews[0].
#[derive(Default)]
pub struct BaselineSelector;

impl Selector for BaselineSelector {
    fn select(
        &mut self,
        previews: &[CandidatePreview],
        _snapshot: &DecisionSnapshot,
        _publication: &InitialPublication,
    ) -> Selection {
        match previews.first() {
            None => Selection::Wait {
                reason: "no legal observable candidate can finish in its known window".to_string(),
                source: "deterministic",
            },
            Some(best) => Selection::Observe {
                tile_id: best.tile_id.clone(),
                program: best.program,
                request_id: best.request_id.clone(),
                reason: "highest public current-snapshot estimate".to_string(),
                source: "deterministic",
            },
        }
    }
}
