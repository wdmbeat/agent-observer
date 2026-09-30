//! Temporary observation requests: loaders and time-safe publication.
//!
//! Port of the runtime-facing parts of `challenge/observation_request_simulator.py`:
//! `ObservationRequest`, `load_requests` / `load_request_tiles`, and
//! `ObservationRequestSimulator.get_observation_requests`. Schedule generation
//! is not ported.

use std::collections::BTreeMap;
use std::path::Path;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::Value;

use crate::contracts::{
    format_utc, parse_utc, read_exact_csv, round6, REQUEST_COLUMNS, REQUEST_TILE_COLUMNS,
};

pub const SCHEMA_VERSION: &str = "observation-requests-v1";

#[derive(Debug, Clone, PartialEq)]
pub struct ObservationRequest {
    pub request_id: String,
    pub issued_at_utc: DateTime<Utc>,
    pub available_from_utc: DateTime<Utc>,
    pub deadline_utc: DateTime<Utc>,
    pub deadline_class: String,
    pub completion_mode: String,
    pub required_tile_count: i64,
    pub completion_reward: f64,
    pub miss_penalty: f64,
    pub reason: String,
}

impl ObservationRequest {
    pub fn public_dict(&self) -> RequestPublication {
        RequestPublication {
            request_id: self.request_id.clone(),
            issued_at_utc: format_utc(&self.issued_at_utc),
            available_from_utc: format_utc(&self.available_from_utc),
            deadline_utc: format_utc(&self.deadline_utc),
            deadline_class: self.deadline_class.clone(),
            completion_mode: self.completion_mode.clone(),
            required_tile_count: self.required_tile_count,
            completion_reward: round6(self.completion_reward),
            miss_penalty: round6(self.miss_penalty),
            reason: self.reason.clone(),
            tile_requirements: Vec::new(),
        }
    }
}

/// `ObservationRequest.public_dict()` plus the joined, sorted tile requirements.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
pub struct RequestPublication {
    pub request_id: String,
    pub issued_at_utc: String,
    pub available_from_utc: String,
    pub deadline_utc: String,
    pub deadline_class: String,
    pub completion_mode: String,
    pub required_tile_count: i64,
    pub completion_reward: f64,
    pub miss_penalty: f64,
    pub reason: String,
    pub tile_requirements: Vec<TileRequirement>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct TileRequirement {
    pub tile_id: String,
    pub required_visits: i64,
}

/// Load and validate `config/request_config.json`.
pub fn load_config(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let config: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if config.get("schema_version").and_then(Value::as_str) != Some(SCHEMA_VERSION) {
        bail!("unsupported request schema_version");
    }
    Ok(config)
}

pub fn load_requests(path: &Path) -> Result<Vec<ObservationRequest>> {
    let rows = read_exact_csv(path, &REQUEST_COLUMNS)?;
    let mut result = Vec::with_capacity(rows.len());
    let mut seen = std::collections::HashSet::new();
    for (index, row) in rows.iter().enumerate() {
        let line = index + 2;
        let cell =
            |name: &str| row[REQUEST_COLUMNS.iter().position(|c| *c == name).unwrap()].as_str();
        let build = || -> Result<ObservationRequest> {
            Ok(ObservationRequest {
                request_id: cell("request_id").to_string(),
                issued_at_utc: parse_utc(cell("issued_at_utc"))?,
                available_from_utc: parse_utc(cell("available_from_utc"))?,
                deadline_utc: parse_utc(cell("deadline_utc"))?,
                deadline_class: cell("deadline_class").to_string(),
                completion_mode: cell("completion_mode").to_string(),
                required_tile_count: cell("required_tile_count").trim().parse::<i64>()?,
                completion_reward: cell("completion_reward").trim().parse::<f64>()?,
                miss_penalty: cell("miss_penalty").trim().parse::<f64>()?,
                reason: cell("reason").to_string(),
            })
        };
        let request = build().with_context(|| format!("{}: row {line}", path.display()))?;
        if request.request_id.is_empty()
            || !seen.insert(request.request_id.clone())
            || request.deadline_utc <= request.available_from_utc
        {
            bail!("invalid request {:?}", request.request_id);
        }
        result.push(request);
    }
    Ok(result)
}

/// request_id → (tile_id → required_visits), sorted by tile_id.
pub fn load_request_tiles(path: &Path) -> Result<BTreeMap<String, BTreeMap<String, i64>>> {
    let rows = read_exact_csv(path, &REQUEST_TILE_COLUMNS)?;
    let mut result: BTreeMap<String, BTreeMap<String, i64>> = BTreeMap::new();
    for (index, row) in rows.iter().enumerate() {
        let line = index + 2;
        let cell = |name: &str| {
            row[REQUEST_TILE_COLUMNS
                .iter()
                .position(|c| *c == name)
                .unwrap()]
            .as_str()
        };
        let request_id = cell("request_id").to_string();
        let tile_id = cell("tile_id").to_string();
        let visits = cell("required_visits")
            .trim()
            .parse::<i64>()
            .with_context(|| format!("{}: row {line}", path.display()))?;
        let entry = result.entry(request_id.clone()).or_default();
        if visits < 1 || entry.contains_key(&tile_id) {
            bail!("invalid request tile row for {request_id}/{tile_id}");
        }
        entry.insert(tile_id, visits);
    }
    Ok(result)
}

/// Participant-safe request publication; the pre-generated future stays hidden.
pub struct ObservationRequestSimulator {
    pub requests: Vec<ObservationRequest>,
    pub request_tiles: BTreeMap<String, BTreeMap<String, i64>>,
}

impl ObservationRequestSimulator {
    pub fn new(
        requests: Vec<ObservationRequest>,
        request_tiles: BTreeMap<String, BTreeMap<String, i64>>,
    ) -> Result<Self> {
        let mut requests = requests;
        requests.sort_by(|left, right| {
            (left.issued_at_utc, &left.request_id).cmp(&(right.issued_at_utc, &right.request_id))
        });
        let known: std::collections::BTreeSet<&str> = requests
            .iter()
            .map(|item| item.request_id.as_str())
            .collect();
        let linked: std::collections::BTreeSet<&str> =
            request_tiles.keys().map(String::as_str).collect();
        if known != linked {
            bail!("request and request-tile IDs do not match");
        }
        Ok(Self {
            requests,
            request_tiles,
        })
    }

    pub fn from_files(requests_path: &Path, request_tiles_path: &Path) -> Result<Self> {
        Self::new(
            load_requests(requests_path)?,
            load_request_tiles(request_tiles_path)?,
        )
    }

    pub fn get_observation_requests(
        &self,
        as_of_utc: DateTime<Utc>,
        include_expired: bool,
    ) -> Vec<RequestPublication> {
        let mut rows = Vec::new();
        for request in &self.requests {
            if request.issued_at_utc > as_of_utc
                || (!include_expired && request.deadline_utc <= as_of_utc)
            {
                continue;
            }
            let mut payload = request.public_dict();
            payload.tile_requirements = self.request_tiles[&request.request_id]
                .iter()
                .map(|(tile_id, visits)| TileRequirement {
                    tile_id: tile_id.clone(),
                    required_visits: *visits,
                })
                .collect();
            rows.push(payload);
        }
        rows
    }
}
