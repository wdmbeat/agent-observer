//! Participant-safe current-action estimates built from the public score
//! contract — port of `agent/scoring_preview.py::preview_actions`.
//!
//! All ranking fields are rounded (6 decimals, 9 for gain-per-second) BEFORE
//! the stable sort, exactly as the Python does.

use std::collections::HashMap;

use anyhow::{bail, Result};
use serde_json::Value;

use crate::contracts::{parse_utc, round6, round9};

pub const PROGRAMS: [&str; 3] = ["DARK", "BRIGHT", "BACKUP"];

/// One legal current-snapshot action and its transparent ranking terms.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct CandidatePreview {
    pub tile_id: String,
    pub program: String,
    pub request_id: String,
    pub region_id: String,
    pub scheduling_class: String,
    pub nominal_exptime_seconds: i64,
    pub atmospheric_quality: f64,
    pub lunar_quality_factor: f64,
    pub combined_quality: f64,
    pub quality_band: String,
    pub tile_science_value: f64,
    pub estimated_science_score: f64,
    pub terminal_penalty_avoidance: f64,
    pub request_policy_value: f64,
    pub estimated_total_gain: f64,
    pub estimated_gain_per_second: f64,
    pub estimate_semantics: String,
}

/// Python `float(value)`: numbers pass through, numeric strings parse.
fn number(value: &Value, name: &str) -> Result<f64> {
    let result = match value {
        Value::Number(number) => number.as_f64(),
        Value::String(text) => text.trim().parse::<f64>().ok(),
        _ => None,
    };
    let result = result.ok_or_else(|| anyhow::anyhow!("{name} must be numeric"))?;
    if !result.is_finite() {
        bail!("{name} must be finite");
    }
    Ok(result)
}

fn quality_band(quality: f64, score_config: &Value) -> Result<&'static str> {
    let thresholds = &score_config["quality_thresholds"];
    if quality >= number(&thresholds["dark"], "dark threshold")? {
        return Ok("DARK");
    }
    if quality >= number(&thresholds["bright"], "bright threshold")? {
        return Ok("BRIGHT");
    }
    Ok("BACKUP")
}

/// Efficiency-free atmospheric quality × lunar; (atmospheric, lunar, combined).
fn combined_quality(candidate: &Value, weather_interface: &Value) -> Result<(f64, f64, f64)> {
    let weather = &candidate["effective_weather"];
    let geometry = &candidate["geometry"];
    let lunar = number(&geometry["lunar_quality_factor"], "lunar factor")?;
    if !weather["is_observable"].as_bool().unwrap_or(false) {
        return Ok((0.0, lunar, 0.0));
    }
    let airmass = number(&geometry["airmass"], "airmass")?;
    if airmass <= 0.0 {
        bail!("airmass must be positive");
    }
    let mut atmospheric = number(&weather["transparency"], "transparency")?
        * number(&weather["sky_quality"], "sky quality")?
        / (number(&weather["seeing_arcsec"], "seeing")?
            * airmass.powf(number(
                &weather_interface["airmass_exponent"],
                "airmass exponent",
            )?));
    atmospheric = atmospheric.min(number(
        &weather_interface["maximum_weather_quality"],
        "maximum weather quality",
    )?);
    Ok((atmospheric, lunar, atmospheric * lunar))
}

/// (request_id, apportioned value) per matching active incomplete request.
fn request_options(snapshot: &Value, tile_id: &str) -> Result<Vec<(String, f64)>> {
    let mut options = Vec::new();
    for request in snapshot["active_requests"].as_array().into_iter().flatten() {
        if request["is_complete"].as_bool().unwrap_or(false) {
            continue;
        }
        let matching = request["tile_requirements"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| {
                item["tile_id"].as_str() == Some(tile_id)
                    && item
                        .get("remaining_visits")
                        .or_else(|| item.get("required_visits"))
                        .and_then(Value::as_i64)
                        .unwrap_or(0)
                        > 0
            });
        if matching.is_none() {
            continue;
        }
        let remaining_tiles = (request["required_tile_count"].as_i64().unwrap_or(0)
            - request["satisfied_tile_count"].as_i64().unwrap_or(0))
        .max(1);
        let eventual_delta = number(&request["completion_reward"], "request reward")?
            + number(&request["miss_penalty"], "request miss penalty")?;
        options.push((
            request["request_id"].as_str().unwrap_or("").to_string(),
            eventual_delta / remaining_tiles as f64,
        ));
    }
    Ok(options)
}

fn known_window_can_finish(snapshot: &Value, candidate: &Value) -> Result<bool> {
    let start = parse_utc(snapshot["cursor"]["timestamp_utc"].as_str().unwrap_or(""))?;
    let end = parse_utc(candidate["window_end_utc"].as_str().unwrap_or(""))?;
    let exposure = candidate["nominal_exptime_seconds"].as_i64().unwrap_or(0);
    Ok(start + chrono::TimeDelta::seconds(exposure) <= end)
}

/// Rank legal starts using public current state without reading future truth.
///
/// `tile_best_scores`: realized per-tile bests from snapshot feedback; `None`
/// means untracked (legacy), where repeats of completed tiles without request
/// value are never candidates.
pub fn preview_actions(
    snapshot: &Value,
    scoring_contract: &Value,
    tile_best_scores: Option<&HashMap<String, f64>>,
) -> Result<Vec<CandidatePreview>> {
    let schema = snapshot["schema_version"].as_str().unwrap_or("");
    if schema != "decision-snapshot-v2" && schema != "decision-snapshot-v3" {
        bail!("unsupported decision snapshot schema_version");
    }
    let score_config = &scoring_contract["score_config"];
    if score_config["schema_version"].as_str() != Some("challenge-score-v3") {
        bail!("unsupported score config schema_version");
    }
    let weather_interface = &scoring_contract["weather_score_interface"];
    let penalties = &score_config["penalties"];
    let bonuses = &score_config["program_bonus"];
    let quota = score_config["flexible_quota_per_region"]
        .as_i64()
        .unwrap_or(0);
    let empty_bests = HashMap::new();
    let best_scores = tile_best_scores.unwrap_or(&empty_bests);
    let flexible_progress = &snapshot["progress"]["flexible_completed_by_region"];
    let mut result = Vec::new();
    for candidate in snapshot["candidate_tiles"].as_array().into_iter().flatten() {
        let weather = &candidate["effective_weather"];
        if !weather["is_observable"].as_bool().unwrap_or(false)
            || !known_window_can_finish(snapshot, candidate)?
        {
            continue;
        }
        let tile_id = candidate["tile_id"].as_str().unwrap_or("").to_string();
        let already_completed = candidate["already_completed"].as_bool().unwrap_or(false);
        let options = request_options(snapshot, &tile_id)?;
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
        let band = quality_band(combined, score_config)?;
        let tile_value = number(&candidate["tile_science_value"], "tile science value")?;
        let potential = tile_value * combined * (1.0 + number(&bonuses[band], "program bonus")?);
        let science = if already_completed {
            match best_scores.get(&tile_id) {
                Some(banked) => (potential - banked).max(0.0),
                None => 0.0,
            }
        } else {
            potential
        };
        let scheduling_class = candidate["scheduling_class"].as_str().unwrap_or("");
        let mut terminal_avoidance = 0.0;
        if !already_completed && scheduling_class == "REQUIRED" {
            terminal_avoidance = number(&penalties["required_miss"], "required miss penalty")?;
        } else if !already_completed
            && scheduling_class == "FLEXIBLE"
            && flexible_progress[candidate["region_id"].as_str().unwrap_or("")]
                .as_i64()
                .unwrap_or(0)
                < quota
        {
            terminal_avoidance = number(
                &penalties["flexible_shortfall_per_tile"],
                "flexible shortfall penalty",
            )?;
        }
        let exposure = candidate["nominal_exptime_seconds"].as_i64().unwrap_or(0);
        if exposure <= 0 {
            bail!("nominal exposure must be positive");
        }
        for (request_id, request_value) in action_options {
            let total = science + terminal_avoidance + request_value;
            result.push(CandidatePreview {
                tile_id: tile_id.clone(),
                program: band.to_string(),
                request_id,
                region_id: candidate["region_id"].as_str().unwrap_or("").to_string(),
                scheduling_class: scheduling_class.to_string(),
                nominal_exptime_seconds: exposure,
                atmospheric_quality: round6(atmospheric),
                lunar_quality_factor: round6(lunar),
                combined_quality: round6(combined),
                quality_band: band.to_string(),
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
