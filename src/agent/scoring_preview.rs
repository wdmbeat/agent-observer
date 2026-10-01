//! Participant-safe current-action estimates built from the public score
//! contract — port of `agent/scoring_preview.py::preview_actions`.
//!
//! All ranking fields are rounded (6 decimals, 9 for gain-per-second) BEFORE
//! the stable sort, exactly as the Python does.

use std::collections::HashMap;

use anyhow::{bail, Result};

use crate::contracts::{round6, round9};

use super::model::{
    CandidateTile, DecisionSnapshot, Program, SchedulingClass, ScoreConfig, ScoringContract,
    WeatherScoreInterface,
};

/// One legal current-snapshot action and its transparent ranking terms.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CandidatePreview {
    pub tile_id: String,
    pub program: Program,
    pub request_id: String,
    pub region_id: String,
    pub scheduling_class: String,
    pub nominal_exptime_seconds: i64,
    pub atmospheric_quality: f64,
    pub lunar_quality_factor: f64,
    pub combined_quality: f64,
    pub quality_band: Program,
    pub tile_science_value: f64,
    pub estimated_science_score: f64,
    pub terminal_penalty_avoidance: f64,
    pub request_policy_value: f64,
    pub estimated_total_gain: f64,
    pub estimated_gain_per_second: f64,
    pub estimate_semantics: String,
}

fn quality_band(quality: f64, score_config: &ScoreConfig) -> Program {
    if quality >= score_config.quality_thresholds.dark {
        return Program::Dark;
    }
    if quality >= score_config.quality_thresholds.bright {
        return Program::Bright;
    }
    Program::Backup
}

/// Efficiency-free atmospheric quality × lunar; (atmospheric, lunar, combined).
fn combined_quality(
    candidate: &CandidateTile,
    weather_interface: &WeatherScoreInterface,
) -> Result<(f64, f64, f64)> {
    let weather = &candidate.effective_weather;
    let lunar = candidate.geometry.lunar_quality_factor;
    if !weather.is_observable {
        return Ok((0.0, lunar, 0.0));
    }
    let airmass = candidate.geometry.airmass;
    if airmass <= 0.0 {
        bail!("airmass must be positive");
    }
    // Observable slots always carry the quality factors on the wire; a
    // force-closed slot (None) is never observable and returns above.
    let seeing = weather
        .seeing_arcsec
        .ok_or_else(|| anyhow::anyhow!("seeing must be numeric"))?;
    let transparency = weather
        .transparency
        .ok_or_else(|| anyhow::anyhow!("transparency must be numeric"))?;
    let sky_quality = weather
        .sky_quality
        .ok_or_else(|| anyhow::anyhow!("sky quality must be numeric"))?;
    let mut atmospheric = transparency * sky_quality
        / (seeing * airmass.powf(weather_interface.airmass_exponent));
    atmospheric = atmospheric.min(weather_interface.maximum_weather_quality);
    Ok((atmospheric, lunar, atmospheric * lunar))
}

/// (request_id, apportioned value) per matching active incomplete request.
fn request_options(snapshot: &DecisionSnapshot, tile_id: &str) -> Vec<(String, f64)> {
    let mut options = Vec::new();
    for request in &snapshot.active_requests {
        if request.is_complete {
            continue;
        }
        let matching = request
            .tile_requirements
            .iter()
            .any(|item| item.tile_id == tile_id && item.remaining_or_required() > 0);
        if !matching {
            continue;
        }
        let remaining_tiles = (request.required_tile_count - request.satisfied_tile_count).max(1);
        let eventual_delta = request.completion_reward + request.miss_penalty;
        options.push((request.request_id.clone(), eventual_delta / remaining_tiles as f64));
    }
    options
}

fn known_window_can_finish(snapshot: &DecisionSnapshot, candidate: &CandidateTile) -> bool {
    snapshot.cursor.timestamp_utc
        + chrono::TimeDelta::seconds(candidate.nominal_exptime_seconds)
        <= candidate.window_end_utc
}

/// Rank legal starts using public current state without reading future truth.
///
/// `tile_best_scores`: realized per-tile bests from snapshot feedback; `None`
/// means untracked (legacy), where repeats of completed tiles without request
/// value are never candidates.
pub fn preview_actions(
    snapshot: &DecisionSnapshot,
    scoring_contract: &ScoringContract,
    tile_best_scores: Option<&HashMap<String, f64>>,
) -> Result<Vec<CandidatePreview>> {
    let score_config = &scoring_contract.score_config;
    if score_config.schema_version != "challenge-score-v3" {
        bail!("unsupported score config schema_version");
    }
    let weather_interface = &scoring_contract.weather_score_interface;
    let penalties = &score_config.penalties;
    let quota = score_config.flexible_quota_per_region;
    let empty_bests = HashMap::new();
    let best_scores = tile_best_scores.unwrap_or(&empty_bests);
    let flexible_progress = &snapshot.progress.flexible_completed_by_region;
    let mut result = Vec::new();
    for candidate in &snapshot.candidate_tiles {
        if !candidate.effective_weather.is_observable || !known_window_can_finish(snapshot, candidate)
        {
            continue;
        }
        let tile_id = candidate.tile_id.clone();
        let already_completed = candidate.already_completed;
        let options = request_options(snapshot, &tile_id);
        if already_completed && tile_best_scores.is_none() && options.is_empty() {
            // Pre-anomaly semantics: without a realized-best ledger a repeat is
            // never a candidate (and would be an invalid duplicate on the platform).
            continue;
        }
        let action_options: Vec<(String, f64)> = if options.is_empty() {
            vec![(String::new(), 0.0)]
        } else {
            options
        };
        let (atmospheric, lunar, combined) = combined_quality(candidate, weather_interface)?;
        let band = quality_band(combined, score_config);
        let tile_value = candidate.tile_science_value;
        let potential = tile_value * combined * (1.0 + score_config.program_bonus.for_program(band));
        let science = if already_completed {
            match best_scores.get(&tile_id) {
                Some(banked) => (potential - banked).max(0.0),
                None => 0.0,
            }
        } else {
            potential
        };
        let mut terminal_avoidance = 0.0;
        if !already_completed && candidate.scheduling_class == SchedulingClass::Required {
            terminal_avoidance = penalties.required_miss;
        } else if !already_completed
            && candidate.scheduling_class == SchedulingClass::Flexible
            && flexible_progress.get(&candidate.region_id).copied().unwrap_or(0) < quota
        {
            terminal_avoidance = penalties.flexible_shortfall_per_tile;
        }
        let exposure = candidate.nominal_exptime_seconds;
        if exposure <= 0 {
            bail!("nominal exposure must be positive");
        }
        for (request_id, request_value) in action_options {
            let total = science + terminal_avoidance + request_value;
            result.push(CandidatePreview {
                tile_id: tile_id.clone(),
                program: band,
                request_id,
                region_id: candidate.region_id.clone(),
                scheduling_class: candidate.scheduling_class.as_str().to_string(),
                nominal_exptime_seconds: exposure,
                atmospheric_quality: round6(atmospheric),
                lunar_quality_factor: round6(lunar),
                combined_quality: round6(combined),
                quality_band: band,
                tile_science_value: round6(tile_value),
                estimated_science_score: round6(science),
                terminal_penalty_avoidance: round6(terminal_avoidance),
                request_policy_value: round6(request_value),
                estimated_total_gain: round6(total),
                estimated_gain_per_second: round9(total / exposure as f64),
                estimate_semantics: "official formula with current conditions held constant; repeat observations show the marginal gain over the caller-supplied banked best (0 while bests are untracked); request value is apportioned over remaining required tiles; authoritative replay may differ after future weather changes".to_string(),
            });
        }
    }
    // Python list.sort is stable; so is slice::sort_by. The rounded fields are
    // the sort keys, with -0.0 == 0.0 under partial_cmp as in Python.
    result.sort_by(|left, right| {
        (-left.estimated_gain_per_second)
            .partial_cmp(&(-right.estimated_gain_per_second))
            .unwrap()
            .then(
                (-left.estimated_total_gain)
                    .partial_cmp(&(-right.estimated_total_gain))
                    .unwrap(),
            )
            .then(left.tile_id.cmp(&right.tile_id))
            .then(left.request_id.cmp(&right.request_id))
            .then(left.program.cmp(&right.program))
    });
    Ok(result)
}
