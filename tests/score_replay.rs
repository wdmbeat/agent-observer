//! Full score_report.json replay comparison against the golden files.
//!
//! Deep, recursive, exact equality: numbers compare as f64 (or i64) with zero
//! tolerance — every rounded field is `round6` on a bit-identical double, and
//! every unrounded field (request rewards, cursor, counters) is integer or
//! config-literal. The comparator reports the first differing JSON paths.

mod common;

use agent_observer::scoring::score_files;
use serde_json::Value;

use common::{golden_dir, scenario_dir, SCENARIOS};

const EXPECTED_TOTALS: [(&str, f64); 3] = [
    ("demo-week", 5909.099093),
    ("dev-reference", 12287.478365),
    ("finals-preview", 8214.257133),
];

fn compare(path: &str, expected: &Value, actual: &Value, mismatches: &mut Vec<String>) {
    if mismatches.len() >= 20 {
        return;
    }
    match (expected, actual) {
        (Value::Object(expected), Value::Object(actual)) => {
            let expected_keys: std::collections::BTreeSet<_> = expected.keys().collect();
            let actual_keys: std::collections::BTreeSet<_> = actual.keys().collect();
            if expected_keys != actual_keys {
                mismatches.push(format!(
                    "{path}: keys differ: only golden {:?}, only actual {:?}",
                    expected_keys.difference(&actual_keys).collect::<Vec<_>>(),
                    actual_keys.difference(&expected_keys).collect::<Vec<_>>(),
                ));
                return;
            }
            for key in expected.keys() {
                compare(
                    &format!("{path}.{key}"),
                    &expected[key],
                    &actual[key],
                    mismatches,
                );
            }
        }
        (Value::Array(expected), Value::Array(actual)) => {
            if expected.len() != actual.len() {
                mismatches.push(format!(
                    "{path}: array lengths {} != {}",
                    expected.len(),
                    actual.len()
                ));
                return;
            }
            for (index, (left, right)) in expected.iter().zip(actual.iter()).enumerate() {
                compare(&format!("{path}[{index}]"), left, right, mismatches);
            }
        }
        (Value::Number(expected), Value::Number(actual)) => {
            if expected.as_f64() != actual.as_f64() {
                mismatches.push(format!("{path}: {expected} != {actual}"));
            }
        }
        _ => {
            if expected != actual {
                mismatches.push(format!("{path}: {expected} != {actual}"));
            }
        }
    }
}

#[test]
fn score_reports_match_goldens_exactly() {
    for name in SCENARIOS {
        let golden: Value = serde_json::from_str(
            &std::fs::read_to_string(golden_dir(name).join("score_report.json")).unwrap(),
        )
        .unwrap();
        let report = score_files(
            &scenario_dir(name),
            &golden_dir(name).join("decisions.csv"),
            None,
            golden["termination_reason"].as_str().unwrap(),
        )
        .unwrap();
        let mut mismatches = Vec::new();
        compare("$", &golden, &report, &mut mismatches);
        assert!(mismatches.is_empty(), "{name}:\n{}", mismatches.join("\n"));
    }
}

#[test]
fn gate_totals() {
    for (name, expected) in EXPECTED_TOTALS {
        let report = score_files(
            &scenario_dir(name),
            &golden_dir(name).join("decisions.csv"),
            None,
            "survey_complete",
        )
        .unwrap();
        assert_eq!(
            report["score"]["total"].as_f64().unwrap(),
            expected,
            "{name} total"
        );
    }
}
