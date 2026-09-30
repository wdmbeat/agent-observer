//! End-to-end workflow gate: run an agent through the full pipeline and
//! require byte-identical decisions.csv plus the golden score total. Covers
//! both CLI agent forms: `python <script>` (JSONL subprocess transport) and
//! `rust baseline` (in-process). Ignored by default (spawns Python / takes
//! ~5s for dev-reference) — run with `cargo test --release -- --ignored`.

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;

use common::{golden_dir, scenario_dir, SCENARIOS};

const EXPECTED_TOTALS: [(&str, f64); 3] = [
    ("demo-week", 5909.099093),
    ("dev-reference", 12287.478365),
    ("finals-preview", 8214.257133),
];

fn python_agent_script() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../agent-observer-starter-kit/agent/minimal_agent.py")
}

#[test]
#[ignore = "spawns the Python reference agent; run explicitly"]
fn workflow_run_matches_golden_decisions() {
    let script = python_agent_script();
    if !script.is_file() {
        eprintln!("skipping: Python starter kit not found at {script:?}");
        return;
    }
    for (name, expected_total) in EXPECTED_TOTALS {
        run_gate(
            name,
            expected_total,
            &["python".into(), script.to_string_lossy().into_owned()],
        );
    }
}

/// Same gate with the native deterministic agent: no Python involved.
#[test]
#[ignore = "run explicitly"]
fn builtin_agent_matches_golden_decisions() {
    for (name, expected_total) in EXPECTED_TOTALS {
        run_gate(name, expected_total, &["rust".into(), "baseline".into()]);
    }
}

fn run_gate(name: &str, expected_total: f64, agent_args: &[String]) {
    assert!(SCENARIOS.contains(&name));
    let binary = env!("CARGO_BIN_EXE_agent-observer");
    let tag = agent_args.join("-").replace(['/', ' '], "_");
    let out_dir = std::env::temp_dir().join(format!("agent-observer-gate-{name}-{tag}"));
    let output = Command::new(binary)
        .args([
            "run",
            "--scenario",
            &scenario_dir(name).to_string_lossy(),
            "--out",
            &out_dir.to_string_lossy(),
            "--quiet",
        ])
        .args(agent_args)
        .output()
        .unwrap();
    assert_eq!(
        output.status.code(),
        Some(0),
        "{name}: exit code, stderr: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let decisions = std::fs::read(out_dir.join("decisions.csv")).unwrap();
    let golden = std::fs::read(golden_dir(name).join("decisions.csv")).unwrap();
    assert_eq!(
        decisions, golden,
        "{name}: decisions.csv not byte-identical"
    );
    let report: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(out_dir.join("score_report.json")).unwrap())
            .unwrap();
    assert_eq!(
        report["score"]["total"].as_f64().unwrap(),
        expected_total,
        "{name}: score total"
    );
}
