//! Shared wire-schema types for the platform publications — the typed shape
//! of `initial-publication-v2` and its sub-documents, plus the serde helpers
//! both sides need.
//!
//! These types live in a neutral top-level module because the producer
//! (`src/workflow.rs`, engine layer — Serialize) and the consumer
//! (`src/agent/model.rs`, agent layer — Deserialize via re-export) must not
//! depend on each other. Serialize impls reproduce the exact wire shape the
//! old hand-built `json!` producer emitted (see
//! `tests/initial_publication.rs`); Deserialize keeps the agent's leniency
//! (numeric strings, tolerant timestamps).
//!
//! Config pass-throughs (`site`, `score_config`, `weather_score_interface`)
//! carry `#[serde(flatten)] extra` catch-alls so unknown scenario keys
//! round-trip untouched.

use chrono::{DateTime, Utc};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::contracts::{format_utc, parse_utc};

/// Python `float(value)`: numbers pass through, numeric strings parse
/// (`scoring_preview.py`'s `number()` / `anomaly_detection.py`'s `value_f64()`).
pub(crate) fn value_to_f64(value: &Value) -> Option<f64> {
    match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn string_or_number<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<f64, D::Error> {
    let value = Value::deserialize(deserializer)?;
    value_to_f64(&value).ok_or_else(|| serde::de::Error::custom("must be numeric"))
}

pub(crate) fn opt_string_or_number<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<f64>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value.as_ref().and_then(value_to_f64))
}

/// Strict contract-timestamp field (the old code propagated the parse error).
pub(crate) fn utc_strict<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<DateTime<Utc>, D::Error> {
    let text = String::deserialize(deserializer)?;
    parse_utc(&text).map_err(serde::de::Error::custom)
}

/// Lenient timestamp field: absent/null/unparseable → `None`
/// (`parse_utc_lenient`).
pub(crate) fn utc_lenient<'de, D: serde::Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<DateTime<Utc>>, D::Error> {
    let value = Option::<Value>::deserialize(deserializer)?;
    Ok(value
        .as_ref()
        .and_then(Value::as_str)
        .and_then(|text| parse_utc(text).ok()))
}

pub(crate) fn fmt_utc<S: serde::Serializer>(
    moment: &DateTime<Utc>,
    serializer: S,
) -> Result<S::Ok, S::Error> {
    serializer.serialize_str(&format_utc(moment))
}

/// The three observing programs. Declaration order doubles as the sort key:
/// `Backup < Bright < Dark` matches the old lexicographic string ordering
/// ("BACKUP" < "BRIGHT" < "DARK") the preview sort depends on.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, serde::Serialize, serde::Deserialize,
)]
pub enum Program {
    #[serde(rename = "BACKUP")]
    Backup,
    #[serde(rename = "BRIGHT")]
    Bright,
    #[serde(rename = "DARK")]
    Dark,
}

impl Program {
    pub fn as_str(&self) -> &'static str {
        match self {
            Program::Backup => "BACKUP",
            Program::Bright => "BRIGHT",
            Program::Dark => "DARK",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
pub enum SchedulingClass {
    #[serde(rename = "REQUIRED")]
    Required,
    #[serde(rename = "FLEXIBLE")]
    Flexible,
}

impl SchedulingClass {
    pub fn as_str(&self) -> &'static str {
        match self {
            SchedulingClass::Required => "REQUIRED",
            SchedulingClass::Flexible => "FLEXIBLE",
        }
    }

    /// Contract label → enum (the producer side reads CSV strings).
    pub fn from_label(label: &str) -> Option<Self> {
        match label {
            "REQUIRED" => Some(Self::Required),
            "FLEXIBLE" => Some(Self::Flexible),
            _ => None,
        }
    }
}

/// The one-time public bootstrap (`initial-publication-v2`).
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct InitialPublication {
    pub schema_version: String,
    pub calendar: CalendarSummary,
    pub site: Site,
    pub tile_catalog: TileCatalog,
    pub target_catalog: Vec<TargetRow>,
    pub scoring_contract: ScoringContract,
    #[serde(deserialize_with = "string_or_number")]
    pub global_wallclock_seconds: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct CalendarSummary {
    pub first_night: String,
    pub last_night: String,
    pub night_count: i64,
    pub slot_count: i64,
    pub slot_duration_seconds: i64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Site {
    #[serde(deserialize_with = "string_or_number")]
    pub latitude_deg: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub longitude_deg: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub utc_offset_hours: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub sun_altitude_limit_deg: f64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TileCatalog {
    pub tile_count: i64,
    pub required_tile_ids: Vec<String>,
    pub region_ids: Vec<String>,
    pub tiles: Vec<CatalogTile>,
}

/// One catalog tile. Wire quirks (preserved by the custom Serialize):
/// `ra_deg`/`dec_deg` are 6-decimal STRINGS (CSV-formatted; the Python agent
/// parses them back to floats — emitting raw f64 would hand it full precision
/// and change detector math), the counts are i64 numbers, and
/// `tile_science_value` is an f64 number.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CatalogTile {
    pub tile_id: String,
    /// Arrives as a numeric STRING on the wire (CSV-formatted float).
    #[serde(deserialize_with = "string_or_number")]
    pub ra_deg: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub dec_deg: f64,
    pub nominal_exptime_seconds: i64,
    pub region_id: String,
    pub scheduling_class: SchedulingClass,
    pub available_from_utc: String,
    pub available_until_utc: String,
    #[serde(default)]
    pub n_lrg: i64,
    #[serde(default)]
    pub n_elg: i64,
    #[serde(default)]
    pub n_qso: i64,
    #[serde(default)]
    pub n_bgs: i64,
    #[serde(deserialize_with = "string_or_number")]
    pub tile_science_value: f64,
}

impl serde::Serialize for CatalogTile {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        #[derive(serde::Serialize)]
        struct Wire<'a> {
            tile_id: &'a str,
            ra_deg: String,
            dec_deg: String,
            nominal_exptime_seconds: i64,
            region_id: &'a str,
            scheduling_class: SchedulingClass,
            available_from_utc: &'a str,
            available_until_utc: &'a str,
            n_lrg: i64,
            n_elg: i64,
            n_qso: i64,
            n_bgs: i64,
            tile_science_value: f64,
        }
        Wire {
            tile_id: &self.tile_id,
            // The same formatting path `geometry::Tile::csv_row` uses.
            ra_deg: format!("{:.6}", self.ra_deg),
            dec_deg: format!("{:.6}", self.dec_deg),
            nominal_exptime_seconds: self.nominal_exptime_seconds,
            region_id: &self.region_id,
            scheduling_class: self.scheduling_class,
            available_from_utc: &self.available_from_utc,
            available_until_utc: &self.available_until_utc,
            n_lrg: self.n_lrg,
            n_elg: self.n_elg,
            n_qso: self.n_qso,
            n_bgs: self.n_bgs,
            tile_science_value: self.tile_science_value,
        }
        .serialize(serializer)
    }
}

/// One target-catalog row (`TARGET_COLUMNS`); all cells stay strings.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct TargetRow {
    pub target_id: String,
    pub tile_id: String,
    pub target_class: String,
    pub feature_flux: String,
    pub redshift: String,
    pub science_weight: String,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScoringContract {
    pub score_config: ScoreConfig,
    pub weather_score_interface: WeatherScoreInterface,
    /// Unread by the agent — stays a pass-through.
    #[serde(default)]
    pub lunar_model: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub preview_semantics: Option<String>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ScoreConfig {
    pub schema_version: String,
    pub quality_thresholds: QualityThresholds,
    pub program_bonus: ProgramBonus,
    pub penalties: Penalties,
    #[serde(default)]
    pub flexible_quota_per_region: i64,
    #[serde(
        default,
        deserialize_with = "opt_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub coverage_bonus_weight: Option<f64>,
    // Present in the shipped configs but unread by the agent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub repeat_observation: Option<RepeatObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reporting: Option<Reporting>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub anomaly_tags: Option<AnomalyTags>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub fault_response: Option<FaultResponse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub one_ordinary_credit_per_tile: Option<bool>,
    #[serde(
        default,
        deserialize_with = "opt_string_or_number",
        skip_serializing_if = "Option::is_none"
    )]
    pub interrupted_exposure_science_score: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub coefficient_status: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct QualityThresholds {
    #[serde(deserialize_with = "string_or_number")]
    pub dark: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub bright: f64,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ProgramBonus {
    #[serde(rename = "DARK", deserialize_with = "string_or_number")]
    pub dark: f64,
    #[serde(rename = "BRIGHT", deserialize_with = "string_or_number")]
    pub bright: f64,
    #[serde(rename = "BACKUP", deserialize_with = "string_or_number")]
    pub backup: f64,
}

impl ProgramBonus {
    pub fn for_program(&self, program: Program) -> f64 {
        match program {
            Program::Dark => self.dark,
            Program::Bright => self.bright,
            Program::Backup => self.backup,
        }
    }
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Penalties {
    #[serde(deserialize_with = "string_or_number")]
    pub unsafe_observation: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub invalid_action: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub avoidable_wait_per_second: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub required_miss: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub flexible_shortfall_per_tile: f64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct RepeatObservation {
    #[serde(default)]
    pub tile_score_aggregation: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Reporting {
    #[serde(deserialize_with = "string_or_number")]
    pub reward_correct: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub penalty_wrong: f64,
    #[serde(default)]
    pub fault_misreport_free_allowance: i64,
    #[serde(deserialize_with = "string_or_number")]
    pub fault_misreport_penalty: f64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct AnomalyTags {
    #[serde(deserialize_with = "string_or_number")]
    pub nova_factor: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub reddening_factor: f64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct FaultResponse {
    #[serde(default)]
    pub response_latency_days: i64,
    #[serde(default)]
    pub repair_duration_days: i64,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct WeatherScoreInterface {
    #[serde(deserialize_with = "string_or_number")]
    pub airmass_exponent: f64,
    #[serde(deserialize_with = "string_or_number")]
    pub maximum_weather_quality: f64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub formula: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

/// Why a run ended. Wire values: survey_complete / global_wallclock_expired /
/// agent_error / agent_initialization_error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub enum TerminationReason {
    #[serde(rename = "survey_complete")]
    SurveyComplete,
    #[serde(rename = "global_wallclock_expired")]
    GlobalWallclockExpired,
    #[serde(rename = "agent_error")]
    AgentError,
    #[serde(rename = "agent_initialization_error")]
    AgentInitializationError,
}

impl TerminationReason {
    pub fn as_str(&self) -> &'static str {
        match self {
            TerminationReason::SurveyComplete => "survey_complete",
            TerminationReason::GlobalWallclockExpired => "global_wallclock_expired",
            TerminationReason::AgentError => "agent_error",
            TerminationReason::AgentInitializationError => "agent_initialization_error",
        }
    }
}

/// One commit-log entry (enum with associated state; `committed` is the bool
/// tag on the wire).
#[derive(Debug, Clone, PartialEq)]
pub enum CommitLogEntry {
    Committed {
        sequence: i64,
        completed_wallclock_seconds: f64,
        decision_id: String,
        outcome: String,
        /// Report-settlement records; typed later with ScoreReport.
        report_outcomes: Vec<Value>,
        dropped_reports: i64,
    },
    Failed {
        sequence: i64,
        error: String,
    },
}

impl serde::Serialize for CommitLogEntry {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        // Mirrors the old conditional json! mutation exactly: "reports" rides
        // only when non-empty, "dropped_reports" only when > 0.
        let mut map = serde_json::Map::new();
        match self {
            CommitLogEntry::Committed {
                sequence,
                completed_wallclock_seconds,
                decision_id,
                outcome,
                report_outcomes,
                dropped_reports,
            } => {
                map.insert("sequence".to_string(), json!(sequence));
                map.insert("committed".to_string(), json!(true));
                map.insert(
                    "completed_wallclock_seconds".to_string(),
                    json!(completed_wallclock_seconds),
                );
                map.insert("decision_id".to_string(), json!(decision_id));
                map.insert("outcome".to_string(), json!(outcome));
                if !report_outcomes.is_empty() {
                    map.insert("reports".to_string(), json!(report_outcomes));
                }
                if *dropped_reports > 0 {
                    map.insert("dropped_reports".to_string(), json!(dropped_reports));
                }
            }
            CommitLogEntry::Failed { sequence, error } => {
                map.insert("sequence".to_string(), json!(sequence));
                map.insert("committed".to_string(), json!(false));
                map.insert("error".to_string(), json!(error));
            }
        }
        map.serialize(serializer)
    }
}

/// workflow-result-v2.
#[derive(Debug, Clone, serde::Serialize)]
pub struct WorkflowResult {
    pub schema_version: String,
    /// `None` = stripped (`--keep-initial-publication` off): key omitted.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub initial_publication: Option<InitialPublication>,
    pub termination_reason: TerminationReason,
    pub global_wallclock_seconds: f64,
    pub accounted_wallclock_seconds: f64,
    pub ignored_in_flight_response: bool,
    pub committed_action_count: usize,
    pub commit_log: Vec<CommitLogEntry>,
    /// Raw score-report-v3 document for now; a typed ScoreReport is a
    /// separate future step.
    pub score_report: Value,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn catalog_tile() -> CatalogTile {
        CatalogTile {
            tile_id: "T00041".to_string(),
            ra_deg: 37.505464,
            dec_deg: -1.532928,
            nominal_exptime_seconds: 450,
            region_id: "R03".to_string(),
            scheduling_class: SchedulingClass::Required,
            available_from_utc: "2026-10-06T02:00:00Z".to_string(),
            available_until_utc: "2026-10-08T10:00:00Z".to_string(),
            n_lrg: 81,
            n_elg: 92,
            n_qso: 17,
            n_bgs: 72,
            tile_science_value: 129.372331,
        }
    }

    #[test]
    fn string_or_number_accepts_both_wire_forms() {
        let from_string: CatalogTile = serde_json::from_value(json!({
            "tile_id": "T00041", "ra_deg": "37.505464", "dec_deg": "-1.532928",
            "nominal_exptime_seconds": 450, "region_id": "R03",
            "scheduling_class": "REQUIRED",
            "available_from_utc": "2026-10-06T02:00:00Z",
            "available_until_utc": "2026-10-08T10:00:00Z",
            "n_lrg": 81, "n_elg": 92, "n_qso": 17, "n_bgs": 72,
            "tile_science_value": 129.372331,
        }))
        .unwrap();
        assert_eq!(from_string, catalog_tile());
        let from_number: Site = serde_json::from_value(json!({
            "latitude_deg": 31.9634, "longitude_deg": -111.599,
            "utc_offset_hours": -7, "sun_altitude_limit_deg": "-12.0",
        }))
        .unwrap();
        assert_eq!(from_number.sun_altitude_limit_deg, -12.0);
        assert!(from_number.extra.is_empty());
    }

    #[test]
    fn catalog_tile_serializes_the_quirky_wire_shape() {
        let value = serde_json::to_value(catalog_tile()).unwrap();
        // ra/dec are 6-decimal strings, counts are numbers, science value f64.
        assert_eq!(value["ra_deg"], json!("37.505464"));
        assert_eq!(value["dec_deg"], json!("-1.532928"));
        assert_eq!(value["nominal_exptime_seconds"], json!(450));
        assert_eq!(value["n_lrg"], json!(81));
        assert_eq!(value["tile_science_value"], json!(129.372331));
        assert_eq!(value["scheduling_class"], json!("REQUIRED"));
    }

    #[test]
    fn program_order_matches_the_old_string_sort() {
        let mut programs = [Program::Dark, Program::Backup, Program::Bright];
        programs.sort();
        assert_eq!(programs, [Program::Backup, Program::Bright, Program::Dark]);
        let mut names = ["DARK", "BACKUP", "BRIGHT"];
        names.sort();
        let as_strings: Vec<&str> = programs.iter().map(Program::as_str).collect();
        assert_eq!(as_strings, names);
    }

    #[test]
    fn publication_round_trip_preserves_everything() {
        let mut config_extra = serde_json::Map::new();
        config_extra.insert("future_key".to_string(), json!({"nested": [1, 2.5, "x"]}));
        let publication = InitialPublication {
            schema_version: "initial-publication-v2".to_string(),
            calendar: CalendarSummary {
                first_night: "2026-10-06".to_string(),
                last_night: "2026-10-12".to_string(),
                night_count: 7,
                slot_count: 294,
                slot_duration_seconds: 900,
            },
            site: Site {
                latitude_deg: 31.9634,
                longitude_deg: -111.599,
                utc_offset_hours: -7.0,
                sun_altitude_limit_deg: -12.0,
                extra: serde_json::Map::new(),
            },
            tile_catalog: TileCatalog {
                tile_count: 1,
                required_tile_ids: vec!["T00041".to_string()],
                region_ids: vec!["R03".to_string()],
                tiles: vec![catalog_tile()],
            },
            target_catalog: vec![TargetRow {
                target_id: "TG1".to_string(),
                tile_id: "T00041".to_string(),
                target_class: "LRG".to_string(),
                feature_flux: "0.42294598".to_string(),
                redshift: "0.819411".to_string(),
                science_weight: "1.0000".to_string(),
            }],
            scoring_contract: ScoringContract {
                score_config: ScoreConfig {
                    schema_version: "challenge-score-v3".to_string(),
                    quality_thresholds: QualityThresholds { dark: 0.65, bright: 0.4 },
                    program_bonus: ProgramBonus { dark: 0.25, bright: 0.15, backup: 0.08 },
                    penalties: Penalties {
                        unsafe_observation: 2000.0,
                        invalid_action: 100.0,
                        avoidable_wait_per_second: 0.001,
                        required_miss: 1000.0,
                        flexible_shortfall_per_tile: 100.0,
                        extra: serde_json::Map::new(),
                    },
                    flexible_quota_per_region: 4,
                    coverage_bonus_weight: Some(0.35),
                    repeat_observation: Some(RepeatObservation {
                        tile_score_aggregation: Some("max".to_string()),
                        extra: serde_json::Map::new(),
                    }),
                    reporting: Some(Reporting {
                        reward_correct: 100.0,
                        penalty_wrong: 150.0,
                        fault_misreport_free_allowance: 1,
                        fault_misreport_penalty: 100.0,
                        extra: serde_json::Map::new(),
                    }),
                    anomaly_tags: Some(AnomalyTags {
                        nova_factor: 1.5,
                        reddening_factor: 0.8,
                        extra: serde_json::Map::new(),
                    }),
                    fault_response: Some(FaultResponse {
                        response_latency_days: 1,
                        repair_duration_days: 2,
                        extra: serde_json::Map::new(),
                    }),
                    one_ordinary_credit_per_tile: None,
                    interrupted_exposure_science_score: Some(0.0),
                    coefficient_status: Some("provisional".to_string()),
                    extra: config_extra,
                },
                weather_score_interface: WeatherScoreInterface {
                    airmass_exponent: 1.0,
                    maximum_weather_quality: 3.0,
                    formula: Some("H(...)".to_string()),
                    extra: serde_json::Map::new(),
                },
                lunar_model: json!({"maximum_penalty": 0.75}),
                preview_semantics: Some("semantics".to_string()),
            },
            global_wallclock_seconds: 900.0,
        };
        let value = serde_json::to_value(&publication).unwrap();
        // Unknown config keys survive the round-trip through the typed layer.
        assert_eq!(value["scoring_contract"]["score_config"]["future_key"], json!({"nested": [1, 2.5, "x"]}));
        // None options stay absent, exactly like the pass-through producer.
        assert!(value["scoring_contract"]["score_config"].get("one_ordinary_credit_per_tile").is_none());
        let back: InitialPublication = serde_json::from_value(value).unwrap();
        assert_eq!(back, publication);
    }

    #[test]
    fn commit_log_entry_wire_shape() {
        let bare = CommitLogEntry::Committed {
            sequence: 3,
            completed_wallclock_seconds: 0.123,
            decision_id: "D000003".to_string(),
            outcome: "completed".to_string(),
            report_outcomes: vec![],
            dropped_reports: 0,
        };
        assert_eq!(
            serde_json::to_value(&bare).unwrap(),
            json!({
                "sequence": 3,
                "committed": true,
                "completed_wallclock_seconds": 0.123,
                "decision_id": "D000003",
                "outcome": "completed",
            })
        );
        // reports ride only when non-empty; dropped_reports only when > 0.
        let with_reports = CommitLogEntry::Committed {
            sequence: 3,
            completed_wallclock_seconds: 0.123,
            decision_id: "D000003".to_string(),
            outcome: "completed".to_string(),
            report_outcomes: vec![json!({"result": "correct"})],
            dropped_reports: 2,
        };
        let value = serde_json::to_value(&with_reports).unwrap();
        assert_eq!(value["reports"], json!([{"result": "correct"}]));
        assert_eq!(value["dropped_reports"], json!(2));
        let failed = CommitLogEntry::Failed { sequence: 4, error: "boom".to_string() };
        assert_eq!(
            serde_json::to_value(&failed).unwrap(),
            json!({"sequence": 4, "committed": false, "error": "boom"})
        );
    }

    #[test]
    fn termination_reason_and_workflow_result_wire() {
        assert_eq!(
            serde_json::to_value(TerminationReason::SurveyComplete).unwrap(),
            json!("survey_complete")
        );
        assert_eq!(
            serde_json::to_value(TerminationReason::GlobalWallclockExpired).unwrap(),
            json!("global_wallclock_expired")
        );
        assert_eq!(
            serde_json::to_value(TerminationReason::AgentError).unwrap(),
            json!("agent_error")
        );
        assert_eq!(
            serde_json::to_value(TerminationReason::AgentInitializationError).unwrap(),
            json!("agent_initialization_error")
        );
        assert_eq!(TerminationReason::SurveyComplete.as_str(), "survey_complete");

        let stripped = WorkflowResult {
            schema_version: "workflow-result-v2".to_string(),
            initial_publication: None,
            termination_reason: TerminationReason::GlobalWallclockExpired,
            global_wallclock_seconds: 900.0,
            accounted_wallclock_seconds: 900.0,
            ignored_in_flight_response: true,
            committed_action_count: 12,
            commit_log: vec![],
            score_report: json!({"schema_version": "score-report-v3"}),
        };
        let value = serde_json::to_value(&stripped).unwrap();
        // initial_publication None omits the key entirely.
        assert!(value.get("initial_publication").is_none());
        assert_eq!(value["termination_reason"], json!("global_wallclock_expired"));
        assert_eq!(value["committed_action_count"], json!(12));
    }
}
