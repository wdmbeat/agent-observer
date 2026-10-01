//! Port of `agent/protocol.py` — participant-side protocol constants and the
//! `decision_response` envelope construction.
//!
//! Python's `parse_platform_message` validates inbound envelopes for the JSONL
//! loop; the in-process agent shares the workflow's data structures directly
//! and the subprocess side is covered by `src/transport.rs`, so only the
//! schema-version checks remain here.

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::contracts::{
    DECISION_SNAPSHOT_VERSION, INITIAL_PUBLICATION_VERSION,
    LEGACY_DECISION_SNAPSHOT_VERSION, PARTICIPANT_PROTOCOL_VERSION,
};

/// The agent always speaks v2; the platform accepts both.
pub const PROTOCOL_VERSION: &str = PARTICIPANT_PROTOCOL_VERSION;
pub const ACCEPTED_SNAPSHOT_VERSIONS: [&str; 2] =
    [LEGACY_DECISION_SNAPSHOT_VERSION, DECISION_SNAPSHOT_VERSION];

/// `parse_platform_message`'s initialize-payload check.
pub fn check_initial_publication(publication: &Value) -> Result<()> {
    if publication["schema_version"].as_str() != Some(INITIAL_PUBLICATION_VERSION) {
        bail!("unsupported initial publication schema_version");
    }
    Ok(())
}

/// Wrap one local decision in the public response envelope
/// (`protocol.py::decision_response`). `reports` rides the envelope only when
/// non-empty; report rows never consume slot time.
pub fn decision_response(sequence: &Value, decision: &Value, reports: Vec<Value>) -> Value {
    let mut envelope = json!({
        "protocol_version": PROTOCOL_VERSION,
        "message_type": "decision_response",
        "decision_sequence": sequence,
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
    envelope
}
