//! Port of `agent/protocol.py` — participant-side protocol constants, the
//! `decision_response` envelope construction, and `parse_platform_message`
//! inbound validation for the JSONL loop (`src/agent/minimal_agent.rs`).

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::contracts::{
    ACCEPTED_PROTOCOL_VERSIONS, DECISION_SNAPSHOT_VERSION, INITIAL_PUBLICATION_VERSION,
    LEGACY_DECISION_SNAPSHOT_VERSION, PARTICIPANT_PROTOCOL_VERSION,
};

/// The agent always speaks v2; the platform accepts both.
pub const PROTOCOL_VERSION: &str = PARTICIPANT_PROTOCOL_VERSION;
pub const ACCEPTED_SNAPSHOT_VERSIONS: [&str; 2] =
    [LEGACY_DECISION_SNAPSHOT_VERSION, DECISION_SNAPSHOT_VERSION];

/// The inbound platform message kinds the JSONL loop dispatches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageType {
    Initialize,
    DecisionRequest,
    Finish,
}

/// `parse_platform_message`'s initialize-payload check.
pub fn check_initial_publication(publication: &Value) -> Result<()> {
    if publication["schema_version"].as_str() != Some(INITIAL_PUBLICATION_VERSION) {
        bail!("unsupported initial publication schema_version");
    }
    Ok(())
}

/// `protocol.py::parse_platform_message`: validate an inbound envelope and
/// return its message type and payload.
pub fn parse_platform_message(message: &Value) -> Result<(MessageType, Value)> {
    if !ACCEPTED_PROTOCOL_VERSIONS.contains(&message["protocol_version"].as_str().unwrap_or("")) {
        bail!("unsupported participant protocol_version");
    }
    let message_type = message["message_type"].as_str().unwrap_or("");
    let payload = match message.get("payload") {
        Some(payload @ Value::Object(_)) => payload.clone(),
        _ => bail!("platform message payload must be an object"),
    };
    let kind = match message_type {
        "initialize" => {
            check_initial_publication(&payload)?;
            MessageType::Initialize
        }
        "decision_request" => {
            if !ACCEPTED_SNAPSHOT_VERSIONS
                .contains(&payload["schema_version"].as_str().unwrap_or(""))
            {
                bail!("unsupported decision snapshot schema_version");
            }
            let envelope_sequence = message["decision_sequence"].as_i64().unwrap_or(-1);
            let payload_sequence = payload["decision_sequence"].as_i64().unwrap_or(-2);
            if envelope_sequence != payload_sequence {
                bail!("decision sequence differs between envelope and payload");
            }
            MessageType::DecisionRequest
        }
        // End-of-run notice: no reply, no schema checks. payload carries
        // termination_reason, last_decision_sequence and grace_seconds.
        "finish" => MessageType::Finish,
        other => bail!("unsupported platform message_type {other:?}"),
    };
    Ok((kind, payload))
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{LEGACY_PARTICIPANT_PROTOCOL_VERSION, PARTICIPANT_PROTOCOL_VERSION};

    fn envelope(protocol_version: &str, message_type: &str, payload: Value) -> Value {
        json!({
            "protocol_version": protocol_version,
            "message_type": message_type,
            "payload": payload,
        })
    }

    #[test]
    fn initialize_accepts_both_protocol_versions() {
        let payload = json!({"schema_version": INITIAL_PUBLICATION_VERSION});
        for version in [LEGACY_PARTICIPANT_PROTOCOL_VERSION, PARTICIPANT_PROTOCOL_VERSION] {
            let (kind, parsed) =
                parse_platform_message(&envelope(version, "initialize", payload.clone())).unwrap();
            assert_eq!(kind, MessageType::Initialize);
            assert_eq!(parsed, payload);
        }
    }

    #[test]
    fn decision_request_requires_matching_sequences() {
        let payload = json!({"schema_version": DECISION_SNAPSHOT_VERSION, "decision_sequence": 7});
        let mut message = envelope(PARTICIPANT_PROTOCOL_VERSION, "decision_request", payload);
        message["decision_sequence"] = json!(7);
        let (kind, _) = parse_platform_message(&message).unwrap();
        assert_eq!(kind, MessageType::DecisionRequest);
        message["decision_sequence"] = json!(8);
        let error = parse_platform_message(&message).unwrap_err();
        assert!(error.to_string().contains("decision sequence differs"));
    }

    #[test]
    fn decision_request_accepts_legacy_snapshot() {
        let payload = json!({
            "schema_version": LEGACY_DECISION_SNAPSHOT_VERSION,
            "decision_sequence": 1,
        });
        let mut message = envelope(PARTICIPANT_PROTOCOL_VERSION, "decision_request", payload);
        message["decision_sequence"] = json!(1);
        assert!(parse_platform_message(&message).is_ok());
        let bad = json!({"schema_version": "decision-snapshot-v9", "decision_sequence": 1});
        let mut message = envelope(PARTICIPANT_PROTOCOL_VERSION, "decision_request", bad);
        message["decision_sequence"] = json!(1);
        let error = parse_platform_message(&message).unwrap_err();
        assert!(error.to_string().contains("decision snapshot schema_version"));
    }

    #[test]
    fn malformed_messages_are_rejected() {
        let payload = json!({"schema_version": INITIAL_PUBLICATION_VERSION});
        let bad_version = envelope("participant-agent-protocol-v0", "initialize", payload.clone());
        assert!(parse_platform_message(&bad_version).is_err());
        let no_payload = json!({
            "protocol_version": PARTICIPANT_PROTOCOL_VERSION,
            "message_type": "initialize",
            "payload": [],
        });
        let error = parse_platform_message(&no_payload).unwrap_err();
        assert!(error.to_string().contains("payload must be an object"));
        let unknown = envelope(PARTICIPANT_PROTOCOL_VERSION, "explode", payload.clone());
        let error = parse_platform_message(&unknown).unwrap_err();
        assert!(error.to_string().contains("unsupported platform message_type"));
        let bad_publication = envelope(
            PARTICIPANT_PROTOCOL_VERSION,
            "initialize",
            json!({"schema_version": "initial-publication-v1"}),
        );
        assert!(parse_platform_message(&bad_publication).is_err());
    }

    #[test]
    fn finish_passes_through_without_schema_checks() {
        let payload = json!({"termination_reason": "completed", "last_decision_sequence": 3});
        let (kind, parsed) =
            parse_platform_message(&envelope(PARTICIPANT_PROTOCOL_VERSION, "finish", payload.clone()))
                .unwrap();
        assert_eq!(kind, MessageType::Finish);
        assert_eq!(parsed, payload);
    }
}
