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
    QualityThresholds, SchedulingClass, ScoreConfig, ScoringContract, Site, TargetRow, TileCatalog,
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
///
/// The astronomy: competition scenarios hide per-tile truth tags that the
/// scorer applies silently. The agent can only suspect them when a tile's
/// realized score deviates from the public-formula estimate — reporting one
/// correctly earns +100, a wrong report costs −150.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind")]
pub enum Report {
    /// The instrument itself is malfunctioning: an unannounced, never-forecast
    /// fault collapses the efficiency of every observation in its scope (down
    /// to ×0.10) and does not end on its own. A correct report publishes the
    /// fault one day later and completes the repair after two; misreports
    /// beyond the free allowance cost 100 each.
    #[serde(rename = "Instrument_Failure")]
    InstrumentFailure,
    /// A nova: a star in the tile suddenly flared up. The hidden tag
    /// multiplies the tile's score ×1.5 — the exposure is worth *more* than
    /// the public catalog says.
    #[serde(rename = "NOVA")]
    Nova { tile_id: String },
    /// Reddening: interstellar dust along the line of sight dims and reddens
    /// the tile's light. The hidden tag multiplies the tile's score ×0.8.
    #[serde(rename = "Reddening")]
    Reddening { tile_id: String },
}

/// A fault's spatial footprint: which part of the sky (or the whole
/// instrument) suffers the efficiency collapse. Payload keys mirror
/// `src/weather.rs`'s `applies` per scope type. Numeric payload values stay
/// numeric-string tolerant.
#[derive(Debug, Clone, PartialEq)]
pub enum FaultScope {
    /// The whole site is affected — every tile scores less.
    All,
    /// Whole survey regions are affected. Regions are the survey's 8 stripes
    /// of right ascension; tiles inherit their region from their coordinates.
    RegionSet { region_ids: Vec<String> },
    /// A circular cap on the celestial sphere, in ICRS equatorial
    /// coordinates — the sky-fixed frame: `ra_deg` is right ascension
    /// (celestial "longitude", 0–360°), `dec_deg` declination (celestial
    /// "latitude", −90…+90°), `radius_deg` the cap's angular radius. A tile
    /// is inside when its angular distance from the cap center ≤ radius.
    SkyCapIcrs {
        ra_deg: f64,
        dec_deg: f64,
        radius_deg: f64,
    },
    /// A sector of the local sky in horizontal (alt-az) coordinates — the
    /// site-fixed frame: altitude is the angle above the horizon, azimuth
    /// the compass bearing (0° = north, 90° = east; the span may wrap
    /// through north).
    HorizonSector {
        min_altitude_deg: f64,
        max_altitude_deg: f64,
        azimuth_start_deg: f64,
        azimuth_end_deg: f64,
    },
    /// An explicit list of affected tiles.
    TileSet { tile_ids: Vec<String> },
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
            "REGION_SET" => Ok(Self::RegionSet {
                region_ids: string_list("region_ids"),
            }),
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
            "TILE_SET" => Ok(Self::TileSet {
                tile_ids: string_list("tile_ids"),
            }),
            other => Err(format!("unsupported spatial scope type {other:?}")),
        }
    }
}

/// The night-start fault feed: an acknowledged unrepaired fault, or a
/// one-shot "instrument normal" answer to a misreport. Published only on the
/// first decision of a night, one simulated day after the report.
#[derive(Debug, Clone, PartialEq)]
pub enum FaultStatus {
    Fault {
        /// The fault event's identifier in the scenario's event catalogue.
        event_id: String,
        /// Which part of the sky/instrument the fault degrades.
        scope: FaultScope,
        /// The efficiency factor currently applied to every affected
        /// exposure (e.g. 0.42, down to 0.10) — this is the multiplier the
        /// agent could only infer from realized-vs-expected score ratios
        /// before the report was acknowledged.
        instrument_efficiency_multiplier: f64,
        /// When the agent's correct report landed.
        reported_at_utc: Option<DateTime<Utc>>,
        /// When this status was (first) published.
        published_at_utc: Option<DateTime<Utc>>,
        /// When the repair completes (two simulated days after the report);
        /// the feed re-publishes nightly until then.
        repair_complete_utc: Option<DateTime<Utc>>,
    },
    Normal {
        /// The misreport this answers ("there is no active fault").
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

/// The per-decision platform snapshot (v2 practice or v3 anomaly mechanics):
/// everything the agent may know at this instant — nothing about the future.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct DecisionSnapshot {
    /// Snapshot contract: v2 on practice scenarios (pre-anomaly rules), v3
    /// under the competition mechanics (feedback, reports, faults).
    pub schema_version: SnapshotSchema,
    /// The sequence number the response envelope must echo.
    pub decision_sequence: i64,
    /// Where the simulated clock stands right now.
    pub cursor: Cursor,
    /// Site-wide weather this slot (dome open? seeing? sky darkness?).
    pub current_site_weather: WeatherView,
    /// Tiles that could legally start an exposure now: their visibility
    /// window contains the cursor and they clear the altitude limit.
    #[serde(default)]
    pub candidate_tiles: Vec<CandidateTile>,
    /// Issued, unexpired observation requests with per-tile visit progress.
    #[serde(default)]
    pub active_requests: Vec<ActiveRequest>,
    /// Tonight's per-tile visibility windows — first decision of the night
    /// only (`None` otherwise).
    #[serde(default)]
    pub night_start: Option<NightStart>,
    /// Forecast revisions + multi-night windows + requests — every 7th
    /// night only (`None` otherwise).
    #[serde(default)]
    pub weekly: Option<WeeklyPublication>,
    /// What the survey has banked so far.
    pub progress: Progress,
    /// Realized official score of the most recently finished exposure — the
    /// anomaly signal: compare it against the public-formula estimate to
    /// expose hidden tags and faults. v3 only; absent/null on v2.
    #[serde(default)]
    pub tile_last_finished: Option<TileFeedback>,
    /// The fault feed (see `FaultStatus`). v3 night-open only; absent/null
    /// otherwise.
    #[serde(default)]
    pub fault_status: Option<FaultStatus>,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct Cursor {
    /// Current slot: the night is discretized into 900-second slots between
    /// dusk and dawn (`<night_id>-S<nnn>`).
    pub slot_id: String,
    /// Current observing night (`N<yyyymmdd>`), counted from local dusk.
    pub night_id: String,
    /// Wall time of the cursor.
    #[serde(deserialize_with = "utc_strict", serialize_with = "fmt_utc")]
    pub timestamp_utc: DateTime<Utc>,
    /// Elapsed seconds inside the current slot — an exposure may cross slot
    /// boundaries, so the cursor is not always at a slot start.
    pub slot_offset_seconds: i64,
}

/// The agent-visible weather view. `instrument_efficiency` is stripped under
/// the v3 mechanics (present on legacy practice snapshots): the agent must
/// infer the hidden instrument side from realized-vs-expected score ratios
/// instead. The quality factors are `None` when the slot is force-closed.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WeatherView {
    /// Is the dome open this slot? Starting an observation while `false` is
    /// an unsafe observation: −2000.
    pub is_observable: bool,
    /// Atmospheric seeing: the angular size of the turbulence blur disk, in
    /// arcseconds. Smaller is sharper; the score divides by it.
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub seeing_arcsec: Option<f64>,
    /// Fraction of starlight surviving atmospheric extinction (clouds,
    /// haze), 0–1; multiplies the score.
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub transparency: Option<f64>,
    /// Sky-background darkness (moonlight, light pollution), 0–1;
    /// multiplies the score.
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub sky_quality: Option<f64>,
    /// The instrument's own throughput factor (per-slot jitter × fault
    /// multiplier). Never shown under v3 — see the module's anomaly design.
    #[serde(default, deserialize_with = "opt_string_or_number")]
    pub instrument_efficiency: Option<f64>,
}

/// One tile that could be observed right now, with everything needed to
/// estimate its score under the public formula.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CandidateTile {
    /// A tile is one fixed survey pointing (a field on the sky) containing
    /// many targets.
    pub tile_id: String,
    /// The tile's survey region (one of 8 RA stripes); coverage evenness and
    /// region-scoped events key off this.
    pub region_id: String,
    /// REQUIRED (never completing it costs 1000) or FLEXIBLE (regions want a
    /// quota of 4 completed tiles).
    pub scheduling_class: SchedulingClass,
    /// The exposure duration an `observe` of this tile runs from the cursor.
    pub nominal_exptime_seconds: i64,
    /// Σ `science_weight` over the tile's targets — the score's `V_tile`.
    #[serde(deserialize_with = "string_or_number")]
    pub tile_science_value: f64,
    /// Tonight's visibility window for this tile: while it sits above the
    /// site's altitude limit and inside its availability dates.
    #[serde(default)]
    pub window_start_utc: String,
    #[serde(deserialize_with = "utc_strict")]
    pub window_end_utc: DateTime<Utc>,
    /// Where the tile is in the sky at the cursor (drives the airmass and
    /// moonlight terms of the score).
    pub geometry: CandidateGeometry,
    /// Site weather overlaid with the directional events applying to this
    /// tile — can be worse than `current_site_weather` (a storm cell over
    /// one horizon sector closes only those tiles).
    pub effective_weather: WeatherView,
    /// Already banked by a previous completed exposure. Under v3, repeats
    /// are legal and bank the per-tile maximum.
    #[serde(default)]
    pub already_completed: bool,
}

#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CandidateGeometry {
    /// Elevation above the horizon right now, in degrees. Candidates always
    /// clear the site's minimum (30°); higher means less atmosphere to look
    /// through.
    #[serde(deserialize_with = "string_or_number")]
    pub altitude_deg: f64,
    /// Airmass: the optical path length through the atmosphere relative to
    /// the zenith (Kasten–Young formula, normalized so the zenith is 1.0;
    /// ≈2.0 at 30° altitude). The score divides by it — low tiles score less.
    #[serde(deserialize_with = "string_or_number")]
    pub airmass: f64,
    /// Moonlight penalty, 0–1 (1.0 = dark sky): grows with lunar illumination
    /// and altitude, and shrinks with the angular separation between the
    /// moon and the tile. Multiplies the score.
    #[serde(deserialize_with = "string_or_number")]
    pub lunar_quality_factor: f64,
}

/// A time-limited request to observe specific tiles before a deadline;
/// issued every few nights with some probability.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct ActiveRequest {
    pub request_id: String,
    /// Expiry: an uncompleted request past this moment costs `miss_penalty`
    /// (waived when no legal opportunity ever existed).
    #[serde(default)]
    pub deadline_utc: String,
    /// Banked when the request completes.
    #[serde(deserialize_with = "string_or_number")]
    pub completion_reward: f64,
    /// Charged when it expires uncompleted.
    #[serde(deserialize_with = "string_or_number")]
    pub miss_penalty: f64,
    /// How many tiles the completion mode needs (all listed tiles for `ALL`,
    /// fewer for `AT_LEAST_N`).
    #[serde(default)]
    pub required_tile_count: i64,
    /// How many of them already have enough visits.
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
    /// Visits the request demands for this tile (a visit = one completed
    /// observation issued after the request).
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

/// One observing night with its per-tile visibility windows, published on the
/// first decision of the night.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NightStart {
    /// The night's calendar row (dusk/dawn bounds).
    #[serde(default)]
    pub night: NightInfo,
    /// Tonight's visibility window per tile (already filtered to runs long
    /// enough for the tile's exposure).
    #[serde(default)]
    pub tile_windows: Vec<TileWindow>,
}

/// One row of the night calendar: the solar bounds of one observing night.
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct NightInfo {
    /// Night identifier (`N<yyyymmdd>`, counted from local dusk).
    #[serde(default)]
    pub night_id: String,
    /// Calendar date of the dusk that opens the night.
    #[serde(default)]
    pub night_date: String,
    /// When the sun drops below the site's altitude limit (−12°) — the
    /// astronomical-dark window opens.
    #[serde(default)]
    pub solar_dusk_utc: String,
    /// When the sun rises back above it — the window closes.
    #[serde(default)]
    pub solar_dawn_utc: String,
    /// Dusk/dawn snapped to whole 900-second slots: the actual observable
    /// interval.
    #[serde(default)]
    pub observing_start_utc: String,
    #[serde(default)]
    pub observing_end_utc: String,
    /// Length of the observable interval in seconds.
    #[serde(default)]
    pub night_seconds: i64,
    /// Number of 900-second slots in it.
    #[serde(default)]
    pub slot_count: i64,
}

/// The 7-night lookahead, published every 7th night: forecast revisions and
/// tile windows issued so far (nothing about the future beyond the forecast
/// horizon).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct WeeklyPublication {
    /// Latest forecast revision per event, issued up to now. Uncertain on
    /// purpose: ~12% of events are never forecast, false alarms exist, and
    /// instrument faults NEVER appear here.
    #[serde(default)]
    pub weather_forecast: Vec<Forecast>,
    /// Per-tile visibility windows for the nights ahead.
    #[serde(default)]
    pub tile_windows: Vec<TileWindow>,
    /// Requests visible as of the publication time (raw, without the visit
    /// progress enrichment `active_requests` carries).
    #[serde(default)]
    pub observation_requests: Vec<PublishedRequest>,
}

/// One request as published in the weekly lookahead (mirrors the producer's
/// `requests::RequestPublication` wire shape).
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PublishedRequest {
    #[serde(default)]
    pub request_id: String,
    /// When the request was issued.
    #[serde(default)]
    pub issued_at_utc: String,
    /// Observations before this moment do not count as visits.
    #[serde(default)]
    pub available_from_utc: String,
    #[serde(default)]
    pub deadline_utc: String,
    /// Urgency class: `ONE_WEEK` / `TWO_WEEKS` / `ONE_MONTH`.
    #[serde(default)]
    pub deadline_class: String,
    /// Completion mode: `ALL` tiles or `AT_LEAST_N` of them.
    #[serde(default)]
    pub completion_mode: String,
    #[serde(default)]
    pub required_tile_count: i64,
    #[serde(default, deserialize_with = "crate::schema::opt_string_or_number")]
    pub completion_reward: Option<f64>,
    #[serde(default, deserialize_with = "crate::schema::opt_string_or_number")]
    pub miss_penalty: Option<f64>,
    /// Free-text reason shown to the agent.
    #[serde(default)]
    pub reason: String,
    #[serde(default)]
    pub tile_requirements: Vec<PublishedRequestTile>,
}

#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct PublishedRequestTile {
    pub tile_id: String,
    /// Visits the request demands for this tile.
    #[serde(default)]
    pub required_visits: i64,
}

/// One forecast revision for a weather event: a *prediction* of when a
/// condition will affect the site, revised daily and getting more accurate
/// as the event approaches.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Forecast {
    /// The event kind: `rainy` / `cloudy` / `smoggy` / `rocket_launch` /
    /// `cold_wave` / `tornado`. The detector cares about `cold_wave`
    /// specifically: it causes a *legitimate* instrument-efficiency dip that
    /// must not be mistaken for a fault.
    #[serde(default)]
    pub condition: String,
    /// Predicted onset of the event.
    #[serde(default, deserialize_with = "utc_lenient")]
    pub predicted_start_utc: Option<DateTime<Utc>>,
    /// Predicted end of the event.
    #[serde(default, deserialize_with = "utc_lenient")]
    pub predicted_end_utc: Option<DateTime<Utc>>,
}

/// One contiguous run of eligible slots for a tile within a night: the tile
/// is above the altitude limit and inside its availability dates, and the run
/// is long enough for the tile's nominal exposure.
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

/// What the survey has banked so far (drives the terminal-penalty terms in
/// the preview ranking).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Progress {
    /// Tiles with at least one completed legal observation.
    #[serde(default)]
    pub completed_tile_ids: Vec<String>,
    /// Completed FLEXIBLE tiles per region — measured against the quota of 4.
    #[serde(default)]
    pub flexible_completed_by_region: BTreeMap<String, i64>,
}

/// Realized score feedback for the most recently finished exposure (`null`
/// before the first one). This is the ground truth the anomaly detector
/// compares against its efficiency-free estimate: the ratio exposes the
/// hidden instrument side (jitter × fault × tag multiplier).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct TileFeedback {
    pub tile_id: String,
    /// The official realized score (base + program bonus); 0 means the
    /// exposure was interrupted. Waits and invalid actions produce no update.
    #[serde(deserialize_with = "string_or_number")]
    pub score: f64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn report_tag_round_trip() {
        let nova = Report::Nova {
            tile_id: "T00041".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&nova).unwrap(),
            json!({"kind": "NOVA", "tile_id": "T00041"})
        );
        let parsed: Report =
            serde_json::from_value(json!({"kind": "Reddening", "tile_id": "T1"})).unwrap();
        assert_eq!(
            parsed,
            Report::Reddening {
                tile_id: "T1".to_string()
            }
        );
        let parsed: Report = serde_json::from_value(json!({"kind": "Instrument_Failure"})).unwrap();
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
        let FaultStatus::Fault {
            scope,
            repair_complete_utc,
            event_id,
            ..
        } = &fault
        else {
            panic!("fault expected");
        };
        assert_eq!(event_id, "EV0007");
        assert_eq!(
            *scope,
            FaultScope::RegionSet {
                region_ids: vec!["R03".into(), "R07".into()]
            }
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
                scope: FaultScope::SkyCapIcrs {
                    ra_deg: 12.5,
                    dec_deg: -3.0,
                    radius_deg: 4.5
                },
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
                "tile" => FaultScope::TileSet {
                    tile_ids: vec!["T1".into()],
                },
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
    fn night_start_and_weekly_decode_typed() {
        let night_start: NightStart = serde_json::from_value(json!({
            "night": {
                "night_id": "N20260907", "night_date": "2026-09-07",
                "solar_dusk_utc": "2026-09-07T02:15:00Z",
                "solar_dawn_utc": "2026-09-07T12:45:00Z",
                "observing_start_utc": "2026-09-07T02:30:00Z",
                "observing_end_utc": "2026-09-07T12:30:00Z",
                "night_seconds": 36000, "slot_count": 40,
            },
            "tile_windows": [{"tile_id": "T00001", "window_end_utc": "2026-09-07T05:00:00Z"}],
        }))
        .unwrap();
        assert_eq!(night_start.night.night_id, "N20260907");
        assert_eq!(night_start.night.slot_count, 40);
        assert_eq!(night_start.tile_windows.len(), 1);

        let weekly: WeeklyPublication = serde_json::from_value(json!({
            "issued_at_utc": "2026-09-07T02:30:00Z",
            "weather_forecast": [{
                "condition": "cold_wave",
                "predicted_start_utc": "2026-09-08T00:00:00Z",
                "predicted_end_utc": "2026-09-10T00:00:00Z",
            }],
            "tile_windows": [],
            "observation_requests": [{
                "request_id": "REQ001", "issued_at_utc": "2026-09-07T02:30:00Z",
                "available_from_utc": "2026-09-07T02:30:00Z",
                "deadline_utc": "2026-09-14T02:30:00Z",
                "deadline_class": "ONE_WEEK", "completion_mode": "ALL",
                "required_tile_count": 2, "completion_reward": 280.0, "miss_penalty": 380.0,
                "reason": "follow-up",
                "tile_requirements": [{"tile_id": "T00037", "required_visits": 1}],
            }],
        }))
        .unwrap();
        assert_eq!(weekly.weather_forecast[0].condition, "cold_wave");
        let request = &weekly.observation_requests[0];
        assert_eq!(request.request_id, "REQ001");
        assert_eq!(request.completion_reward, Some(280.0));
        assert_eq!(request.tile_requirements[0].tile_id, "T00037");
        // Absent blocks stay default (None/empty), matching the old Value tolerance.
        let sparse: WeeklyPublication = serde_json::from_value(json!({})).unwrap();
        assert!(sparse.observation_requests.is_empty() && sparse.weather_forecast.is_empty());
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
