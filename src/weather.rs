//! Directional weather: loaders and the time-safe runtime simulator.
//!
//! Port of the runtime-facing parts of `challenge/weather_simulator.py`:
//! `WeatherSlot` / `WeatherEvent` / `Forecast`, their loaders, config
//! validation, `WeatherSimulator.get_effective_conditions` /
//! `get_weather_forecast`, and the `weather_quality` formula. Generation
//! (`generate_events`, `generate_weather`, `generate_forecasts`) is not ported.

use std::collections::HashMap;
use std::path::Path;
use std::rc::Rc;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, TimeDelta, Utc};
use serde_json::Value;

use crate::contracts::{
    epoch_seconds, format_utc, json_at, json_f64, json_i64, parse_bool, parse_utc, read_exact_csv,
    round6, EVENT_COLUMNS, FORECAST_COLUMNS, WEATHER_COLUMNS,
};
use crate::geometry::{angular_separation_deg, TileGeometrySimulator};

pub const SCHEMA_VERSION: &str = "directional-weather-v2";
pub const LEGACY_SCHEMA_VERSION: &str = "directional-weather-v1";
pub const CONDITIONS: [&str; 7] = [
    "rainy",
    "cloudy",
    "smoggy",
    "rocket_launch",
    "cold_wave",
    "tornado",
    "instrument_fault",
];
pub const UNFORECASTABLE_CONDITIONS: [&str; 1] = ["instrument_fault"];
pub const FORECASTABLE_CONDITIONS: [&str; 6] = [
    "rainy",
    "cloudy",
    "smoggy",
    "rocket_launch",
    "cold_wave",
    "tornado",
];
pub const SCOPE_TYPES: [&str; 5] = [
    "ALL",
    "REGION_SET",
    "SKY_CAP_ICRS",
    "HORIZON_SECTOR",
    "TILE_SET",
];

/// Compact JSON with sorted keys — Python's
/// `json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":"))`.
pub fn json_payload(value: &Value) -> String {
    // Keys are emitted in sorted order regardless of serde_json's
    // preserve_order feature.
    fn render(value: &Value, out: &mut String) {
        match value {
            Value::Object(map) => {
                out.push('{');
                let mut keys: Vec<&String> = map.keys().collect();
                keys.sort();
                for (index, key) in keys.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    out.push_str(&serde_json::to_string(key).unwrap());
                    out.push(':');
                    render(&map[*key], out);
                }
                out.push('}');
            }
            Value::Array(items) => {
                out.push('[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(',');
                    }
                    render(item, out);
                }
                out.push(']');
            }
            other => out.push_str(&serde_json::to_string(other).unwrap()),
        }
    }
    let mut out = String::new();
    render(value, &mut out);
    out
}

/// Validate a `spatial_scope_payload` cell against its scope type; returns the
/// parsed JSON object.
pub fn parse_payload(scope_type: &str, raw: &str) -> Result<Value> {
    let payload: Value = serde_json::from_str(raw)
        .map_err(|error| anyhow::anyhow!("invalid spatial_scope_payload: {error}"))?;
    let object = payload
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("spatial_scope_payload must be a JSON object"))?;
    let expected: &[&str] = match scope_type {
        "ALL" => &[],
        "REGION_SET" => &["region_ids"],
        "TILE_SET" => &["tile_ids"],
        "SKY_CAP_ICRS" => &["ra_deg", "dec_deg", "radius_deg"],
        "HORIZON_SECTOR" => &[
            "azimuth_start_deg",
            "azimuth_end_deg",
            "min_altitude_deg",
            "max_altitude_deg",
        ],
        _ => bail!("invalid payload keys for {scope_type}"),
    };
    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    let mut expected_sorted = expected.to_vec();
    expected_sorted.sort_unstable();
    if keys != expected_sorted {
        bail!("invalid payload keys for {scope_type}");
    }
    match scope_type {
        "REGION_SET" | "TILE_SET" => {
            let key = if scope_type == "REGION_SET" {
                "region_ids"
            } else {
                "tile_ids"
            };
            let list = object[key]
                .as_array()
                .ok_or_else(|| anyhow::anyhow!("{key} must be a non-empty unique list"))?;
            let unique: std::collections::HashSet<&Value> = list.iter().collect();
            if list.is_empty() || unique.len() != list.len() {
                bail!("{key} must be a non-empty unique list");
            }
        }
        "SKY_CAP_ICRS" => {
            let ra = json_f64(&payload, "ra_deg")?;
            let dec = json_f64(&payload, "dec_deg")?;
            let radius = json_f64(&payload, "radius_deg")?;
            if !(0.0..360.0).contains(&ra)
                || !(-90.0..=90.0).contains(&dec)
                || !(0.0 < radius && radius <= 180.0)
            {
                bail!("invalid SKY_CAP_ICRS payload");
            }
        }
        "HORIZON_SECTOR" => {
            let azimuth_start = json_f64(&payload, "azimuth_start_deg")?;
            let azimuth_end = json_f64(&payload, "azimuth_end_deg")?;
            let min_altitude = json_f64(&payload, "min_altitude_deg")?;
            let max_altitude = json_f64(&payload, "max_altitude_deg")?;
            if !(0.0..=360.0).contains(&azimuth_start) || !(0.0..=360.0).contains(&azimuth_end) {
                bail!("invalid horizon azimuth range");
            }
            if !(-90.0 <= min_altitude && min_altitude < max_altitude && max_altitude <= 90.0) {
                bail!("invalid horizon altitude range");
            }
        }
        _ => {}
    }
    Ok(payload)
}

#[derive(Debug, Clone, PartialEq)]
pub struct WeatherSlot {
    pub slot_id: String,
    pub night_id: String,
    pub timestamp_utc: DateTime<Utc>,
    pub duration_seconds: i64,
    pub is_observable: bool,
    pub seeing_arcsec: Option<f64>,
    pub transparency: Option<f64>,
    pub sky_quality: Option<f64>,
    pub instrument_efficiency: Option<f64>,
}

impl WeatherSlot {
    pub fn end_utc(&self) -> DateTime<Utc> {
        self.timestamp_utc + TimeDelta::seconds(self.duration_seconds)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct WeatherEvent {
    pub event_id: String,
    pub condition: String,
    pub actual_start_utc: DateTime<Utc>,
    pub actual_end_utc: DateTime<Utc>,
    pub spatial_scope_type: String,
    pub spatial_scope_payload: Value,
    pub severity: f64,
    pub force_close: bool,
    pub seeing_multiplier: f64,
    pub transparency_multiplier: f64,
    pub sky_quality_multiplier: f64,
    pub instrument_efficiency_multiplier: f64,
}

impl WeatherEvent {
    pub fn overlaps(&self, slot: &WeatherSlot) -> bool {
        self.actual_start_utc < slot.end_utc() && self.actual_end_utc > slot.timestamp_utc
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct Forecast {
    pub forecast_id: String,
    pub event_id: String,
    pub revision: i64,
    pub issued_at_utc: DateTime<Utc>,
    pub condition: String,
    pub predicted_start_utc: DateTime<Utc>,
    pub predicted_end_utc: DateTime<Utc>,
    pub spatial_scope_type: String,
    pub spatial_scope_payload: Value,
    pub severity: f64,
    pub probability: f64,
    pub start_uncertainty_seconds: i64,
    pub end_uncertainty_seconds: i64,
}

/// `Forecast.public_dict()` — the participant-facing publication form.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct ForecastPublication {
    pub forecast_id: String,
    pub event_id: String,
    pub revision: i64,
    pub issued_at_utc: String,
    pub condition: String,
    pub predicted_start_utc: String,
    pub predicted_end_utc: String,
    pub spatial_scope_type: String,
    pub spatial_scope_payload: String,
    pub severity: f64,
    pub probability: f64,
    pub start_uncertainty_seconds: i64,
    pub end_uncertainty_seconds: i64,
}

impl Forecast {
    pub fn public_dict(&self) -> ForecastPublication {
        ForecastPublication {
            forecast_id: self.forecast_id.clone(),
            event_id: self.event_id.clone(),
            revision: self.revision,
            issued_at_utc: format_utc(&self.issued_at_utc),
            condition: self.condition.clone(),
            predicted_start_utc: format_utc(&self.predicted_start_utc),
            predicted_end_utc: format_utc(&self.predicted_end_utc),
            spatial_scope_type: self.spatial_scope_type.clone(),
            spatial_scope_payload: json_payload(&self.spatial_scope_payload),
            severity: round6(self.severity),
            probability: round6(self.probability),
            start_uncertainty_seconds: self.start_uncertainty_seconds,
            end_uncertainty_seconds: self.end_uncertainty_seconds,
        }
    }
}

/// Load and validate `config/weather_config.json`.
pub fn load_config(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let config: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    let schema = config.get("schema_version").and_then(Value::as_str);
    if schema != Some(SCHEMA_VERSION) && schema != Some(LEGACY_SCHEMA_VERSION) {
        bail!("unsupported weather schema_version");
    }
    let events = json_at(&config, "events")?
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("weather config events must be an object"))?;
    // v1 configs predate instrument_fault; every condition they do define must be known.
    let all_known = events.keys().all(|key| CONDITIONS.contains(&key.as_str()));
    let covers_forecastable = FORECASTABLE_CONDITIONS
        .iter()
        .all(|condition| events.contains_key(*condition));
    if !all_known || !covers_forecastable {
        bail!("weather config must define every condition exactly once");
    }
    for definition in events.values() {
        let scopes = json_at(definition, "scope_weights")?
            .as_object()
            .ok_or_else(|| anyhow::anyhow!("invalid event scope weights"))?;
        if scopes.is_empty()
            || !scopes.keys().all(|key| SCOPE_TYPES.contains(&key.as_str()))
            || scopes
                .values()
                .any(|weight| weight.as_f64().unwrap_or(0.0) <= 0.0)
        {
            bail!("invalid event scope weights");
        }
        if definition
            .get("persists_until_survey_end")
            .and_then(Value::as_bool)
            .unwrap_or(false)
        {
            if json_i64(definition, "count")? > 1 {
                bail!("at most one persistent event can exist (it never ends on its own)");
            }
        } else if definition.get("duration_slots").is_none() {
            bail!("event needs duration_slots unless persists_until_survey_end is set");
        }
        let multiplier_range = definition.get("instrument_efficiency_multiplier_range");
        for (name, value) in [
            ("severity_range", definition.get("severity_range")),
            ("instrument_efficiency_multiplier_range", multiplier_range),
        ] {
            let Some(value) = value else { continue };
            let pair = value
                .as_array()
                .and_then(|items| {
                    if items.len() == 2 {
                        Some((items[0].as_f64()?, items[1].as_f64()?))
                    } else {
                        None
                    }
                })
                .ok_or_else(|| anyhow::anyhow!("{name} must be [lo, hi]"))?;
            if !(pair.0.is_finite() && pair.1.is_finite()) || !(0.0 < pair.0 && pair.0 <= pair.1) {
                bail!("{name} needs two finite numbers with 0 < lo <= hi");
            }
        }
        if multiplier_range.is_none()
            && definition.get("instrument_efficiency_multiplier").is_none()
        {
            bail!("event needs instrument_efficiency_multiplier or instrument_efficiency_multiplier_range");
        }
    }
    Ok(config)
}

fn optional_float(cell: &str) -> Result<Option<f64>> {
    if cell.trim().is_empty() {
        Ok(None)
    } else {
        Ok(Some(cell.trim().parse::<f64>()?))
    }
}

pub fn load_weather(path: &Path) -> Result<Vec<WeatherSlot>> {
    let rows = read_exact_csv(path, &WEATHER_COLUMNS)?;
    let mut result = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let line = index + 2;
        let cell =
            |name: &str| row[WEATHER_COLUMNS.iter().position(|c| *c == name).unwrap()].as_str();
        let build = || -> Result<WeatherSlot> {
            Ok(WeatherSlot {
                slot_id: cell("slot_id").to_string(),
                night_id: cell("night_id").to_string(),
                timestamp_utc: parse_utc(cell("timestamp_utc"))?,
                duration_seconds: cell("duration_seconds").trim().parse::<i64>()?,
                is_observable: parse_bool(cell("is_observable"))?,
                seeing_arcsec: optional_float(cell("seeing_arcsec"))?,
                transparency: optional_float(cell("transparency"))?,
                sky_quality: optional_float(cell("sky_quality"))?,
                instrument_efficiency: optional_float(cell("instrument_efficiency"))?,
            })
        };
        let item = build().with_context(|| format!("{}: row {line}", path.display()))?;
        let all_present = item.seeing_arcsec.is_some()
            && item.transparency.is_some()
            && item.sky_quality.is_some()
            && item.instrument_efficiency.is_some();
        if item.is_observable != all_present {
            bail!(
                "{}: quality fields must be all present iff observable",
                item.slot_id
            );
        }
        result.push(item);
    }
    Ok(result)
}

pub fn load_events(path: &Path) -> Result<Vec<WeatherEvent>> {
    let rows = read_exact_csv(path, &EVENT_COLUMNS)?;
    let mut result = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let line = index + 2;
        let cell =
            |name: &str| row[EVENT_COLUMNS.iter().position(|c| *c == name).unwrap()].as_str();
        let build = || -> Result<WeatherEvent> {
            let scope = cell("spatial_scope_type");
            Ok(WeatherEvent {
                event_id: cell("event_id").to_string(),
                condition: cell("condition").to_string(),
                actual_start_utc: parse_utc(cell("actual_start_utc"))?,
                actual_end_utc: parse_utc(cell("actual_end_utc"))?,
                spatial_scope_type: scope.to_string(),
                spatial_scope_payload: parse_payload(scope, cell("spatial_scope_payload"))?,
                severity: cell("severity").trim().parse::<f64>()?,
                force_close: parse_bool(cell("force_close"))?,
                seeing_multiplier: cell("seeing_multiplier").trim().parse::<f64>()?,
                transparency_multiplier: cell("transparency_multiplier").trim().parse::<f64>()?,
                sky_quality_multiplier: cell("sky_quality_multiplier").trim().parse::<f64>()?,
                instrument_efficiency_multiplier: cell("instrument_efficiency_multiplier")
                    .trim()
                    .parse::<f64>()?,
            })
        };
        result.push(build().with_context(|| format!("{}: row {line}", path.display()))?);
    }
    Ok(result)
}

pub fn load_forecasts(path: &Path) -> Result<Vec<Forecast>> {
    let rows = read_exact_csv(path, &FORECAST_COLUMNS)?;
    let mut result = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let line = index + 2;
        let cell =
            |name: &str| row[FORECAST_COLUMNS.iter().position(|c| *c == name).unwrap()].as_str();
        let build = || -> Result<Forecast> {
            let scope = cell("spatial_scope_type");
            Ok(Forecast {
                forecast_id: cell("forecast_id").to_string(),
                event_id: cell("event_id").to_string(),
                revision: cell("revision").trim().parse::<i64>()?,
                issued_at_utc: parse_utc(cell("issued_at_utc"))?,
                condition: cell("condition").to_string(),
                predicted_start_utc: parse_utc(cell("predicted_start_utc"))?,
                predicted_end_utc: parse_utc(cell("predicted_end_utc"))?,
                spatial_scope_type: scope.to_string(),
                spatial_scope_payload: parse_payload(scope, cell("spatial_scope_payload"))?,
                severity: cell("severity").trim().parse::<f64>()?,
                probability: cell("probability").trim().parse::<f64>()?,
                start_uncertainty_seconds: cell("start_uncertainty_seconds")
                    .trim()
                    .parse::<i64>()?,
                end_uncertainty_seconds: cell("end_uncertainty_seconds").trim().parse::<i64>()?,
            })
        };
        result.push(build().with_context(|| format!("{}: row {line}", path.display()))?);
    }
    Ok(result)
}

fn clip(value: f64, model: &Value) -> Result<f64> {
    Ok(json_f64(model, "minimum")?.max(json_f64(model, "maximum")?.min(value)))
}

/// `start <= value <= end`, wrap-aware when `start > end`.
pub fn azimuth_inside(value: f64, start: f64, end: f64) -> bool {
    if start <= end {
        start <= value && value <= end
    } else {
        value >= start || value <= end
    }
}

/// `get_effective_conditions` result — the truth view payload.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct EffectiveConditions {
    pub slot_id: String,
    pub night_id: String,
    pub timestamp_utc: String,
    pub duration_seconds: i64,
    pub is_observable: bool,
    pub seeing_arcsec: Option<f64>,
    pub transparency: Option<f64>,
    pub sky_quality: Option<f64>,
    pub instrument_efficiency: Option<f64>,
    pub tile_id: Option<String>,
    pub active_event_ids: Vec<String>,
}

/// Time-safe current conditions and as-of forecast publication.
pub struct WeatherSimulator {
    pub weather: Vec<WeatherSlot>,
    pub forecasts: Vec<Forecast>,
    pub events: Vec<WeatherEvent>,
    pub config: Value,
    pub geometry: Option<Rc<TileGeometrySimulator>>,
    /// Run-local overlay: an acknowledged instrument fault stops applying at its repair time.
    pub end_overrides: HashMap<String, DateTime<Utc>>,
    by_slot: HashMap<String, usize>,
}

impl WeatherSimulator {
    pub fn new(
        weather: Vec<WeatherSlot>,
        forecasts: Vec<Forecast>,
        events: Vec<WeatherEvent>,
        config: Value,
        geometry: Option<Rc<TileGeometrySimulator>>,
    ) -> Result<Self> {
        let mut by_slot = HashMap::with_capacity(weather.len());
        for (index, item) in weather.iter().enumerate() {
            if by_slot.insert(item.slot_id.clone(), index).is_some() {
                bail!("weather slot IDs are not unique");
            }
        }
        Ok(Self {
            weather,
            forecasts,
            events,
            config,
            geometry,
            end_overrides: HashMap::new(),
            by_slot,
        })
    }

    fn event_active(&self, event: &WeatherEvent, slot: &WeatherSlot) -> bool {
        let end = self
            .end_overrides
            .get(&event.event_id)
            .copied()
            .unwrap_or(event.actual_end_utc);
        event.actual_start_utc < slot.end_utc() && end > slot.timestamp_utc
    }

    fn applies(&self, event: &WeatherEvent, tile_id: &str, slot: &WeatherSlot) -> Result<bool> {
        if event.spatial_scope_type == "ALL" {
            return Ok(true);
        }
        let geometry = self
            .geometry
            .as_ref()
            .filter(|geometry| geometry.tiles.contains_key(tile_id))
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "directional weather query requires a known tile and geometry simulator"
                )
            })?;
        let tile = &geometry.tiles[tile_id];
        let payload = &event.spatial_scope_payload;
        match event.spatial_scope_type.as_str() {
            "REGION_SET" => Ok(payload["region_ids"]
                .as_array()
                .map(|ids| {
                    ids.iter()
                        .any(|id| id.as_str() == Some(tile.region_id.as_str()))
                })
                .unwrap_or(false)),
            "TILE_SET" => Ok(payload["tile_ids"]
                .as_array()
                .map(|ids| ids.iter().any(|id| id.as_str() == Some(tile_id)))
                .unwrap_or(false)),
            "SKY_CAP_ICRS" => Ok(angular_separation_deg(
                tile.ra_deg,
                tile.dec_deg,
                json_f64(payload, "ra_deg")?,
                json_f64(payload, "dec_deg")?,
            ) <= json_f64(payload, "radius_deg")?),
            "HORIZON_SECTOR" => {
                let midpoint_epoch =
                    epoch_seconds(&slot.timestamp_utc) + slot.duration_seconds as f64 / 2.0;
                let sample = crate::geometry::geometry_sample(
                    tile,
                    midpoint_epoch,
                    &geometry.tile_config,
                    &geometry.calendar_config,
                )?;
                Ok(
                    json_f64(payload, "min_altitude_deg")? <= sample.altitude_deg
                        && sample.altitude_deg <= json_f64(payload, "max_altitude_deg")?
                        && azimuth_inside(
                            sample.azimuth_deg,
                            json_f64(payload, "azimuth_start_deg")?,
                            json_f64(payload, "azimuth_end_deg")?,
                        ),
                )
            }
            _ => Ok(false),
        }
    }

    /// Truth view by default; pass `include_instrument_faults = false` for the
    /// agent-visible snapshot view.
    pub fn get_effective_conditions(
        &self,
        slot_id: &str,
        tile_id: Option<&str>,
        include_instrument_faults: bool,
    ) -> Result<EffectiveConditions> {
        let base = self
            .by_slot
            .get(slot_id)
            .map(|index| &self.weather[*index])
            .ok_or_else(|| anyhow::anyhow!("unknown slot_id {slot_id:?}"))?;
        let mut active: Vec<&WeatherEvent> = Vec::new();
        for event in &self.events {
            if !self.event_active(event, base) {
                continue;
            }
            if !include_instrument_faults && event.condition == "instrument_fault" {
                continue;
            }
            if event.spatial_scope_type == "ALL"
                || (tile_id.is_some() && self.applies(event, tile_id.unwrap(), base)?)
            {
                active.push(event);
            }
        }
        let mut payload = EffectiveConditions {
            slot_id: base.slot_id.clone(),
            night_id: base.night_id.clone(),
            timestamp_utc: format_utc(&base.timestamp_utc),
            duration_seconds: base.duration_seconds,
            is_observable: base.is_observable,
            seeing_arcsec: base.seeing_arcsec,
            transparency: base.transparency,
            sky_quality: base.sky_quality,
            instrument_efficiency: base.instrument_efficiency,
            tile_id: tile_id.map(str::to_string),
            active_event_ids: active.iter().map(|event| event.event_id.clone()).collect(),
        };
        if !base.is_observable {
            return Ok(payload);
        }
        let directional: Vec<&&WeatherEvent> = active
            .iter()
            .filter(|event| event.spatial_scope_type != "ALL")
            .collect();
        if directional.iter().any(|event| event.force_close) {
            payload.is_observable = false;
            payload.seeing_arcsec = None;
            payload.transparency = None;
            payload.sky_quality = None;
            payload.instrument_efficiency = None;
            return Ok(payload);
        }
        let mut seeing = 1.0;
        let mut transparency = 1.0;
        let mut sky_quality = 1.0;
        let mut instrument_efficiency = 1.0;
        for event in &directional {
            seeing *= event.seeing_multiplier;
            transparency *= event.transparency_multiplier;
            sky_quality *= event.sky_quality_multiplier;
            instrument_efficiency *= event.instrument_efficiency_multiplier;
        }
        let quality = json_at(&self.config, "quality")?;
        payload.seeing_arcsec = Some(clip(
            payload.seeing_arcsec.unwrap() * seeing,
            json_at(quality, "seeing_arcsec")?,
        )?);
        payload.transparency = Some(clip(
            payload.transparency.unwrap() * transparency,
            json_at(quality, "transparency")?,
        )?);
        payload.sky_quality = Some(clip(
            payload.sky_quality.unwrap() * sky_quality,
            json_at(quality, "sky_quality")?,
        )?);
        payload.instrument_efficiency = Some(clip(
            payload.instrument_efficiency.unwrap() * instrument_efficiency,
            json_at(quality, "instrument_efficiency")?,
        )?);
        Ok(payload)
    }

    /// Latest revision per event issued at or before `as_of_utc`, within the
    /// forecast horizon window, sorted by `(predicted_start_utc, event_id)`.
    pub fn get_weather_forecast(
        &self,
        as_of_utc: DateTime<Utc>,
        days: Option<i64>,
    ) -> Result<Vec<ForecastPublication>> {
        let horizon_days = match days {
            Some(days) if days != 0 => days,
            _ => json_i64(json_at(&self.config, "forecast")?, "horizon_days")?,
        };
        let horizon_end = as_of_utc + TimeDelta::days(horizon_days);
        let mut latest: HashMap<&str, &Forecast> = HashMap::new();
        for item in &self.forecasts {
            if item.issued_at_utc <= as_of_utc
                && latest
                    .get(item.event_id.as_str())
                    .map(|current| item.revision > current.revision)
                    .unwrap_or(true)
            {
                latest.insert(item.event_id.as_str(), item);
            }
        }
        let mut visible: Vec<&Forecast> = latest
            .values()
            .copied()
            .filter(|item| {
                item.predicted_end_utc > as_of_utc && item.predicted_start_utc < horizon_end
            })
            .collect();
        visible.sort_by(|left, right| {
            (left.predicted_start_utc, &left.event_id)
                .cmp(&(right.predicted_start_utc, &right.event_id))
        });
        Ok(visible.into_iter().map(Forecast::public_dict).collect())
    }
}

/// `H(is_observable) * efficiency * transparency * sky_quality / (seeing * airmass^exponent)`,
/// clipped to the configured maximum.
///
/// `include_efficiency = false` gives the band-determination quality: program
/// bands never depend on instrument efficiency, so preview and replay agree.
pub fn weather_quality(
    weather: &EffectiveConditions,
    airmass: f64,
    config: &Value,
    include_efficiency: bool,
) -> Result<f64> {
    if !weather.is_observable {
        return Ok(0.0);
    }
    if !airmass.is_finite() || airmass <= 0.0 {
        bail!("airmass must be positive and finite");
    }
    let efficiency = if include_efficiency {
        weather.instrument_efficiency.unwrap()
    } else {
        1.0
    };
    let interface = json_at(config, "score_interface")?;
    let raw = efficiency * weather.transparency.unwrap() * weather.sky_quality.unwrap()
        / (weather.seeing_arcsec.unwrap() * airmass.powf(json_f64(interface, "airmass_exponent")?));
    Ok(raw.min(json_f64(interface, "maximum_weather_quality")?))
}
