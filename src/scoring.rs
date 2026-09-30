//! Authoritative replay engine for decisions — port of `challenge/scoring_core.py`.
//!
//! Float operation order mirrors the Python source; per-segment and per-action
//! values are rounded with `round6` exactly where the Python rounds, and the
//! pending/banked science accumulators stay unrounded until report time.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::{json, Value};

use crate::calendar::{load_slots, Slot};
use crate::contracts::{
    anomaly_mechanics_enabled, datetime_from_epoch, epoch_seconds, format_utc, json_f64, json_i64,
    read_exact_csv, round6, sha256_file, write_text_lf, DECISION_COLUMNS, REPORT_ACTIONS,
    TARGET_COLUMNS, TILE_ANOMALY_COLUMNS,
};
use crate::geometry::{load_tiles, Tile, TileGeometrySimulator};
use crate::requests::{load_request_tiles, load_requests, ObservationRequest};
use crate::weather::{
    load_config as load_weather_config, load_events, load_forecasts, load_weather, weather_quality,
    WeatherSimulator,
};

pub const PROGRAMS: [&str; 3] = ["DARK", "BRIGHT", "BACKUP"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub decision_id: String,
    pub slot_id: String,
    pub action: String,
    pub tile_id: String,
    pub program: String,
    pub request_id: String,
    pub reason: String,
}

impl Decision {
    pub fn csv_row(&self) -> Vec<String> {
        vec![
            self.decision_id.clone(),
            self.slot_id.clone(),
            self.action.clone(),
            self.tile_id.clone(),
            self.program.clone(),
            self.request_id.clone(),
            self.reason.clone(),
        ]
    }
}

fn report_kind(action: &str) -> Option<&'static str> {
    REPORT_ACTIONS
        .iter()
        .find(|(name, _)| *name == action)
        .map(|(_, kind)| *kind)
}

pub fn load_decisions(path: &Path, allow_reports: bool) -> Result<Vec<Decision>> {
    let rows = read_exact_csv(path, &DECISION_COLUMNS)?;
    let mut result = Vec::with_capacity(rows.len());
    let mut seen = std::collections::HashSet::new();
    for row in &rows {
        let item = Decision {
            decision_id: row[0].trim().to_string(),
            slot_id: row[1].trim().to_string(),
            action: row[2].trim().to_string(),
            tile_id: row[3].trim().to_string(),
            program: row[4].trim().to_string(),
            request_id: row[5].trim().to_string(),
            reason: row[6].trim().to_string(),
        };
        if item.decision_id.is_empty() || !seen.insert(item.decision_id.clone()) {
            bail!("decision_id must be non-empty and unique");
        }
        let is_report = report_kind(&item.action).is_some();
        if item.action != "observe" && item.action != "wait" && !is_report {
            bail!("{}: unknown action {:?}", item.decision_id, item.action);
        }
        if is_report && !allow_reports {
            bail!("{}: action must be observe or wait", item.decision_id);
        }
        if item.action == "wait"
            && (!item.tile_id.is_empty() || !item.program.is_empty() || !item.request_id.is_empty())
        {
            bail!(
                "{}: wait must not name tile, program, or request",
                item.decision_id
            );
        }
        if item.action == "observe"
            && (item.tile_id.is_empty() || !PROGRAMS.contains(&item.program.as_str()))
        {
            bail!("{}: invalid observe fields", item.decision_id);
        }
        if is_report {
            let kind = report_kind(&item.action).unwrap();
            if !item.program.is_empty() || !item.request_id.is_empty() {
                bail!(
                    "{}: report rows must not name program or request",
                    item.decision_id
                );
            }
            if (kind == "Instrument_Failure") == !item.tile_id.is_empty() {
                bail!(
                    "{}: fault reports name no tile; tag reports require one",
                    item.decision_id
                );
            }
        }
        result.push(item);
    }
    Ok(result)
}

/// Hidden per-tile anomaly tags (nova/reddening); absent file means no tags.
pub fn load_tile_anomalies(
    path: &Path,
    tiles: &HashMap<String, Tile>,
) -> Result<HashMap<String, BTreeSet<String>>> {
    let rows = read_exact_csv(path, &TILE_ANOMALY_COLUMNS)?;
    let mut tags: HashMap<String, BTreeSet<String>> = HashMap::new();
    for row in &rows {
        let tile_id = row[0].trim().to_string();
        let tag = row[1].trim().to_string();
        if !tiles.contains_key(&tile_id) {
            bail!("tile_anomalies: unknown tile_id {tile_id:?}");
        }
        if !crate::contracts::ANOMALY_TAG_VALUES.contains(&tag.as_str()) {
            bail!("tile_anomalies: unknown anomaly_tag {tag:?}");
        }
        tags.entry(tile_id).or_default().insert(tag);
    }
    Ok(tags)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Report {
    pub report_id: String,
    pub kind: String,
    pub tile_id: String,
}

pub fn load_score_config(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let config: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if config.get("schema_version").and_then(Value::as_str) != Some("challenge-score-v3") {
        bail!("unsupported score config");
    }
    let aggregation = config
        .get("repeat_observation")
        .and_then(|section| section.get("tile_score_aggregation"))
        .and_then(Value::as_str)
        .unwrap_or("max");
    if aggregation != "max" {
        bail!("unsupported repeat_observation.tile_score_aggregation {aggregation:?}");
    }
    Ok(config)
}

/// Sum of target science weights per tile.
pub fn load_tile_values(
    path: &Path,
    tiles: &HashMap<String, Tile>,
) -> Result<HashMap<String, f64>> {
    let rows = read_exact_csv(path, &TARGET_COLUMNS)?;
    let mut values: HashMap<String, f64> = HashMap::new();
    let mut seen = std::collections::HashSet::new();
    for row in &rows {
        let target_id = row[0].trim();
        let tile_id = row[1].trim();
        if target_id.is_empty()
            || !seen.insert(target_id.to_string())
            || !tiles.contains_key(tile_id)
        {
            bail!("invalid targets catalog relation");
        }
        let value: f64 = row[5].trim().parse()?;
        if !value.is_finite() || value <= 0.0 {
            bail!("science_weight must be positive and finite");
        }
        *values.entry(tile_id.to_string()).or_insert(0.0) += value;
    }
    if values.len() != tiles.len() || !tiles.keys().all(|key| values.contains_key(key)) {
        bail!("every tile must have target-derived value");
    }
    Ok(values)
}

/// Jain's fairness index over per-region completed-tile counts:
/// (Σx)² / (n·Σx²); 0 when nothing was observed.
pub fn coverage_evenness(completed_by_region: &HashMap<String, i64>, regions: &[String]) -> f64 {
    if regions.is_empty() {
        return 0.0;
    }
    let counts: Vec<f64> = regions
        .iter()
        .map(|region| completed_by_region.get(region).copied().unwrap_or(0) as f64)
        .collect();
    let total: f64 = counts.iter().sum();
    if total <= 0.0 {
        return 0.0;
    }
    let squares = counts.iter().fold(0.0, |sum, value| sum + value * value);
    (total * total) / (counts.len() as f64 * squares)
}

/// Insertion-ordered float counter with Python `Counter` semantics: `add` and
/// `set` insert missing keys (even for 0.0); `get` does not insert. `sum`
/// folds in insertion order, matching `sum(counter.values())`.
#[derive(Debug, Default)]
pub struct OrderedCounter {
    entries: Vec<(String, f64)>,
}

impl OrderedCounter {
    pub fn get(&self, key: &str) -> f64 {
        self.entries
            .iter()
            .find(|(name, _)| name == key)
            .map(|(_, value)| *value)
            .unwrap_or(0.0)
    }

    pub fn add(&mut self, key: &str, delta: f64) {
        if let Some(entry) = self.entries.iter_mut().find(|(name, _)| name == key) {
            entry.1 += delta;
        } else {
            self.entries.push((key.to_string(), delta));
        }
    }

    pub fn set(&mut self, key: &str, value: f64) {
        if let Some(entry) = self.entries.iter_mut().find(|(name, _)| name == key) {
            entry.1 = value;
        } else {
            self.entries.push((key.to_string(), value));
        }
    }

    pub fn sum(&self) -> f64 {
        self.entries.iter().fold(0.0, |sum, (_, value)| sum + value)
    }

    /// Sorted key → round6(value) map, as in the score report.
    pub fn sorted_rounded_json(&self) -> Value {
        let map: serde_json::Map<String, Value> = self
            .entries
            .iter()
            .map(|(key, value)| (key.clone(), json!(round6(*value))))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect();
        Value::Object(map)
    }
}

pub struct ChallengeScorer {
    pub slots: Vec<Slot>,
    pub slot_indices: HashMap<String, usize>,
    pub tiles: HashMap<String, Tile>,
    pub tile_values: HashMap<String, f64>,
    pub geometry: Rc<TileGeometrySimulator>,
    pub weather: WeatherSimulator,
    pub requests: HashMap<String, ObservationRequest>,
    pub request_tiles: HashMap<String, HashMap<String, i64>>,
    pub config: Value,
    pub mechanics: bool,
    pub tile_anomalies: HashMap<String, BTreeSet<String>>,
    nova_factor: f64,
    reddening_factor: f64,
    report_reward: f64,
    report_penalty: f64,
    misreport_allowance: i64,
    misreport_penalty: f64,
    repair_duration_seconds: f64,
    pub slot_index: usize,
    pub offset_seconds: i64,
    pub completed_tiles: BTreeSet<String>,
    pub tile_best_scores: HashMap<String, (f64, f64)>,
    pub request_visits: HashMap<(String, String), i64>,
    pub fault_acknowledgements: HashMap<String, DateTime<Utc>>,
    pub fault_correct_reports: i64,
    pub misreport_count: i64,
    pub misreport_total: i64,
    pub tag_reports: BTreeMap<(String, String), String>,
    pub actions: Vec<Value>,
    pub base_science_score: f64,
    pub program_bonus_score: f64,
    pub penalties: OrderedCounter,
    pub wait_seconds: HashMap<String, i64>,
}

impl ChallengeScorer {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        slots: Vec<Slot>,
        tiles: Vec<Tile>,
        tile_values: HashMap<String, f64>,
        geometry: Rc<TileGeometrySimulator>,
        weather: WeatherSimulator,
        requests: Vec<ObservationRequest>,
        request_tiles: HashMap<String, HashMap<String, i64>>,
        score_config: Value,
        tile_anomalies: Option<HashMap<String, BTreeSet<String>>>,
    ) -> Self {
        let slot_indices = slots
            .iter()
            .enumerate()
            .map(|(index, slot)| (slot.slot_id.clone(), index))
            .collect();
        let tiles: HashMap<String, Tile> = tiles
            .into_iter()
            .map(|tile| (tile.tile_id.clone(), tile))
            .collect();
        let requests = requests
            .into_iter()
            .map(|item| (item.request_id.clone(), item))
            .collect();
        let mechanics = anomaly_mechanics_enabled(&score_config);
        let tag_config = score_config
            .get("anomaly_tags")
            .cloned()
            .unwrap_or(Value::Null);
        let tag_factor = |key: &str, default: f64| {
            tag_config
                .get(key)
                .and_then(Value::as_f64)
                .unwrap_or(default)
        };
        let reporting = score_config
            .get("reporting")
            .cloned()
            .unwrap_or(Value::Null);
        let report_value = |key: &str, default: f64| {
            reporting
                .get(key)
                .and_then(Value::as_f64)
                .unwrap_or(default)
        };
        let fault_response = score_config
            .get("fault_response")
            .cloned()
            .unwrap_or(Value::Null);
        let repair_days = fault_response
            .get("repair_duration_days")
            .and_then(Value::as_f64)
            .unwrap_or(2.0);
        Self {
            slots,
            slot_indices,
            tiles,
            tile_values,
            geometry,
            weather,
            requests,
            request_tiles,
            config: score_config,
            mechanics,
            tile_anomalies: tile_anomalies.unwrap_or_default(),
            nova_factor: tag_factor("nova_factor", 1.5),
            reddening_factor: tag_factor("reddening_factor", 0.8),
            report_reward: report_value("reward_correct", 100.0),
            report_penalty: report_value("penalty_wrong", 150.0),
            misreport_allowance: reporting
                .get("fault_misreport_free_allowance")
                .and_then(Value::as_i64)
                .unwrap_or(1),
            misreport_penalty: report_value("fault_misreport_penalty", 100.0),
            repair_duration_seconds: repair_days * 86400.0,
            slot_index: 0,
            offset_seconds: 0,
            completed_tiles: BTreeSet::new(),
            tile_best_scores: HashMap::new(),
            request_visits: HashMap::new(),
            fault_acknowledgements: HashMap::new(),
            fault_correct_reports: 0,
            misreport_count: 0,
            misreport_total: 0,
            tag_reports: BTreeMap::new(),
            actions: Vec::new(),
            base_science_score: 0.0,
            program_bonus_score: 0.0,
            penalties: OrderedCounter::default(),
            wait_seconds: HashMap::new(),
        }
    }

    pub fn from_files(root: &Path) -> Result<Self> {
        let config = root.join("config");
        let output = root.join("outputs").join("reference");
        let tiles = load_tiles(&output.join("tiles.csv"))?;
        let geometry = Rc::new(TileGeometrySimulator::from_files(
            &output.join("tiles.csv"),
            &config.join("tile_config.json"),
            &config.join("calendar_config.json"),
            &output.join("night_calendar.csv"),
            &output.join("slots.csv"),
        )?);
        let weather = WeatherSimulator::new(
            load_weather(&output.join("weather.csv"))?,
            load_forecasts(&output.join("weather_forecasts.csv"))?,
            load_events(&output.join("weather_events.csv"))?,
            load_weather_config(&config.join("weather_config.json"))?,
            Some(geometry.clone()),
        )?;
        let tile_map: HashMap<String, Tile> = tiles
            .iter()
            .map(|tile| (tile.tile_id.clone(), tile.clone()))
            .collect();
        let anomalies_path = output.join("tile_anomalies.csv");
        let tile_anomalies = if anomalies_path.exists() {
            Some(load_tile_anomalies(&anomalies_path, &tile_map)?)
        } else {
            None
        };
        Ok(Self::new(
            load_slots(&output.join("slots.csv"))?,
            tiles,
            load_tile_values(&output.join("targets.csv"), &tile_map)?,
            geometry,
            weather,
            load_requests(&output.join("observation_requests.csv"))?,
            load_request_tiles(&output.join("observation_request_tiles.csv"))?
                .into_iter()
                .map(|(key, value)| (key, value.into_iter().collect()))
                .collect(),
            load_score_config(&config.join("score_config.json"))?,
            tile_anomalies,
        ))
    }

    pub fn current_slot(&self) -> Option<&Slot> {
        self.slots.get(self.slot_index)
    }

    pub fn current_time(&self) -> Option<DateTime<Utc>> {
        self.current_slot()
            .map(|slot| slot.timestamp_utc + TimeDelta::seconds(self.offset_seconds))
    }

    fn advance(&mut self, seconds: i64) {
        let duration = self
            .current_slot()
            .expect("invalid simulation cursor advance")
            .duration_seconds;
        assert!(
            0 <= seconds && seconds <= duration - self.offset_seconds,
            "invalid simulation cursor advance"
        );
        self.offset_seconds += seconds;
        if self.offset_seconds == duration {
            self.slot_index += 1;
            self.offset_seconds = 0;
        }
    }

    fn quality_band(&self, quality: f64) -> &'static str {
        let thresholds = &self.config["quality_thresholds"];
        if quality >= json_f64(thresholds, "dark").unwrap() {
            "DARK"
        } else if quality >= json_f64(thresholds, "bright").unwrap() {
            "BRIGHT"
        } else {
            "BACKUP"
        }
    }

    /// Hidden per-tile truth multiplier; the published tile_science_value stays untagged.
    fn anomaly_factor(&self, tile_id: &str) -> f64 {
        let mut factor = 1.0;
        if let Some(tags) = self.tile_anomalies.get(tile_id) {
            for tag in tags {
                factor *= match tag.as_str() {
                    "nova" => self.nova_factor,
                    "reddening" => self.reddening_factor,
                    _ => unreachable!("anomaly tags validated at load"),
                };
            }
        }
        factor
    }

    fn minimum_altitude(&self) -> f64 {
        json_f64(
            &self.geometry.tile_config["geometry"],
            "minimum_altitude_deg",
        )
        .unwrap()
    }

    fn sample_at(&self, tile: &Tile, epoch: f64) -> crate::geometry::GeometrySample {
        crate::geometry::geometry_sample(
            tile,
            epoch,
            &self.geometry.tile_config,
            &self.geometry.calendar_config,
        )
        .expect("geometry config validated at load")
    }

    fn tile_legal(&self, tile: &Tile, moment: DateTime<Utc>) -> bool {
        if !(tile.available_from_utc <= moment && moment < tile.available_until_utc) {
            return false;
        }
        self.sample_at(tile, epoch_seconds(&moment)).altitude_deg >= self.minimum_altitude()
    }
}

impl ChallengeScorer {
    fn has_actionable_tile(&self) -> bool {
        if self.current_slot().is_none() {
            return false;
        }
        for tile in self.tiles.values() {
            if !self.completed_tiles.contains(&tile.tile_id) {
                if self.can_complete_from(tile, self.slot_index, self.offset_seconds) {
                    return true;
                }
                continue;
            }
            if !self.mechanics {
                continue;
            }
            // A completed tile stays actionable while a repeat started now could beat its banked best.
            let banked = self
                .tile_best_scores
                .get(&tile.tile_id)
                .map(|best| best.0 + best.1)
                .unwrap_or(0.0);
            if self.repeat_score_potential(tile, self.slot_index, self.offset_seconds)
                > banked + 1e-9
            {
                return true;
            }
        }
        false
    }

    /// Best-program score a repeat observation started at the cursor would earn
    /// under truth weather (0 when it cannot complete).
    fn repeat_score_potential(
        &self,
        tile: &Tile,
        mut slot_index: usize,
        mut offset_seconds: i64,
    ) -> f64 {
        if slot_index >= self.slots.len() {
            return 0.0;
        }
        let first_night = self.slots[slot_index].night_id.clone();
        let mut remaining = tile.nominal_exptime_seconds;
        let mut segments: Vec<(f64, &str)> = Vec::new();
        while remaining > 0 && slot_index < self.slots.len() {
            let slot = &self.slots[slot_index];
            if slot.night_id != first_night {
                return 0.0;
            }
            let start = slot.timestamp_utc + TimeDelta::seconds(offset_seconds);
            let seconds = remaining.min(slot.duration_seconds - offset_seconds);
            let midpoint_epoch = epoch_seconds(&start) + seconds as f64 / 2.0;
            if !self.tile_legal(tile, start)
                || !self.tile_legal(tile, datetime_from_epoch(midpoint_epoch))
            {
                return 0.0;
            }
            let conditions = self
                .weather
                .get_effective_conditions(&slot.slot_id, Some(&tile.tile_id), true)
                .expect("slot from calendar");
            if !conditions.is_observable {
                return 0.0;
            }
            let geometry = self.sample_at(tile, midpoint_epoch);
            let combined =
                weather_quality(&conditions, geometry.airmass, &self.weather.config, true)
                    .expect("airmass finite at legal altitude")
                    * geometry.lunar_quality_factor;
            let band_quality =
                weather_quality(&conditions, geometry.airmass, &self.weather.config, false)
                    .expect("airmass finite at legal altitude")
                    * geometry.lunar_quality_factor;
            let base = self.tile_values[&tile.tile_id] * seconds as f64
                / tile.nominal_exptime_seconds as f64
                * combined
                * self.anomaly_factor(&tile.tile_id);
            segments.push((base, self.quality_band(band_quality)));
            remaining -= seconds;
            slot_index += 1;
            offset_seconds = 0;
        }
        if remaining > 0 {
            return 0.0;
        }
        let bonuses = &self.config["program_bonus"];
        PROGRAMS
            .iter()
            .map(|program| {
                segments.iter().fold(0.0, |sum, (base, band)| {
                    sum + base
                        * (if band == program {
                            1.0 + json_f64(bonuses, program).unwrap()
                        } else {
                            1.0
                        })
                })
            })
            .fold(f64::NEG_INFINITY, f64::max)
    }

    fn can_complete_from(
        &self,
        tile: &Tile,
        mut slot_index: usize,
        mut offset_seconds: i64,
    ) -> bool {
        if slot_index >= self.slots.len() {
            return false;
        }
        let first_night = self.slots[slot_index].night_id.clone();
        let mut remaining = tile.nominal_exptime_seconds;
        while remaining > 0 && slot_index < self.slots.len() {
            let slot = &self.slots[slot_index];
            if slot.night_id != first_night {
                return false;
            }
            let start = slot.timestamp_utc + TimeDelta::seconds(offset_seconds);
            let seconds = remaining.min(slot.duration_seconds - offset_seconds);
            let midpoint_epoch = epoch_seconds(&start) + seconds as f64 / 2.0;
            if !self.tile_legal(tile, start)
                || !self.tile_legal(tile, datetime_from_epoch(midpoint_epoch))
            {
                return false;
            }
            let observable = self
                .weather
                .get_effective_conditions(&slot.slot_id, Some(&tile.tile_id), true)
                .expect("slot from calendar")
                .is_observable;
            if !observable {
                return false;
            }
            remaining -= seconds;
            slot_index += 1;
            offset_seconds = 0;
        }
        remaining == 0
    }

    fn tile_has_request_opportunity(
        &self,
        tile: &Tile,
        available_from: DateTime<Utc>,
        deadline: DateTime<Utc>,
    ) -> bool {
        for (index, slot) in self.slots.iter().enumerate() {
            if slot.timestamp_utc < available_from {
                continue;
            }
            if slot.timestamp_utc >= deadline {
                break;
            }
            if slot.timestamp_utc + TimeDelta::seconds(tile.nominal_exptime_seconds) > deadline {
                continue;
            }
            if self.can_complete_from(tile, index, 0) {
                return true;
            }
        }
        false
    }

    fn consume_wait(&mut self, seconds: i64, category: &str) {
        let avoidable = self.has_actionable_tile();
        *self.wait_seconds.entry(category.to_string()).or_insert(0) += seconds;
        *self
            .wait_seconds
            .entry(
                if avoidable {
                    "avoidable"
                } else {
                    "unavailable"
                }
                .to_string(),
            )
            .or_insert(0) += seconds;
        if avoidable {
            let per_second =
                json_f64(&self.config["penalties"], "avoidable_wait_per_second").unwrap();
            self.penalties
                .add("avoidable_wait", seconds as f64 * per_second);
        }
        self.advance(seconds);
    }

    fn consume_until(&mut self, target_index: usize) {
        while self.slot_index < target_index && self.current_slot().is_some() {
            let remaining = self.current_slot().unwrap().duration_seconds - self.offset_seconds;
            self.consume_wait(remaining, "implicit");
        }
    }

    fn invalid(&mut self, decision: &Decision, outcome: &str, unsafe_: bool) -> Value {
        let started = self.current_time();
        let mut elapsed = 0;
        if let Some(slot) = self.current_slot() {
            elapsed = slot.duration_seconds - self.offset_seconds;
            self.consume_wait(elapsed, "invalid");
        }
        let key = if unsafe_ {
            "unsafe_observation"
        } else {
            "invalid_action"
        };
        let penalty = json_f64(&self.config["penalties"], key).unwrap();
        self.penalties.add(key, penalty);
        let action = json!({
            "decision_id": decision.decision_id,
            "slot_id": decision.slot_id,
            "action": decision.action,
            "tile_id": decision.tile_id,
            "program": decision.program,
            "request_id": decision.request_id,
            "start_utc": started.map(|moment| format_utc(&moment)).unwrap_or_default(),
            "elapsed_seconds": elapsed,
            "outcome": outcome,
            "base_science_score": 0.0,
            "program_bonus_score": 0.0,
            "penalty": penalty,
            "segments": [],
        });
        self.actions.push(action.clone());
        action
    }
}

impl ChallengeScorer {
    pub fn apply_decision(&mut self, decision: &Decision) -> Result<Value> {
        if report_kind(&decision.action).is_some() && !self.mechanics {
            return Ok(self.invalid(decision, "unknown_action", false));
        }
        if let Some(kind) = report_kind(&decision.action) {
            // Report rows ride the trace: they never touch the slot cursor and act
            // at the current cursor time (right after their carrier decision).
            let as_of = self
                .current_time()
                .unwrap_or_else(|| self.slots.last().unwrap().end_utc());
            if kind != "Instrument_Failure" && !self.tiles.contains_key(&decision.tile_id) {
                let action = json!({
                    "decision_id": decision.decision_id,
                    "slot_id": decision.slot_id,
                    "action": decision.action,
                    "tile_id": decision.tile_id,
                    "program": "",
                    "request_id": "",
                    "start_utc": format_utc(&as_of),
                    "elapsed_seconds": 0,
                    "outcome": "report_dropped",
                    "base_science_score": 0.0,
                    "program_bonus_score": 0.0,
                    "penalty": 0.0,
                    "segments": [],
                    "report_result": {
                        "report_id": decision.decision_id,
                        "kind": kind,
                        "result": "dropped_unknown_tile",
                    },
                });
                self.actions.push(action.clone());
                return Ok(action);
            }
            let before = self.penalties.get("fault_misreport");
            let outcome = self.apply_report(
                &Report {
                    report_id: decision.decision_id.clone(),
                    kind: kind.to_string(),
                    tile_id: decision.tile_id.clone(),
                },
                as_of,
            );
            let action = json!({
                "decision_id": decision.decision_id,
                "slot_id": decision.slot_id,
                "action": decision.action,
                "tile_id": decision.tile_id,
                "program": "",
                "request_id": "",
                "start_utc": format_utc(&as_of),
                "elapsed_seconds": 0,
                "outcome": format!("report_{}", outcome["result"].as_str().unwrap()),
                "base_science_score": 0.0,
                "program_bonus_score": 0.0,
                "penalty": round6(self.penalties.get("fault_misreport") - before),
                "segments": [],
                "report_result": outcome,
            });
            self.actions.push(action.clone());
            return Ok(action);
        }
        if !self.slot_indices.contains_key(&decision.slot_id) {
            return Ok(self.invalid(decision, "unknown_slot", false));
        }
        let target = self.slot_indices[&decision.slot_id];
        if target < self.slot_index {
            let penalty = json_f64(&self.config["penalties"], "invalid_action").unwrap();
            self.penalties.add("invalid_action", penalty);
            let action = json!({
                "decision_id": decision.decision_id,
                "slot_id": decision.slot_id,
                "action": decision.action,
                "tile_id": decision.tile_id,
                "program": decision.program,
                "request_id": decision.request_id,
                "start_utc": "",
                "elapsed_seconds": 0,
                "outcome": "stale_decision",
                "base_science_score": 0.0,
                "program_bonus_score": 0.0,
                "penalty": penalty,
                "segments": [],
            });
            self.actions.push(action.clone());
            return Ok(action);
        }
        self.consume_until(target);
        if decision.action == "wait" {
            let started = self.current_time();
            let mut elapsed = 0;
            if let Some(slot) = self.current_slot() {
                elapsed = slot.duration_seconds - self.offset_seconds;
                self.consume_wait(elapsed, "explicit");
            }
            let action = json!({
                "decision_id": decision.decision_id,
                "slot_id": decision.slot_id,
                "action": "wait",
                "tile_id": "",
                "program": "",
                "request_id": "",
                "start_utc": started.map(|moment| format_utc(&moment)).unwrap_or_default(),
                "elapsed_seconds": elapsed,
                "outcome": "wait",
                "base_science_score": 0.0,
                "program_bonus_score": 0.0,
                "penalty": 0.0,
                "segments": [],
            });
            self.actions.push(action.clone());
            return Ok(action);
        }
        let tile = self.tiles.get(&decision.tile_id).cloned();
        let slot = self.current_slot().cloned();
        let started = self.current_time();
        let (tile, slot, started) = match (tile, slot, started) {
            (Some(tile), Some(slot), Some(started))
                if PROGRAMS.contains(&decision.program.as_str()) =>
            {
                (tile, slot, started)
            }
            _ => return Ok(self.invalid(decision, "invalid_observe", false)),
        };
        let request = if decision.request_id.is_empty() {
            None
        } else {
            self.requests.get(&decision.request_id).cloned()
        };
        let request_tiles_empty = HashMap::new();
        let request_tiles = self
            .request_tiles
            .get(&decision.request_id)
            .unwrap_or(&request_tiles_empty)
            .clone();
        if !decision.request_id.is_empty()
            && (request.is_none()
                || !request_tiles.contains_key(&tile.tile_id)
                || !(request.as_ref().unwrap().available_from_utc <= started
                    && started < request.as_ref().unwrap().deadline_utc))
        {
            return Ok(self.invalid(decision, "invalid_request_tag", false));
        }
        if !self.mechanics && self.completed_tiles.contains(&tile.tile_id) && request.is_none() {
            return Ok(self.invalid(decision, "duplicate_tile", false));
        }
        let initial_weather =
            self.weather
                .get_effective_conditions(&slot.slot_id, Some(&tile.tile_id), true)?;
        if !initial_weather.is_observable {
            return Ok(self.invalid(decision, "unsafe_observation", true));
        }
        if !self.tile_legal(&tile, started) {
            return Ok(self.invalid(decision, "outside_tile_window", false));
        }
        let mut remaining = tile.nominal_exptime_seconds;
        let mut pending_base = 0.0;
        let mut pending_bonus = 0.0;
        let mut segments: Vec<Value> = Vec::new();
        let mut outcome = "completed";
        while remaining > 0 {
            let (current_slot, moment) = match (self.current_slot(), self.current_time()) {
                (Some(current_slot), Some(moment)) => (current_slot.clone(), moment),
                _ => {
                    outcome = "geometry_or_night_interrupted";
                    break;
                }
            };
            if current_slot.night_id != slot.night_id || !self.tile_legal(&tile, moment) {
                outcome = "geometry_or_night_interrupted";
                break;
            }
            let conditions = self.weather.get_effective_conditions(
                &current_slot.slot_id,
                Some(&tile.tile_id),
                true,
            )?;
            if !conditions.is_observable {
                outcome = "weather_interrupted";
                break;
            }
            let seconds = remaining.min(current_slot.duration_seconds - self.offset_seconds);
            let midpoint_epoch = epoch_seconds(&moment) + seconds as f64 / 2.0;
            let geometry = self.sample_at(&tile, midpoint_epoch);
            let atmospheric_quality =
                weather_quality(&conditions, geometry.airmass, &self.weather.config, true)?;
            // Bands never see instrument efficiency: preview and replay always agree.
            let band_quality =
                weather_quality(&conditions, geometry.airmass, &self.weather.config, false)?;
            let lunar_quality = geometry.lunar_quality_factor;
            let combined_quality = atmospheric_quality * lunar_quality;
            let band = self.quality_band(
                (if self.mechanics {
                    band_quality
                } else {
                    atmospheric_quality
                }) * lunar_quality,
            );
            let base = self.tile_values[&tile.tile_id] * seconds as f64
                / tile.nominal_exptime_seconds as f64
                * combined_quality
                * self.anomaly_factor(&tile.tile_id);
            let bonus = base
                * (if decision.program == band {
                    json_f64(&self.config["program_bonus"], &decision.program).unwrap()
                } else {
                    0.0
                });
            pending_base += base;
            pending_bonus += bonus;
            segments.push(json!({
                "slot_id": current_slot.slot_id,
                "start_utc": format_utc(&moment),
                "duration_seconds": seconds,
                "airmass": round6(geometry.airmass),
                "active_event_ids": conditions.active_event_ids,
                "atmospheric_quality": round6(atmospheric_quality),
                "lunar_quality_factor": round6(lunar_quality),
                "combined_quality": round6(combined_quality),
                "quality_band": band,
                "program_matched": decision.program == band,
                "base_science_score": round6(base),
                "program_bonus_score": round6(bonus),
            }));
            self.advance(seconds);
            remaining -= seconds;
        }
        let completed = remaining == 0 && outcome == "completed";
        let mut action_penalty = 0.0;
        if !completed && outcome == "geometry_or_night_interrupted" {
            action_penalty = json_f64(&self.config["penalties"], "invalid_action").unwrap();
            self.penalties.add("invalid_action", action_penalty);
        }
        if completed {
            if !self.mechanics {
                // Pre-anomaly semantics: ordinary science/completion credit banks once.
                if !self.completed_tiles.contains(&tile.tile_id) {
                    self.completed_tiles.insert(tile.tile_id.clone());
                    self.base_science_score += pending_base;
                    self.program_bonus_score += pending_bonus;
                } else {
                    // A request-tagged revisit is operationally valid but cannot
                    // duplicate the tile's ordinary science/completion credit.
                    pending_base = 0.0;
                    pending_bonus = 0.0;
                }
            } else {
                // Completion banks once, on the first legal observation; the tile's
                // science contribution is the per-observation maximum and only grows.
                self.completed_tiles.insert(tile.tile_id.clone());
                let banked = self
                    .tile_best_scores
                    .get(&tile.tile_id)
                    .copied()
                    .unwrap_or((0.0, 0.0));
                if pending_base + pending_bonus > banked.0 + banked.1 {
                    self.base_science_score += pending_base - banked.0;
                    self.program_bonus_score += pending_bonus - banked.1;
                    self.tile_best_scores
                        .insert(tile.tile_id.clone(), (pending_base, pending_bonus));
                }
            }
            if let Some(request) = request {
                *self
                    .request_visits
                    .entry((request.request_id.clone(), tile.tile_id.clone()))
                    .or_insert(0) += 1;
            }
        } else {
            pending_base = 0.0;
            pending_bonus = 0.0;
        }
        let action = json!({
            "decision_id": decision.decision_id,
            "slot_id": decision.slot_id,
            "action": "observe",
            "tile_id": tile.tile_id,
            "program": decision.program,
            "request_id": decision.request_id,
            "start_utc": format_utc(&started),
            "elapsed_seconds": tile.nominal_exptime_seconds - remaining,
            "outcome": outcome,
            "base_science_score": round6(pending_base),
            "program_bonus_score": round6(pending_bonus),
            "penalty": action_penalty,
            "segments": segments,
        });
        self.actions.push(action.clone());
        Ok(action)
    }

    /// Book one participant report at cursor time `as_of`; never touches the cursor.
    pub fn apply_report(&mut self, report: &Report, as_of: DateTime<Utc>) -> Value {
        if report.kind == "Instrument_Failure" {
            let active: Vec<&crate::weather::WeatherEvent> = self
                .weather
                .events
                .iter()
                .filter(|event| {
                    event.condition == "instrument_fault"
                        && event.actual_start_utc <= as_of
                        && as_of
                            < self
                                .weather
                                .end_overrides
                                .get(&event.event_id)
                                .copied()
                                .unwrap_or(event.actual_end_utc)
                })
                .collect();
            let unacknowledged: Vec<&crate::weather::WeatherEvent> = active
                .iter()
                .copied()
                .filter(|event| !self.fault_acknowledgements.contains_key(&event.event_id))
                .collect();
            let result: &str;
            let mut acknowledged_event_ids: Vec<String> = Vec::new();
            let mut repair_complete_utc: Option<String> = None;
            if !unacknowledged.is_empty() {
                let repair_at =
                    datetime_from_epoch(epoch_seconds(&as_of) + self.repair_duration_seconds);
                for event in &unacknowledged {
                    self.fault_acknowledgements
                        .insert(event.event_id.clone(), repair_at);
                    self.weather
                        .end_overrides
                        .insert(event.event_id.clone(), event.actual_end_utc.min(repair_at));
                    acknowledged_event_ids.push(event.event_id.clone());
                }
                self.fault_correct_reports += 1;
                self.misreport_count = 0;
                result = "correct";
                repair_complete_utc = Some(format_utc(&repair_at));
            } else if !active.is_empty() {
                result = "neutral"; // acknowledged fault, still under repair
            } else {
                self.misreport_count += 1;
                self.misreport_total += 1;
                if self.misreport_count > self.misreport_allowance {
                    self.penalties
                        .add("fault_misreport", self.misreport_penalty);
                }
                result = "misreport";
            }
            return json!({
                "report_id": report.report_id,
                "kind": report.kind,
                "result": result,
                "acknowledged_event_ids": acknowledged_event_ids,
                "repair_complete_utc": repair_complete_utc,
            });
        }
        let tag = if report.kind == "NOVA" {
            "nova"
        } else {
            "reddening"
        };
        let key = (report.tile_id.clone(), tag.to_string());
        if self.tag_reports.contains_key(&key) {
            return json!({
                "report_id": report.report_id,
                "kind": report.kind,
                "result": "duplicate_ignored",
            });
        }
        self.tag_reports.insert(key, report.report_id.clone());
        json!({
            "report_id": report.report_id,
            "kind": report.kind,
            "result": "recorded",
        })
    }

    fn settle_tag_reports(&mut self) -> (Vec<Value>, f64) {
        let mut rows = Vec::new();
        let mut reward_total = 0.0;
        let mut penalty_total = 0.0;
        for ((tile_id, tag), report_id) in self.tag_reports.clone() {
            let correct = self
                .tile_anomalies
                .get(&tile_id)
                .map(|tags| tags.contains(&tag))
                .unwrap_or(false);
            let delta = if correct {
                self.report_reward
            } else {
                -self.report_penalty
            };
            reward_total += delta.max(0.0);
            penalty_total += (-delta).max(0.0);
            rows.push(json!({
                "tile_id": tile_id,
                "tag": tag,
                "report_id": report_id,
                "settled": if correct { "correct" } else { "wrong" },
                "delta": round6(delta),
            }));
        }
        if penalty_total != 0.0 {
            self.penalties.add("wrong_tag_report", penalty_total);
        }
        (rows, reward_total)
    }
}

impl ChallengeScorer {
    pub fn finalize(&mut self, termination_reason: &str) -> Result<Value> {
        let required_missing: Vec<String> = self
            .tiles
            .values()
            .filter(|tile| {
                tile.scheduling_class == "REQUIRED" && !self.completed_tiles.contains(&tile.tile_id)
            })
            .map(|tile| tile.tile_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let required_miss = json_f64(&self.config["penalties"], "required_miss").unwrap();
        self.penalties.set(
            "required_miss",
            required_missing.len() as f64 * required_miss,
        );
        let mut flexible: HashMap<String, i64> = HashMap::new();
        for tile_id in &self.completed_tiles {
            let tile = &self.tiles[tile_id];
            if tile.scheduling_class == "FLEXIBLE" {
                *flexible.entry(tile.region_id.clone()).or_insert(0) += 1;
            }
        }
        let regions: Vec<String> = self
            .tiles
            .values()
            .map(|tile| tile.region_id.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let quota = json_i64(&self.config, "flexible_quota_per_region").unwrap();
        let shortfall: BTreeMap<String, i64> = regions
            .iter()
            .map(|region| {
                (
                    region.clone(),
                    (quota - flexible.get(region).copied().unwrap_or(0)).max(0),
                )
            })
            .collect();
        let shortfall_per_tile =
            json_f64(&self.config["penalties"], "flexible_shortfall_per_tile").unwrap();
        self.penalties.set(
            "flexible_shortfall",
            shortfall.values().sum::<i64>() as f64 * shortfall_per_tile,
        );
        let mut final_time = self.current_time();
        if final_time.is_none() && !self.slots.is_empty() {
            final_time = Some(self.slots.last().unwrap().end_utc());
        }
        let mut request_rows: Vec<Value> = Vec::new();
        let mut request_reward = 0.0;
        let mut request_penalty = 0.0;
        let mut ordered_requests: Vec<&ObservationRequest> = self.requests.values().collect();
        ordered_requests.sort_by(|left, right| left.request_id.cmp(&right.request_id));
        for request in ordered_requests {
            if final_time.is_none() || request.issued_at_utc > final_time.unwrap() {
                continue;
            }
            let final_time = final_time.unwrap();
            let tiles_for_request = &self.request_tiles[&request.request_id];
            let satisfied = tiles_for_request
                .iter()
                .filter(|(tile_id, visits)| {
                    self.request_visits
                        .get(&(request.request_id.clone(), (*tile_id).clone()))
                        .copied()
                        .unwrap_or(0)
                        >= **visits
                })
                .count() as i64;
            let completed = satisfied >= request.required_tile_count;
            let expired = request.deadline_utc <= final_time;
            let feasible_count: Option<i64> = if expired && !completed {
                Some(
                    tiles_for_request
                        .keys()
                        .filter(|tile_id| {
                            self.tile_has_request_opportunity(
                                &self.tiles[*tile_id],
                                request.available_from_utc,
                                request.deadline_utc,
                            )
                        })
                        .count() as i64,
                )
            } else {
                None
            };
            let excused = feasible_count
                .map(|count| count < request.required_tile_count)
                .unwrap_or(false);
            let status = if completed {
                "completed"
            } else if excused {
                "excused_unobservable"
            } else if expired {
                "missed"
            } else {
                "active_incomplete"
            };
            let reward = if completed {
                request.completion_reward
            } else {
                0.0
            };
            let penalty = if expired && !completed && !excused {
                request.miss_penalty
            } else {
                0.0
            };
            request_reward += reward;
            request_penalty += penalty;
            request_rows.push(json!({
                "request_id": request.request_id,
                "status": status,
                "satisfied_tile_count": satisfied,
                "required_tile_count": request.required_tile_count,
                "feasible_tile_count": feasible_count,
                "reward": reward,
                "penalty": penalty,
            }));
        }
        self.penalties.set("request_miss", request_penalty);
        let (tag_settlements, report_reward) = self.settle_tag_reports();
        let mut completed_by_region: HashMap<String, i64> = HashMap::new();
        for tile_id in &self.completed_tiles {
            *completed_by_region
                .entry(self.tiles[tile_id].region_id.clone())
                .or_insert(0) += 1;
        }
        let evenness = coverage_evenness(&completed_by_region, &regions);
        let coverage_weight = self
            .config
            .get("coverage_bonus_weight")
            .and_then(Value::as_f64)
            .unwrap_or(0.0);
        let coverage_bonus = coverage_weight * self.base_science_score * evenness;
        let subtotal = self.base_science_score
            + self.program_bonus_score
            + request_reward
            + coverage_bonus
            + report_reward;
        let total_penalty = self.penalties.sum();
        let slot = self.current_slot();
        let wait_seconds: serde_json::Map<String, Value> = self
            .wait_seconds
            .iter()
            .map(|(key, value)| (key.clone(), json!(value)))
            .collect::<BTreeMap<_, _>>()
            .into_iter()
            .collect();
        Ok(json!({
            "schema_version": "score-report-v3",
            "termination_reason": termination_reason,
            "final_cursor": {
                "slot_id": slot.map(|slot| slot.slot_id.clone()),
                "slot_index": self.slot_index,
                "offset_seconds": self.offset_seconds,
                "timestamp_utc": final_time.map(|moment| format_utc(&moment)),
            },
            "score": {
                "total": round6(subtotal - total_penalty),
                "base_science": round6(self.base_science_score),
                "program_bonus": round6(self.program_bonus_score),
                "request_reward": round6(request_reward),
                "coverage_bonus": round6(coverage_bonus),
                "coverage_evenness": round6(evenness),
                "report_reward": round6(report_reward),
                "penalties": self.penalties.sorted_rounded_json(),
            },
            "completion": {
                "completed_tiles": self.completed_tiles.iter().collect::<Vec<_>>(),
                "required_missing": required_missing,
                "flexible_by_region": flexible.into_iter().collect::<BTreeMap<_, _>>(),
                "flexible_shortfall": shortfall,
            },
            "requests": request_rows,
            "wait_seconds": Value::Object(wait_seconds),
            "reports": {
                "tag_settlements": tag_settlements,
                "fault_correct_reports": self.fault_correct_reports,
                "fault_misreports": self.misreport_total,
                "fault_acknowledged_event_ids": self
                    .fault_acknowledgements
                    .keys()
                    .collect::<BTreeSet<_>>()
                    .into_iter()
                    .collect::<Vec<_>>(),
            },
            "actions": self.actions,
            "parameters": {
                "score_config": self.config,
                "weather_score_interface": self.weather.config["score_interface"].clone(),
                "lunar_model": self.geometry.tile_config["lunar_model"].clone(),
            },
        }))
    }
}

/// Recursively rebuild a JSON value with object keys sorted (Python's
/// `json.dumps(..., sort_keys=True)`).
pub fn sort_json_keys(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), sort_json_keys(&map[key])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(sort_json_keys).collect()),
        other => other.clone(),
    }
}

/// Python `json.dumps(report, indent=2, sort_keys=True)` (LF endings are added
/// by `write_text_lf` at the call site).
pub fn dumps_report(report: &Value) -> String {
    serde_json::to_string_pretty(&sort_json_keys(report)).unwrap()
}

/// Replay `decisions_path` against the scenario at `root`, finalize, attach
/// input hashes, and optionally write the report JSON (LF endings).
pub fn score_files(
    root: &Path,
    decisions_path: &Path,
    output_path: Option<&Path>,
    termination_reason: &str,
) -> Result<Value> {
    let mut scorer = ChallengeScorer::from_files(root)?;
    for decision in load_decisions(decisions_path, scorer.mechanics)? {
        scorer.apply_decision(&decision)?;
    }
    let mut report = scorer.finalize(termination_reason)?;
    report["input_sha256"] = json!({
        "decisions": sha256_file(decisions_path)?,
        "score_config": sha256_file(&root.join("config").join("score_config.json"))?,
    });
    if let Some(output_path) = output_path {
        write_text_lf(output_path, &(dumps_report(&report) + "\n"))?;
    }
    Ok(report)
}
