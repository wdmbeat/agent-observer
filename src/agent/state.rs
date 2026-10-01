//! Port of `agent/state.py` — the per-run state container.
//!
//! Python's `DecisionState` is a per-decision TypedDict threaded through the
//! graph nodes; the run-long half of it — the initial publication handle and
//! the anomaly detector's memory — lives here and is shared by every
//! `decide()` call. Per-strategy state (e.g. the reference strategy's memory)
//! lives on the selector instead.

use serde_json::Value;

use super::anomaly_detection::AnomalyDetector;
use super::protocol;

/// Run-long agent state: what the pipeline carries across decisions.
#[derive(Default)]
pub struct RunState {
    pub initial_publication: Option<Value>,
    pub detector: Option<AnomalyDetector>,
}

impl RunState {
    /// Bind the initial publication and start the detector (Python: the
    /// `initialize` branch of the minimal-agent loop).
    pub fn publish_initial(&mut self, publication: &Value) -> anyhow::Result<()> {
        protocol::check_initial_publication(publication)?;
        self.detector = Some(AnomalyDetector::new(publication));
        self.initial_publication = Some(publication.clone());
        Ok(())
    }
}
