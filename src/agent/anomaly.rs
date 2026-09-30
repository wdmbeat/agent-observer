//! Deterministic anomaly detection and calibrated reporting — port of
//! `agent/anomaly_detection.py::AnomalyDetector`.
//!
//! Thresholds are env-overridable via `SAC_ANOMALY_*` with the same defaults.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use serde_json::{json, Value};

use crate::contracts::parse_utc;
use crate::geometry::angular_separation_deg;

use super::preview::CandidatePreview;

fn float_env(name: &str, default: f64) -> f64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(default)
}

fn int_env(name: &str, default: i64) -> i64 {
    std::env::var(name)
        .ok()
        .and_then(|value| value.parse::<i64>().ok())
        .unwrap_or(default)
}

fn parse_utc_lenient(value: &Value) -> Option<DateTime<Utc>> {
    value.as_str().and_then(|text| parse_utc(text).ok())
}

fn value_f64(value: &Value, default: f64) -> f64 {
    value
        .as_f64()
        .or_else(|| value.as_str().and_then(|text| text.trim().parse().ok()))
        .unwrap_or(default)
}

/// Run-long memory for realized-vs-baseline deviation tracking.
pub struct AnomalyDetector {
    pub nova_ratio_min: f64,
    pub nova_ratio_max: f64,
    pub reddening_ratio_min: f64,
    pub reddening_ratio_max: f64,
    pub fault_ratio_max: f64,
    pub fault_min_evidence: i64,
    pub fault_evidence_window: i64,
    pub tag_min_reads: i64,
    pub tag_min_fraction: f64,
    program_bonus: HashMap<String, f64>,
    tile_coords: HashMap<String, (f64, f64)>,
    pub bests: HashMap<String, f64>,
    pending: Option<PendingObservation>,
    last_feedback: Option<(String, f64)>,
    reported_tags: HashSet<(String, String)>,
    /// tile → per-read (band, night_id); band is None when out of both bands.
    tag_reads: HashMap<String, Vec<(Option<String>, String)>>,
    recent_ratios: Vec<f64>,
    fault_pending: bool,
    fault_scope: Option<(String, Value)>,
    fault_repair_until: Option<DateTime<Utc>>,
    forecasts: Vec<Value>,
}

struct PendingObservation {
    tile_id: String,
    expected: f64,
    cold_wave: bool,
}

impl AnomalyDetector {
    pub fn new(initial_publication: &Value) -> Self {
        let mut program_bonus: HashMap<String, f64> = initial_publication
            .get("scoring_contract")
            .and_then(|contract| contract.get("score_config"))
            .and_then(|config| config.get("program_bonus"))
            .and_then(Value::as_object)
            .map(|object| {
                object
                    .iter()
                    .map(|(key, value)| (key.clone(), value_f64(value, 0.0)))
                    .collect()
            })
            .unwrap_or_default();
        if program_bonus.is_empty() {
            program_bonus = HashMap::from([
                ("DARK".to_string(), 0.25),
                ("BRIGHT".to_string(), 0.15),
                ("BACKUP".to_string(), 0.08),
            ]);
        }
        let mut tile_coords = HashMap::new();
        for tile in initial_publication
            .get("tile_catalog")
            .and_then(|catalog| catalog.get("tiles"))
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            if tile.get("ra_deg").is_some() && tile.get("dec_deg").is_some() {
                tile_coords.insert(
                    tile["tile_id"].as_str().unwrap_or("").to_string(),
                    (value_f64(&tile["ra_deg"], 0.0), value_f64(&tile["dec_deg"], 0.0)),
                );
            }
        }
        Self {
            nova_ratio_min: float_env("SAC_ANOMALY_NOVA_RATIO_MIN", 1.30),
            nova_ratio_max: float_env("SAC_ANOMALY_NOVA_RATIO_MAX", 1.65),
            reddening_ratio_min: float_env("SAC_ANOMALY_REDDENING_RATIO_MIN", 0.70),
            reddening_ratio_max: float_env("SAC_ANOMALY_REDDENING_RATIO_MAX", 0.87),
            fault_ratio_max: float_env("SAC_ANOMALY_FAULT_RATIO_MAX", 0.60),
            fault_min_evidence: int_env("SAC_ANOMALY_FAULT_MIN_EVIDENCE", 2),
            fault_evidence_window: int_env("SAC_ANOMALY_FAULT_EVIDENCE_WINDOW", 6),
            tag_min_reads: int_env("SAC_ANOMALY_TAG_MIN_READS", 5),
            tag_min_fraction: float_env("SAC_ANOMALY_TAG_MIN_FRACTION", 0.8),
            program_bonus,
            tile_coords,
            bests: HashMap::new(),
            pending: None,
            last_feedback: None,
            reported_tags: HashSet::new(),
            tag_reads: HashMap::new(),
            recent_ratios: Vec::new(),
            fault_pending: false,
            fault_scope: None,
            fault_repair_until: None,
            forecasts: Vec::new(),
        }
    }

    /// The public-baseline score of one exposure under the commit-time snapshot.
    pub fn potential_of(&self, preview_row: &CandidatePreview) -> f64 {
        let bonus = self
            .program_bonus
            .get(&preview_row.quality_band)
            .copied()
            .unwrap_or(0.0);
        preview_row.tile_science_value * preview_row.combined_quality * (1.0 + bonus)
    }

    /// Remember the estimate of the observation just committed (None for waits).
    pub fn note_observation(
        &mut self,
        tile_id: Option<&str>,
        expected: Option<f64>,
        under_cold_wave: bool,
    ) {
        let (Some(tile_id), Some(expected)) = (tile_id, expected) else {
            self.pending = None;
            return;
        };
        self.pending = Some(PendingObservation {
            tile_id: tile_id.to_string(),
            expected,
            cold_wave: under_cold_wave,
        });
        let banked = self.bests.get(tile_id).copied().unwrap_or(0.0);
        self.bests.insert(tile_id.to_string(), banked.max(expected));
    }

    /// Whether a published forecast currently predicts a cold_wave (the only
    /// forecastable event that also moves instrument_efficiency).
    pub fn under_cold_wave(&mut self, snapshot: &Value) -> bool {
        if let Some(weekly) = snapshot.get("weekly") {
            if weekly.is_object() && !weekly["weather_forecast"].is_null() {
                self.forecasts = weekly["weather_forecast"].as_array().cloned().unwrap_or_default();
            }
        }
        let Some(now) = parse_utc_lenient(&snapshot["cursor"]["timestamp_utc"]) else {
            return false;
        };
        for forecast in &self.forecasts {
            if forecast["condition"].as_str() != Some("cold_wave") {
                continue;
            }
            let start = parse_utc_lenient(&forecast["predicted_start_utc"]);
            let end = parse_utc_lenient(&forecast["predicted_end_utc"]);
            if let (Some(start), Some(end)) = (start, end) {
                if start <= now && now < end {
                    return true;
                }
            }
        }
        false
    }

    /// Consume feedback/fault publications and return the reports to attach now.
    pub fn process_snapshot(&mut self, snapshot: &Value) -> Vec<Value> {
        let mut reports = Vec::new();
        if let Some(fault_status) = snapshot.get("fault_status") {
            if fault_status.is_object() {
                match fault_status["status"].as_str() {
                    Some("fault") => {
                        self.fault_pending = false;
                        self.recent_ratios.clear();
                        self.fault_scope = Some((
                            fault_status["spatial_scope_type"]
                                .as_str()
                                .unwrap_or("")
                                .to_string(),
                            fault_status
                                .get("spatial_scope_payload")
                                .filter(|payload| !payload.is_null())
                                .cloned()
                                .unwrap_or_else(|| json!({})),
                        ));
                        self.fault_repair_until =
                            parse_utc_lenient(&fault_status["repair_complete_utc"]);
                    }
                    Some("normal") => {
                        // The platform answered a misreport: clear the pending flag
                        // and require fresh collapse evidence before reporting again.
                        self.fault_pending = false;
                        self.recent_ratios.clear();
                    }
                    _ => {}
                }
            }
        }
        if let Some(feedback) = snapshot.get("tile_last_finished") {
            if feedback.is_object() && !feedback["tile_id"].as_str().unwrap_or("").is_empty() {
                let key = (
                    feedback["tile_id"].as_str().unwrap().to_string(),
                    value_f64(&feedback["score"], 0.0),
                );
                if self.last_feedback.as_ref() != Some(&key) {
                    self.last_feedback = Some(key.clone());
                    let pending = self.pending.take();
                    if let Some(pending) = pending {
                        if pending.tile_id == key.0 && pending.expected > 0.0 {
                            let realized = key.1;
                            let expected = pending.expected;
                            let banked = self.bests.get(&key.0).copied().unwrap_or(0.0);
                            self.bests.insert(key.0.clone(), banked.max(realized));
                            // zero means the exposure was interrupted: no anomaly signal.
                            // a forecasted cold_wave is a public efficiency dip: not an anomaly either.
                            if realized > 0.0 && !pending.cold_wave {
                                reports.extend(self.classify(&key.0, realized / expected, snapshot));
                            }
                        }
                    }
                }
            }
        }
        reports
    }

    fn classify(&mut self, tile_id: &str, ratio: f64, snapshot: &Value) -> Vec<Value> {
        let mut reports = Vec::new();
        let band = if self.nova_ratio_min <= ratio && ratio <= self.nova_ratio_max {
            Some("NOVA")
        } else if self.reddening_ratio_min <= ratio && ratio <= self.reddening_ratio_max {
            Some("Reddening")
        } else {
            None
        };
        // Tag reads accumulate per tile; a tag is permanent, so it must dominate
        // the tile's whole read history, not just appear once, and across more
        // than one night.
        let night = snapshot["cursor"]["night_id"]
            .as_str()
            .unwrap_or("")
            .to_string();
        let reads = self.tag_reads.entry(tile_id.to_string()).or_default();
        reads.push((band.map(str::to_string), night));
        if let Some(band) = band {
            if !self.reported_tags.contains(&(tile_id.to_string(), band.to_string())) {
                let reads = &self.tag_reads[tile_id];
                let hits: Vec<&String> = reads
                    .iter()
                    .filter(|(read_band, _)| read_band.as_deref() == Some(band))
                    .map(|(_, read_night)| read_night)
                    .collect();
                let distinct_nights: HashSet<&String> = hits.iter().copied().collect();
                if reads.len() as i64 >= self.tag_min_reads
                    && hits.len() as f64 / reads.len() as f64 >= self.tag_min_fraction
                    && distinct_nights.len() >= 2
                {
                    self.reported_tags
                        .insert((tile_id.to_string(), band.to_string()));
                    reports.push(json!({"kind": band, "tile_id": tile_id}));
                }
            }
        }
        // Faults persist for many hours but the agent keeps observing other tiles,
        // so evidence accumulates over a rolling window rather than consecutive reads.
        self.recent_ratios.push(ratio);
        let window = self.fault_evidence_window.max(0) as usize;
        if self.recent_ratios.len() > window {
            let excess = self.recent_ratios.len() - window;
            self.recent_ratios.drain(..excess);
        }
        let collapses = self
            .recent_ratios
            .iter()
            .filter(|value| **value <= self.fault_ratio_max)
            .count() as i64;
        let now = parse_utc_lenient(&snapshot["cursor"]["timestamp_utc"]);
        let repair_active = self
            .fault_repair_until
            .map(|until| now.map(|now| now < until).unwrap_or(true))
            .unwrap_or(false);
        if collapses >= self.fault_min_evidence && !self.fault_pending && !repair_active {
            self.fault_pending = true;
            self.recent_ratios.clear();
            reports.push(json!({"kind": "Instrument_Failure"}));
        }
        reports
    }

    /// The best-ranked preview whose tile's latest read was in-band and unreported.
    pub fn top_suspect<'a>(&self, previews: &'a [CandidatePreview]) -> Option<&'a CandidatePreview> {
        previews.iter().find(|row| {
            let reads = self.tag_reads.get(&row.tile_id);
            match reads.and_then(|reads| reads.last()) {
                Some((Some(band), _)) => {
                    !self.reported_tags.contains(&(row.tile_id.clone(), band.clone()))
                }
                _ => false,
            }
        })
    }

    fn in_fault_scope(&self, candidate: &Value) -> bool {
        let Some((scope_type, payload)) = &self.fault_scope else {
            return false;
        };
        match scope_type.as_str() {
            "REGION_SET" => {
                let region_id = candidate["region_id"].as_str().unwrap_or("");
                payload["region_ids"]
                    .as_array()
                    .map(|ids| ids.iter().any(|id| id.as_str() == Some(region_id)))
                    .unwrap_or(false)
            }
            "SKY_CAP_ICRS" => {
                let tile_id = candidate["tile_id"].as_str().unwrap_or("");
                let Some((ra, dec)) = self.tile_coords.get(tile_id) else {
                    return false;
                };
                angular_separation_deg(
                    *ra,
                    *dec,
                    value_f64(&payload["ra_deg"], 0.0),
                    value_f64(&payload["dec_deg"], 0.0),
                ) <= value_f64(&payload["radius_deg"], 0.0)
            }
            // Shipped configs scope faults to REGION_SET only, so avoidance is
            // complete; HORIZON_SECTOR would need mount-side geometry the agent
            // does not have: keep the candidate.
            _ => false,
        }
    }

    /// Drop candidates inside a known-active fault's scope while alternatives exist.
    /// Borrows the snapshot when nothing changes (the common case), matching
    /// Python returning the same object.
    pub fn filter_fault_scope<'a>(&self, snapshot: &'a Value) -> std::borrow::Cow<'a, Value> {
        let now = parse_utc_lenient(&snapshot["cursor"]["timestamp_utc"]);
        let active = self.fault_scope.is_some()
            && self
                .fault_repair_until
                .map(|until| now.map(|now| now < until).unwrap_or(false))
                .unwrap_or(false);
        if !active {
            return std::borrow::Cow::Borrowed(snapshot);
        }
        let candidates = snapshot["candidate_tiles"].as_array().cloned().unwrap_or_default();
        let kept: Vec<Value> = candidates
            .iter()
            .filter(|candidate| !self.in_fault_scope(candidate))
            .cloned()
            .collect();
        if kept.is_empty() || kept.len() == candidates.len() {
            return std::borrow::Cow::Borrowed(snapshot);
        }
        let mut filtered = snapshot.clone();
        filtered["candidate_tiles"] = Value::Array(kept);
        std::borrow::Cow::Owned(filtered)
    }
}
