# Toward a Learning Policy

The model of chapter 2 says the problem is a semi-Markov scheduling POMDP
with a variable action set, dominant terminal accounts, and a known, fast,
exact simulator. This chapter turns that into an engineering plan — and
states the constraints that bound what a submission may look like.

## Why learning is well-founded *here*

Most RL projects die on simulator fidelity. This one starts with the
simulator already solved: our engine is byte-identical to the platform's
scorer on every bundled scenario, runs a full 180-night episode in seconds
in-process, and the kit's `make_scenario.py` produces unlimited training
scenarios from fresh seeds. The observation is already a typed Rust struct
(`DecisionSnapshot`) — featurization is a struct walk, not a parsing
project. Few scheduling problems offer this combination.

## Design

**Environment wrapper.** A gym-style step API over `ChallengeWorkflow`:
`reset(scenario_seed)` → initial snapshot; `step(action_index)` →
(next snapshot, reward delta, done). The existing `DecisionProvider` trait
is the seam — the learned policy is just another provider.

**Featurization.** Per candidate: quality terms (combined quality, airmass,
lunar factor), science value, exposure time, terminal exposure (REQUIRED?
quota-short region?), request value and deadline distance, window scarcity
(remaining published windows), coverage marginal (Δ Jain). Plus a small
global vector: progress counts, night fraction, forecast summary, detector
state (suspected tags, fault scope).

**Policy.** A shared MLP scores each candidate; a masked softmax over the
scores gives a categorical policy; a scalar state head provides the value
baseline. Small (≈10⁵ parameters) — CPU inference is microseconds, far under
any wall-clock concern.

**Training.** Two stages:

1. **Behavior cloning** from the `preview_actions` ranking — the network
   first learns to *be* greedy. This validates the whole pipeline: a cloned
   policy must reproduce the baseline's golden scores.
2. **PPO fine-tuning** across generated scenarios (fresh seeds, varied
   lengths). Reward = per-step score delta including terminal-account
   changes, so the sparse settlements propagate into per-step signal.

**Evaluation discipline.** The three shipped scenarios plus a generated
held-out set are *never* trained on; `cargo xtask compare` is the report
card. Overfitting to weather sequences is the principal failure mode — the
hidden final scenario shares the generator but not the seed.

## The honest alternative: search, not learning

The same known model enables planning instead of training: rolling-horizon
search (e.g. Monte Carlo tree search or simple beam lookahead) with the
preview heuristic as the rollout policy. No training, no generalization
risk, and the simulator is fast enough for hundreds of rollouts per
decision. The learning path is chosen here as a research goal — but the
search path is the control experiment, and it should be built anyway: if a
beam search of depth 2 matches the trained policy, the honest conclusion is
to ship the simpler one.

## Hard constraints (from the competition rules)

1. **Award eligibility requires LLM-driven techniques in at least two
   stages** (natural-language understanding, data parsing, task planning,
   action decision-making, tool calling, plan adaptation). A trained
   policy is not an LLM. A pure-RL submission competes but cannot win
   awards. The compatible architecture is *hybrid*: the learned policy
   drives action selection while an LLM performs genuine stages — weekly
   plan adaptation and report adjudication are natural fits, and the
   `LlmSelector` seam already exists in the agent.
2. **Runtime limits**: the platform container has 2 CPU cores and 2 GB
   memory; 3600 s per scenario; a 10-minute build. A small policy with the
   NdArray backend compiles well inside the window; weights ship in the
   submission ZIP (50 MB limit).
3. **The hidden final run has no page open**: any LLM stage must use a key
   saved encrypted before the deadline, and every LLM stage needs a
   deterministic fallback — which the agent's pipeline already provides.

## Phases

| Phase | Deliverable | Exit criterion |
|---|---|---|
| 0 — Calibration | score distributions under random / greedy / lookahead policies on many seeds | known headroom per account |
| 1 — Env + featurization | gym-style wrapper, candidate features | step-through a full episode |
| 2 — Behavior cloning | net reproduces greedy | golden totals matched exactly |
| 3 — PPO fine-tune | trained policy | ≥ greedy on held-out seeds |
| 4 — Packaging | `ml` strategy in `sac-agent`, weights in ZIP | submission flow passes end-to-end |

## What success looks like

Not "beat the baseline on demo-week" — the structural analysis says the
reachable margin there is ~1 %. Success is: never lose a *reachable*
terminal account on unseen scenarios, price waiting correctly (the
temporal gap rules cannot express), and degrade gracefully — a policy whose
worst case is the greedy baseline and whose best case is the lookahead
greedy cannot do.
