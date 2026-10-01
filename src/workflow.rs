//! Global-wallclock workflow joining all simulator interfaces — port of
//! `challenge/challenge_workflow.py::ChallengeWorkflow` plus the
//! `local_runner.py` orchestration (`run_local`).

use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::time::Instant;

use anyhow::{bail, Context, Result};
use chrono::{DateTime, Utc};
use serde_json::{json, Map, Value};

use crate::contracts::{
    epoch_seconds, format_utc, parse_utc, read_exact_csv, round6, write_exact_csv,
    ACCEPTED_PROTOCOL_VERSIONS, DECISION_COLUMNS, DECISION_SNAPSHOT_VERSION,
    INITIAL_PUBLICATION_VERSION, LEGACY_DECISION_SNAPSHOT_VERSION, REPORT_KINDS, TARGET_COLUMNS,
    WORKFLOW_RESULT_VERSION,
};
use crate::geometry::TileWindowRow;
use crate::requests::ObservationRequestSimulator;
use crate::schema::{CalendarSummary, CatalogTile, InitialPublication, ScoringContract, Site, TargetRow, TileCatalog};
use crate::scoring::{dumps_report, ChallengeScorer, Decision};
use crate::transport::{is_global_deadline_expired, AgentProcess};

/// Anything that can accept the initial publication and answer decision
/// requests: the external JSONL subprocess transport or the in-process
/// builtin agent. Both consume the same snapshot `Value` the wire would carry.
pub trait DecisionProvider {
    /// Send the one-time bootstrap message (default: ignore, like a provider
    /// without `publish_initial` in Python).
    fn publish_initial(&mut self, _publication: &Value) -> Result<()> {
        Ok(())
    }
    /// Ask for one decision response to `snapshot` before `deadline`.
    fn call(&mut self, snapshot: &Value, deadline: Instant) -> Result<Value>;
    /// Release resources (the subprocess transport kills the process group).
    fn shutdown(&mut self) {}
}

impl DecisionProvider for AgentProcess {
    fn publish_initial(&mut self, publication: &Value) -> Result<()> {
        AgentProcess::publish_initial(self, publication)
    }
    fn call(&mut self, snapshot: &Value, deadline: Instant) -> Result<Value> {
        AgentProcess::call(self, snapshot, deadline)
    }
    fn shutdown(&mut self) {
        let _ = self.close(true);
    }
}

/// Outcomes that refresh `tile_last_finished` feedback.
pub const FEEDBACK_OUTCOMES: [&str; 3] = [
    "completed",
    "weather_interrupted",
    "geometry_or_night_interrupted",
];

/// The agent-visible weather view never carries instrument_efficiency.
fn public_weather(conditions: &Value) -> Value {
    let mut map = conditions.as_object().unwrap().clone();
    map.remove("instrument_efficiency");
    Value::Object(map)
}

pub fn load_workflow_config(path: &Path) -> Result<Value> {
    let text =
        std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
    let config: Value =
        serde_json::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;
    if config.get("schema_version").and_then(Value::as_str) != Some("challenge-workflow-v1") {
        bail!("unsupported workflow schema_version");
    }
    if config
        .get("global_wallclock_seconds")
        .and_then(Value::as_f64)
        .unwrap_or(0.0)
        <= 0.0
    {
        bail!("global wallclock must be positive");
    }
    if !config["per_decision_timeout_seconds"].is_null()
        || !config["synthetic_timeout_action"].is_null()
    {
        bail!("example3 forbids per-decision timeout rules and synthetic timeout actions");
    }
    Ok(config)
}

/// `Night.csv_row()` as a JSON object, used by the night-start publication.
fn night_csv_row_json(night: &crate::calendar::Night) -> Value {
    let cells = night.csv_row();
    let mut map = Map::new();
    for (index, column) in crate::contracts::NIGHT_COLUMNS.iter().enumerate() {
        let value = if matches!(*column, "night_seconds" | "slot_count") {
            json!(cells[index].parse::<i64>().unwrap())
        } else {
            json!(cells[index])
        };
        map.insert(column.to_string(), value);
    }
    Value::Object(map)
}

struct FaultFeedEntry {
    event_id: String,
    reported_at: DateTime<Utc>,
    repair_complete_utc: DateTime<Utc>,
}

struct NormalNotice {
    reference_report_id: String,
    respond_at: DateTime<Utc>,
    published: bool,
}

pub struct ChallengeWorkflow {
    pub root: PathBuf,
    pub config: Value,
    pub scorer: ChallengeScorer,
    pub requests: ObservationRequestSimulator,
    pub target_catalog: Vec<Vec<String>>,
    pub mechanics: bool,
    /// The scenario score config, parsed once (the publication's contract).
    pub score_config: crate::schema::ScoreConfig,
    pub committed: Vec<Decision>,
    pub commit_log: Vec<crate::schema::CommitLogEntry>,
    row_seq: i64,
    last_finished: Option<Value>,
    fault_feed: Vec<FaultFeedEntry>,
    normal_notices: Vec<NormalNotice>,
    fault_latency_seconds: f64,
    night_cache: HashMap<String, Vec<TileWindowRow>>,
    week_cache: HashMap<String, Value>,
}

impl ChallengeWorkflow {
    pub fn new(root: &Path) -> Result<Self> {
        let config = load_workflow_config(&root.join("config").join("workflow_config.json"))?;
        let scorer = ChallengeScorer::from_files(root)?;
        let requests = ObservationRequestSimulator::from_files(
            &root
                .join("outputs")
                .join("reference")
                .join("observation_requests.csv"),
            &root
                .join("outputs")
                .join("reference")
                .join("observation_request_tiles.csv"),
        )?;
        let target_catalog = read_exact_csv(
            &root.join("outputs").join("reference").join("targets.csv"),
            &TARGET_COLUMNS,
        )?;
        let mechanics = scorer.mechanics;
        let score_config: crate::schema::ScoreConfig =
            serde_json::from_value(scorer.config.clone()).context("parsing score config")?;
        let fault_latency_days = scorer
            .config
            .get("fault_response")
            .and_then(|section| section.get("response_latency_days"))
            .and_then(Value::as_f64)
            .unwrap_or(1.0);
        Ok(Self {
            root: root.to_path_buf(),
            config,
            scorer,
            requests,
            target_catalog,
            mechanics,
            score_config,
            committed: Vec::new(),
            commit_log: Vec::new(),
            row_seq: 0,
            last_finished: None,
            fault_feed: Vec::new(),
            normal_notices: Vec::new(),
            fault_latency_seconds: fault_latency_days * 86400.0,
            night_cache: HashMap::new(),
            week_cache: HashMap::new(),
        })
    }

    fn ordered_night_ids(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.scorer.geometry.nights.keys().cloned().collect();
        ids.sort();
        ids
    }

    /// Immutable public catalogs and the exact official score contract, as the
    /// typed `schema::InitialPublication`. Serializing it yields the wire
    /// document byte-value-identical to the old hand-built `json!`
    /// (regression-tested against goldens in `tests/initial_publication.rs`).
    pub fn initial_publication(&self) -> Result<InitialPublication> {
        let night_ids = self.ordered_night_ids();
        let nights: Vec<&crate::calendar::Night> = night_ids
            .iter()
            .map(|id| &self.scorer.geometry.nights[id])
            .collect();
        let mut tiles: Vec<&crate::geometry::Tile> = self.scorer.tiles.values().collect();
        tiles.sort_by(|left, right| left.tile_id.cmp(&right.tile_id));
        let catalog_tiles: Vec<CatalogTile> = tiles
            .iter()
            .map(|tile| CatalogTile {
                tile_id: tile.tile_id.clone(),
                ra_deg: tile.ra_deg,
                dec_deg: tile.dec_deg,
                nominal_exptime_seconds: tile.nominal_exptime_seconds,
                region_id: tile.region_id.clone(),
                scheduling_class: crate::schema::SchedulingClass::from_label(&tile.scheduling_class)
                    .expect("tile scheduling_class is REQUIRED or FLEXIBLE"),
                available_from_utc: format_utc(&tile.available_from_utc),
                available_until_utc: format_utc(&tile.available_until_utc),
                n_lrg: tile.n_lrg,
                n_elg: tile.n_elg,
                n_qso: tile.n_qso,
                n_bgs: tile.n_bgs,
                tile_science_value: round6(self.scorer.tile_values[&tile.tile_id]),
            })
            .collect();
        let required_tile_ids: Vec<String> = tiles
            .iter()
            .filter(|tile| tile.scheduling_class == "REQUIRED")
            .map(|tile| tile.tile_id.clone())
            .collect();
        let region_ids: Vec<String> = tiles
            .iter()
            .map(|tile| tile.region_id.clone())
            .collect::<std::collections::BTreeSet<_>>()
            .into_iter()
            .collect();
        let target_catalog: Vec<TargetRow> = self
            .target_catalog
            .iter()
            .map(|row| TargetRow {
                target_id: row[0].clone(),
                tile_id: row[1].clone(),
                target_class: row[2].clone(),
                feature_flux: row[3].clone(),
                redshift: row[4].clone(),
                science_weight: row[5].clone(),
            })
            .collect();
        let site: Site = serde_json::from_value(self.scorer.geometry.calendar_config["site"].clone())
            .context("calendar config site does not match the schema")?;
        let weather_score_interface =
            serde_json::from_value(self.scorer.weather.config["score_interface"].clone())
                .context("weather score interface does not match the schema")?;
        let publication = InitialPublication {
            schema_version: INITIAL_PUBLICATION_VERSION.to_string(),
            calendar: CalendarSummary {
                first_night: nights[0].night_date.format("%Y-%m-%d").to_string(),
                last_night: nights[nights.len() - 1].night_date.format("%Y-%m-%d").to_string(),
                night_count: nights.len() as i64,
                slot_count: self.scorer.slots.len() as i64,
                slot_duration_seconds: self.scorer.slots[0].duration_seconds,
            },
            site,
            tile_catalog: TileCatalog {
                tile_count: tiles.len() as i64,
                required_tile_ids,
                region_ids,
                tiles: catalog_tiles,
            },
            target_catalog,
            scoring_contract: ScoringContract {
                score_config: self.score_config.clone(),
                weather_score_interface,
                lunar_model: self.scorer.geometry.tile_config["lunar_model"].clone(),
                preview_semantics: Some("Current-snapshot estimates use the official public formula but cannot know unreleased future slot weather. Authoritative scores are computed by segmented replay.".to_string()),
            },
            global_wallclock_seconds: self.config["global_wallclock_seconds"].as_f64().unwrap(),
        };
        Ok(publication)
    }
}

impl ChallengeWorkflow {
    fn night_windows(&mut self, night_id: &str) -> Result<&Vec<TileWindowRow>> {
        if !self.night_cache.contains_key(night_id) {
            let night = &self.scorer.geometry.nights[night_id];
            let windows = self.scorer.geometry.get_tile_windows(night.night_date, 1)?;
            self.night_cache.insert(night_id.to_string(), windows);
        }
        Ok(&self.night_cache[night_id])
    }

    fn weekly_publication(&mut self, slot: &crate::calendar::Slot) -> Result<Value> {
        if !self.week_cache.contains_key(&slot.night_id) {
            let night = &self.scorer.geometry.nights[&slot.night_id];
            let ordered = self.ordered_night_ids();
            let index = ordered.iter().position(|id| id == &slot.night_id).unwrap();
            let remaining_nights = ordered.len() - index;
            let days = (self.config["tile_window_horizon_days"].as_i64().unwrap() as usize)
                .min(remaining_nights);
            let windows = self
                .scorer
                .geometry
                .get_tile_windows(night.night_date, days)?;
            let forecast = self.scorer.weather.get_weather_forecast(
                slot.timestamp_utc,
                Some(self.config["weekly_horizon_days"].as_i64().unwrap()),
            )?;
            let requests = self
                .requests
                .get_observation_requests(slot.timestamp_utc, false);
            self.week_cache.insert(
                slot.night_id.clone(),
                json!({
                    "issued_at_utc": format_utc(&slot.timestamp_utc),
                    "weather_forecast": forecast,
                    "tile_windows": windows,
                    "observation_requests": requests,
                }),
            );
        }
        Ok(self.week_cache[&slot.night_id].clone())
    }

    /// Time-safe request publications with current visit progress added.
    fn active_requests(&self, moment: DateTime<Utc>) -> Vec<Value> {
        let publications = self.requests.get_observation_requests(moment, false);
        let mut enriched = Vec::with_capacity(publications.len());
        for request in publications {
            let mut requirements = Vec::new();
            let mut satisfied = 0i64;
            for requirement in &request.tile_requirements {
                let completed = self
                    .scorer
                    .request_visits
                    .get(&(request.request_id.clone(), requirement.tile_id.clone()))
                    .copied()
                    .unwrap_or(0);
                satisfied += (completed >= requirement.required_visits) as i64;
                requirements.push(json!({
                    "tile_id": requirement.tile_id,
                    "required_visits": requirement.required_visits,
                    "completed_visits": completed,
                    "remaining_visits": (requirement.required_visits - completed).max(0),
                }));
            }
            enriched.push(json!({
                "request_id": request.request_id,
                "issued_at_utc": request.issued_at_utc,
                "available_from_utc": request.available_from_utc,
                "deadline_utc": request.deadline_utc,
                "deadline_class": request.deadline_class,
                "completion_mode": request.completion_mode,
                "required_tile_count": request.required_tile_count,
                "completion_reward": request.completion_reward,
                "miss_penalty": request.miss_penalty,
                "reason": request.reason,
                "tile_requirements": requirements,
                "satisfied_tile_count": satisfied,
                "is_complete": satisfied >= request.required_tile_count,
            }));
        }
        enriched
    }

    /// Night-start publication: acknowledged unrepaired fault, or a one-shot
    /// "instrument normal" answer.
    fn fault_status(&mut self, moment: DateTime<Utc>) -> Option<Value> {
        let latency = self.fault_latency_seconds;
        let publishable: Vec<&FaultFeedEntry> = self
            .fault_feed
            .iter()
            .filter(|feed| {
                epoch_seconds(&feed.reported_at) + latency <= epoch_seconds(&moment)
                    && moment < feed.repair_complete_utc
            })
            .collect();
        if let Some(feed) = publishable.iter().max_by_key(|feed| feed.reported_at) {
            let event = self
                .scorer
                .weather
                .events
                .iter()
                .find(|event| event.event_id == feed.event_id)
                .unwrap();
            return Some(json!({
                "status": "fault",
                "event_id": event.event_id,
                "spatial_scope_type": event.spatial_scope_type,
                "spatial_scope_payload": event.spatial_scope_payload,
                "instrument_efficiency_multiplier": round6(event.instrument_efficiency_multiplier),
                "reported_at_utc": format_utc(&feed.reported_at),
                "published_at_utc": format_utc(&moment),
                "repair_complete_utc": format_utc(&feed.repair_complete_utc),
            }));
        }
        for notice in &mut self.normal_notices {
            if !notice.published && notice.respond_at <= moment {
                notice.published = true;
                return Some(json!({
                    "status": "normal",
                    "reference_report_id": notice.reference_report_id,
                    "published_at_utc": format_utc(&moment),
                }));
            }
        }
        None
    }

    pub fn decision_snapshot(&mut self, sequence: i64) -> Result<Value> {
        let slot = self
            .scorer
            .current_slot()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("survey is complete"))?;
        let moment = self.scorer.current_time().unwrap();
        let night_rows = self.night_windows(&slot.night_id)?.clone();
        let active_windows: Vec<&TileWindowRow> = night_rows
            .iter()
            .filter(|row| {
                parse_utc(&row.window_start_utc).unwrap() <= moment
                    && moment < parse_utc(&row.window_end_utc).unwrap()
            })
            .collect();
        let minimum_altitude: f64 = self.scorer.geometry.tile_config["geometry"]
            ["minimum_altitude_deg"]
            .as_f64()
            .unwrap();
        let mut candidates = Vec::new();
        for row in active_windows {
            let tile_id = &row.tile_id;
            let conditions = self.scorer.weather.get_effective_conditions(
                &slot.slot_id,
                Some(tile_id),
                !self.mechanics,
            )?;
            let conditions = serde_json::to_value(&conditions)?;
            let conditions = if self.mechanics {
                public_weather(&conditions)
            } else {
                conditions
            };
            let geometry = self.scorer.geometry.get_tile_geometry(tile_id, moment)?;
            if geometry.sample.altitude_deg < minimum_altitude {
                continue;
            }
            candidates.push(json!({
                "tile_id": tile_id,
                "region_id": row.region_id,
                "scheduling_class": row.scheduling_class,
                "nominal_exptime_seconds": row.nominal_exptime_seconds,
                "tile_science_value": round6(self.scorer.tile_values[tile_id]),
                "window_start_utc": row.window_start_utc,
                "window_end_utc": row.window_end_utc,
                "geometry": geometry,
                "effective_weather": conditions,
                "already_completed": self.scorer.completed_tiles.contains(tile_id),
            }));
        }
        let night = self.scorer.geometry.nights[&slot.night_id].clone();
        let ordered = self.ordered_night_ids();
        let night_index = ordered.iter().position(|id| id == &slot.night_id).unwrap();
        let first_slot = &self.scorer.geometry.slots_by_night[&slot.night_id][0];
        let night_open = self.scorer.offset_seconds == 0 && slot.slot_id == first_slot.slot_id;
        let night_start = if night_open {
            let night_rows = self.night_windows(&slot.night_id)?.clone();
            Some(json!({
                "night": night_csv_row_json(&night),
                "tile_windows": night_rows,
            }))
        } else {
            None
        };
        let weekly = if night_index as i64 % self.config["weekly_horizon_days"].as_i64().unwrap()
            == 0
            && night_open
        {
            Some(self.weekly_publication(&slot)?)
        } else {
            None
        };
        let site_weather =
            self.scorer
                .weather
                .get_effective_conditions(&slot.slot_id, None, !self.mechanics)?;
        let site_weather = serde_json::to_value(&site_weather)?;
        let site_weather = if self.mechanics {
            public_weather(&site_weather)
        } else {
            site_weather
        };
        let mut flexible_by_region: HashMap<String, i64> = HashMap::new();
        for tile_id in &self.scorer.completed_tiles {
            let tile = &self.scorer.tiles[tile_id];
            if tile.scheduling_class == "FLEXIBLE" {
                *flexible_by_region
                    .entry(tile.region_id.clone())
                    .or_insert(0) += 1;
            }
        }
        let mut snapshot = json!({
            "schema_version": if self.mechanics { DECISION_SNAPSHOT_VERSION } else { LEGACY_DECISION_SNAPSHOT_VERSION },
            "decision_sequence": sequence,
            "cursor": {
                "slot_id": slot.slot_id,
                "night_id": slot.night_id,
                "timestamp_utc": format_utc(&moment),
                "slot_offset_seconds": self.scorer.offset_seconds,
            },
            "current_site_weather": site_weather,
            "candidate_tiles": candidates,
            "active_requests": self.active_requests(moment),
            "night_start": night_start,
            "weekly": weekly,
            "progress": {
                "completed_tile_ids": self.scorer.completed_tiles.iter().collect::<Vec<_>>(),
                "flexible_completed_by_region": flexible_by_region.into_iter().collect::<BTreeMap<_,_>>(),
            },
        });
        if self.mechanics {
            snapshot["tile_last_finished"] = self.last_finished.clone().unwrap_or(Value::Null);
            if night_open {
                if let Some(fault_status) = self.fault_status(moment) {
                    snapshot["fault_status"] = fault_status;
                }
            }
        }
        Ok(snapshot)
    }
}

/// Python `str(value)` for the JSON scalars that can appear in agent responses.
fn py_str(value: &Value) -> String {
    match value {
        Value::String(text) => text.clone(),
        Value::Null => "None".to_string(),
        Value::Bool(flag) => if *flag { "True" } else { "False" }.to_string(),
        Value::Number(number) => number.to_string(),
        other => other.to_string(),
    }
}

/// (decision, accepted reports as (kind, tile_id), dropped report count).
type ParsedResponse = (Decision, Vec<(String, String)>, i64);

fn py_int(value: &Value) -> Result<i64> {
    if let Some(number) = value.as_i64() {
        return Ok(number);
    }
    if let Some(number) = value.as_f64() {
        return Ok(number as i64);
    }
    if let Some(text) = value.as_str() {
        return Ok(text.trim().parse::<i64>()?);
    }
    bail!("not an integer: {value}")
}

impl ChallengeWorkflow {
    /// Validate one response; malformed report entries are dropped (counted),
    /// never the action.
    fn decision_from_response(
        &self,
        sequence: i64,
        slot_id: &str,
        response: &Value,
    ) -> Result<ParsedResponse> {
        if let Some(version) = response.get("protocol_version") {
            if !ACCEPTED_PROTOCOL_VERSIONS.contains(&py_str(version).as_str()) {
                bail!("unsupported participant protocol_version");
            }
        }
        if let Some(message_type) = response.get("message_type") {
            if py_str(message_type) != "decision_response" {
                bail!("agent response message_type must be decision_response");
            }
        }
        if let Some(decision_sequence) = response.get("decision_sequence") {
            if py_int(decision_sequence)? != sequence {
                bail!("agent response decision_sequence does not match request");
            }
        }
        let action = response.get("action").map(py_str).unwrap_or_default();
        if action != "observe" && action != "wait" {
            bail!("agent response action must be observe or wait");
        }
        let mut tile_id = response.get("tile_id").map(py_str).unwrap_or_default();
        let mut program = response.get("program").map(py_str).unwrap_or_default();
        let mut request_id = response.get("request_id").map(py_str).unwrap_or_default();
        let reason = response.get("reason").map(py_str).unwrap_or_default();
        if action == "wait" {
            tile_id = String::new();
            program = String::new();
            request_id = String::new();
        }
        let mut reports: Vec<(String, String)> = Vec::new();
        let mut dropped = 0i64;
        let raw_reports = response.get("reports").cloned().unwrap_or(Value::Null);
        let raw_reports = if self.mechanics {
            raw_reports
        } else {
            // Legacy scenarios have nothing to report against; note and drop.
            dropped = match &raw_reports {
                Value::Null => 0,
                Value::Array(entries) => entries.len() as i64,
                _ => 1,
            };
            Value::Null
        };
        if let Value::Array(entries) = &raw_reports {
            for entry in entries {
                let Some(entry) = entry.as_object() else {
                    dropped += 1;
                    continue;
                };
                let kind = entry.get("kind").map(py_str).unwrap_or_default();
                let report_tile = entry.get("tile_id").map(py_str).unwrap_or_default();
                if !REPORT_KINDS.contains(&kind.as_str())
                    || (kind == "Instrument_Failure") == !report_tile.is_empty()
                {
                    dropped += 1;
                    continue;
                }
                if !report_tile.is_empty() && !self.scorer.tiles.contains_key(&report_tile) {
                    dropped += 1;
                    continue;
                }
                reports.push((kind, report_tile));
            }
        } else if !raw_reports.is_null() {
            dropped += 1;
        }
        Ok((
            Decision {
                decision_id: format!("D{sequence:06}"),
                slot_id: slot_id.to_string(),
                action,
                tile_id,
                program,
                request_id,
                reason,
            },
            reports,
            dropped,
        ))
    }

    /// Apply one decision and its riding reports, flattened into the
    /// decisions.csv trace. The carrier decision and each report row share one
    /// incrementing row-id sequence; report rows act at the post-decision
    /// cursor time and never move the cursor.
    fn commit(
        &mut self,
        decision: &Decision,
        reports: &[(String, String)],
    ) -> Result<(Value, Vec<Value>)> {
        self.row_seq += 1;
        let mut decision = decision.clone();
        decision.decision_id = format!("D{:06}", self.row_seq);
        let result = self.scorer.apply_decision(&decision)?;
        self.committed.push(decision.clone());
        if FEEDBACK_OUTCOMES.contains(&result["outcome"].as_str().unwrap_or("")) {
            let score = result["base_science_score"].as_f64().unwrap()
                + result["program_bonus_score"].as_f64().unwrap();
            self.last_finished = Some(json!({
                "tile_id": result["tile_id"],
                "score": round6(score),
            }));
        }
        let mut report_outcomes = Vec::new();
        for (kind, tile_id) in reports {
            let action = if kind == "Instrument_Failure" {
                "report_instrument_failure".to_string()
            } else {
                format!("report_{}", kind.to_lowercase())
            };
            self.row_seq += 1;
            let row = Decision {
                decision_id: format!("D{:06}", self.row_seq),
                slot_id: decision.slot_id.clone(),
                action,
                tile_id: tile_id.clone(),
                program: String::new(),
                request_id: String::new(),
                reason: String::new(),
            };
            let record = self.scorer.apply_decision(&row)?;
            self.committed.push(row.clone());
            let outcome = record["report_result"].clone();
            match outcome["result"].as_str().unwrap_or("") {
                "correct" => {
                    let as_of = parse_utc(record["start_utc"].as_str().unwrap())?;
                    let repair_complete =
                        parse_utc(outcome["repair_complete_utc"].as_str().unwrap())?;
                    for event_id in outcome["acknowledged_event_ids"].as_array().unwrap() {
                        self.fault_feed.push(FaultFeedEntry {
                            event_id: event_id.as_str().unwrap().to_string(),
                            reported_at: as_of,
                            repair_complete_utc: repair_complete,
                        });
                    }
                }
                "misreport" => {
                    let as_of = parse_utc(record["start_utc"].as_str().unwrap())?;
                    self.normal_notices.push(NormalNotice {
                        reference_report_id: row.decision_id.clone(),
                        respond_at: crate::contracts::datetime_from_epoch(
                            epoch_seconds(&as_of) + self.fault_latency_seconds,
                        ),
                        published: false,
                    });
                }
                _ => {}
            }
            report_outcomes.push(outcome);
        }
        Ok((result, report_outcomes))
    }

    /// Run the survey loop against a live agent transport.
    pub fn run(
        &mut self,
        provider: &mut dyn DecisionProvider,
        wallclock_seconds: Option<f64>,
    ) -> Result<crate::schema::WorkflowResult> {
        use crate::schema::{CommitLogEntry, TerminationReason, WorkflowResult};
        let initial = self.initial_publication()?;
        let initial_value =
            serde_json::to_value(&initial).expect("initial publication serialization");
        let budget = wallclock_seconds
            .unwrap_or_else(|| self.config["global_wallclock_seconds"].as_f64().unwrap());
        if budget <= 0.0 {
            bail!("wallclock_seconds must be positive");
        }
        if let Err(error) = provider.publish_initial(&initial_value) {
            let report = self.scorer.finalize("agent_initialization_error")?;
            return Ok(WorkflowResult {
                schema_version: WORKFLOW_RESULT_VERSION.to_string(),
                initial_publication: Some(initial),
                termination_reason: TerminationReason::AgentInitializationError,
                global_wallclock_seconds: budget,
                accounted_wallclock_seconds: 0.0,
                ignored_in_flight_response: false,
                committed_action_count: 0,
                commit_log: vec![CommitLogEntry::Failed {
                    sequence: 0,
                    error: format!("{error}"),
                }],
                score_report: report,
            });
        }
        let started = Instant::now();
        let deadline = started + std::time::Duration::from_secs_f64(budget);
        let mut termination = TerminationReason::SurveyComplete;
        let mut ignored_in_flight = false;
        let mut sequence: i64 = 1;
        while self.scorer.current_slot().is_some() {
            if Instant::now() >= deadline {
                termination = TerminationReason::GlobalWallclockExpired;
                break;
            }
            let snapshot = self.decision_snapshot(sequence)?;
            let response = match provider.call(&snapshot, deadline) {
                Ok(response) => response,
                Err(error) if is_global_deadline_expired(&error) => {
                    termination = TerminationReason::GlobalWallclockExpired;
                    ignored_in_flight = true;
                    break;
                }
                Err(error) => {
                    termination = TerminationReason::AgentError;
                    self.commit_log.push(CommitLogEntry::Failed {
                        sequence,
                        error: format!("{error}"),
                    });
                    break;
                }
            };
            let completed_at = Instant::now();
            if completed_at >= deadline {
                termination = TerminationReason::GlobalWallclockExpired;
                ignored_in_flight = true;
                break;
            }
            let slot_id = self.scorer.current_slot().unwrap().slot_id.clone();
            let (decision, reports, dropped_reports) =
                match self.decision_from_response(sequence, &slot_id, &response) {
                    Ok(parsed) => parsed,
                    Err(error) => {
                        termination = TerminationReason::AgentError;
                        self.commit_log.push(CommitLogEntry::Failed {
                            sequence,
                            error: format!("{error}"),
                        });
                        break;
                    }
                };
            let (result, report_outcomes) = self.commit(&decision, &reports)?;
            self.commit_log.push(CommitLogEntry::Committed {
                sequence,
                completed_wallclock_seconds: (completed_at - started).as_secs_f64(),
                decision_id: decision.decision_id,
                outcome: result["outcome"].as_str().unwrap_or("").to_string(),
                report_outcomes,
                dropped_reports,
            });
            sequence += 1;
        }
        let now = Instant::now();
        let accounted = (now.min(deadline) - started).as_secs_f64().max(0.0);
        let report = self.scorer.finalize(termination.as_str())?;
        Ok(WorkflowResult {
            schema_version: WORKFLOW_RESULT_VERSION.to_string(),
            initial_publication: Some(initial),
            termination_reason: termination,
            global_wallclock_seconds: budget,
            accounted_wallclock_seconds: accounted,
            ignored_in_flight_response: ignored_in_flight,
            committed_action_count: self.committed.len(),
            commit_log: std::mem::take(&mut self.commit_log),
            score_report: report,
        })
    }

    /// decisions.csv carries the whole trace, including report_* action rows.
    pub fn write_outputs(&self, output_dir: &Path, result: &crate::schema::WorkflowResult) -> Result<()> {
        std::fs::create_dir_all(output_dir)?;
        let rows: Vec<Vec<String>> = self.committed.iter().map(Decision::csv_row).collect();
        write_exact_csv(&output_dir.join("decisions.csv"), &DECISION_COLUMNS, &rows)?;
        crate::contracts::write_text_lf(
            &output_dir.join("workflow_result.json"),
            &(dumps_report(&serde_json::to_value(result)?) + "\n"),
        )?;
        Ok(())
    }
}

/// Options for the local-runner orchestration (`local_runner.py` analog).
pub struct RunOptions {
    pub scenario: PathBuf,
    /// Typed agent selection; resolved to a native strategy or a subprocess.
    pub agent: crate::agent::AgentSpec,
    pub out_dir: PathBuf,
    pub wallclock: Option<f64>,
    pub init_timeout: f64,
    pub keep_initial_publication: bool,
    pub quiet: bool,
}

/// Outcome of a local run: the workflow result, the authoritative re-scored
/// report, the summary object, and the process exit code (0 for
/// survey_complete/global_wallclock_expired, 2 otherwise).
pub struct RunOutcome {
    pub result: crate::schema::WorkflowResult,
    pub report: Value,
    pub summary: Value,
    pub exit_code: i32,
}

/// Run a participant agent against a scenario exactly the way the evaluation
/// platform does, then re-score the trace (port of `local_runner.py::main`).
pub fn run_local(options: &RunOptions) -> Result<RunOutcome> {
    let scenario = &options.scenario;
    if !scenario
        .join("config")
        .join("workflow_config.json")
        .is_file()
    {
        bail!(
            "{} is not a scenario directory (missing config/workflow_config.json)",
            scenario.display()
        );
    }
    let out_dir = &options.out_dir;
    std::fs::create_dir_all(out_dir)?;
    let scratch = out_dir.join("scratch");
    std::fs::create_dir_all(&scratch)?;

    let mut workflow = ChallengeWorkflow::new(scenario)?;
    let wallclock = options.wallclock.unwrap_or_else(|| {
        workflow.config["global_wallclock_seconds"]
            .as_f64()
            .unwrap()
    });
    if wallclock <= 0.0 {
        bail!("--wallclock must be positive");
    }
    let scenario_slug = scenario
        .join("config")
        .join("scenario_config.json")
        .exists()
        .then(|| {
            std::fs::read_to_string(scenario.join("config").join("scenario_config.json"))
                .ok()
                .and_then(|text| serde_json::from_str::<Value>(&text).ok())
                .and_then(|config| {
                    config
                        .get("scenario_id")
                        .and_then(Value::as_str)
                        .map(str::to_string)
                })
        })
        .flatten()
        .unwrap_or_else(|| scenario.file_name().unwrap().to_string_lossy().into_owned());
    let protocol_version = crate::transport::protocol_version_for(workflow.mechanics);
    let resolved = options.agent.resolve()?;
    if !options.quiet {
        eprintln!(
            "[local-runner] scenario={} agent={} wallclock={wallclock}s",
            scenario.display(),
            resolved.describe()
        );
    }

    let log_path = out_dir.join("agent.log");
    let mut agent_log = std::fs::File::create(&log_path)?;
    use std::io::Write as _;
    writeln!(
        agent_log,
        "[local-runner] {} wallclock={wallclock}s",
        resolved.describe()
    )?;
    agent_log.flush()?;
    let mut provider: Box<dyn DecisionProvider> = match &resolved {
        crate::agent::ResolvedAgent::Builtin(strategy) => strategy.build(),
        crate::agent::ResolvedAgent::Subprocess { command, agent_dir } => {
            let (env, dotenv_keys) = crate::transport::build_agent_env(
                agent_dir,
                &scratch,
                wallclock,
                &scenario_slug,
                protocol_version,
            );
            if !options.quiet {
                eprintln!("[local-runner] dotenv_keys={dotenv_keys:?}");
            }
            Box::new(AgentProcess::new(
                command,
                agent_dir.clone(),
                env,
                Some(agent_log),
                options.init_timeout,
                protocol_version,
            )?)
        }
    };
    let run_result = workflow.run(provider.as_mut(), Some(wallclock));
    provider.shutdown();
    let mut result = run_result?;
    // Strip the initialize payload unless asked to keep it (same output as
    // the old re-read/remove/rewrite pass, without the double write).
    if !options.keep_initial_publication {
        result.initial_publication = None;
    }
    workflow.write_outputs(out_dir, &result)?;

    // Authoritative score: replay decisions.csv with the public scorer, exactly
    // as the platform does after a run.
    let report = crate::scoring::score_files(
        scenario,
        &out_dir.join("decisions.csv"),
        Some(&out_dir.join("score_report.json")),
        result.termination_reason.as_str(),
    )?;
    let live_total = result.score_report["score"]["total"].as_f64().unwrap();
    let replay_total = report["score"]["total"].as_f64().unwrap();
    if (replay_total - live_total).abs() > 1e-6 && !options.quiet {
        eprintln!(
            "[local-runner] warning: replay total {replay_total} differs from live total {live_total}"
        );
    }
    if !options.quiet {
        for entry in &result.commit_log {
            if let crate::schema::CommitLogEntry::Failed { sequence, error } = entry {
                eprintln!(
                    "[local-runner] agent error at decision {sequence}: {error} (see agent.log)"
                );
            }
        }
    }

    let score = &report["score"];
    let completion = &report["completion"];
    let mut status_counts: BTreeMap<String, i64> = BTreeMap::new();
    for row in report["requests"].as_array().unwrap() {
        *status_counts
            .entry(row["status"].as_str().unwrap().to_string())
            .or_insert(0) += 1;
    }
    let summary = json!({
        "termination_reason": result.termination_reason,
        "total": score["total"],
        "base_science": score["base_science"],
        "program_bonus": score["program_bonus"],
        "request_reward": score["request_reward"],
        "penalties": score["penalties"],
        "completed_tiles": completion["completed_tiles"].as_array().unwrap().len(),
        "required_missing": completion["required_missing"].as_array().unwrap().len(),
        "flexible_shortfall_tiles": completion["flexible_shortfall"]
            .as_object()
            .unwrap()
            .values()
            .map(|value| value.as_i64().unwrap())
            .sum::<i64>(),
        "requests": status_counts,
        "committed_actions": result.committed_action_count,
        "final_cursor": report["final_cursor"]["timestamp_utc"],
        "wall_seconds": format!("{:.3}", result.accounted_wallclock_seconds).parse::<f64>().unwrap(),
        "global_wallclock_seconds": result.global_wallclock_seconds,
        "outputs": {
            "decisions.csv": out_dir.join("decisions.csv").to_string_lossy(),
            "workflow_result.json": out_dir.join("workflow_result.json").to_string_lossy(),
            "score_report.json": out_dir.join("score_report.json").to_string_lossy(),
            "agent.log": log_path.to_string_lossy(),
        },
    });
    let exit_code = if matches!(
        result.termination_reason,
        crate::schema::TerminationReason::SurveyComplete
            | crate::schema::TerminationReason::GlobalWallclockExpired
    ) {
        0
    } else {
        2
    };
    Ok(RunOutcome {
        result,
        report,
        summary,
        exit_code,
    })
}
