# Introduction

This book is a study of the **agent-observer** challenge: scheduling a robotic
telescope's survey, one 900-second decision at a time, under weather, moonlight,
deadlines — and a scorer that hides part of the truth.

The project has two lives:

- **An engineering artifact** — a Rust reimplementation of the competition's
  runtime core that is *byte-identical* to the official Python scorer on every
  bundled scenario. It gives us something most optimization projects never
  have: an exact, fast, deterministic model of the environment we are
  optimizing against.
- **A research question** — what is the *nature* of this scheduling problem,
  and which family of solutions (rules, lookahead, learned policies) actually
  matches that nature?

## Why write a mathematical model first?

The shipped baseline is a greedy policy: at every decision, observe the
candidate with the highest estimated gain per second, computed from the public
scoring formula. It is remarkably strong — on the 180-night reference scenario
it completes every tile, fills every quota, and finishes 17 of 18 requests
with zero penalties.

Yet a more "clever" rule-based strategy (deadline overrides, coverage
re-ranking) measured *worse* than the greedy baseline on the rehearsal
scenario. That result is a smell: it means improvements are being made in the
wrong places. Before writing a smarter algorithm — and before training a
learning policy — we should be able to write down, precisely:

- what the agent knows and does not know (the observation structure),
- what it controls (the action structure),
- what it is paid for (the reward decomposition),
- and which structural properties of those three make the problem easy or
  hard (what greedy cannot see).

That is the work of chapter 2. Chapter 3 uses the model to justify the
solution family we choose.

## The cast of characters

| Name | What it is |
|---|---|
| **tile** | One fixed telescope pointing (a field on the sky) holding many targets. The unit of scheduling. |
| **slot** | 900 seconds of wall time; the night is discretized into slots between dusk and dawn. |
| **window** | A contiguous run of slots in which a tile is above the altitude limit and available. |
| **program** | The observing mode chosen per exposure — `DARK`, `BRIGHT`, or `BACKUP` — paid a bonus when it matches the sky's quality band. |
| **request** | A time-limited side quest: observe these tiles before a deadline, or pay a miss penalty. |
| **snapshot** | The per-decision JSON document the platform sends: everything the agent may know right now. |
| **the hidden truth** | Per-tile anomaly tags, instrument-efficiency jitter, and unannounced faults — applied silently by the scorer, inferable only from score feedback. |

## How to read this book

Chapter 1 summarizes the competition as given by the platform — no analysis,
just the contract. Chapter 2 formalizes it as a partially observable Markov
decision process and extracts the structural properties that decide which
algorithms can work. Chapter 3 turns those properties into a concrete plan for
a learned scheduling policy trained against our own byte-exact simulator.

All score numbers quoted are produced by the project's own engine and can be
reproduced with `cargo xtask run` / `cargo xtask compare`.
