//! Versioned file contracts and shared serialization helpers.
//!
//! Port of `challenge/contracts.py`. Column lists are the exact CSV headers, in
//! order; readers reject any file whose header differs.

use std::io::Read;
use std::path::Path;

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;
use sha2::{Digest, Sha256};

pub const PARTICIPANT_PROTOCOL_VERSION: &str = "participant-agent-protocol-v2";
pub const INITIAL_PUBLICATION_VERSION: &str = "initial-publication-v2";
pub const DECISION_SNAPSHOT_VERSION: &str = "decision-snapshot-v3";
pub const WORKFLOW_RESULT_VERSION: &str = "workflow-result-v2";

pub const LEGACY_PARTICIPANT_PROTOCOL_VERSION: &str = "participant-agent-protocol-v1";
pub const LEGACY_DECISION_SNAPSHOT_VERSION: &str = "decision-snapshot-v2";
pub const ACCEPTED_PROTOCOL_VERSIONS: [&str; 2] = [
    LEGACY_PARTICIPANT_PROTOCOL_VERSION,
    PARTICIPANT_PROTOCOL_VERSION,
];

pub const ANOMALY_CONFIG_SECTIONS: [&str; 4] = [
    "repeat_observation",
    "reporting",
    "anomaly_tags",
    "fault_response",
];

/// The single switch: a scenario opts into the anomaly mechanics through its score config.
pub fn anomaly_mechanics_enabled(score_config: &Value) -> bool {
    ANOMALY_CONFIG_SECTIONS
        .iter()
        .any(|section| score_config.get(section).is_some())
}

pub const NIGHT_COLUMNS: [&str; 8] = [
    "night_id",
    "night_date",
    "solar_dusk_utc",
    "solar_dawn_utc",
    "observing_start_utc",
    "observing_end_utc",
    "night_seconds",
    "slot_count",
];

pub const SLOT_COLUMNS: [&str; 4] = ["slot_id", "night_id", "timestamp_utc", "duration_seconds"];

pub const TILE_COLUMNS: [&str; 12] = [
    "tile_id",
    "ra_deg",
    "dec_deg",
    "nominal_exptime_seconds",
    "region_id",
    "scheduling_class",
    "available_from_utc",
    "available_until_utc",
    "n_lrg",
    "n_elg",
    "n_qso",
    "n_bgs",
];

pub const TARGET_COLUMNS: [&str; 6] = [
    "target_id",
    "tile_id",
    "target_class",
    "feature_flux",
    "redshift",
    "science_weight",
];

pub const TILE_WINDOW_COLUMNS: [&str; 16] = [
    "window_id",
    "night_id",
    "night_date",
    "tile_id",
    "window_start_utc",
    "window_end_utc",
    "window_seconds",
    "best_time_utc",
    "best_airmass",
    "mean_airmass",
    "mean_lunar_quality_factor",
    "minimum_lunar_quality_factor",
    "region_id",
    "scheduling_class",
    "nominal_exptime_seconds",
    "available_until_utc",
];

pub const WEATHER_COLUMNS: [&str; 9] = [
    "slot_id",
    "night_id",
    "timestamp_utc",
    "duration_seconds",
    "is_observable",
    "seeing_arcsec",
    "transparency",
    "sky_quality",
    "instrument_efficiency",
];

pub const FORECAST_COLUMNS: [&str; 13] = [
    "forecast_id",
    "event_id",
    "revision",
    "issued_at_utc",
    "condition",
    "predicted_start_utc",
    "predicted_end_utc",
    "spatial_scope_type",
    "spatial_scope_payload",
    "severity",
    "probability",
    "start_uncertainty_seconds",
    "end_uncertainty_seconds",
];

pub const EVENT_COLUMNS: [&str; 12] = [
    "event_id",
    "condition",
    "actual_start_utc",
    "actual_end_utc",
    "spatial_scope_type",
    "spatial_scope_payload",
    "severity",
    "force_close",
    "seeing_multiplier",
    "transparency_multiplier",
    "sky_quality_multiplier",
    "instrument_efficiency_multiplier",
];

pub const REQUEST_COLUMNS: [&str; 10] = [
    "request_id",
    "issued_at_utc",
    "available_from_utc",
    "deadline_utc",
    "deadline_class",
    "completion_mode",
    "required_tile_count",
    "completion_reward",
    "miss_penalty",
    "reason",
];

pub const REQUEST_TILE_COLUMNS: [&str; 3] = ["request_id", "tile_id", "required_visits"];

pub const DECISION_COLUMNS: [&str; 7] = [
    "decision_id",
    "slot_id",
    "action",
    "tile_id",
    "program",
    "request_id",
    "reason",
];

pub const REPORT_KINDS: [&str; 3] = ["Instrument_Failure", "NOVA", "Reddening"];

pub const REPORT_ACTIONS: [(&str, &str); 3] = [
    ("report_instrument_failure", "Instrument_Failure"),
    ("report_nova", "NOVA"),
    ("report_reddening", "Reddening"),
];

pub const ANOMALY_TAG_VALUES: [&str; 2] = ["nova", "reddening"];

pub const TILE_ANOMALY_COLUMNS: [&str; 2] = ["tile_id", "anomaly_tag"];

/// Parse the contract UTC timestamp form and require an aware value.
///
/// Mirrors Python's `datetime.fromisoformat(value.strip().replace("Z", "+00:00"))`
/// followed by an aware-check and `astimezone(UTC)`. RFC 3339 parsing covers the
/// contract form `%Y-%m-%dT%H:%M:%SZ`, explicit offsets, and fractional seconds;
/// a space separator is normalized to `T` as Python's fromisoformat allows it.
/// Naive timestamps (no offset) are rejected because RFC 3339 requires an offset.
pub fn parse_utc(value: &str) -> Result<DateTime<Utc>> {
    let trimmed = value.trim();
    let replaced = trimmed.replace('Z', "+00:00");
    let normalized = if replaced.len() > 10 && replaced.as_bytes()[10] == b' ' {
        format!("{}T{}", &replaced[..10], &replaced[11..])
    } else {
        replaced
    };
    let parsed = DateTime::parse_from_rfc3339(&normalized)
        .map_err(|_| anyhow!("invalid UTC timestamp {value:?}"))?;
    Ok(parsed.with_timezone(&Utc))
}

/// Format at second precision with the contract `Z` suffix.
pub fn format_utc(value: &DateTime<Utc>) -> String {
    value.format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Only the literals "true"/"false" (case-insensitive after strip) are accepted.
pub fn parse_bool(value: &str) -> Result<bool> {
    match value.trim().to_lowercase().as_str() {
        "true" => Ok(true),
        "false" => Ok(false),
        _ => bail!("invalid boolean {value:?}; expected true or false"),
    }
}

/// Read a CSV whose header must exactly equal `columns`, in order.
/// Tolerates a UTF-8 BOM (Python's `utf-8-sig`). Rows are returned as raw
/// string cells aligned to `columns`.
pub fn read_exact_csv(path: &Path, columns: &[&str]) -> Result<Vec<Vec<String>>> {
    let raw = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    let body: &[u8] = if raw.starts_with(&[0xEF, 0xBB, 0xBF]) {
        &raw[3..]
    } else {
        &raw[..]
    };
    let mut reader = csv::ReaderBuilder::new().flexible(false).from_reader(body);
    let headers = reader
        .headers()
        .with_context(|| format!("reading header of {}", path.display()))?
        .clone();
    let actual: Vec<&str> = headers.iter().collect();
    if actual != columns {
        bail!(
            "{} columns are {actual:?}; expected {columns:?}",
            path.display()
        );
    }
    let mut rows = Vec::new();
    for record in reader.records() {
        let record = record.with_context(|| format!("parsing {}", path.display()))?;
        rows.push(record.iter().map(str::to_string).collect());
    }
    Ok(rows)
}

/// Write a CSV with the exact header and LF line endings on every platform.
/// Every row must have exactly `columns.len()` cells (Python's
/// `extrasaction="raise"` analog).
pub fn write_exact_csv(path: &Path, columns: &[&str], rows: &[Vec<String>]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut writer = csv::WriterBuilder::new()
        .terminator(csv::Terminator::Any(b'\n'))
        .from_path(path)?;
    writer.write_record(columns)?;
    for row in rows {
        if row.len() != columns.len() {
            bail!(
                "{}: row has {} cells; expected {}",
                path.display(),
                row.len(),
                columns.len()
            );
        }
        writer.write_record(row)?;
    }
    writer.flush()?;
    Ok(())
}

/// Write UTF-8 text with LF line endings, so generated files hash identically everywhere.
pub fn write_text_lf(path: &Path, text: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, text.as_bytes())?;
    Ok(())
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut file =
        std::fs::File::open(path).with_context(|| format!("reading {}", path.display()))?;
    let mut digest = Sha256::new();
    let mut chunk = vec![0u8; 1024 * 1024];
    loop {
        let read = file.read(&mut chunk)?;
        if read == 0 {
            break;
        }
        digest.update(&chunk[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// Python `round(x, 6)` — round-half-even on the exact binary float. Rust float
/// formatting is correctly rounded ties-to-even, so format-then-parse matches.
pub fn round6(x: f64) -> f64 {
    format!("{x:.6}").parse().unwrap()
}

/// Python `round(x, 9)`; see [`round6`].
pub fn round9(x: f64) -> f64 {
    format!("{x:.9}").parse().unwrap()
}

/// JSON object key access with a contract-style error.
pub fn json_at<'a>(value: &'a Value, key: &str) -> Result<&'a Value> {
    value
        .get(key)
        .ok_or_else(|| anyhow!("missing config key {key:?}"))
}

/// Read a JSON number as f64 (Python `float(config[key])`).
pub fn json_f64(value: &Value, key: &str) -> Result<f64> {
    json_at(value, key)?
        .as_f64()
        .ok_or_else(|| anyhow!("config key {key:?} is not a number"))
}

/// Read a JSON number as i64 (Python `int(config[key])` — truncation toward zero).
pub fn json_i64(value: &Value, key: &str) -> Result<i64> {
    let number = json_at(value, key)?;
    if let Some(value) = number.as_i64() {
        return Ok(value);
    }
    if let Some(value) = number.as_u64() {
        return Ok(value as i64);
    }
    number
        .as_f64()
        .map(|value| value as i64)
        .ok_or_else(|| anyhow!("config key {key:?} is not a number"))
}

/// Seconds since the Unix epoch as f64, matching Python's `datetime.timestamp()`
/// (whole seconds plus microseconds / 1e6).
pub fn epoch_seconds(moment: &DateTime<Utc>) -> f64 {
    moment.timestamp() as f64 + moment.timestamp_subsec_micros() as f64 / 1e6
}

/// Inverse of [`epoch_seconds`] for the whole/half-second values this kit produces.
pub fn datetime_from_epoch(epoch: f64) -> DateTime<Utc> {
    let seconds = epoch.floor() as i64;
    let nanos = ((epoch - epoch.floor()) * 1e9).round() as u32;
    DateTime::from_timestamp(seconds, nanos).expect("epoch out of range")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_utc_accepts_contract_form() {
        let moment = parse_utc("2026-10-06T02:00:00Z").unwrap();
        assert_eq!(format_utc(&moment), "2026-10-06T02:00:00Z");
    }

    #[test]
    fn parse_utc_converts_offsets() {
        let moment = parse_utc("2026-10-06T04:00:00+02:00").unwrap();
        assert_eq!(format_utc(&moment), "2026-10-06T02:00:00Z");
    }

    #[test]
    fn parse_utc_rejects_naive() {
        assert!(parse_utc("2026-10-06T02:00:00").is_err());
        assert!(parse_utc("not a timestamp").is_err());
    }

    #[test]
    fn parse_bool_is_strict() {
        assert!(parse_bool("true").unwrap());
        assert!(!parse_bool(" False ").unwrap());
        assert!(parse_bool("1").is_err());
        assert!(parse_bool("yes").is_err());
    }

    #[test]
    fn round6_is_ties_to_even() {
        assert_eq!(round6(0.1234565), 0.123456); // binary double is below the tie
        assert_eq!(round6(1.0), 1.0);
        assert_eq!(round6(0.99832), 0.99832);
    }
}
