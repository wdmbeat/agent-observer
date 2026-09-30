# agent-observer (Rust)

A faithful Rust port of the **runtime core** of the agent-observer
telescope-survey competition kit (Python original at
`../../agent-observer-starter-kit`). It replays a survey against a scenario,
drives an agent through the platform's decision loop, and scores the resulting
trace — bit-for-bit compatible with the Python implementation on all bundled
scenarios.

This crate is a learning/experimentation platform for developing observing
strategies in Rust: you get the full simulation loop and authoritative scorer
locally, iterate on a strategy, and a competition submission only needs the
`decisions.csv` trace (or an equivalent agent run on the platform).

## Usage

```
cargo build --release
```

Run a survey with the built-in deterministic agent (no Python required):

```
./target/release/agent-observer run \
    --scenario ../../agent-observer-starter-kit/scenarios/demo-week \
    --out out/demo-week \
    rust baseline
```

Run the Python reference agent through the JSON-Lines subprocess transport
(the same protocol the platform uses: one `initialize` envelope, then one
`decision_request`/`decision_response` per slot):

```
./target/release/agent-observer run \
    --scenario ../../agent-observer-starter-kit/scenarios/demo-week \
    --out out/demo-week-py \
    python ../../agent-observer-starter-kit/agent/minimal_agent.py
```

(`python <script>` spawns `python3 -B <script>` with cwd = the script's
parent, so `.env` loading keeps working; override with `--agent-dir`. The
escape hatch for anything else is `external "<any shell command>"`, spawned
via `sh -c`.)

Score an existing trace (writes the report to stdout; use `--out <path>` to
write a file, `--termination-reason` to override the default `trace_complete`):

```
./target/release/agent-observer score \
    --scenario ../../agent-observer-starter-kit/scenarios/demo-week \
    --decisions out/demo-week/decisions.csv
```

`run` writes `decisions.csv`, `workflow_result.json`, `score_report.json`, and
`agent.log` to `--out`, and prints a JSON summary on the last stdout line. Exit
code is 0 for `survey_complete`/`global_wallclock_expired`, 2 otherwise.
The agent is chosen by a trailing subcommand — `rust <STRATEGY>`,
`python <script.py> [--agent-dir <dir>]`, or
`external "<cmd>" [--agent-dir <dir>]`. Other useful flags:
`--wallclock <secs>`, `--init-timeout <secs>` (30s default),
`--keep-initial-publication`, `--quiet`.

## What was ported

- **Contracts** (`src/contracts.rs`): exact CSV column contracts, strict
  readers/writers (LF endings, BOM-tolerant), UTC timestamp and boolean
  parsing, sha256, `round6`/`round9` (Python `round` ties-to-even semantics).
- **Calendar** (`src/calendar.rs`): night/slot loaders.
- **Geometry** (`src/geometry.rs`): tiles, sidereal/Sun/Moon positions,
  normalized Kasten-Young airmass, lunar quality factor, tile-window
  computation. Float operation order mirrors the Python for bit-identical
  results.
- **Weather** (`src/weather.rs`): directional weather events, forecasts,
  effective-conditions overlay (scope matching, force-close, multipliers,
  clipping), forecast publication, weather-quality formula.
- **Requests** (`src/requests.rs`): observation requests, time-safe
  publication.
- **Scorer** (`src/scoring.rs`): the authoritative replay engine — cursor
  model, segmented exposure integration, interruption rules, legacy vs.
  anomaly-mechanics banking, waits/penalties, report settlement, request
  settlement with feasibility excusal, coverage bonus.
- **Workflow** (`src/workflow.rs`): the platform simulation loop —
  initial publication, decision snapshots (v2/v3), candidate previews,
  weekly/night-start publications, fault-status feed, commit log, wall-clock
  termination handling.
- **Transport** (`src/transport.rs`): JSON-Lines subprocess transport with
  scrubbed environment, `.env` loading, 30s initialization deadline, global
  wall-clock cutoff, and process-group kill (SIGTERM then SIGKILL).
- **Deterministic agent** (`src/agent/`): the shipped reference strategy —
  public-formula preview ranking (`preview.rs`), the anomaly detector with
  nova/reddening/instrument-fault reporting (`anomaly.rs`), and the decision
  pipeline (`builtin.rs`).

## Deliberately not ported

- Scenario **generation** (all `generate_*`/`build_*` functions): at run time
  everything loads from the pre-generated CSVs under
  `scenarios/<name>/outputs/reference/`.
- The **HTML replay renderer** (`challenge/replay.py`).
- **LLM support** in the agent (LangGraph/model factory): the deterministic
  path is what the shipped `minimal_agent.py` does when no model is
  configured, which is also what the bundled goldens were produced with.
- The kit's **fetch/pack/submit** tooling (`fetch_scenario.py`,
  `pack_agent.py`, `sac_submit.py`) and the Windows `.bat` wrappers.

## Verification

The ported pipeline reproduces the Python kit **byte-identically** on all
three bundled scenarios (golden outputs live in `tests/golden/<scenario>/`):

| Scenario | decisions.csv | score_report.json | Total |
|---|---|---|---|
| demo-week | byte-identical (`cmp`) | byte-identical | 5909.099093 |
| dev-reference | byte-identical | byte-identical | 12287.478365 |
| finals-preview | byte-identical (incl. anomaly report rows) | byte-identical | 8214.257133 |

Byte-parity holds for both agent paths: the external Python agent
(`minimal_agent.py` via the JSONL transport, CLI form `python <script>`) and
the native `rust baseline`.

## Tests

```
cargo test
```

runs the fast suite: contract unit tests, loading all three scenarios'
CSVs/configs, recomputed tile windows vs. the shipped `tile_windows.csv`
(exact match), geometry/weather spot-checks against golden score-report
segments (exact, 6-decimal), and a full deep-JSON replay of the scorer against
every golden `score_report.json` (score, completion, requests, waits, reports,
penalties, and the entire actions trace — exact).

The end-to-end gates (run the full workflow, compare `decisions.csv`
byte-for-byte and the total exactly) are `#[ignore]`d for speed; run them with:

```
cargo test --release -- --ignored
```

This covers both the Python-agent transport path (skipped if the Python kit
checkout is not next to this crate) and the builtin agent.

Scenarios are located via the `AGENT_OBSERVER_SCENARIOS` env var, defaulting
to `../../agent-observer-starter-kit/scenarios` relative to this crate.

## Writing your own strategy

Plug in at `src/agent/builtin.rs`: `BuiltinAgent::decide` receives the full
decision snapshot as a `serde_json::Value` and returns the response envelope.
The deterministic default is "observe the top-ranked preview"; the ranked
candidates come from `preview_actions` in `src/agent/preview.rs`, where each
`CandidatePreview` exposes:

- `tile_id`, `program`, `request_id`, `region_id`, `scheduling_class`,
  `nominal_exptime_seconds`
- `combined_quality` (current atmospheric × lunar; DARK ≥ 0.65, BRIGHT ≥ 0.40
  under the bundled score configs)
- `estimated_science_score`, `terminal_penalty_avoidance`,
  `request_policy_value`, `estimated_total_gain`,
  `estimated_gain_per_second` (the default ranking key)

The snapshot also carries `active_requests` (with remaining visits),
`progress`, `night_start`/`weekly` publications, and — under the v3 anomaly
mechanics — `tile_last_finished` feedback and `fault_status` publications,
which `AnomalyDetector` in `src/agent/anomaly.rs` turns into calibrated
reports. Anomaly thresholds are env-overridable via `SAC_ANOMALY_*`.

To try a strategy end-to-end: edit `decide`, then
`cargo run --release -- run --scenario <scenario> --out out/mine rust baseline`
and compare `out/mine/score_report.json` against the baselines above.
