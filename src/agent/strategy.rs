//! Typed strategy selection for the in-process agent. Rust-only CLI glue with
//! no Python counterpart (the Python kit picks its strategy via the editable
//! `my_strategy.py` file instead).
//!
//! Adding a strategy = add an enum case + a module; `Strategy::build` is the
//! factory the workflow uses to construct the in-process agent.

use crate::workflow::DecisionProvider;

use super::decision_graph::{BuiltinAgent, DeterministicAgent};
use super::reference_strategy::ReferenceSelector;

/// Native deterministic strategies, selectable on the CLI (`rust <STRATEGY>`)
/// and programmatically via `AgentSpec::Builtin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Strategy {
    /// The reference deterministic policy: observe the highest-ranked public
    /// preview each slot, with calibrated anomaly reporting under the v3
    /// mechanics (port of the kit's minimal_agent deterministic path).
    Baseline,
    /// The worked teaching example (port of reference_strategy.py): trust the
    /// platform ranking, override only when an end-of-game account is about to
    /// come due (REQUIRED tile at risk, coverage-evenness rerank).
    Reference,
}

impl Strategy {
    pub fn build(&self) -> Box<dyn DecisionProvider> {
        match self {
            Strategy::Baseline => Box::new(BuiltinAgent::new()),
            Strategy::Reference => Box::new(DeterministicAgent::with_selector(
                ReferenceSelector::default(),
            )),
        }
    }
}
