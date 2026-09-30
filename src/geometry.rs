//! Tiles, tile geometry, and calendar-aligned observing windows.
//!
//! Port of the runtime-facing parts of `challenge/tile_geometry_simulator.py`:
//! `Tile`, `load_tiles`, config load/validate, `TileGeometrySimulator` with
//! `get_tile_geometry` / `get_tile_windows`, and the astronomy math. Catalog
//! generation (`build_catalog`, `generate_catalog`) is not ported.
//!
//! Float operation order mirrors the Python source statement for statement so
//! results are bit-identical: Python `x % 360.0` (floored) is `rem_euclid`,
//! `math.radians` is `to_radians`, `a ** b` is `powf`.

use std::collections::BTreeMap;
use std::collections::HashMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};
use serde_json::Value;

use crate::calendar::{load_nights, load_slots, Night, Slot};
use crate::contracts::{
    datetime_from_epoch, epoch_seconds, format_utc, json_at, json_f64, json_i64, parse_utc,
    read_exact_csv, round6, TILE_COLUMNS,
};

pub const SCHEMA_VERSION: &str = "tile-geometry-v2";
pub const TARGET_CLASSES: [&str; 4] = ["LRG", "ELG", "QSO", "BGS"];

#[derive(Debug, Clone, PartialEq)]
pub struct Tile {
    pub tile_id: String,
    pub ra_deg: f64,
    pub dec_deg: f64,
    pub nominal_exptime_seconds: i64,
    pub region_id: String,
    pub scheduling_class: String,
    pub available_from_utc: DateTime<Utc>,
    pub available_until_utc: DateTime<Utc>,
    pub n_lrg: i64,
    pub n_elg: i64,
    pub n_qso: i64,
    pub n_bgs: i64,
}

impl Tile {
    pub fn csv_row(&self) -> Vec<String> {
        vec![
            self.tile_id.clone(),
            format!("{:.6}", self.ra_deg),
            format!("{:.6}", self.dec_deg),
            self.nominal_exptime_seconds.to_string(),
            self.region_id.clone(),
            self.scheduling_class.clone(),
            format_utc(&self.available_from_utc),
            format_utc(&self.available_until_utc),
            self.n_lrg.to_string(),
            self.n_elg.to_string(),
            self.n_qso.to_string(),
            self.n_bgs.to_string(),
        ]
    }
}

/// Load and validate `config/tile_config.json`.
pub fn load_config(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    let config: Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {}", path.display()))?;
    validate_config(&config)?;
    Ok(config)
}

/// Read a JSON config without validation (mirrors how the Python runtime reads
/// `calendar_config.json` via plain `json.load`).
pub fn load_json(path: &Path) -> Result<Value> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))
}

pub fn validate_config(config: &Value) -> Result<()> {
    let required = [
        "schema_version",
        "seed",
        "geometry",
        "catalog",
        "lunar_model",
        "target_models",
    ];
    let object = config
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("tile geometry config must be an object"))?;
    let has_all_required = required.iter().all(|key| object.contains_key(*key));
    let only_known = object
        .keys()
        .all(|key| required.contains(&key.as_str()) || key == "anomaly_tags");
    // anomaly_tags is optional: absent means the scenario ships no hidden tile tags.
    if !has_all_required
        || !only_known
        || config.get("schema_version") != Some(&Value::from(SCHEMA_VERSION))
    {
        bail!("invalid tile geometry config keys or schema_version");
    }
    let catalog = json_at(config, "catalog")?;
    for key in [
        "n_regions",
        "tiles_per_region",
        "required_per_region",
        "time_limited_required_per_region",
        "time_limited_window_days",
    ] {
        if json_i64(catalog, key)? < 0 {
            bail!("catalog {key} must be non-negative");
        }
    }
    if json_i64(catalog, "n_regions")? < 1 || json_i64(catalog, "tiles_per_region")? < 1 {
        bail!("catalog must contain regions and tiles");
    }
    if json_i64(catalog, "required_per_region")? > json_i64(catalog, "tiles_per_region")? {
        bail!("required_per_region exceeds tiles_per_region");
    }
    if json_i64(catalog, "time_limited_required_per_region")?
        > json_i64(catalog, "required_per_region")?
    {
        bail!("time-limited required count exceeds required count");
    }
    let choices = json_at(catalog, "nominal_exptime_choices_seconds")?
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("nominal_exptime_choices_seconds must be a list"))?;
    if choices.is_empty() || choices.iter().any(|value| value.as_i64().unwrap_or(0) <= 0) {
        bail!("nominal exposure choices must be positive");
    }
    let minimum_altitude = json_f64(json_at(config, "geometry")?, "minimum_altitude_deg")?;
    if !(0.0 < minimum_altitude && minimum_altitude < 90.0) {
        bail!("minimum altitude must lie in (0, 90)");
    }
    let lunar = json_at(config, "lunar_model")?;
    let lunar_keys: Vec<&str> = lunar
        .as_object()
        .map(|object| object.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let mut lunar_keys_sorted = lunar_keys.clone();
    lunar_keys_sorted.sort_unstable();
    if lunar_keys_sorted
        != [
            "altitude_exponent",
            "angular_decay_scale_deg",
            "maximum_penalty",
        ]
    {
        bail!("invalid lunar_model keys");
    }
    if json_f64(lunar, "angular_decay_scale_deg")? <= 0.0 {
        bail!("lunar angular decay scale must be positive");
    }
    if json_f64(lunar, "altitude_exponent")? <= 0.0 {
        bail!("lunar altitude exponent must be positive");
    }
    let maximum_penalty = json_f64(lunar, "maximum_penalty")?;
    if !(0.0..1.0).contains(&maximum_penalty) {
        bail!("lunar maximum penalty must lie in [0, 1)");
    }
    let target_models = json_at(config, "target_models")?;
    let model_keys: Vec<&str> = target_models
        .as_object()
        .map(|object| object.keys().map(String::as_str).collect())
        .unwrap_or_default();
    let mut model_keys_sorted = model_keys.clone();
    model_keys_sorted.sort_unstable();
    let mut classes = TARGET_CLASSES;
    classes.sort_unstable();
    if model_keys_sorted != classes {
        bail!("target_models must define LRG, ELG, QSO, and BGS");
    }
    Ok(())
}

pub fn load_tiles(path: &Path) -> Result<Vec<Tile>> {
    let rows = read_exact_csv(path, &TILE_COLUMNS)?;
    let mut result = Vec::with_capacity(rows.len());
    let mut seen = std::collections::HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        let line = index + 2;
        let cell = |name: &str| row[TILE_COLUMNS.iter().position(|c| *c == name).unwrap()].as_str();
        let build = || -> Result<Tile> {
            Ok(Tile {
                tile_id: cell("tile_id").trim().to_string(),
                ra_deg: cell("ra_deg").trim().parse::<f64>()?,
                dec_deg: cell("dec_deg").trim().parse::<f64>()?,
                nominal_exptime_seconds: cell("nominal_exptime_seconds").trim().parse::<i64>()?,
                region_id: cell("region_id").trim().to_string(),
                scheduling_class: cell("scheduling_class").trim().to_string(),
                available_from_utc: parse_utc(cell("available_from_utc"))?,
                available_until_utc: parse_utc(cell("available_until_utc"))?,
                n_lrg: cell("n_lrg").trim().parse::<i64>()?,
                n_elg: cell("n_elg").trim().parse::<i64>()?,
                n_qso: cell("n_qso").trim().parse::<i64>()?,
                n_bgs: cell("n_bgs").trim().parse::<i64>()?,
            })
        };
        let tile = build().with_context(|| format!("{}: row {line}", path.display()))?;
        if tile.tile_id.is_empty() || !seen.insert(tile.tile_id.clone()) {
            bail!("{}: row {line}: tile_id must be non-empty and unique", path.display());
        }
        if tile.scheduling_class != "REQUIRED" && tile.scheduling_class != "FLEXIBLE" {
            bail!("{}: row {line}: invalid scheduling_class", path.display());
        }
        if !(0.0 <= tile.ra_deg && tile.ra_deg < 360.0)
            || !(-90.0 <= tile.dec_deg && tile.dec_deg <= 90.0)
        {
            bail!("{}: row {line}: invalid coordinates", path.display());
        }
        if tile.available_until_utc <= tile.available_from_utc {
            bail!("{}: row {line}: empty availability interval", path.display());
        }
        result.push(tile);
    }
    Ok(result)
}

/// `moment.timestamp() / 86400.0 + 2440587.5`
pub fn julian_date(epoch: f64) -> f64 {
    epoch / 86400.0 + 2440587.5
}

pub fn local_sidereal_deg(epoch: f64, longitude_deg: f64) -> f64 {
    let days = julian_date(epoch) - 2451545.0;
    (280.46061837 + 360.98564736629 * days + longitude_deg).rem_euclid(360.0)
}

pub fn sun_equatorial_deg(epoch: f64) -> (f64, f64) {
    let days = julian_date(epoch) - 2451545.0;
    let mean_longitude = (280.460 + 0.9856474 * days).rem_euclid(360.0);
    let mean_anomaly = (357.528 + 0.9856003 * days).rem_euclid(360.0).to_radians();
    let longitude = (mean_longitude
        + 1.915 * mean_anomaly.sin()
        + 0.020 * (2.0 * mean_anomaly).sin())
    .rem_euclid(360.0)
    .to_radians();
    let obliquity = (23.439 - 0.0000004 * days).to_radians();
    (
        (obliquity.cos() * longitude.sin())
            .atan2(longitude.cos())
            .to_degrees()
            .rem_euclid(360.0),
        (obliquity.sin() * longitude.sin()).asin().to_degrees(),
    )
}

pub fn moon_equatorial_deg(epoch: f64) -> (f64, f64) {
    let days = julian_date(epoch) - 2451545.0;
    let mean_longitude = (218.316 + 13.176396 * days).rem_euclid(360.0).to_radians();
    let mean_anomaly = (134.963 + 13.064993 * days).rem_euclid(360.0).to_radians();
    let argument_latitude = (93.272 + 13.229350 * days).rem_euclid(360.0).to_radians();
    let longitude = mean_longitude + 6.289f64.to_radians() * mean_anomaly.sin();
    let latitude = 5.128f64.to_radians() * argument_latitude.sin();
    let obliquity = (23.439 - 0.0000004 * days).to_radians();
    let x = longitude.cos() * latitude.cos();
    let y = longitude.sin() * latitude.cos() * obliquity.cos()
        - latitude.sin() * obliquity.sin();
    let z = longitude.sin() * latitude.cos() * obliquity.sin()
        + latitude.sin() * obliquity.cos();
    (y.atan2(x).to_degrees().rem_euclid(360.0), z.asin().to_degrees())
}

pub fn angular_separation_deg(ra1: f64, dec1: f64, ra2: f64, dec2: f64) -> f64 {
    let (ra1r, dec1r, ra2r, dec2r) =
        (ra1.to_radians(), dec1.to_radians(), ra2.to_radians(), dec2.to_radians());
    let cosine =
        dec1r.sin() * dec2r.sin() + dec1r.cos() * dec2r.cos() * (ra1r - ra2r).cos();
    cosine.clamp(-1.0, 1.0).acos().to_degrees()
}

/// Kasten-Young airmass normalized to zenith = 1; infinite at or below the horizon.
pub fn normalized_airmass(altitude_deg: f64) -> f64 {
    if altitude_deg <= 0.0 {
        return f64::INFINITY;
    }
    let zenith_deg = 90.0 - altitude_deg;
    let raw = 1.0
        / (zenith_deg.to_radians().cos() + 0.50572 * (96.07995 - zenith_deg).powf(-1.6364));
    let zenith_raw = 1.0 / (1.0 + 0.50572 * 96.07995f64.powf(-1.6364));
    raw / zenith_raw
}

#[derive(Debug, Clone, Copy, PartialEq, serde::Serialize)]
pub struct GeometrySample {
    pub altitude_deg: f64,
    pub azimuth_deg: f64,
    pub hour_angle_deg: f64,
    pub airmass: f64,
    pub moon_separation_deg: f64,
    pub lunar_quality_factor: f64,
}

/// Full geometry sample (position + airmass + lunar quality) at `epoch`.
pub fn geometry_sample(
    tile: &Tile,
    epoch: f64,
    tile_config: &Value,
    calendar_config: &Value,
) -> Result<GeometrySample> {
    let site = json_at(calendar_config, "site")?;
    let latitude = json_f64(site, "latitude_deg")?.to_radians();
    let longitude = json_f64(site, "longitude_deg")?;
    let declination = tile.dec_deg.to_radians();
    let hour_angle_deg =
        (local_sidereal_deg(epoch, longitude) - tile.ra_deg + 180.0).rem_euclid(360.0) - 180.0;
    let hour_angle = hour_angle_deg.to_radians();
    let sin_altitude = latitude.sin() * declination.sin()
        + latitude.cos() * declination.cos() * hour_angle.cos();
    let altitude = sin_altitude.clamp(-1.0, 1.0).asin();
    let cos_altitude = altitude.cos().max(1e-12);
    let sin_azimuth = -hour_angle.sin() * declination.cos() / cos_altitude;
    let cos_azimuth = (declination.sin() - altitude.sin() * latitude.sin())
        / (cos_altitude * latitude.cos().max(1e-12));
    let azimuth = sin_azimuth.atan2(cos_azimuth).to_degrees().rem_euclid(360.0);
    let altitude_deg = altitude.to_degrees();

    let (sun_ra, sun_dec) = sun_equatorial_deg(epoch);
    let (moon_ra, moon_dec) = moon_equatorial_deg(epoch);
    let sun_moon_separation = angular_separation_deg(sun_ra, sun_dec, moon_ra, moon_dec);
    let illumination = (1.0 - sun_moon_separation.to_radians().cos()) / 2.0;
    let tile_moon_separation =
        angular_separation_deg(tile.ra_deg, tile.dec_deg, moon_ra, moon_dec);
    let moon_altitude =
        geometry_sample_without_lunar_coords(moon_ra, moon_dec, epoch, calendar_config)?.0;
    let lunar = json_at(tile_config, "lunar_model")?;
    let altitude_weight = moon_altitude
        .max(0.0)
        .to_radians()
        .sin()
        .powf(json_f64(lunar, "altitude_exponent")?);
    let angular_weight = (-tile_moon_separation / json_f64(lunar, "angular_decay_scale_deg")?).exp();
    let lunar_quality = 1.0
        - json_f64(lunar, "maximum_penalty")? * illumination * altitude_weight * angular_weight;
    Ok(GeometrySample {
        altitude_deg,
        azimuth_deg: azimuth,
        hour_angle_deg,
        airmass: normalized_airmass(altitude_deg),
        moon_separation_deg: tile_moon_separation,
        lunar_quality_factor: lunar_quality.clamp(0.0, 1.0),
    })
}

/// Positional sample only (no Sun/Moon work). Returns `(altitude_deg, azimuth_deg)`.
pub fn geometry_sample_without_lunar(
    tile: &Tile,
    epoch: f64,
    calendar_config: &Value,
) -> Result<(f64, f64)> {
    geometry_sample_without_lunar_coords(tile.ra_deg, tile.dec_deg, epoch, calendar_config)
}

fn geometry_sample_without_lunar_coords(
    ra_deg: f64,
    dec_deg: f64,
    epoch: f64,
    calendar_config: &Value,
) -> Result<(f64, f64)> {
    let site = json_at(calendar_config, "site")?;
    let latitude = json_f64(site, "latitude_deg")?.to_radians();
    let declination = dec_deg.to_radians();
    let hour_angle_deg =
        (local_sidereal_deg(epoch, json_f64(site, "longitude_deg")?) - ra_deg + 180.0)
            .rem_euclid(360.0)
            - 180.0;
    let hour_angle = hour_angle_deg.to_radians();
    let sin_altitude = latitude.sin() * declination.sin()
        + latitude.cos() * declination.cos() * hour_angle.cos();
    let altitude = sin_altitude.clamp(-1.0, 1.0).asin();
    let cos_altitude = altitude.cos().max(1e-12);
    let sin_azimuth = -hour_angle.sin() * declination.cos() / cos_altitude;
    let cos_azimuth = (declination.sin() - altitude.sin() * latitude.sin())
        / (cos_altitude * latitude.cos().max(1e-12));
    Ok((
        altitude.to_degrees(),
        sin_azimuth.atan2(cos_azimuth).to_degrees().rem_euclid(360.0),
    ))
}

/// `get_tile_geometry` result: the sample plus its publication envelope.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TileGeometry {
    pub tile_id: String,
    pub timestamp_utc: String,
    #[serde(flatten)]
    pub sample: GeometrySample,
}

/// One row of `get_tile_windows`, aligned to `TILE_WINDOW_COLUMNS`.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct TileWindowRow {
    pub window_id: String,
    pub night_id: String,
    pub night_date: String,
    pub tile_id: String,
    pub window_start_utc: String,
    pub window_end_utc: String,
    pub window_seconds: i64,
    pub best_time_utc: String,
    pub best_airmass: f64,
    pub mean_airmass: f64,
    pub mean_lunar_quality_factor: f64,
    pub minimum_lunar_quality_factor: f64,
    pub region_id: String,
    pub scheduling_class: String,
    pub nominal_exptime_seconds: i64,
    pub available_until_utc: String,
}

impl TileWindowRow {
    /// Cells in `TILE_WINDOW_COLUMNS` order. Floats use Rust's shortest round-trip
    /// formatting, which matches Python's `str(round(x, 6))` for these values.
    pub fn csv_row(&self) -> Vec<String> {
        vec![
            self.window_id.clone(),
            self.night_id.clone(),
            self.night_date.clone(),
            self.tile_id.clone(),
            self.window_start_utc.clone(),
            self.window_end_utc.clone(),
            self.window_seconds.to_string(),
            self.best_time_utc.clone(),
            format!("{}", self.best_airmass),
            format!("{}", self.mean_airmass),
            format!("{}", self.mean_lunar_quality_factor),
            format!("{}", self.minimum_lunar_quality_factor),
            self.region_id.clone(),
            self.scheduling_class.clone(),
            self.nominal_exptime_seconds.to_string(),
            self.available_until_utc.clone(),
        ]
    }
}

pub struct TileGeometrySimulator {
    pub tiles: BTreeMap<String, Tile>,
    pub tile_config: Value,
    pub calendar_config: Value,
    pub nights: HashMap<String, Night>,
    pub slots_by_night: HashMap<String, Vec<Slot>>,
}

impl TileGeometrySimulator {
    pub fn new(
        tiles: Vec<Tile>,
        tile_config: Value,
        calendar_config: Value,
        nights: Vec<Night>,
        slots: Vec<Slot>,
    ) -> Self {
        let tiles = tiles.into_iter().map(|tile| (tile.tile_id.clone(), tile)).collect();
        let nights = nights
            .into_iter()
            .map(|night| (night.night_id.clone(), night))
            .collect();
        let mut slots_by_night: HashMap<String, Vec<Slot>> = HashMap::new();
        for slot in slots {
            slots_by_night.entry(slot.night_id.clone()).or_default().push(slot);
        }
        Self { tiles, tile_config, calendar_config, nights, slots_by_night }
    }

    pub fn from_files(
        tiles_path: &Path,
        tile_config_path: &Path,
        calendar_config_path: &Path,
        nights_path: &Path,
        slots_path: &Path,
    ) -> Result<Self> {
        Ok(Self::new(
            load_tiles(tiles_path)?,
            load_config(tile_config_path)?,
            load_json(calendar_config_path)?,
            load_nights(nights_path)?,
            load_slots(slots_path)?,
        ))
    }

    pub fn get_tile_geometry(
        &self,
        tile_id: &str,
        timestamp_utc: DateTime<Utc>,
    ) -> Result<TileGeometry> {
        let tile = self
            .tiles
            .get(tile_id)
            .ok_or_else(|| anyhow::anyhow!("unknown tile_id {tile_id:?}"))?;
        let sample = geometry_sample(
            tile,
            epoch_seconds(&timestamp_utc),
            &self.tile_config,
            &self.calendar_config,
        )?;
        Ok(TileGeometry {
            tile_id: tile_id.to_string(),
            timestamp_utc: format_utc(&timestamp_utc),
            sample,
        })
    }

    pub fn get_tile_windows(&self, first_night: NaiveDate, days: usize) -> Result<Vec<TileWindowRow>> {
        if days < 1 {
            bail!("days must be positive");
        }
        let night_ids: Vec<String> = (0..days)
            .map(|offset| {
                let date = first_night
                    .checked_add_signed(TimeDelta::days(offset as i64))
                    .expect("night date out of range");
                format!("N{}", date.format("%Y%m%d"))
            })
            .collect();
        let missing: Vec<&String> = night_ids
            .iter()
            .filter(|night_id| !self.nights.contains_key(*night_id))
            .collect();
        if !missing.is_empty() {
            bail!("requested nights outside calendar: {missing:?}");
        }
        let minimum_altitude =
            json_f64(json_at(&self.tile_config, "geometry")?, "minimum_altitude_deg")?;
        let mut rows = Vec::new();
        for night_id in &night_ids {
            let night = &self.nights[night_id];
            let empty: Vec<Slot> = Vec::new();
            let slots = self.slots_by_night.get(night_id).unwrap_or(&empty);
            for tile in self.tiles.values() {
                let mut groups: Vec<Vec<(Slot, GeometrySample)>> = Vec::new();
                let mut current: Vec<(Slot, GeometrySample)> = Vec::new();
                for slot in slots {
                    let midpoint_epoch =
                        epoch_seconds(&slot.timestamp_utc) + slot.duration_seconds as f64 / 2.0;
                    let geometry = geometry_sample(
                        tile,
                        midpoint_epoch,
                        &self.tile_config,
                        &self.calendar_config,
                    )?;
                    let eligible = slot.timestamp_utc >= tile.available_from_utc
                        && slot.end_utc() <= tile.available_until_utc
                        && geometry.altitude_deg >= minimum_altitude;
                    if eligible {
                        current.push((slot.clone(), geometry));
                    } else if !current.is_empty() {
                        groups.push(std::mem::take(&mut current));
                    }
                }
                if !current.is_empty() {
                    groups.push(current);
                }
                let valid: Vec<&Vec<(Slot, GeometrySample)>> = groups
                    .iter()
                    .filter(|group| {
                        group.iter().map(|item| item.0.duration_seconds).sum::<i64>()
                            >= tile.nominal_exptime_seconds
                    })
                    .collect();
                for (sequence, group) in valid.iter().enumerate() {
                    let start = group[0].0.timestamp_utc;
                    let end = group[group.len() - 1].0.end_utc();
                    // Python's min returns the first element among ties; Rust's
                    // min_by does the same.
                    let (best_slot, best_geometry) = group
                        .iter()
                        .min_by(|left, right| {
                            left.1.airmass.partial_cmp(&right.1.airmass).unwrap()
                        })
                        .map(|(slot, geometry)| (slot, geometry))
                        .unwrap();
                    let airmass_sum = group.iter().fold(0.0, |sum, item| sum + item.1.airmass);
                    let lunar_sum =
                        group.iter().fold(0.0, |sum, item| sum + item.1.lunar_quality_factor);
                    let minimum_lunar = group
                        .iter()
                        .min_by(|left, right| {
                            left.1
                                .lunar_quality_factor
                                .partial_cmp(&right.1.lunar_quality_factor)
                                .unwrap()
                        })
                        .unwrap()
                        .1
                        .lunar_quality_factor;
                    let count = group.len() as f64;
                    let night_date = night.night_date.format("%Y-%m-%d").to_string();
                    rows.push(TileWindowRow {
                        window_id: format!("{}_{}_W{:02}", night_date, tile.tile_id, sequence + 1),
                        night_id: night_id.clone(),
                        night_date,
                        tile_id: tile.tile_id.clone(),
                        window_start_utc: format_utc(&start),
                        window_end_utc: format_utc(&end),
                        window_seconds: (end - start).num_seconds(),
                        best_time_utc: format_utc(&datetime_from_epoch(
                            epoch_seconds(&best_slot.timestamp_utc)
                                + best_slot.duration_seconds as f64 / 2.0,
                        )),
                        best_airmass: round6(best_geometry.airmass),
                        mean_airmass: round6(airmass_sum / count),
                        mean_lunar_quality_factor: round6(lunar_sum / count),
                        minimum_lunar_quality_factor: round6(minimum_lunar),
                        region_id: tile.region_id.clone(),
                        scheduling_class: tile.scheduling_class.clone(),
                        nominal_exptime_seconds: tile.nominal_exptime_seconds,
                        available_until_utc: format_utc(&tile.available_until_utc),
                    });
                }
            }
        }
        Ok(rows)
    }
}
