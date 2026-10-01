//! Port of `agent/protocol.py` — participant-side protocol constants, inbound
//! envelope validation (`parse_platform_message` → typed `PlatformMessage`),
//! and the outbound `DecisionResponse` envelope.

use anyhow::{bail, Context, Result};
use serde_json::Value;

use crate::contracts::{
    ACCEPTED_PROTOCOL_VERSIONS, DECISION_SNAPSHOT_VERSION, INITIAL_PUBLICATION_VERSION,
    LEGACY_DECISION_SNAPSHOT_VERSION, PARTICIPANT_PROTOCOL_VERSION,
};

use super::model::{DecisionSnapshot, InitialPublication, Report};

/// The agent always speaks v2; the platform accepts both.
pub const PROTOCOL_VERSION: &str = PARTICIPANT_PROTOCOL_VERSION;
pub const ACCEPTED_SNAPSHOT_VERSIONS: [&str; 2] =
    [LEGACY_DECISION_SNAPSHOT_VERSION, DECISION_SNAPSHOT_VERSION];

/// One validated inbound platform envelope with its typed payload.
#[derive(Debug, Clone, PartialEq)]
pub enum PlatformMessage {
    Initialize(InitialPublication),
    DecisionRequest { sequence: i64, snapshot: DecisionSnapshot },
    Finish {
        termination_reason: String,
        last_decision_sequence: i64,
        grace_seconds: Option<f64>,
    },
}

/// `parse_platform_message`'s initialize-payload check.
pub fn check_initial_publication(publication: &Value) -> Result<()> {
    if publication["schema_version"].as_str() != Some(INITIAL_PUBLICATION_VERSION) {
        bail!("unsupported initial publication schema_version");
    }
    Ok(())
}

#[derive(serde::Deserialize)]
struct FinishPayload {
    termination_reason: String,
    last_decision_sequence: i64,
    #[serde(default)]
    grace_seconds: Option<f64>,
}

/// `protocol.py::parse_platform_message`: validate an inbound envelope and
/// decode its payload into the typed model. The validation order and error
/// messages are unchanged from the `Value` version.
pub fn parse_platform_message(message: &Value) -> Result<PlatformMessage> {
    if !ACCEPTED_PROTOCOL_VERSIONS.contains(&message["protocol_version"].as_str().unwrap_or("")) {
        bail!("unsupported participant protocol_version");
    }
    let message_type = message["message_type"].as_str().unwrap_or("");
    let payload = match message.get("payload") {
        Some(payload @ Value::Object(_)) => payload,
        _ => bail!("platform message payload must be an object"),
    };
    match message_type {
        "initialize" => {
            check_initial_publication(payload)?;
            let publication = serde_json::from_value(payload.clone())
                .context("invalid initial publication")?;
            Ok(PlatformMessage::Initialize(publication))
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
            let snapshot = serde_json::from_value(payload.clone())
                .context("invalid decision snapshot")?;
            Ok(PlatformMessage::DecisionRequest { sequence: envelope_sequence, snapshot })
        }
        // End-of-run notice: no reply, no schema checks. payload carries
        // termination_reason, last_decision_sequence and grace_seconds.
        "finish" => {
            let finish: FinishPayload = serde_json::from_value(payload.clone())
                .context("invalid finish payload")?;
            Ok(PlatformMessage::Finish {
                termination_reason: finish.termination_reason,
                last_decision_sequence: finish.last_decision_sequence,
                grace_seconds: finish.grace_seconds,
            })
        }
        other => bail!("unsupported platform message_type {other:?}"),
    }
}

/// The outbound `decision_response` envelope — the typed replacement for
/// `protocol.py::decision_response`. Serializes to exactly the same keys;
/// `reports` rides the envelope only when non-empty (report rows never
/// consume slot time).
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct DecisionResponse {
    pub protocol_version: String,
    pub message_type: String,
    pub decision_sequence: i64,
    pub action: String,
    pub tile_id: String,
    pub program: String,
    pub request_id: String,
    pub reason: String,
    pub decision_source: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub reports: Vec<Report>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::contracts::{LEGACY_PARTICIPANT_PROTOCOL_VERSION, PARTICIPANT_PROTOCOL_VERSION};
    use crate::agent::model::{FaultStatus, Program, SnapshotSchema};
    use serde_json::json;

    /// A minimal valid initial publication for the typed decode.
    fn publication_payload() -> Value {
        json!({
            "schema_version": INITIAL_PUBLICATION_VERSION,
            "calendar": {
                "first_night": "2026-10-06", "last_night": "2026-10-12",
                "night_count": 7, "slot_count": 100, "slot_duration_seconds": 900,
            },
            "site": {
                "latitude_deg": 31.9634, "longitude_deg": -111.599,
                "utc_offset_hours": -7.0, "sun_altitude_limit_deg": -12.0,
            },
            "tile_catalog": {
                "tile_count": 1,
                "required_tile_ids": ["T00041"],
                "region_ids": ["R03"],
                "tiles": [{
                    "tile_id": "T00041", "ra_deg": "37.505464", "dec_deg": "-1.532928",
                    "nominal_exptime_seconds": 450, "region_id": "R03",
                    "scheduling_class": "REQUIRED",
                    "available_from_utc": "2026-10-06T02:00:00Z",
                    "available_until_utc": "2026-10-08T10:00:00Z",
                    "tile_science_value": 129.372331,
                }],
            },
            "target_catalog": [],
            "scoring_contract": {
                "score_config": {
                    "schema_version": "challenge-score-v3",
                    "quality_thresholds": {"dark": 0.65, "bright": 0.4},
                    "program_bonus": {"DARK": 0.25, "BRIGHT": 0.15, "BACKUP": 0.08},
                    "penalties": {
                        "unsafe_observation": 2000.0, "invalid_action": 100.0,
                        "avoidable_wait_per_second": 0.001,
                        "required_miss": 1000.0, "flexible_shortfall_per_tile": 100.0,
                    },
                    "flexible_quota_per_region": 4,
                },
                "weather_score_interface": {
                    "airmass_exponent": 1.0, "maximum_weather_quality": 3.0,
                },
                "lunar_model": {},
            },
            "global_wallclock_seconds": 600.0,
        })
    }

    /// A minimal valid decision snapshot for the typed decode.
    fn snapshot_payload(schema: &str, sequence: i64) -> Value {
        json!({
            "schema_version": schema,
            "decision_sequence": sequence,
            "cursor": {
                "slot_id": "N1-S001", "night_id": "N1",
                "timestamp_utc": "2026-10-06T02:00:00Z", "slot_offset_seconds": 0,
            },
            "current_site_weather": {
                "is_observable": true, "seeing_arcsec": 1.0,
                "transparency": 1.0, "sky_quality": 1.0,
            },
            "candidate_tiles": [],
            "active_requests": [],
            "progress": {"completed_tile_ids": [], "flexible_completed_by_region": {}},
        })
    }

    fn envelope(protocol_version: &str, message_type: &str, payload: Value) -> Value {
        json!({
            "protocol_version": protocol_version,
            "message_type": message_type,
            "payload": payload,
        })
    }

    #[test]
    fn initialize_accepts_both_protocol_versions() {
        for version in [LEGACY_PARTICIPANT_PROTOCOL_VERSION, PARTICIPANT_PROTOCOL_VERSION] {
            let message =
                parse_platform_message(&envelope(version, "initialize", publication_payload()))
                    .unwrap();
            let PlatformMessage::Initialize(publication) = message else {
                panic!("initialize expected");
            };
            assert_eq!(publication.schema_version, INITIAL_PUBLICATION_VERSION);
            assert_eq!(publication.tile_catalog.tiles[0].ra_deg, 37.505464);
            assert_eq!(
                publication
                    .scoring_contract
                    .score_config
                    .program_bonus
                    .for_program(Program::Dark),
                0.25
            );
        }
    }

    #[test]
    fn decision_request_requires_matching_sequences() {
        let mut message = envelope(
            PARTICIPANT_PROTOCOL_VERSION,
            "decision_request",
            snapshot_payload(DECISION_SNAPSHOT_VERSION, 7),
        );
        message["decision_sequence"] = json!(7);
        let PlatformMessage::DecisionRequest { sequence, snapshot } =
            parse_platform_message(&message).unwrap()
        else {
            panic!("decision_request expected");
        };
        assert_eq!(sequence, 7);
        assert_eq!(snapshot.schema_version, SnapshotSchema::V3);
        assert_eq!(snapshot.cursor.night_id, "N1");
        message["decision_sequence"] = json!(8);
        let error = parse_platform_message(&message).unwrap_err();
        assert!(error.to_string().contains("decision sequence differs"));
    }

    #[test]
    fn decision_request_accepts_v2_and_v3_snapshots() {
        let mut message = envelope(
            PARTICIPANT_PROTOCOL_VERSION,
            "decision_request",
            snapshot_payload(LEGACY_DECISION_SNAPSHOT_VERSION, 1),
        );
        message["decision_sequence"] = json!(1);
        let PlatformMessage::DecisionRequest { snapshot, .. } =
            parse_platform_message(&message).unwrap()
        else {
            panic!("decision_request expected");
        };
        assert_eq!(snapshot.schema_version, SnapshotSchema::V2);
        assert!(snapshot.tile_last_finished.is_none());
        assert!(snapshot.fault_status.is_none());

        let mut bad = envelope(
            PARTICIPANT_PROTOCOL_VERSION,
            "decision_request",
            snapshot_payload("decision-snapshot-v9", 1),
        );
        bad["decision_sequence"] = json!(1);
        let error = parse_platform_message(&bad).unwrap_err();
        assert!(error.to_string().contains("decision snapshot schema_version"));
    }

    #[test]
    fn v3_feedback_and_fault_status_decode() {
        let mut payload = snapshot_payload(DECISION_SNAPSHOT_VERSION, 3);
        payload["tile_last_finished"] = json!({"tile_id": "T00041", "score": 123.456});
        payload["fault_status"] = json!({
            "status": "fault", "event_id": "EV0007",
            "spatial_scope_type": "REGION_SET",
            "spatial_scope_payload": {"region_ids": ["R03"]},
            "instrument_efficiency_multiplier": 0.42,
            "reported_at_utc": "2026-10-06T02:00:00Z",
            "published_at_utc": "2026-10-06T02:00:00Z",
            "repair_complete_utc": "2026-10-08T02:00:00Z",
        });
        let mut message =
            envelope(PARTICIPANT_PROTOCOL_VERSION, "decision_request", payload);
        message["decision_sequence"] = json!(3);
        let PlatformMessage::DecisionRequest { snapshot, .. } =
            parse_platform_message(&message).unwrap()
        else {
            panic!("decision_request expected");
        };
        assert_eq!(snapshot.tile_last_finished.unwrap().score, 123.456);
        assert!(matches!(
            snapshot.fault_status,
            Some(FaultStatus::Fault { .. })
        ));
    }

    #[test]
    fn malformed_messages_are_rejected() {
        let bad_version = envelope("participant-agent-protocol-v0", "initialize", publication_payload());
        assert!(parse_platform_message(&bad_version).is_err());
        let no_payload = json!({
            "protocol_version": PARTICIPANT_PROTOCOL_VERSION,
            "message_type": "initialize",
            "payload": [],
        });
        let error = parse_platform_message(&no_payload).unwrap_err();
        assert!(error.to_string().contains("payload must be an object"));
        let unknown = envelope(PARTICIPANT_PROTOCOL_VERSION, "explode", publication_payload());
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
        let payload = json!({
            "termination_reason": "survey_complete",
            "last_decision_sequence": 3,
            "grace_seconds": 5.0,
        });
        let message =
            parse_platform_message(&envelope(PARTICIPANT_PROTOCOL_VERSION, "finish", payload))
                .unwrap();
        assert_eq!(
            message,
            PlatformMessage::Finish {
                termination_reason: "survey_complete".to_string(),
                last_decision_sequence: 3,
                grace_seconds: Some(5.0),
            }
        );
    }

    #[test]
    fn decision_response_wire_keys_match_the_old_builder() {
        let response = DecisionResponse {
            protocol_version: PROTOCOL_VERSION.to_string(),
            message_type: "decision_response".to_string(),
            decision_sequence: 7,
            action: "observe".to_string(),
            tile_id: "T00041".to_string(),
            program: "DARK".to_string(),
            request_id: "RQ0001".to_string(),
            reason: "highest public current-snapshot estimate".to_string(),
            decision_source: "deterministic".to_string(),
            reports: vec![],
        };
        let value = serde_json::to_value(&response).unwrap();
        let expected = json!({
            "protocol_version": PROTOCOL_VERSION,
            "message_type": "decision_response",
            "decision_sequence": 7,
            "action": "observe",
            "tile_id": "T00041",
            "program": "DARK",
            "request_id": "RQ0001",
            "reason": "highest public current-snapshot estimate",
            "decision_source": "deterministic",
        });
        assert_eq!(value, expected);
        let mut with_reports = response.clone();
        with_reports.reports = vec![Report::Nova { tile_id: "T00041".to_string() }];
        let value = serde_json::to_value(&with_reports).unwrap();
        assert_eq!(
            value["reports"],
            json!([{"kind": "NOVA", "tile_id": "T00041"}])
        );
    }
}
