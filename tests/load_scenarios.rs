//! Every shipped scenario CSV/config must load through the ported data layer.

mod common;

use agent_observer::calendar;
use agent_observer::contracts::{
    anomaly_mechanics_enabled, read_exact_csv, TARGET_COLUMNS, TILE_ANOMALY_COLUMNS,
    TILE_WINDOW_COLUMNS,
};
use agent_observer::geometry;
use agent_observer::requests::{self, ObservationRequestSimulator};
use agent_observer::weather;

use common::{config_dir, reference_dir, SCENARIOS};

#[test]
fn all_scenarios_load() {
    for name in SCENARIOS {
        let reference = reference_dir(name);
        let config = config_dir(name);

        let nights = calendar::load_nights(&reference.join("night_calendar.csv")).unwrap();
        let slots = calendar::load_slots(&reference.join("slots.csv")).unwrap();
        assert!(!nights.is_empty() && !slots.is_empty(), "{name}: empty calendar");

        // Slot counts and night durations must be self-consistent.
        for night in &nights {
            let owned: Vec<_> = slots.iter().filter(|s| s.night_id == night.night_id).collect();
            assert_eq!(owned.len() as i64, night.slot_count, "{name} {}", night.night_id);
            assert_eq!(
                owned.iter().map(|s| s.duration_seconds).sum::<i64>(),
                night.night_seconds(),
                "{name} {}",
                night.night_id
            );
        }

        let tiles = geometry::load_tiles(&reference.join("tiles.csv")).unwrap();
        assert!(!tiles.is_empty(), "{name}: no tiles");
        geometry::load_config(&config.join("tile_config.json")).unwrap();
        geometry::load_json(&config.join("calendar_config.json")).unwrap();

        let weather_rows = weather::load_weather(&reference.join("weather.csv")).unwrap();
        assert_eq!(weather_rows.len(), slots.len(), "{name}: weather/slot count");
        weather::load_events(&reference.join("weather_events.csv")).unwrap();
        weather::load_forecasts(&reference.join("weather_forecasts.csv")).unwrap();
        weather::load_config(&config.join("weather_config.json")).unwrap();

        let simulator = ObservationRequestSimulator::from_files(
            &reference.join("observation_requests.csv"),
            &reference.join("observation_request_tiles.csv"),
        )
        .unwrap();
        assert!(!simulator.requests.is_empty(), "{name}: no requests");
        requests::load_config(&config.join("request_config.json")).unwrap();

        // Participant-facing artifacts and optional anomaly files parse too.
        read_exact_csv(&reference.join("tile_windows.csv"), &TILE_WINDOW_COLUMNS).unwrap();
        read_exact_csv(&reference.join("targets.csv"), &TARGET_COLUMNS).unwrap();
        let anomalies = reference.join("tile_anomalies.csv");
        if anomalies.exists() {
            read_exact_csv(&anomalies, &TILE_ANOMALY_COLUMNS).unwrap();
        }

        // The anomaly switch must agree with the score config contents.
        let score_config = geometry::load_json(&config.join("score_config.json")).unwrap();
        let expected = name == "finals-preview";
        assert_eq!(anomaly_mechanics_enabled(&score_config), expected, "{name}");
    }
}
