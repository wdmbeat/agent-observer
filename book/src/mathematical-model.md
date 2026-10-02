# A Mathematical Model

We now strip the competition to its mathematical skeleton. The goal is not
formalism for its own sake: the model should tell us *which algorithm
families can work* and where the greedy baseline is structurally blind.

## The process: a POMDP

The environment is a finite-horizon **partially observable Markov decision
process** `(S, A, O, T, R, Ω, γ)` with `γ = 1`.

### Hidden state `S_t`

Everything the simulator knows and the agent does not:

- the weather truth: per-slot seeing, transparency, sky quality (AR(1)
  processes with seasonal drift), the background closure chain, and the
  directional weather events with their spatial scopes;
- the per-slot **instrument-efficiency jitter** in [0.90, 1.00];
- the hidden **tile tags** (nova ×1.5, reddening ×0.8);
- the **fault events**: scopes, efficiency multipliers, and repair state;
- the future request schedule, and each request's hidden feasibility;
- plus the fully known part: cursor position, completion set, per-tile
  banked maxima, request visit counters.

### Observation `O_t = Ω(S_t)`

The decision snapshot: cursor, current site weather, the candidate tiles
(with geometry, effective weather, window ends, science value), active
requests with visit progress, survey progress, scheduled publications
(night windows, weekly forecast revisions), and the realized-score feedback
`tile_last_finished`. The observation is *lossy by design*: it never
contains `instrument_efficiency`, future weather truth, or tag/fault truth.

### Action `A_t`

```
A_t = {wait} ∪ { (tile, program, request_id) : tile ∈ candidates(O_t) }
```

The action set is **variable-size and observation-dependent** — there is no
fixed action space. Any policy must score and select among the currently
legal candidates (a pointer/attention structure), not learn a fixed softmax
head. Illegal triples are not masked by the environment; they cost 100 each,
so legality must be enforced by the policy itself.

### Transition `T`

Deterministic given the hidden state — the simulator is a known function.
Stochastic from the agent's perspective, because the hidden drivers (weather
processes, jitter, events) are stochastic. One step advances simulated time
by the chosen exposure's duration (or the rest of the slot for `wait`),
which means **actions have variable duration** — a semi-Markov property.

### Reward `R`

The total decomposes into a dense part paid during the run and a sparse part
settled at the end:

```
total = Σ_tiles  max over exposures  V · (t/nominal) · A_used · (1+bonus)     (dense)
      + 140·(request tiles completed) − 190·(request tiles expired)            (sparse)
      − 1000·(REQUIRED missed) − 100·(FLEXIBLE short of quota 4/region)        (sparse)
      + 0.35 · base_science · Jain(completions per region)                     (sparse, coupled)
      + report settlements (+100/−150 per tag; fault repair)                   (belief sub-problem)
      − 2000·unsafe − 100·invalid − 0.001·(avoidable wait seconds)             (dense penalties)
```

## What the structure tells us

### 1. It is stochastic scheduling with deadlines and time windows

Strip the astronomy and the skeleton is a classic: jobs (tiles) with weights
(science value), processing times (exposures), time windows (visibility),
deadlines (requests), on a machine (the telescope) whose efficiency varies
stochastically (weather). Weighted scheduling with time windows is NP-hard in
general — exact optimization is out, and the sensible families are greedy
heuristics, lookahead/search, and learned policies.

### 2. Waiting is nearly free, and quality varies by ~10×

The avoidable-wait penalty is 0.001 per second — 0.9 points per slot — while
the quality multiplier `A_used` sweeps roughly an order of magnitude with
airmass, seeing, and moonlight. **When** to observe is therefore worth far
more than **whether** to observe, per tile. The greedy baseline never prices
this: it observes the best candidate *now* even if the same tile transits in
two hours at half the airmass. This is a temporal credit-assignment problem —
precisely the shape that rule-based policies handle badly and value-based
learning handles natively.

### 3. Max-banking couples actions across time

A tile banks the *maximum* over its observations, so the value of observing
it again is `max(0, potential − banked)` — the action's value depends on the
policy's own history. Greedy marginal accounting handles this only locally;
a value function absorbs it automatically.

### 4. Terminal accounts dominate the variance

−1000 per missed REQUIRED tile, −190 per expired request tile, ±35 % of
base science from coverage evenness. Against these, per-exposure quality
differences are second-order. Any policy that never loses a terminal account
already ranks well; quality timing separates the safe policies. The measured
losses confirm it: on the rehearsal scenario the baseline loses 1500 points
to *structural* causes (an unobservable REQUIRED tile, an unfillable region)
that no policy can recover — knowing which losses are reachable is part of
the model.

### 5. The anomaly layer is a belief sub-MDP

Detecting tags and faults is sequential hypothesis testing on the ratio
`realized / efficiency-free-estimate`, with asymmetric payoffs (+100
correct, −150 wrong; fault repair value vs. −100 misreports). The optimal
report policy is an optimal-stopping rule on a posterior belief — small,
well-understood, and already near-optimally handled by the shipped
threshold detector. Learning effort is better spent on scheduling.

### 6. The legality mask is cheap and must be exact

Invalid actions cost 100 and waste the slot. For a learned policy this means
candidate masking is not optional; it is part of the environment interface.

## Consequences for the solution family

| Property | Consequence |
|---|---|
| Variable, observation-dependent action set | per-candidate scoring + masked selection (pointer-style policy) |
| NP-hard scheduling core | no exact methods; heuristics, search, or learning |
| Nearly-free waiting, high quality variance | lookahead/timing is the biggest reachable gain |
| Sparse, dominant terminal rewards | reward shaping must surface terminal accounts per step |
| Known, fast, exact transition model | simulation-based training and planning are both viable |
| Belief sub-MDP for anomalies | keep the calibrated detector; don't learn this part |

The next chapter turns these into a concrete design for a learned policy —
and checks it against the honest alternative: using the same known model for
search instead of learning.
