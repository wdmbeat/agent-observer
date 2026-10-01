//! Deterministic anomaly detection and calibrated reporting — port of
//! `agent/anomaly_detection.py::AnomalyDetector`.
//!
//! Thresholds are env-overridable via `SAC_ANOMALY_*` with the same defaults.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};

use crate::geometry::angular_separation_deg;

use super::model::{
    CandidateTile, DecisionSnapshot, FaultScope, FaultStatus, Forecast, InitialPublication,
    ProgramBonus, Report,
};
use super::scoring_preview::CandidatePreview;

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
    program_bonus: ProgramBonus,
    tile_coords: HashMap<String, (f64, f64)>,
    pub bests: HashMap<String, f64>,
    pending: Option<PendingObservation>,
    last_feedback: Option<(String, f64)>,
    reported_tags: HashSet<(String, String)>,
    /// tile → per-read (band, night_id); band is None when out of both bands.
    tag_reads: HashMap<String, Vec<(Option<String>, String)>>,
    recent_ratios: Vec<f64>,
    fault_pending: bool,
    fault_scope: Option<FaultScope>,
    fault_repair_until: Option<DateTime<Utc>>,
    forecasts: Vec<Forecast>,
}

struct PendingObservation {
    tile_id: String,
    expected: f64,
    cold_wave: bool,
}

impl AnomalyDetector {
    pub fn new(initial_publication: &InitialPublication) -> Self {
        let mut tile_coords = HashMap::new();
        for tile in &initial_publication.tile_catalog.tiles {
            tile_coords.insert(tile.tile_id.clone(), (tile.ra_deg, tile.dec_deg));
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
            program_bonus: initial_publication
                .scoring_contract
                .score_config
                .program_bonus
                .clone(),
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
        preview_row.tile_science_value
            * preview_row.combined_quality
            * (1.0 + self.program_bonus.for_program(preview_row.quality_band))
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
    pub fn under_cold_wave(&mut self, snapshot: &DecisionSnapshot) -> bool {
        if let Some(weekly) = &snapshot.weekly {
            self.forecasts = weekly.weather_forecast.clone();
        }
        let now = snapshot.cursor.timestamp_utc;
        for forecast in &self.forecasts {
            if forecast.condition != "cold_wave" {
                continue;
            }
            if let (Some(start), Some(end)) =
                (forecast.predicted_start_utc, forecast.predicted_end_utc)
            {
                if start <= now && now < end {
                    return true;
                }
            }
        }
        false
    }

    /// Consume feedback/fault publications and return the reports to attach now.
    pub fn process_snapshot(&mut self, snapshot: &DecisionSnapshot) -> Vec<Report> {
        let mut reports = Vec::new();
        if let Some(fault_status) = &snapshot.fault_status {
            match fault_status {
                FaultStatus::Fault { scope, repair_complete_utc, .. } => {
                    self.fault_pending = false;
                    self.recent_ratios.clear();
                    self.fault_scope = Some(scope.clone());
                    self.fault_repair_until = *repair_complete_utc;
                }
                FaultStatus::Normal { .. } => {
                    // The platform answered a misreport: clear the pending flag
                    // and require fresh collapse evidence before reporting again.
                    self.fault_pending = false;
                    self.recent_ratios.clear();
                }
            }
        }
        if let Some(feedback) = &snapshot.tile_last_finished {
            if !feedback.tile_id.is_empty() {
                let key = (feedback.tile_id.clone(), feedback.score);
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
                                reports.extend(self.classify(
                                    &key.0,
                                    realized / expected,
                                    snapshot,
                                ));
                            }
                        }
                    }
                }
            }
        }
        reports
    }

    fn classify(&mut self, tile_id: &str, ratio: f64, snapshot: &DecisionSnapshot) -> Vec<Report> {
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
        let night = snapshot.cursor.night_id.clone();
        let reads = self.tag_reads.entry(tile_id.to_string()).or_default();
        reads.push((band.map(str::to_string), night));
        if let Some(band) = band {
            if !self
                .reported_tags
                .contains(&(tile_id.to_string(), band.to_string()))
            {
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
                    let tile_id = tile_id.to_string();
                    reports.push(match band {
                        "NOVA" => Report::Nova { tile_id },
                        _ => Report::Reddening { tile_id },
                    });
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
        let now = snapshot.cursor.timestamp_utc;
        let repair_active = self
            .fault_repair_until
            .map(|until| now < until)
            .unwrap_or(false);
        if collapses >= self.fault_min_evidence && !self.fault_pending && !repair_active {
            self.fault_pending = true;
            self.recent_ratios.clear();
            reports.push(Report::InstrumentFailure);
        }
        reports
    }

    /// The best-ranked preview whose tile's latest read was in-band and unreported.
    pub fn top_suspect<'a>(
        &self,
        previews: &'a [CandidatePreview],
    ) -> Option<&'a CandidatePreview> {
        previews.iter().find(|row| {
            let reads = self.tag_reads.get(&row.tile_id);
            match reads.and_then(|reads| reads.last()) {
                Some((Some(band), _)) => !self
                    .reported_tags
                    .contains(&(row.tile_id.clone(), band.clone())),
                _ => false,
            }
        })
    }

    fn in_fault_scope(&self, candidate: &CandidateTile) -> bool {
        match &self.fault_scope {
            Some(FaultScope::RegionSet { region_ids }) => {
                region_ids.iter().any(|id| id == &candidate.region_id)
            }
            Some(FaultScope::SkyCapIcrs { ra_deg, dec_deg, radius_deg }) => {
                let Some((ra, dec)) = self.tile_coords.get(&candidate.tile_id) else {
                    return false;
                };
                angular_separation_deg(*ra, *dec, *ra_deg, *dec_deg) <= *radius_deg
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
    pub fn filter_fault_scope<'a>(
        &self,
        snapshot: &'a DecisionSnapshot,
    ) -> std::borrow::Cow<'a, DecisionSnapshot> {
        let now = snapshot.cursor.timestamp_utc;
        let active = self.fault_scope.is_some()
            && self
                .fault_repair_until
                .map(|until| now < until)
                .unwrap_or(false);
        if !active {
            return std::borrow::Cow::Borrowed(snapshot);
        }
        let kept: Vec<CandidateTile> = snapshot
            .candidate_tiles
            .iter()
            .filter(|candidate| !self.in_fault_scope(candidate))
            .cloned()
            .collect();
        if kept.is_empty() || kept.len() == snapshot.candidate_tiles.len() {
            return std::borrow::Cow::Borrowed(snapshot);
        }
        let mut filtered = snapshot.clone();
        filtered.candidate_tiles = kept;
        std::borrow::Cow::Owned(filtered)
    }
}
