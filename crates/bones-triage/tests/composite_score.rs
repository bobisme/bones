//! Composite score sanity tests: urgency overrides, punt exclusion, and
//! Thompson Sampling feedback-learning weight adaptation.
//!
//! # Acceptance Criteria
//!
//! - ✅ Urgency override verified for all ranking scenarios (urgent always scores f64::MAX)
//! - ✅ Punt exclusion verified (punt scores f64::NEG_INFINITY, filtered from results)
//! - ✅ Thompson Sampling weight adaptation verified directionally

use bones_core::model::item::Urgency;
use bones_core::model::item_id::ItemId;
use bones_triage::feedback::{AgentProfile, FeedbackAction, FeedbackEvent, update_from_feedback};
use bones_triage::score::{CompositeWeights, MetricInputs, composite_score};
use chrono::Utc;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn make_feedback_event(
    action: FeedbackAction,
    weights_used: CompositeWeights<f64>,
) -> FeedbackEvent {
    FeedbackEvent {
        timestamp: Utc::now(),
        agent_id: "test-agent".to_string(),
        item_id: ItemId::parse("bn-a1b").expect("valid item id"),
        action,
        composite_score: 0.75,
        weights_used,
    }
}

// ---------------------------------------------------------------------------
// Urgency Override Tests
// ---------------------------------------------------------------------------

/// Urgent item with minimal graph metrics must rank above a well-connected
/// non-urgent hub node.
#[test]
fn urgent_item_always_scores_highest() {
    // Item A: urgent=true, leaf node (no deps, low graph centrality)
    let a = MetricInputs {
        critical_path: 0.0,
        pagerank: 0.0,
        betweenness: 0.0,
        urgency: Urgency::Urgent,
        decay_days: 0.0,
    };

    // Item B: urgent=false, hub node (highest PageRank, highest betweenness)
    let b = MetricInputs {
        critical_path: 1.0,
        pagerank: 1.0,
        betweenness: 1.0,
        urgency: Urgency::Default,
        decay_days: 14.0,
    };

    let weights = CompositeWeights::default();
    let score_a = composite_score(&a, &weights);
    let score_b = composite_score(&b, &weights);

    assert_eq!(score_a, f64::MAX, "urgent item must return f64::MAX");
    assert!(
        score_a > score_b,
        "Urgent item A ({score_a}) must score above non-urgent hub B ({score_b})"
    );
}

/// Urgency override holds regardless of the weight configuration applied.
#[test]
fn urgent_item_beats_hub_across_all_weight_configs() {
    let urgent_leaf = MetricInputs {
        critical_path: 0.0,
        pagerank: 0.0,
        betweenness: 0.0,
        urgency: Urgency::Urgent,
        decay_days: 0.0,
    };

    let default_hub = MetricInputs {
        critical_path: 1.0,
        pagerank: 1.0,
        betweenness: 1.0,
        urgency: Urgency::Default,
        decay_days: 14.0,
    };

    // Three different weight configurations including extreme cases.
    let weight_configs = [
        CompositeWeights {
            alpha: 0.5,
            beta: 0.5,
            gamma: 0.0,
            delta: 0.0,
            epsilon: 0.0,
        },
        CompositeWeights {
            alpha: 0.0,
            beta: 0.0,
            gamma: 0.0,
            delta: 1.0,
            epsilon: 0.0,
        },
        CompositeWeights {
            alpha: 0.2,
            beta: 0.2,
            gamma: 0.2,
            delta: 0.2,
            epsilon: 0.2,
        },
    ];

    for weights in &weight_configs {
        let score_urgent = composite_score(&urgent_leaf, weights);
        let score_hub = composite_score(&default_hub, weights);
        assert!(
            score_urgent > score_hub,
            "Urgent item must beat hub under weights {weights:?}: got {score_urgent} vs {score_hub}"
        );
    }
}

/// High-urgency leaf with no deps must rank above a default-urgency hub that
/// has maximum graph centrality metrics.
#[test]
fn high_urgency_no_deps_beats_many_deps_default_urgency() {
    let a_leaf_urgent = MetricInputs {
        critical_path: 0.0,
        pagerank: 0.05,
        betweenness: 0.0,
        urgency: Urgency::Urgent,
        decay_days: 0.0,
    };

    let b_hub_default = MetricInputs {
        critical_path: 1.0,
        pagerank: 1.0,
        betweenness: 1.0,
        urgency: Urgency::Default,
        decay_days: 14.0,
    };

    let weights = CompositeWeights::default();
    let score_a = composite_score(&a_leaf_urgent, &weights);
    let score_b = composite_score(&b_hub_default, &weights);

    assert!(
        score_a > score_b,
        "Urgent leaf A ({score_a}) must rank above default hub B ({score_b})"
    );
}

// ---------------------------------------------------------------------------
// Punt Exclusion Tests
// ---------------------------------------------------------------------------

/// Punted item must return f64::NEG_INFINITY so it can be filtered from
/// any ranked list.
#[test]
fn punted_item_returns_negative_infinity() {
    let punt = MetricInputs {
        critical_path: 0.9,
        pagerank: 0.8,
        betweenness: 0.7,
        urgency: Urgency::Punt,
        decay_days: 0.0,
    };

    let score = composite_score(&punt, &CompositeWeights::default());

    assert_eq!(
        score,
        f64::NEG_INFINITY,
        "punted item must return NEG_INFINITY"
    );
    assert!(!score.is_finite(), "punted item score must not be finite");
}

/// Punted item must be absent from a simulated `bn next` ranked result list.
#[test]
fn punted_item_excluded_from_results() {
    // Items: A and C are normal, B is punted.
    let items = [
        (
            "A",
            MetricInputs {
                critical_path: 0.5,
                pagerank: 0.5,
                betweenness: 0.5,
                urgency: Urgency::Default,
                decay_days: 0.0,
            },
        ),
        (
            "B",
            MetricInputs {
                // B has top graph metrics but is punted — must never appear.
                critical_path: 0.9,
                pagerank: 0.9,
                betweenness: 0.9,
                urgency: Urgency::Punt,
                decay_days: 0.0,
            },
        ),
        (
            "C",
            MetricInputs {
                critical_path: 0.3,
                pagerank: 0.3,
                betweenness: 0.3,
                urgency: Urgency::Default,
                decay_days: 0.0,
            },
        ),
    ];

    let weights = CompositeWeights::default();

    // Simulate `bn next`: score all items, filter non-finite, sort descending.
    let ranked: Vec<&str> = {
        let mut scored: Vec<(&str, f64)> = items
            .iter()
            .map(|(id, m)| (*id, composite_score(m, &weights)))
            .filter(|(_, score)| score.is_finite())
            .collect();
        scored.sort_by(|x, y| y.1.partial_cmp(&x.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.iter().map(|(id, _)| *id).collect()
    };

    assert!(
        !ranked.contains(&"B"),
        "Punted item B should not appear in results, got: {ranked:?}"
    );
    assert!(
        ranked.contains(&"A"),
        "Normal item A should appear in results"
    );
    assert!(
        ranked.contains(&"C"),
        "Normal item C should appear in results"
    );
}

// ---------------------------------------------------------------------------
// Thompson Sampling: Feedback Learning Direction
// ---------------------------------------------------------------------------

/// After 100 `bn did` events for high-PageRank items, the PageRank weight
/// posterior (beta) should shift toward success — its alpha_param increases.
#[test]
fn bn_did_feedback_shifts_weights_toward_pagerank() {
    let mut profile = AgentProfile::new("test-agent");

    let initial_beta_alpha = profile.posteriors.beta.alpha_param;

    // beta (PageRank, field `beta` in CompositeWeights) is the dominant contributor.
    let weights_used = CompositeWeights {
        alpha: 0.1,
        beta: 0.9, // beta dominates — PageRank drove the selection
        gamma: 0.2,
        delta: 0.1,
        epsilon: 0.1,
    };

    for _ in 0..100 {
        let event = make_feedback_event(FeedbackAction::Did, weights_used);
        update_from_feedback(&mut profile, &event);
    }

    let updated_beta_alpha = profile.posteriors.beta.alpha_param;

    assert!(
        updated_beta_alpha > initial_beta_alpha,
        "PageRank weight (beta) alpha_param should increase after 100 'did' events: \
         {updated_beta_alpha} vs initial {initial_beta_alpha}"
    );

    // With a uniform Beta(1,1) prior and 100 successes, alpha_param = 1 + 100 = 101.
    assert_eq!(
        updated_beta_alpha, 101.0,
        "After 100 did events targeting beta, alpha_param should be 1 + 100 = 101"
    );

    // Non-dominant posteriors must remain at their initial prior values.
    assert_eq!(
        profile.posteriors.alpha.alpha_param, 1.0,
        "Non-dominant alpha posterior alpha_param should be unchanged"
    );
    assert_eq!(
        profile.posteriors.gamma.alpha_param, 1.0,
        "Non-dominant gamma posterior alpha_param should be unchanged"
    );
    assert_eq!(
        profile.posteriors.delta.alpha_param, 1.0,
        "Non-dominant delta posterior alpha_param should be unchanged"
    );
    assert_eq!(
        profile.posteriors.epsilon.alpha_param, 1.0,
        "Non-dominant epsilon posterior alpha_param should be unchanged"
    );
}

/// After 100 `bn skip` events for betweenness-central items, the betweenness
/// weight posterior (gamma) should shift toward failure — its beta_param increases.
#[test]
fn bn_skip_feedback_shifts_weights_away_from_betweenness() {
    let mut profile = AgentProfile::new("test-agent");

    let initial_gamma_beta = profile.posteriors.gamma.beta_param;

    // gamma (betweenness centrality) is the dominant contributor.
    let weights_used = CompositeWeights {
        alpha: 0.1,
        beta: 0.2,
        gamma: 0.9, // gamma dominates — betweenness drove selection but user skipped
        delta: 0.1,
        epsilon: 0.1,
    };

    for _ in 0..100 {
        let event = make_feedback_event(FeedbackAction::Skip, weights_used);
        update_from_feedback(&mut profile, &event);
    }

    let updated_gamma_beta = profile.posteriors.gamma.beta_param;

    assert!(
        updated_gamma_beta > initial_gamma_beta,
        "Betweenness weight (gamma) beta_param should increase after 100 'skip' events: \
         {updated_gamma_beta} vs initial {initial_gamma_beta}"
    );

    // With a uniform Beta(1,1) prior and 100 failures, beta_param = 1 + 100 = 101.
    assert_eq!(
        updated_gamma_beta, 101.0,
        "After 100 skip events targeting gamma, beta_param should be 1 + 100 = 101"
    );

    // Non-dominant posteriors must remain at their initial prior values.
    assert_eq!(
        profile.posteriors.alpha.beta_param, 1.0,
        "Non-dominant alpha posterior beta_param should be unchanged"
    );
    assert_eq!(
        profile.posteriors.beta.beta_param, 1.0,
        "Non-dominant beta posterior beta_param should be unchanged"
    );
    assert_eq!(
        profile.posteriors.delta.beta_param, 1.0,
        "Non-dominant delta posterior beta_param should be unchanged"
    );
    assert_eq!(
        profile.posteriors.epsilon.beta_param, 1.0,
        "Non-dominant epsilon posterior beta_param should be unchanged"
    );
}

// ---------------------------------------------------------------------------
// Score Stability
// ---------------------------------------------------------------------------

/// Computing scores twice on the exact same inputs must produce bit-identical
/// results — the scoring function is deterministic.
#[test]
fn unchanged_graph_produces_stable_scores() {
    let inputs = [
        MetricInputs {
            critical_path: 0.7,
            pagerank: 0.4,
            betweenness: 0.6,
            urgency: Urgency::Default,
            decay_days: 5.0,
        },
        MetricInputs {
            critical_path: 0.2,
            pagerank: 0.9,
            betweenness: 0.1,
            urgency: Urgency::Default,
            decay_days: 0.0,
        },
        MetricInputs {
            critical_path: 0.0,
            pagerank: 0.0,
            betweenness: 0.0,
            urgency: Urgency::Urgent,
            decay_days: 0.0,
        },
        MetricInputs {
            critical_path: 0.8,
            pagerank: 0.8,
            betweenness: 0.8,
            urgency: Urgency::Punt,
            decay_days: 7.0,
        },
    ];

    let weights = CompositeWeights::default();

    // Run scoring twice on the same unchanged inputs.
    let scores1: Vec<f64> = inputs
        .iter()
        .map(|m| composite_score(m, &weights))
        .collect();
    let scores2: Vec<f64> = inputs
        .iter()
        .map(|m| composite_score(m, &weights))
        .collect();

    assert_eq!(
        scores1, scores2,
        "Scores should be deterministically stable for unchanged input"
    );
}
