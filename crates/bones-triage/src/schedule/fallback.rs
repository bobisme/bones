//! Constrained optimisation fallback scheduler for multi-agent assignment.
//!
//! Used when the Whittle indexability gate fires (e.g., dependency-graph
//! cycles). Provides greedy min-cost-style assignment with fairness and
//! anti-duplication guarantees.
//!
//! # Algorithm
//!
//! 1. Sort items by composite score descending.
//! 2. Assign each item to the **least-loaded** agent that has not previously
//!    skipped that item (history-aware).  If all agents have skipped the item,
//!    fall back to the globally least-loaded agent (anti-starvation).
//! 3. **Fairness**: when `items.len() >= agent_count`, every agent receives at
//!    least one item.  A configurable `max_load_skew` cap (default 1) limits
//!    how many more items than the average any single agent can carry.
//! 4. **Anti-duplicate**: each item appears in at most one assignment.
//!
//! # Regime Reporting
//!
//! The [`ScheduleRegime`] enum is used by `bn plan --explain` to surface which
//! scheduler (Whittle or fallback) was active and why.

use std::collections::{HashMap, HashSet};

// ---------------------------------------------------------------------------
// Public types
// ---------------------------------------------------------------------------

/// A single agent–item assignment produced by the scheduler.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Assignment {
    /// Zero-based index into the agent roster.
    pub agent_idx: usize,
    /// The work-item ID that was assigned.
    pub item_id: String,
}

/// Which scheduling regime was used, for `bn plan --explain`.
#[derive(Debug, Clone, PartialEq)]
pub enum ScheduleRegime {
    /// Whittle Index was used (normal path).
    Whittle {
        /// Aggregate indexability score (1.0 = fully indexable, 0.0 = not).
        indexability_score: f64,
    },
    /// Fallback constrained-optimisation scheduler was used.
    Fallback {
        /// Human-readable reason why Whittle was bypassed.
        reason: String,
    },
}

impl ScheduleRegime {
    /// Returns `true` if the Whittle regime is active.
    #[must_use]
    pub fn is_whittle(&self) -> bool {
        matches!(self, ScheduleRegime::Whittle { .. })
    }

    /// Returns `true` if the fallback regime is active.
    #[must_use]
    pub fn is_fallback(&self) -> bool {
        matches!(self, ScheduleRegime::Fallback { .. })
    }

    /// Short one-line description suitable for CLI output.
    #[must_use]
    pub fn explain(&self) -> String {
        match self {
            ScheduleRegime::Whittle { indexability_score } => {
                format!("Whittle Index (indexability score: {indexability_score:.3})")
            }
            ScheduleRegime::Fallback { reason } => {
                format!("Fallback scheduler — {reason}")
            }
        }
    }
}

/// Configuration for the fallback scheduler.
#[derive(Debug, Clone, PartialEq)]
pub struct FallbackConfig {
    /// Maximum number of extra items any agent may receive above the per-agent
    /// average (floor).  Prevents one agent from hoarding all the work.
    /// Default: `1`.
    pub max_load_skew: usize,
}

impl Default for FallbackConfig {
    fn default() -> Self {
        Self { max_load_skew: 1 }
    }
}

// ---------------------------------------------------------------------------
// Core assignment function
// ---------------------------------------------------------------------------

/// Assign work items to agents using a greedy constrained-optimisation
/// approach.
///
/// # Arguments
///
/// * `items` — Item IDs to assign. Duplicates are silently deduplicated.
/// * `agent_count` — Number of agents available. Must be ≥ 1.
/// * `scores` — Composite scores keyed by item ID. Missing items get `0.0`.
/// * `history` — Previously attempted assignments (agent_idx, item_id) that
///   were **not completed** (i.e., skipped). The scheduler avoids re-pairing
///   the same (agent, item) when possible.
///
/// # Returns
///
/// A `Vec<Assignment>` in score-descending order (highest-priority items
/// listed first). Every item appears **at most once**. When
/// `items.len() >= agent_count`, every agent receives at least one item.
///
/// # Panics
///
/// Panics if `agent_count == 0`.
#[must_use]
pub fn assign_fallback(
    items: &[String],
    agent_count: usize,
    scores: &HashMap<String, f64>,
    history: &[Assignment],
) -> Vec<Assignment> {
    assign_fallback_with_config(
        items,
        agent_count,
        scores,
        history,
        &FallbackConfig::default(),
    )
}

/// Like [`assign_fallback`] but accepts explicit [`FallbackConfig`].
#[must_use]
pub fn assign_fallback_with_config(
    items: &[String],
    agent_count: usize,
    scores: &HashMap<String, f64>,
    history: &[Assignment],
    config: &FallbackConfig,
) -> Vec<Assignment> {
    assert!(agent_count >= 1, "agent_count must be at least 1");

    // Deduplicate items while preserving the first occurrence order.
    let unique_items: Vec<String> = {
        let mut seen: HashSet<&str> = HashSet::new();
        items
            .iter()
            .filter(|id| seen.insert(id.as_str()))
            .cloned()
            .collect()
    };

    if unique_items.is_empty() {
        return Vec::new();
    }

    // Sort items by score descending; ties broken by item ID for determinism.
    let mut sorted: Vec<&str> = unique_items.iter().map(String::as_str).collect();
    sorted.sort_by(|&a, &b| {
        let sa = scores.get(a).copied().unwrap_or(0.0);
        let sb = scores.get(b).copied().unwrap_or(0.0);
        sb.partial_cmp(&sa)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.cmp(b))
    });

    // Build a skip-set from history: (agent_idx, item_id) pairs to avoid.
    let skip_set: HashSet<(usize, &str)> = history
        .iter()
        .filter(|a| a.agent_idx < agent_count)
        .map(|a| (a.agent_idx, a.item_id.as_str()))
        .collect();

    // Per-agent load counters.
    let mut load: Vec<usize> = vec![0; agent_count];

    // Compute per-agent max load cap: floor(items / agents) + max_load_skew.
    // This prevents any single agent from accumulating too much more than
    // their fair share.  We recompute after each assignment.
    let total_items = sorted.len();

    let mut assignments: Vec<Assignment> = Vec::with_capacity(total_items);

    for &item_id in &sorted {
        // Preferred agent: least-loaded among those who haven't skipped this item.
        let preferred = pick_agent(&load, agent_count, config, total_items, |ag_idx| {
            !skip_set.contains(&(ag_idx, item_id))
        });

        // If no preferred agent found (all have skipped or all are at cap),
        // fall back to absolute least-loaded without the skip filter.
        let agent_idx = preferred.unwrap_or_else(|| {
            pick_agent(&load, agent_count, config, total_items, |_| true)
                .unwrap_or(least_loaded_agent(&load))
        });

        load[agent_idx] += 1;
        assignments.push(Assignment {
            agent_idx,
            item_id: item_id.to_string(),
        });
    }

    // Fairness pass: ensure every agent has at least one item when
    // items >= agent_count. Steal the last (lowest-priority) item from the
    // most-loaded agent and re-assign it to any starved agent.
    if total_items >= agent_count {
        enforce_fairness(&mut assignments, &mut load, agent_count, scores);
    }

    assignments
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

/// Pick the best agent index subject to a predicate and load cap.
///
/// Returns `None` if no agent satisfies the predicate within the cap.
fn pick_agent(
    load: &[usize],
    agent_count: usize,
    config: &FallbackConfig,
    total_items: usize,
    predicate: impl Fn(usize) -> bool,
) -> Option<usize> {
    // Fair-share cap: base per-agent allocation + skew allowance.
    let base = total_items / agent_count;
    let cap = base + config.max_load_skew;

    (0..agent_count)
        .filter(|&ag| predicate(ag) && load[ag] < cap)
        .min_by_key(|&ag| load[ag])
}

/// Return the index of the least-loaded agent (ties broken by lowest index).
fn least_loaded_agent(load: &[usize]) -> usize {
    load.iter()
        .enumerate()
        .min_by_key(|&(_, &l)| l)
        .map(|(idx, _)| idx)
        .unwrap_or(0)
}

/// Enforce fairness: steal the lowest-priority item from over-loaded agents
/// and give it to any agent with zero items, when items >= agent_count.
fn enforce_fairness(
    assignments: &mut Vec<Assignment>,
    load: &mut Vec<usize>,
    agent_count: usize,
    scores: &HashMap<String, f64>,
) {
    for starved_agent in 0..agent_count {
        if load[starved_agent] > 0 {
            continue;
        }

        // Find the most-loaded agent with more than 1 item (has items to spare).
        let donor = (0..agent_count)
            .filter(|&ag| load[ag] > 1)
            .max_by_key(|&ag| load[ag]);

        let Some(donor_idx) = donor else {
            break; // Cannot fix starvation — not enough items to redistribute.
        };

        // Find the lowest-scoring assignment belonging to the donor.
        let steal_pos = assignments
            .iter()
            .enumerate()
            .filter(|(_, a)| a.agent_idx == donor_idx)
            .min_by(|(_, a1), (_, a2)| {
                let s1 = scores.get(a1.item_id.as_str()).copied().unwrap_or(0.0);
                let s2 = scores.get(a2.item_id.as_str()).copied().unwrap_or(0.0);
                s1.partial_cmp(&s2)
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then_with(|| a2.item_id.cmp(&a1.item_id))
            })
            .map(|(pos, _)| pos);

        if let Some(pos) = steal_pos {
            load[donor_idx] -= 1;
            load[starved_agent] += 1;
            assignments[pos].agent_idx = starved_agent;
        }
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn scores(pairs: &[(&str, f64)]) -> HashMap<String, f64> {
        pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect()
    }

    fn items(ids: &[&str]) -> Vec<String> {
        ids.iter().map(|s| s.to_string()).collect()
    }

    fn history(pairs: &[(usize, &str)]) -> Vec<Assignment> {
        pairs
            .iter()
            .map(|(ag, id)| Assignment {
                agent_idx: *ag,
                item_id: id.to_string(),
            })
            .collect()
    }

    // -----------------------------------------------------------------------
    // Basic assignment
    // -----------------------------------------------------------------------

    #[test]
    fn assigns_single_item_to_single_agent() {
        let s = scores(&[("bn-a", 5.0)]);
        let result = assign_fallback(&items(&["bn-a"]), 1, &s, &[]);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].agent_idx, 0);
        assert_eq!(result[0].item_id, "bn-a");
    }

    #[test]
    fn assigns_multiple_items_to_multiple_agents() {
        let s = scores(&[("bn-a", 3.0), ("bn-b", 5.0), ("bn-c", 1.0)]);
        let result = assign_fallback(&items(&["bn-a", "bn-b", "bn-c"]), 2, &s, &[]);

        assert_eq!(result.len(), 3);
        // All items assigned.
        let assigned: HashSet<&str> = result.iter().map(|a| a.item_id.as_str()).collect();
        assert!(assigned.contains("bn-a"));
        assert!(assigned.contains("bn-b"));
        assert!(assigned.contains("bn-c"));
    }

    #[test]
    fn highest_score_assigned_first() {
        // bn-b has highest score; it should be the first assignment.
        let s = scores(&[("bn-a", 3.0), ("bn-b", 9.0), ("bn-c", 1.0)]);
        let result = assign_fallback(&items(&["bn-a", "bn-b", "bn-c"]), 2, &s, &[]);

        assert_eq!(result[0].item_id, "bn-b", "highest score first");
    }

    #[test]
    fn empty_items_returns_empty() {
        let s = scores(&[]);
        let result = assign_fallback(&[], 3, &s, &[]);
        assert!(result.is_empty());
    }

    #[test]
    fn single_agent_gets_all_items() {
        let s = scores(&[("bn-a", 2.0), ("bn-b", 5.0), ("bn-c", 1.0)]);
        let result = assign_fallback(&items(&["bn-a", "bn-b", "bn-c"]), 1, &s, &[]);

        assert_eq!(result.len(), 3);
        assert!(result.iter().all(|a| a.agent_idx == 0));
    }

    // -----------------------------------------------------------------------
    // Anti-duplicate
    // -----------------------------------------------------------------------

    #[test]
    fn no_item_assigned_twice() {
        let s = scores(&[("bn-a", 1.0), ("bn-b", 2.0), ("bn-c", 3.0)]);
        let result = assign_fallback(&items(&["bn-a", "bn-b", "bn-c"]), 2, &s, &[]);

        let ids: Vec<&str> = result.iter().map(|a| a.item_id.as_str()).collect();
        let unique: HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(ids.len(), unique.len(), "no item appears twice");
    }

    #[test]
    fn duplicate_input_items_deduplicated() {
        let s = scores(&[("bn-a", 5.0)]);
        let result = assign_fallback(&items(&["bn-a", "bn-a", "bn-a"]), 2, &s, &[]);
        // Only one unique item, so only one assignment.
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].item_id, "bn-a");
    }

    // -----------------------------------------------------------------------
    // Fairness constraint
    // -----------------------------------------------------------------------

    #[test]
    fn fairness_every_agent_gets_one_item_when_items_gte_agents() {
        // 3 items, 3 agents → each agent gets exactly 1 item.
        let s = scores(&[("bn-a", 3.0), ("bn-b", 5.0), ("bn-c", 1.0)]);
        let result = assign_fallback(&items(&["bn-a", "bn-b", "bn-c"]), 3, &s, &[]);

        assert_eq!(result.len(), 3);
        let mut per_agent = vec![0usize; 3];
        for a in &result {
            per_agent[a.agent_idx] += 1;
        }
        for (ag, &count) in per_agent.iter().enumerate() {
            assert_eq!(count, 1, "agent {ag} should have exactly 1 item");
        }
    }

    #[test]
    fn fairness_no_agent_starved_with_four_items_three_agents() {
        // 4 items, 3 agents → all agents get at least 1.
        let s = scores(&[("bn-a", 4.0), ("bn-b", 3.0), ("bn-c", 2.0), ("bn-d", 1.0)]);
        let result = assign_fallback(&items(&["bn-a", "bn-b", "bn-c", "bn-d"]), 3, &s, &[]);

        let mut per_agent = vec![0usize; 3];
        for a in &result {
            per_agent[a.agent_idx] += 1;
        }
        for (ag, &count) in per_agent.iter().enumerate() {
            assert!(
                count >= 1,
                "agent {ag} should have at least 1 item (got {count})"
            );
        }
    }

    #[test]
    fn fairness_ok_when_items_less_than_agents() {
        // 2 items, 3 agents → 2 assignments (not all agents get work, that's OK).
        let s = scores(&[("bn-a", 5.0), ("bn-b", 3.0)]);
        let result = assign_fallback(&items(&["bn-a", "bn-b"]), 3, &s, &[]);

        assert_eq!(result.len(), 2);
    }

    // -----------------------------------------------------------------------
    // History / anti-starvation
    // -----------------------------------------------------------------------

    #[test]
    fn history_avoids_previous_skip_assignment() {
        // Agent 0 previously skipped bn-a. bn-a should go to agent 1.
        let s = scores(&[("bn-a", 5.0), ("bn-b", 3.0)]);
        let h = history(&[(0, "bn-a")]);

        let result = assign_fallback(&items(&["bn-a", "bn-b"]), 2, &s, &h);

        let bn_a = result.iter().find(|a| a.item_id == "bn-a").unwrap();
        assert_eq!(
            bn_a.agent_idx, 1,
            "bn-a should not go to agent 0 (who skipped it)"
        );
    }

    #[test]
    fn history_falls_back_when_all_agents_skipped() {
        // Both agents skipped bn-a — scheduler must still assign it (no panic).
        let s = scores(&[("bn-a", 5.0)]);
        let h = history(&[(0, "bn-a"), (1, "bn-a")]);

        let result = assign_fallback(&items(&["bn-a"]), 2, &s, &h);

        assert_eq!(result.len(), 1);
        assert_eq!(result[0].item_id, "bn-a");
    }

    #[test]
    fn history_with_unknown_agent_idx_is_ignored() {
        // agent_idx = 99 is out of range for a 2-agent run — should not panic.
        let s = scores(&[("bn-a", 5.0)]);
        let h = history(&[(99, "bn-a")]);

        let result = assign_fallback(&items(&["bn-a"]), 2, &s, &h);
        assert_eq!(result.len(), 1);
    }

    // -----------------------------------------------------------------------
    // ScheduleRegime
    // -----------------------------------------------------------------------

    #[test]
    fn regime_whittle_explain() {
        let r = ScheduleRegime::Whittle {
            indexability_score: 0.95,
        };
        assert!(r.is_whittle());
        assert!(!r.is_fallback());
        let s = r.explain();
        assert!(s.contains("Whittle"), "explain: {s}");
        assert!(s.contains("0.950"), "explain: {s}");
    }

    #[test]
    fn regime_fallback_explain() {
        let r = ScheduleRegime::Fallback {
            reason: "dependency cycle detected".to_string(),
        };
        assert!(r.is_fallback());
        assert!(!r.is_whittle());
        let s = r.explain();
        assert!(s.contains("Fallback"), "explain: {s}");
        assert!(s.contains("dependency cycle"), "explain: {s}");
    }

    // -----------------------------------------------------------------------
    // Determinism
    // -----------------------------------------------------------------------

    #[test]
    fn assignment_is_deterministic() {
        let s = scores(&[("bn-a", 5.0), ("bn-b", 5.0), ("bn-c", 5.0)]);
        let result1 = assign_fallback(&items(&["bn-a", "bn-b", "bn-c"]), 2, &s, &[]);
        let result2 = assign_fallback(&items(&["bn-a", "bn-b", "bn-c"]), 2, &s, &[]);

        let r1: Vec<(&str, usize)> = result1
            .iter()
            .map(|a| (a.item_id.as_str(), a.agent_idx))
            .collect();
        let r2: Vec<(&str, usize)> = result2
            .iter()
            .map(|a| (a.item_id.as_str(), a.agent_idx))
            .collect();
        assert_eq!(r1, r2, "assignment must be deterministic");
    }

    // -----------------------------------------------------------------------
    // Missing score defaults
    // -----------------------------------------------------------------------

    #[test]
    fn missing_score_defaults_to_zero() {
        let s = scores(&[]);
        let result = assign_fallback(&items(&["bn-a", "bn-b"]), 2, &s, &[]);
        assert_eq!(result.len(), 2);
    }
}
