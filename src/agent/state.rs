//! Port of `agent/state.py` — the per-run state container.
//!
//! Python's `DecisionState` is a per-decision TypedDict threaded through the
//! graph nodes; the run-long half of it — the initial publication handle and
//! the anomaly detector's memory — lives here and is shared by every
//! `decide()` call. Per-strategy state (e.g. the reference strategy's memory)
//! lives on the selector instead.

use anyhow::{bail, Result};

use crate::contracts::INITIAL_PUBLICATION_VERSION;

use super::anomaly_detection::AnomalyDetector;
use super::model::InitialPublication;

/// Run-long agent state: what the pipeline carries across decisions.
#[derive(Default)]
pub struct RunState {
    pub initial_publication: Option<InitialPublication>,
    pub detector: Option<AnomalyDetector>,
}

impl RunState {
    /// Bind the initial publication and start the detector (Python: the
    /// `initialize` branch of the minimal-agent loop).
    pub fn publish_initial(&mut self, publication: &InitialPublication) -> Result<()> {
        if publication.schema_version != INITIAL_PUBLICATION_VERSION {
            bail!("unsupported initial publication schema_version");
        }
        self.detector = Some(AnomalyDetector::new(publication));
        self.initial_publication = Some(publication.clone());
        Ok(())
    }
}
