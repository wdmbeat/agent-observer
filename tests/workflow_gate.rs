//! End-to-end workflow gate: run the Python `minimal_agent.py` through the
//! JSONL transport and require byte-identical decisions.csv plus the golden
//! score total. Ignored by default (spawns Python; ~5s for dev-reference) —
//! run with `cargo test -- --ignored`.

mod common;

use std::process::Command;

use common::{golden_dir, scenario_dir, SCENARIOS};

const EXPECTED_TOTALS: [(&str, f64); 3] = [
    ("demo-week", 5909.099093),
    ("dev-reference", 12287.478365),
    ("finals-preview", 8214.257133),
];

#[test]
#[ignore = "spawns the Python reference agent; run explicitly"]
fn workflow_run_matches_golden_decisions() {
    let python_agent = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../agent-observer-starter-kit/agent/minimal_agent.py");
    if !python_agent.is_file() {
        eprintln!("skipping: Python starter kit not found at {python_agent:?}");
        return;
    }
    let agent = format!("python3 {}", python_agent.to_string_lossy());
    let agent_dir = python_agent.parent().unwrap().to_path_buf();
    for (name, expected_total) in EXPECTED_TOTALS {
        run_gate(name, expected_total, &agent, &agent_dir);
    }
}

/// Same gate with the native deterministic agent: no Python involved.
#[test]
#[ignore = "run explicitly"]
fn builtin_agent_matches_golden_decisions() {
    let agent_dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../agent-observer-starter-kit/agent");
    for (name, expected_total) in EXPECTED_TOTALS {
        run_gate(name, expected_total, "builtin", &agent_dir);
    }
}

fn run_gate(
    name: &str,
    expected_total: f64,
    agent: &str,
    agent_dir: &std::path::Path,
) {
    assert!(SCENARIOS.contains(&name));
    let binary = env!("CARGO_BIN_EXE_agent-observer");
    let out_dir =
        std::env::temp_dir().join(format!("agent-observer-gate-{name}-{}", agent.replace(['/', ' '], "_")));
    let output = Command::new(binary)
        .args([
            "run",
            "--scenario",
            &scenario_dir(name).to_string_lossy(),
            "--agent",
            agent,
            "--agent-dir",
            &agent_dir.to_string_lossy(),
            "--out",
            &out_dir.to_string_lossy(),
            "--quiet",
        ])
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
    assert_eq!(decisions, golden, "{name}: decisions.csv not byte-identical");
    let report: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(out_dir.join("score_report.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        report["score"]["total"].as_f64().unwrap(),
        expected_total,
        "{name}: score total"
    );
}
