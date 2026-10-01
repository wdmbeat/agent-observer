//! Shared helpers for integration tests.
//!
//! Not every test file uses every helper.
#![allow(dead_code)]

use std::path::PathBuf;

use agent_observer::geometry::TileGeometrySimulator;
use agent_observer::weather::{self, WeatherSimulator};

pub const SCENARIOS: [&str; 3] = ["demo-week", "dev-reference", "finals-preview"];

/// Root of the Python starter kit's scenarios. Override with
/// `AGENT_OBSERVER_SCENARIOS` when the checkout lives elsewhere.
pub fn scenarios_root() -> PathBuf {
    std::env::var("AGENT_OBSERVER_SCENARIOS")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("tmp/agent-observer-starter-kit/scenarios")
        })
}

pub fn scenario_dir(name: &str) -> PathBuf {
    scenarios_root().join(name)
}

pub fn reference_dir(name: &str) -> PathBuf {
    scenario_dir(name).join("outputs/reference")
}

pub fn config_dir(name: &str) -> PathBuf {
    scenario_dir(name).join("config")
}

pub fn golden_dir(name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/golden")
        .join(name)
}

pub fn build_geometry(name: &str) -> TileGeometrySimulator {
    let reference = reference_dir(name);
    let config = config_dir(name);
    TileGeometrySimulator::from_files(
        &reference.join("tiles.csv"),
        &config.join("tile_config.json"),
        &config.join("calendar_config.json"),
        &reference.join("night_calendar.csv"),
        &reference.join("slots.csv"),
    )
    .unwrap()
}

pub fn build_weather(
    name: &str,
    geometry: Option<std::rc::Rc<TileGeometrySimulator>>,
) -> WeatherSimulator {
    let reference = reference_dir(name);
    WeatherSimulator::new(
        weather::load_weather(&reference.join("weather.csv")).unwrap(),
        weather::load_forecasts(&reference.join("weather_forecasts.csv")).unwrap(),
        weather::load_events(&reference.join("weather_events.csv")).unwrap(),
        weather::load_config(&config_dir(name).join("weather_config.json")).unwrap(),
        geometry,
    )
    .unwrap()
}
