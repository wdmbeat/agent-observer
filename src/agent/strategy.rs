//! Typed strategy selection for the in-process agent.
//!
//! Adding a strategy = add an enum case + a module; `Strategy::build` is the
//! factory the workflow uses to construct the in-process agent.

use super::builtin::BuiltinAgent;

/// Native deterministic strategies, selectable on the CLI (`rust <STRATEGY>`)
/// and programmatically via `AgentSpec::Builtin`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Strategy {
    /// The reference deterministic policy: observe the highest-ranked public
    /// preview each slot, with calibrated anomaly reporting under the v3
    /// mechanics (port of the kit's minimal_agent deterministic path).
    Baseline,
}

impl Strategy {
    pub fn build(&self) -> BuiltinAgent {
        match self {
            Strategy::Baseline => BuiltinAgent::new(),
        }
    }
}
