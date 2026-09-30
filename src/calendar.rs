//! Shared, site-configured observing calendar (loaders only).
//!
//! Port of the runtime-facing parts of `challenge/observing_calendar.py`:
//! the `Night`/`Slot` records and their CSV loaders. Scenario generation
//! (`build_calendar`, crossing solvers) is intentionally not ported — at run
//! time the calendar is loaded from pre-generated CSVs.

use std::path::Path;

use anyhow::{Context, Result};
use chrono::{DateTime, NaiveDate, TimeDelta, Utc};

use crate::contracts::{format_utc, parse_utc, read_exact_csv, NIGHT_COLUMNS, SLOT_COLUMNS};

pub const SCHEMA_VERSION: &str = "observing-calendar-v1";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Night {
    pub night_id: String,
    pub night_date: NaiveDate,
    pub solar_dusk_utc: DateTime<Utc>,
    pub solar_dawn_utc: DateTime<Utc>,
    pub observing_start_utc: DateTime<Utc>,
    pub observing_end_utc: DateTime<Utc>,
    pub slot_count: i64,
}

impl Night {
    /// `int((observing_end_utc - observing_start_utc).total_seconds())`
    pub fn night_seconds(&self) -> i64 {
        (self.observing_end_utc - self.observing_start_utc).num_seconds()
    }

    pub fn csv_row(&self) -> Vec<String> {
        vec![
            self.night_id.clone(),
            self.night_date.format("%Y-%m-%d").to_string(),
            format_utc(&self.solar_dusk_utc),
            format_utc(&self.solar_dawn_utc),
            format_utc(&self.observing_start_utc),
            format_utc(&self.observing_end_utc),
            self.night_seconds().to_string(),
            self.slot_count.to_string(),
        ]
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Slot {
    pub slot_id: String,
    pub night_id: String,
    pub timestamp_utc: DateTime<Utc>,
    pub duration_seconds: i64,
}

impl Slot {
    pub fn end_utc(&self) -> DateTime<Utc> {
        self.timestamp_utc + TimeDelta::seconds(self.duration_seconds)
    }

    pub fn csv_row(&self) -> Vec<String> {
        vec![
            self.slot_id.clone(),
            self.night_id.clone(),
            format_utc(&self.timestamp_utc),
            self.duration_seconds.to_string(),
        ]
    }
}

fn column_index(columns: &[&str], name: &str) -> usize {
    columns.iter().position(|column| *column == name).unwrap()
}

pub fn load_nights(path: &Path) -> Result<Vec<Night>> {
    let rows = read_exact_csv(path, &NIGHT_COLUMNS)?;
    let mut result = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let line = index + 2;
        let cell = |name: &str| row[column_index(&NIGHT_COLUMNS, name)].as_str();
        let build = || -> Result<Night> {
            Ok(Night {
                night_id: cell("night_id").trim().to_string(),
                night_date: NaiveDate::parse_from_str(cell("night_date"), "%Y-%m-%d")?,
                solar_dusk_utc: parse_utc(cell("solar_dusk_utc"))?,
                solar_dawn_utc: parse_utc(cell("solar_dawn_utc"))?,
                observing_start_utc: parse_utc(cell("observing_start_utc"))?,
                observing_end_utc: parse_utc(cell("observing_end_utc"))?,
                slot_count: cell("slot_count").trim().parse::<i64>()?,
            })
        };
        result.push(build().with_context(|| format!("{}: row {line}", path.display()))?);
    }
    Ok(result)
}

pub fn load_slots(path: &Path) -> Result<Vec<Slot>> {
    let rows = read_exact_csv(path, &SLOT_COLUMNS)?;
    let mut result = Vec::with_capacity(rows.len());
    for (index, row) in rows.iter().enumerate() {
        let line = index + 2;
        let cell = |name: &str| row[column_index(&SLOT_COLUMNS, name)].as_str();
        let build = || -> Result<Slot> {
            Ok(Slot {
                slot_id: cell("slot_id").trim().to_string(),
                night_id: cell("night_id").trim().to_string(),
                timestamp_utc: parse_utc(cell("timestamp_utc"))?,
                duration_seconds: cell("duration_seconds").trim().parse::<i64>()?,
            })
        };
        result.push(build().with_context(|| format!("{}: row {line}", path.display()))?);
    }
    Ok(result)
}
