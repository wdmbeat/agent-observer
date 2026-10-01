//! Typed schema layer for the agent side — the wire types the decision
//! pipeline consumes, replacing `serde_json::Value` inside `src/agent/`.
//! There is no Python counterpart: the Python agent passes dicts around; here
//! the inbound JSON is decoded once at the seam (`DecisionProvider::call` /
//! `parse_platform_message`) and the pipeline runs on typed values.
//!
//! Publication-side types (`InitialPublication` and friends) live in
//! `crate::schema` — shared with the producer in `src/workflow.rs` — and are
//! re-exported here so agent imports stay stable. This module keeps the
//! agent-only snapshot/decision types.
//!
//! Tolerance parity: fields the old code read through `number()` /
//! `value_f64()` keep accepting numeric strings; timestamps the old code
//! parsed leniently decode to `Option<DateTime>`.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::Value;

use crate::contracts::parse_utc;
use crate::schema::{
    fmt_utc, opt_string_or_number, string_or_number, utc_lenient, utc_strict, value_to_f64,
};

// The publication-side schema lives in `crate::schema`; re-exported so the
// agent modules keep importing from `super::model`.
pub use crate::schema::{
    CalendarSummary, CatalogTile, InitialPublication, Penalties, Program, ProgramBonus,
    QualityThresholds, ScoreConfig, ScoringContract, SchedulingClass, Site, TargetRow, TileCatalog,
    WeatherScoreInterface,
};

/// Decision-snapshot schema versions the agent accepts (practice scenarios
/// speak the pre-anomaly v2 contract).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum SnapshotSchema {
    #[serde(rename = "decision-snapshot-v2")]
    V2,
    #[serde(rename = "decision-snapshot-v3")]
    V3,
}

impl SnapshotSchema {
    pub fn as_str(&self) -> &'static str {
        match self {
            SnapshotSchema::V2 => "decision-snapshot-v2",
            SnapshotSchema::V3 => "decision-snapshot-v3",
        }
    }
}

/// One anomaly report riding a decision response; reports never consume slot
/// time. Wire shape: `{"kind": "Instrument_Failure"}` /
/// `{"kind": "NOVA" | "Reddening", "tile_id": ...}`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind")]
pub enum Report {
    #[serde(rename = "Instrument_Failure")]
    InstrumentFailure,
    #[serde(rename = "NOVA")]
    Nova { tile_id: String },
    #[serde(rename = "Reddening")]
    Reddening { tile_id: String },
}

/// A fault's spatial scope with its payload as associated state; payload keys
/// mirror `src/weather.rs`'s `applies` per scope type. Numeric payload values
/// stay numeric-string tolerant.
#[derive(Debug, Clone, PartialEq)]
pub enum FaultScope {
    All,
    RegionSet {
        region_ids: Vec<String>,
    },
    SkyCapIcrs {
        ra_deg: f64,
        dec_deg: f64,
        radius_deg: f64,
    },
    HorizonSector {
        min_altitude_deg: f64,
        max_altitude_deg: f64,
        azimuth_start_deg: f64,
        azimuth_end_deg: f64,
    },
    TileSet {
        tile_ids: Vec<String>,
    },
}

impl FaultScope {
    /// Decode the two-field wire shape (`spatial_scope_type` +
    /// `spatial_scope_payload`, payload possibly absent/null).
    pub fn from_wire(scope_type: &str, payload: Option<&Value>) -> Result<Self, String> {
        let field = |key: &str| {
            payload
                .and_then(|payload| payload.get(key))
                .and_then(value_to_f64)
                .ok_or_else(|| format!("fault scope payload {key:?} must be numeric"))
        };
        let string_list = |key: &str| {
            payload
                .and_then(|payload| payload.get(key))
                .and_then(Value::as_array)
                .map(|ids| {
                    ids.iter()
                        .filter_map(|id| id.as_str().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        };
        match scope_type {
            "ALL" => Ok(Self::All),
            "REGION_SET" => Ok(Self::RegionSet { region_ids: string_list("region_ids") }),
            "SKY_CAP_ICRS" => Ok(Self::SkyCapIcrs {
                ra_deg: field("ra_deg")?,
                dec_deg: field("dec_deg")?,
                radius_deg: field("radius_deg")?,
            }),
            "HORIZON_SECTOR" => Ok(Self::HorizonSector {
                min_altitude_deg: field("min_altitude_deg")?,
                max_altitude_deg: field("max_altitude_deg")?,
                azimuth_start_deg: field("azimuth_start_deg")?,
                azimuth_end_deg: field("azimuth_end_deg")?,
            }),
            "TILE_SET" => Ok(Self::TileSet { tile_ids: string_list("tile_ids") }),
            other => Err(format!("unsupported spatial scope type {other:?}")),
        }
    }
}

/// The night-start fault feed: an acknowledged unrepaired fault, or a
/// one-shot "instrument normal" answer to a misreport.
#[derive(Debug, Clone, PartialEq)]
pub enum FaultStatus {
    Fault {
        event_id: String,
        scope: FaultScope,
        instrument_efficiency_multiplier: f64,
        reported_at_utc: Option<DateTime<Utc>>,
        published_at_utc: Option<DateTime<Utc>>,
        repair_complete_utc: Option<DateTime<Utc>>,
    },
    Normal {
        reference_report_id: String,
        published_at_utc: Option<DateTime<Utc>>,
    },
}

impl<'de> Deserialize<'de> for FaultStatus {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let text = |key: &str| value[key].as_str().unwrap_or("").to_string();
        let moment = |key: &str| value[key].as_str().and_then(|text| parse_utc(text).ok());
        match value["status"].as_str() {
            Some("fault") => Ok(Self::Fault {
                event_id: text("event_id"),
                scope: FaultScope::from_wire(
                    value["spatial_scope_type"].as_str().unwrap_or(""),
                    value.get("spatial_scope_payload").filter(|p| !p.is_null()),
                )
                .map_err(serde::de::Error::custom)?,
                instrument_efficiency_multiplier: value_to_f64(
                    &value["instrument_efficiency_multiplier"],
                )
                .unwrap_or(0.0),
                reported_at_utc: moment("reported_at_utc"),
                published_at_utc: moment("published_at_utc"),
                repair_complete_utc: moment("repair_complete_utc"),
            }),
            Some("normal") => Ok(Self::Normal {
                reference_report_id: text("reference_report_id"),
                published_at_utc: moment("published_at_utc"),
            }),
            other => Err(serde::de::Error::custom(format!(
                "unsupported fault status {other:?}"
            ))),
        }
    }
}

/// The per-decision platform snapshot (v2 practice or v3 anomaly mechanics).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DecisionSnapshot {
    pub schema_version: SnapshotSchema,
    pub decision_sequence: i64,
    pub cursor: Cursor,
    pub current_site_weather: WeatherView,
    #[serde(default)]
    pub candidate_tiles: Vec<CandidateTile>,
    #[serde(default)]
    pub active_requests: Vec<ActiveRequest>,
    #[serde(default)]
    pub night_start: Option<NightStart>,
    #[serde(default)]
    pub weekly: Option<WeeklyPublication>,
    pub progress: Progress,
    /// v3 only; absent/null on v2.
    #[serde(default)]
    pub tile_last_finished: Option<TileFeedback>,
    /// v3 night-open only; absent/null otherwise.
    #[serde(default)]
    pub fault_status: Option<FaultStatus>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Cursor {
    pub slot_id: String,
    pub night_id: String,
    #[serde(deserialize_with = "utc_strict", serialize_with = "fmt_utc")]
    pub timestamp_utc: DateTime<Utc>,
    pub slot_offset_seconds: i64,
}

/// The agent-visible weather view: `instrument_efficiency` is stripped under
/// the v3 mechanics (present on legacy practice snapshots); the quality
/// factors are `None` when the slot is force-closed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WeatherView {
    pub is_observable: bool,
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub seeing_arcsec: Option<f64>,
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub transparency: Option<f64>,
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub sky_quality: Option<f64>,
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub instrument_efficiency: Option<f64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CandidateTile {
    pub tile_id: String,
    pub region_id: String,
    pub scheduling_class: SchedulingClass,
    pub nominal_exptime_seconds: i64,
    #[serde(deserialize_with = "string_or_number")]
    pub tile_science_value: f64,
    #[serde(default)]
    pub window_start_utc: String,
    #[serde(deserialize_with = "utc_strict")]
    pub window_end_utc: DateTime<Utc>,
    pub geometry: CandidateGeometry,
    pub effective_weather: WeatherView,
    #[serde(default)]
    pub already_completed: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CandidateGeometry {
    #[serde(deserialize_with = "string_or_number")]
    pub altitude_deg: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub airmass: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub lunar_quality_factor: f64,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ActiveRequest {
    pub request_id: String,
    #[serde(default)]
    pub deadline_utc: String,
    #[serde(deserialize_with = "string_or_number")]
    pub completion_reward: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub miss_penalty: f64,
    #[serde(default)]
    pub required_tile_count: i64,
    #[serde(default)]
    pub satisfied_tile_count: i64,
    #[serde(default)]
    pub tile_requirements: Vec<TileRequirement>,
    #[serde(default)]
    pub is_complete: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TileRequirement {
    pub tile_id: String,
    #[serde(default)]
    pub required_visits: i64,
    #[serde(default)]
    pub completed_visits: Option<i64>,
    #[serde(default)]
    pub remaining_visits: Option<i64>,
}

impl TileRequirement {
    /// `remaining_visits.or(required_visits)` — the wire carries both, but
    /// the pre-anomaly fallback order is preserved.
    pub fn remaining_or_required(&self) -> i64 {
        self.remaining_visits.unwrap_or(self.required_visits)
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NightStart {
    /// The night row is unused by the agent — shallow.
    #[serde(default)]
    pub night: Value,
    #[serde(default)]
    pub tile_windows: Vec<TileWindow>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WeeklyPublication {
    #[serde(default)]
    pub weather_forecast: Vec<Forecast>,
    #[serde(default)]
    pub tile_windows: Vec<TileWindow>,
    /// Unused by the agent — shallow.
    #[serde(default)]
    pub observation_requests: Value,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Forecast {
    #[serde(default)]
    pub condition: String,
    #[serde(default, deserialize_with = "utc_lenient")]
    pub predicted_start_utc: Option<DateTime<Utc>>,
    #[serde(default, deserialize_with = "utc_lenient")]
    pub predicted_end_utc: Option<DateTime<Utc>>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TileWindow {
    pub tile_id: String,
    #[serde(default)]
    pub window_start_utc: String,
    /// Raw string on purpose: `reference_strategy` counts windows with an
    /// UNPARSEABLE end as remaining chances (Python `end=None` never compares
    /// `<= now`) — a typed `DateTime` would drop that fidelity quirk.
    #[serde(default)]
    pub window_end_utc: String,
}

impl TileWindow {
    /// Python `_utc`: empty or unparseable → `None` (counts as a chance).
    pub fn window_end(&self) -> Option<DateTime<Utc>> {
        if self.window_end_utc.is_empty() {
            return None;
        }
        parse_utc(&self.window_end_utc).ok()
    }
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Progress {
    #[serde(default)]
    pub completed_tile_ids: Vec<String>,
    #[serde(default)]
    pub flexible_completed_by_region: BTreeMap<String, i64>,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TileFeedback {
    pub tile_id: String,
    #[serde(deserialize_with = "string_or_number")]
    pub score: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn report_tag_round_trip() {
        let nova = Report::Nova { tile_id: "T00041".to_string() };
        assert_eq!(serde_json::to_value(&nova).unwrap(), json!({"kind": "NOVA", "tile_id": "T00041"}));
        let parsed: Report = serde_json::from_value(json!({"kind": "Reddening", "tile_id": "T1"})).unwrap();
        assert_eq!(parsed, Report::Reddening { tile_id: "T1".to_string() });
        let parsed: Report =
            serde_json::from_value(json!({"kind": "Instrument_Failure"})).unwrap();
        assert_eq!(parsed, Report::InstrumentFailure);
        assert!(serde_json::from_value::<Report>(json!({"kind": "Alien"})).is_err());
    }

    #[test]
    fn fault_status_decodes_fault_and_normal() {
        let fault: FaultStatus = serde_json::from_value(json!({
            "status": "fault", "event_id": "EV0007",
            "spatial_scope_type": "REGION_SET",
            "spatial_scope_payload": {"region_ids": ["R03", "R07"]},
            "instrument_efficiency_multiplier": 0.42,
            "reported_at_utc": "2026-10-06T03:00:00Z",
            "published_at_utc": "2026-10-07T02:00:00Z",
            "repair_complete_utc": "2026-10-09T02:00:00Z",
        }))
        .unwrap();
        let FaultStatus::Fault { scope, repair_complete_utc, event_id, .. } = &fault else {
            panic!("fault expected");
        };
        assert_eq!(event_id, "EV0007");
        assert_eq!(
            *scope,
            FaultScope::RegionSet { region_ids: vec!["R03".into(), "R07".into()] }
        );
        assert!(repair_complete_utc.is_some());

        // Numeric strings stay tolerated in scope payloads.
        let cap: FaultStatus = serde_json::from_value(json!({
            "status": "fault", "event_id": "EV0001",
            "spatial_scope_type": "SKY_CAP_ICRS",
            "spatial_scope_payload": {"ra_deg": "12.5", "dec_deg": -3.0, "radius_deg": 4.5},
            "instrument_efficiency_multiplier": 0.5,
            "reported_at_utc": "2026-10-06T03:00:00Z",
            "published_at_utc": "2026-10-06T03:00:00Z",
            "repair_complete_utc": "2026-10-08T03:00:00Z",
        }))
        .unwrap();
        assert!(matches!(
            cap,
            FaultStatus::Fault {
                scope: FaultScope::SkyCapIcrs { ra_deg: 12.5, dec_deg: -3.0, radius_deg: 4.5 },
                ..
            }
        ));
        for (scope_type, payload, expect) in [
            ("ALL", json!(null), "all"),
            ("TILE_SET", json!({"tile_ids": ["T1"]}), "tile"),
            (
                "HORIZON_SECTOR",
                json!({"min_altitude_deg": 20.0, "max_altitude_deg": 60.0,
                       "azimuth_start_deg": 90.0, "azimuth_end_deg": 270.0}),
                "horizon",
            ),
        ] {
            let status: FaultStatus = serde_json::from_value(json!({
                "status": "fault", "event_id": "EV",
                "spatial_scope_type": scope_type,
                "spatial_scope_payload": payload,
                "instrument_efficiency_multiplier": 0.5,
                "reported_at_utc": "2026-10-06T03:00:00Z",
                "published_at_utc": "2026-10-06T03:00:00Z",
                "repair_complete_utc": "2026-10-08T03:00:00Z",
            }))
            .unwrap();
            let wanted = match expect {
                "all" => FaultScope::All,
                "tile" => FaultScope::TileSet { tile_ids: vec!["T1".into()] },
                _ => FaultScope::HorizonSector {
                    min_altitude_deg: 20.0,
                    max_altitude_deg: 60.0,
                    azimuth_start_deg: 90.0,
                    azimuth_end_deg: 270.0,
                },
            };
            assert!(matches!(status, FaultStatus::Fault { scope, .. } if scope == wanted));
        }

        let normal: FaultStatus = serde_json::from_value(json!({
            "status": "normal", "reference_report_id": "RPT-1",
            "published_at_utc": "2026-10-07T02:00:00Z",
        }))
        .unwrap();
        assert_eq!(
            normal,
            FaultStatus::Normal {
                reference_report_id: "RPT-1".to_string(),
                published_at_utc: parse_utc("2026-10-07T02:00:00Z").ok(),
            }
        );
    }

    #[test]
    fn snapshot_schema_and_tile_window_quirk() {
        assert_eq!(
            serde_json::from_value::<SnapshotSchema>(json!("decision-snapshot-v2")).unwrap(),
            SnapshotSchema::V2
        );
        assert_eq!(
            serde_json::from_value::<SnapshotSchema>(json!("decision-snapshot-v3")).unwrap(),
            SnapshotSchema::V3
        );
        assert!(serde_json::from_value::<SnapshotSchema>(json!("decision-snapshot-v9")).is_err());
        // The reference-strategy fidelity quirk: an unparseable end is still a chance.
        let window: TileWindow = serde_json::from_value(json!({
            "tile_id": "T1", "window_end_utc": "not a timestamp",
        }))
        .unwrap();
        assert!(window.window_end().is_none());
        let window: TileWindow = serde_json::from_value(json!({
            "tile_id": "T1", "window_end_utc": "2026-10-06T10:00:00Z",
        }))
        .unwrap();
        assert!(window.window_end().is_some());
    }
}
