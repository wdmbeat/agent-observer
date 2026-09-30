//! Spot-check geometry (airmass, lunar_quality_factor, altitude) and the
//! weather-quality pipeline against segment values recorded in the golden
//! `score_report.json` files.
//!
//! The Python scorer samples geometry at each segment midpoint
//! (`start_utc + duration_seconds / 2`) and stores `round(x, 6)` values, so all
//! comparisons are exact f64 equality after `round6` — no tolerance.
//!
//! Altitude is not recorded in the segments, so it is checked two ways: by
//! inverting the normalized-airmass formula from the recorded segment airmass
//! (bisection, 1e-6 deg), and against fixed reference values captured from the
//! Python simulator for three tile/timestamp pairs.

mod common;

use agent_observer::contracts::{datetime_from_epoch, epoch_seconds, parse_utc, round6};
use agent_observer::geometry::{normalized_airmass, GeometrySample};
use agent_observer::weather::weather_quality;
use chrono::{DateTime, Utc};
use serde_json::Value;

use common::{build_geometry, build_weather, golden_dir, SCENARIOS};

const SEGMENTS_PER_SCENARIO: usize = 25;

/// Invert `normalized_airmass` by bisection on (0, 90] degrees.
fn altitude_from_airmass(airmass: f64) -> f64 {
    let (mut low, mut high) = (0.0f64, 90.0f64);
    for _ in 0..200 {
        let middle = (low + high) / 2.0;
        if normalized_airmass(middle) > airmass {
            low = middle;
        } else {
            high = middle;
        }
    }
    (low + high) / 2.0
}

#[test]
fn altitude_matches_python_reference_values() {
    // Captured from TileGeometrySimulator.get_tile_geometry on demo-week:
    //   T00041 @ 2026-10-06T02:07:30Z, T00001 @ 2026-10-06T07:22:30Z,
    //   T00010 @ 2026-10-07T05:00:00Z
    let simulator = build_geometry("demo-week");
    let cases: [(&str, &str, GeometrySample); 3] = [
        (
            "T00041",
            "2026-10-06T02:07:30Z",
            GeometrySample {
                altitude_deg: 54.8821156932461,
                azimuth_deg: 328.5517754908301,
                hour_angle_deg: 34.65583697607849,
                airmass: 1.2219845539601881,
                moon_separation_deg: 90.55706914423196,
                lunar_quality_factor: 1.0,
            },
        ),
        (
            "T00001",
            "2026-10-06T07:22:30Z",
            GeometrySample {
                altitude_deg: 69.98551473858551,
                azimuth_deg: 182.1128500834502,
                hour_angle_deg: 0.739039337736557,
                airmass: 1.0641041867410672,
                moon_separation_deg: 122.1478043708935,
                lunar_quality_factor: 1.0,
            },
        ),
        (
            "T00010",
            "2026-10-07T05:00:00Z",
            GeometrySample {
                altitude_deg: 23.488960108528566,
                azimuth_deg: 66.14862247961673,
                hour_angle_deg: -80.40365620458437,
                airmass: 2.4972612222642048,
                moon_separation_deg: 88.02338585695806,
                lunar_quality_factor: 1.0,
            },
        ),
    ];
    for (tile_id, timestamp, expected) in cases {
        let moment: DateTime<Utc> = parse_utc(timestamp).unwrap();
        let actual = simulator.get_tile_geometry(tile_id, moment).unwrap().sample;
        assert_eq!(actual, expected, "{tile_id} @ {timestamp}");
    }
}

#[test]
fn segments_match_golden_score_reports() {
    for name in SCENARIOS {
        let geometry = std::rc::Rc::new(build_geometry(name));
        let mut weather = build_weather(name, Some(geometry.clone()));

        let report_text = std::fs::read_to_string(golden_dir(name).join("score_report.json"))
            .unwrap();
        let report: Value = serde_json::from_str(&report_text).unwrap();

        // Reproduce the scorer's run-local overlay: acknowledged instrument
        // faults stop applying at their repair time.
        for action in report["actions"].as_array().unwrap() {
            if let Some(result) = action.get("report_result") {
                let repair = parse_utc(result["repair_complete_utc"].as_str().unwrap()).unwrap();
                for event_id in result["acknowledged_event_ids"].as_array().unwrap() {
                    weather
                        .end_overrides
                        .insert(event_id.as_str().unwrap().to_string(), repair);
                }
            }
        }

        // Collect (tile_id, segment) pairs from observe actions and sample
        // evenly across the report.
        let mut segments: Vec<(&str, &Value)> = Vec::new();
        for action in report["actions"].as_array().unwrap() {
            if action["action"].as_str() != Some("observe") {
                continue;
            }
            for segment in action["segments"].as_array().unwrap() {
                segments.push((action["tile_id"].as_str().unwrap(), segment));
            }
        }
        assert!(!segments.is_empty(), "{name}: no observe segments");
        let step = (segments.len() / SEGMENTS_PER_SCENARIO).max(1);
        let sampled: Vec<_> = segments.iter().step_by(step).collect();

        for (tile_id, segment) in sampled {
            let context = format!("{name} {} {}", tile_id, segment["slot_id"].as_str().unwrap());
            let start = parse_utc(segment["start_utc"].as_str().unwrap()).unwrap();
            let midpoint_epoch =
                epoch_seconds(&start) + segment["duration_seconds"].as_f64().unwrap() / 2.0;
            let sample = geometry
                .get_tile_geometry(tile_id, datetime_from_epoch(midpoint_epoch))
                .unwrap()
                .sample;

            let expected_airmass = segment["airmass"].as_f64().unwrap();
            assert_eq!(round6(sample.airmass), expected_airmass, "{context} airmass");

            let expected_lunar = segment["lunar_quality_factor"].as_f64().unwrap();
            assert_eq!(
                round6(sample.lunar_quality_factor),
                expected_lunar,
                "{context} lunar_quality_factor"
            );

            // Altitude check via airmass inversion (segments carry no altitude).
            // The segment airmass is rounded to 6 decimals first, and airmass
            // is insensitive to altitude near culmination, so 1e-3 deg is the
            // meaningful tolerance here (the exact airmass equality above is
            // the real fidelity gate).
            let recovered = altitude_from_airmass(expected_airmass);
            assert!(
                (sample.altitude_deg - recovered).abs() < 1e-3,
                "{context} altitude: {} vs recovered {recovered}",
                sample.altitude_deg
            );

            // Full weather-quality pipeline: effective conditions + formula.
            let conditions = weather
                .get_effective_conditions(segment["slot_id"].as_str().unwrap(), Some(tile_id), true)
                .unwrap();
            let expected_events: Vec<String> = segment["active_event_ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| id.as_str().unwrap().to_string())
                .collect();
            assert_eq!(conditions.active_event_ids, expected_events, "{context} events");

            let atmospheric =
                weather_quality(&conditions, sample.airmass, &weather.config, true).unwrap();
            assert_eq!(
                round6(atmospheric),
                segment["atmospheric_quality"].as_f64().unwrap(),
                "{context} atmospheric_quality"
            );
            assert_eq!(
                round6(atmospheric * sample.lunar_quality_factor),
                segment["combined_quality"].as_f64().unwrap(),
                "{context} combined_quality"
            );
        }
    }
}
