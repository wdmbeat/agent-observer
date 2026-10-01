//! The reference teaching strategy — port of `agent/reference_strategy.py`.
//!
//! Trusts the platform ranking normally and overrides only when an end-of-game
//! account is about to come due: a REQUIRED tile running out of windows, or
//! (competition scenes) the coverage-evenness marginal gain. Rules ② and ③
//! measured as net-negative on the shipped scenarios and stay off behind const
//! toggles, mirroring the Python `if False` blocks.

use std::collections::HashMap;

use crate::contracts::epoch_seconds;

use super::decision_graph::{Selection, Selector};
use super::model::{DecisionSnapshot, InitialPublication};
use super::scoring_preview::CandidatePreview;

// These numbers come from the public score_config.json; keep them in sync if
// the competition configuration changes. (Documentation constants: the rules
// below read the live snapshot/config values, like the Python original.)
pub const MISS_REQUIRED: f64 = 1000.0; // per unfinished REQUIRED tile
pub const SHORT_FLEXIBLE: f64 = 100.0; // per missing FLEXIBLE tile under quota
pub const FLEXIBLE_QUOTA: i64 = 4; // completed FLEXIBLE tiles needed per region
pub const REQUEST_REWARD: f64 = 140.0; // per required tile on completion
pub const REQUEST_MISS: f64 = 190.0; // per required tile on expiry

/// This few remaining windows counts as "about to miss", worth overriding the
/// platform ranking.
pub const LAST_CHANCES: i64 = 2;
/// Requests expiring within this many days are urgent (rule ② only).
pub const EXPIRING_WITHIN_DAYS: f64 = 1.0;

/// Rule ② request jump — ⚠️ off by default: on this 5%-subscription scenario it
/// measured net-negative (the jumped request reward doesn't cover the science
/// given up). Enable for a tighter competition. Mirrors Python's `if False`.
const ENABLE_REQUEST_JUMP: bool = false;

/// Rule ③ quota rescue — ⚠️ likewise off by default; some regions are
/// structurally unfillable (their sky only transits in daylight this month), so
/// the shortfall is unrecoverable and rescuing it wastes good nights. Mirrors
/// Python's `if False`.
const ENABLE_QUOTA_RESCUE: bool = false;

/// Protocol ISO timestamp → epoch seconds; unparseable → None (Python `_utc`).
fn utc(value: &str) -> Option<f64> {
    if value.is_empty() {
        return None;
    }
    crate::contracts::parse_utc(value).ok().map(|moment| epoch_seconds(&moment))
}

/// Per-tile remaining opportunities in the published windows: tonight's
/// `night_start.tile_windows` plus the lookahead `weekly.tile_windows`,
/// concatenated literally (a window listed in both counts twice — as does the
/// Python). Windows whose end is unparseable or already past do not count…
/// except that Python counts windows with an *unparseable* end (end=None never
/// compares `<= now`); preserved for fidelity, though shipped data always
/// parses.
fn remaining_chances(snapshot: &DecisionSnapshot, now: f64) -> HashMap<String, i64> {
    let mut counts: HashMap<String, i64> = HashMap::new();
    let weekly = snapshot.weekly.iter().flat_map(|weekly| weekly.tile_windows.iter());
    let tonight = snapshot
        .night_start
        .iter()
        .flat_map(|night_start| night_start.tile_windows.iter());
    for window in weekly.chain(tonight) {
        if let Some(end) = window.window_end() {
            if epoch_seconds(&end) <= now {
                continue; // a window already past is not a chance
            }
        }
        if !window.tile_id.is_empty() {
            *counts.entry(window.tile_id.clone()).or_insert(0) += 1;
        }
    }
    counts
}

/// Per-region shortfall toward the flexible quota (only regions present in
/// `progress.flexible_completed_by_region` appear — Python iterates that dict).
fn region_shortfall(snapshot: &DecisionSnapshot) -> HashMap<String, i64> {
    snapshot
        .progress
        .flexible_completed_by_region
        .iter()
        .map(|(region, count)| (region.clone(), (FLEXIBLE_QUOTA - count).max(0)))
        .collect()
}

/// Requests expiring within `EXPIRING_WITHIN_DAYS`. Simplification versus
/// Python, which probed five possible deadline keys in order: the platform
/// wire only ever carries `deadline_utc`, so the typed field is the whole
/// probe (the other keys never occur in real data; the gates verify
/// byte-identical decisions).
fn expiring_requests(snapshot: &DecisionSnapshot, now: f64) -> std::collections::HashSet<String> {
    let mut urgent = std::collections::HashSet::new();
    for request in &snapshot.active_requests {
        if request.request_id.is_empty() {
            continue;
        }
        let Some(deadline) = utc(&request.deadline_utc) else {
            continue;
        };
        if (deadline - now) / 86400.0 <= EXPIRING_WITHIN_DAYS {
            urgent.insert(request.request_id.clone());
        }
    }
    urgent
}

/// Jain fairness index gain from one more completion in `region`. Integer
/// counts throughout, divided once at the end — matching Python's int math.
fn evenness_gain(done_by_region: &[(String, i64)], region: &str, n_regions: i64) -> f64 {
    let counts: Vec<i64> = done_by_region.iter().map(|(_, count)| *count).collect();
    let total: i64 = counts.iter().sum();
    let squares: i64 = counts.iter().map(|value| value * value).sum();
    if total <= 0 {
        return 0.0;
    }
    let before = if squares != 0 {
        (total * total) as f64 / (n_regions * squares) as f64
    } else {
        0.0
    };
    let x = done_by_region
        .iter()
        .find(|(name, _)| name == region)
        .map(|(_, count)| *count)
        .unwrap_or(0);
    let after = ((total + 1) * (total + 1)) as f64 / (n_regions * (squares + 2 * x + 1)) as f64;
    after - before
}

/// The strategy's per-run memory (Python's `memory` dict): insertion-ordered
/// completed-counts per region plus the accumulated estimated science.
#[derive(Default)]
pub struct StrategyMemory {
    coverage: Vec<(String, i64)>,
    science: f64,
}

/// The reference strategy selector (`rust reference`).
#[derive(Default)]
pub struct ReferenceSelector {
    memory: StrategyMemory,
}

/// `" ".join(reason.split())[:240]` — the graph's reason normalization.
fn normalize_reason(reason: &str) -> String {
    let squashed: String = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    squashed.chars().take(240).collect()
}

impl Selector for ReferenceSelector {
    fn select(
        &mut self,
        previews: &[CandidatePreview],
        snapshot: &DecisionSnapshot,
        publication: &InitialPublication,
    ) -> Selection {
        // `choose_action` is only reached with a non-empty candidate list in the
        // Python graph (empty → deterministic wait before the strategy runs).
        let Some(fallback) = previews.first() else {
            return Selection::Wait {
                reason: "no legal observable candidate can finish in its known window".to_string(),
                source: "deterministic",
            };
        };
        let observe = |preview: &CandidatePreview, reason: String| Selection::Observe {
            tile_id: preview.tile_id.clone(),
            program: preview.program,
            request_id: preview.request_id.clone(),
            reason: normalize_reason(&reason),
            source: "strategy",
        };

        let now = epoch_seconds(&snapshot.cursor.timestamp_utc);
        let chances = remaining_chances(snapshot, now);
        let shortfall = region_shortfall(snapshot);
        let urgent_requests = expiring_requests(snapshot, now);

        // ① REQUIRED tile at risk — missing one costs 1000, the heaviest
        //    penalty, so it outranks everything.
        let mut at_risk: Vec<(i64, usize)> = previews
            .iter()
            .enumerate()
            .filter(|(_, candidate)| {
                candidate.scheduling_class.eq_ignore_ascii_case("REQUIRED")
                    && chances.get(&candidate.tile_id).copied().unwrap_or(99) <= LAST_CHANCES
            })
            .map(|(rank, candidate)| {
                (chances.get(&candidate.tile_id).copied().unwrap_or(99), rank)
            })
            .collect();
        if !at_risk.is_empty() {
            // Fewest chances first; ties follow the platform ranking.
            at_risk.sort_by_key(|(chances, rank)| (*chances, *rank));
            let (chances_left, rank) = at_risk[0];
            return observe(
                &previews[rank],
                format!("required tile with only {chances_left} window(s) left"),
            );
        }

        // ② Request expiring — a 330-point swing, larger than most single
        //    exposures. Off by default; see ENABLE_REQUEST_JUMP.
        if ENABLE_REQUEST_JUMP {
            for candidate in previews {
                if urgent_requests.contains(&candidate.request_id) {
                    return observe(
                        candidate,
                        "observation request expiring within a day".to_string(),
                    );
                }
            }
        }

        // ③ Region quota at risk — this FLEXIBLE tile is nearly out of chances
        //    and its region is still short of the quota. Off by default; see
        //    ENABLE_QUOTA_RESCUE.
        if ENABLE_QUOTA_RESCUE {
            for candidate in previews {
                if candidate.scheduling_class.eq_ignore_ascii_case("REQUIRED") {
                    continue;
                }
                if shortfall.get(&candidate.region_id).copied().unwrap_or(0) <= 0 {
                    continue;
                }
                if chances.get(&candidate.tile_id).copied().unwrap_or(99) <= LAST_CHANCES {
                    return observe(
                        candidate,
                        "last chance at a region still short of its flexible quota".to_string(),
                    );
                }
            }
        }

        // ④ Coverage evenness — worth about a fifth of the total in the
        //    competition scenario, the single most valuable account. Weight 0 in
        //    practice scenarios disables this rule automatically. The Python
        //    pipeline injects `score_config` into the snapshot before the
        //    strategy sees it; the typed model reads the publication's
        //    scoring contract directly (identical visibility).
        let weight = publication
            .scoring_contract
            .score_config
            .coverage_bonus_weight
            .unwrap_or(0.0);
        if weight > 0.0 {
            let memory = &mut self.memory;
            let science_so_far = memory.science;
            // Python: n_regions = max(1, len(done) or 8) — empty memory → 8.
            let n_regions = if memory.coverage.is_empty() {
                8
            } else {
                (memory.coverage.len() as i64).max(1)
            };
            let mut best: Option<(usize, f64)> = None;
            for (index, candidate) in previews.iter().enumerate() {
                let seconds = (candidate.nominal_exptime_seconds as f64).max(1.0);
                let mut value = candidate.estimated_total_gain;
                value += weight
                    * science_so_far
                    * evenness_gain(&memory.coverage, &candidate.region_id, n_regions);
                value /= seconds;
                // Strictly greater wins: the first candidate keeps ties.
                if best.map(|(_, best_value)| value > best_value).unwrap_or(true) {
                    best = Some((index, value));
                }
            }
            if let Some((index, _)) = best {
                let chosen = &previews[index];
                // Python updates memory at decision time — even if a later
                // pipeline override replaces the pick.
                let region = chosen.region_id.clone();
                if let Some(entry) =
                    memory.coverage.iter_mut().find(|(name, _)| *name == region)
                {
                    entry.1 += 1;
                } else {
                    memory.coverage.push((region, 1));
                }
                memory.science = science_so_far + chosen.estimated_science_score;
                return observe(
                    chosen,
                    "immediate gain plus what it does to coverage evenness".to_string(),
                );
            }
        }

        // ⑤ No future account is at risk: trust the platform's
        //    gain-per-second ranking — it already prices atmosphere, lunar
        //    penalty, science weight, and program bonus.
        observe(
            fallback,
            "platform ranking: highest estimated gain per second".to_string(),
        )
    }
}
