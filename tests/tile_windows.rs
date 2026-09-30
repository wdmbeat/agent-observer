//! Recomputed tile windows must reproduce the shipped `tile_windows.csv`.
//!
//! The shipped artifact covers the first three nights of each scenario. All
//! comparisons are exact: timestamps/IDs as strings, the four float columns as
//! f64 equality after `round6` — no tolerance. Identical operation order with
//! the Python simulator makes the doubles bit-identical.

mod common;

use agent_observer::calendar::load_nights;
use agent_observer::contracts::{read_exact_csv, TILE_WINDOW_COLUMNS};

use common::{build_geometry, reference_dir, SCENARIOS};

#[test]
fn tile_windows_match_shipped_artifact() {
    for name in SCENARIOS {
        let reference = reference_dir(name);
        let shipped =
            read_exact_csv(&reference.join("tile_windows.csv"), &TILE_WINDOW_COLUMNS).unwrap();
        let nights = load_nights(&reference.join("night_calendar.csv")).unwrap();
        let simulator = build_geometry(name);

        // The shipped file spans the first three nights of the survey.
        let shipped_nights: std::collections::BTreeSet<&str> =
            shipped.iter().map(|row| row[1].as_str()).collect();
        assert_eq!(shipped_nights.len(), 3, "{name}: unexpected night coverage");

        let rows = simulator.get_tile_windows(nights[0].night_date, 3).unwrap();
        assert_eq!(rows.len(), shipped.len(), "{name}: window row count");

        fn cell<'a>(row: &'a [String], column: &str) -> &'a str {
            row[TILE_WINDOW_COLUMNS
                .iter()
                .position(|c| *c == column)
                .unwrap()]
            .as_str()
        }
        for (row, expected) in rows.iter().zip(shipped.iter()) {
            let context = format!("{name} {}", row.window_id);
            assert_eq!(row.window_id, cell(expected, "window_id"), "{context}");
            assert_eq!(row.night_id, cell(expected, "night_id"), "{context}");
            assert_eq!(row.night_date, cell(expected, "night_date"), "{context}");
            assert_eq!(row.tile_id, cell(expected, "tile_id"), "{context}");
            assert_eq!(
                row.window_start_utc,
                cell(expected, "window_start_utc"),
                "{context}"
            );
            assert_eq!(
                row.window_end_utc,
                cell(expected, "window_end_utc"),
                "{context}"
            );
            assert_eq!(
                row.window_seconds.to_string(),
                cell(expected, "window_seconds"),
                "{context}"
            );
            assert_eq!(
                row.best_time_utc,
                cell(expected, "best_time_utc"),
                "{context}"
            );
            assert_eq!(row.region_id, cell(expected, "region_id"), "{context}");
            assert_eq!(
                row.scheduling_class,
                cell(expected, "scheduling_class"),
                "{context}"
            );
            assert_eq!(
                row.nominal_exptime_seconds.to_string(),
                cell(expected, "nominal_exptime_seconds"),
                "{context}"
            );
            assert_eq!(
                row.available_until_utc,
                cell(expected, "available_until_utc"),
                "{context}"
            );
            for (actual, column) in [
                (row.best_airmass, "best_airmass"),
                (row.mean_airmass, "mean_airmass"),
                (row.mean_lunar_quality_factor, "mean_lunar_quality_factor"),
                (
                    row.minimum_lunar_quality_factor,
                    "minimum_lunar_quality_factor",
                ),
            ] {
                let shipped_value: f64 = cell(expected, column).parse().unwrap();
                assert_eq!(actual, shipped_value, "{context} {column}");
            }
        }
    }
}
