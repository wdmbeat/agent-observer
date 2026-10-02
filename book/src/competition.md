# The Competition

This chapter is the contract, condensed from the platform's rules and
documentation. Nothing here is analysis — it is what the environment does.

## The setting

A telescope at latitude 31.9634° N, longitude −111.599° (UTC−7) surveys a
catalog of **64 tiles** arranged in **8 regions** (stripes of right
ascension), over a sequence of nights. Time is a calendar of **900-second
slots** between solar dusk (sun below −12°) and dawn.

The agent runs as a persistent process. The platform sends one `initialize`
message (immutable catalogs and the full scoring contract), then one
`decision_request` per decision opportunity; the agent answers
`decision_response` with the same sequence number. Each answer is either:

- `observe` a candidate tile for its nominal exposure (which may cross slot
  boundaries and is integrated in segments), with a chosen **program**
  (`DARK` / `BRIGHT` / `BACKUP`) and an optional request tag, or
- `wait`, consuming the rest of the current slot.

A response may also carry **reports** — anomaly claims that consume no slot
time. The platform commits each decision before revealing the next snapshot;
the future is never shown. One global wall clock (3600 s per formal scenario)
covers everything, including the agent's thinking time.

## Scoring (`challenge-score-v3`)

Each completed exposure segment scores

```
A       = instrument_efficiency · transparency · sky_quality / (seeing · airmass)
A_used  = A · lunar_quality_factor
S       = V_tile · (segment_seconds / nominal_exptime) · A_used · (1 + program_bonus)
```

- `V_tile` — the tile's published science value (Σ of its targets' weights).
- `seeing` — turbulence blur in arcseconds (smaller is sharper; divides).
- `transparency`, `sky_quality` — 0–1 factors for extinction and sky darkness.
- `airmass` — optical path length through the atmosphere (1.0 at zenith).
- `lunar_quality_factor` — 0–1 moonlight penalty from lunar illumination,
  altitude, and angular separation.
- `program_bonus` — +0.25 / +0.15 / +0.08, paid only when the chosen program
  matches the quality band of the exposure (DARK ≥ 0.65, BRIGHT ≥ 0.40,
  else BACKUP; bands are computed on the *efficiency-free* quality).

A tile's science score is the **maximum** over its observations (a worse
repeat never lowers it); completion banks on the first legal observation.

```
total = science + program_bonus + request_reward + coverage_bonus + report_reward
      − unsafe(2000) − invalid(100) − avoidable_wait(0.001/s)
      − required_miss(1000/tile) − flexible_shortfall(100/tile)
      − request_miss(penalty) − fault_misreports − wrong_tag_reports(150)
```

**Coverage bonus** (competition scenarios, weight W = 0.35):

```
coverage_bonus = W · base_science · Jain(regions)
Jain(x)        = (Σx)² / (n · Σx²)      over completed tiles per region
```

1.0 when completions are spread evenly across all 8 regions, 1/8 when one
region takes everything.

## Versioning and the living contract

Every protocol document names its contract revision in a `schema_version`
field: `participant-agent-protocol-v2` for the envelopes,
`initial-publication-v2` for the initialize payload, `decision-snapshot-v2`
or `-v3` for each snapshot (v3 is the anomaly mechanics), and
`challenge-score-v3` for the score configuration. An agent should validate
the version on arrival and fail loudly on one it does not know — a version
bump is how the platform announces a contract change.

Crucially, `initialize` does not just describe the *shape* of the scoring —
it delivers the scoring **constants themselves**, per scenario: the quality
thresholds and program bonuses, every penalty amount, the request reward and
miss penalty, the report settlements, the coverage weight, and the
fault-repair parameters. These are calibration values, not laws of nature:
practice scenarios ship `coverage_bonus_weight = 0` while competition
scenarios use 0.35, and the organizers state that the constants are
provisional until the online competition opens — any change is announced
with a version bump and applies to every submission of the phase.

The consequence is a design rule: **never hardcode scoring constants in a
strategy** — read them from `scoring_contract.score_config` at runtime. An
agent that does so adapts automatically to recalibration; one that bakes in
1000 / 100 / 140 / 190 plans with yesterday's numbers.

## The hidden layer (v3 mechanics)

Competition scenarios hide three things the agent can only infer from
*realized score feedback* (`tile_last_finished`) compared against the
efficiency-free public-formula estimate:

1. **Tile tags** — a *nova* (a star flared: score ×1.5) or *reddening*
   (dust dims the light: ×0.8), applied silently. A correct report earns
   +100, a wrong one costs −150.
2. **Instrument-efficiency jitter** — a per-slot factor in [0.90, 1.00]
   baked into every exposure but never shown in snapshots.
3. **Instrument faults** — an unannounced, never-forecast efficiency
   collapse (down to ×0.10) over a spatial scope, persisting until reported.
   A correct fault report publishes the fault one simulated day later and
   completes the repair in two; misreports beyond a free allowance cost 100.

## The observation (what the agent sees per decision)

Cursor (slot, night, time), current site weather, the legal **candidate
tiles** (geometry, effective weather with directional events applied,
window ends, science value), **active requests** with per-tile visit
progress, survey **progress**, and — on schedule — tonight's windows, a
7-night lookahead (forecast revisions, multi-night windows), and the fault
feed. Forecasts are uncertain and revised daily; instrument faults never
appear in them.

## The competition format

- Three **fixed formal scenarios** (A, B, C), identical for every team;
  their weather and events are never published.
- 10 evaluation batches per team per day; a batch runs all three scenarios,
  3600 s each; the batch score is the mean.
- After the online phase, each team's **final version** is evaluated **once
  on one hidden scenario** — only that score decides the ranking.
- Awards additionally require LLM-driven agent techniques in at least two of:
  natural-language understanding, data parsing, task planning, action
  decision-making, tool calling, plan adaptation.
