use bones_core::model::item::Urgency;
use serde::{Deserialize, Serialize};

const DECAY_WINDOW_DAYS: f64 = 14.0;

/// Raw metric values used to compute a composite priority score.
///
/// Metric fields are clamped to `[0, 1]` by [`composite_score`]. Callers can
/// pre-normalize per-metric vectors with [`normalize_metric`].
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct MetricInputs {
    pub critical_path: f64,
    pub pagerank: f64,
    pub betweenness: f64,
    pub urgency: Urgency,
    pub decay_days: f64,
}

/// Configurable weights for the composite formula:
///
/// `P(v) = alpha*CP + beta*PR + gamma*BC + delta*U + epsilon*D`
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct CompositeWeights<T = f64> {
    pub alpha: T,
    pub beta: T,
    pub gamma: T,
    pub delta: T,
    pub epsilon: T,
}

impl Default for CompositeWeights<f64> {
    fn default() -> Self {
        Self {
            alpha: 0.25,
            beta: 0.25,
            gamma: 0.20,
            delta: 0.15,
            epsilon: 0.15,
        }
    }
}

/// Compute composite priority score from normalized inputs.
///
/// - Returns `f64::MAX` when urgency is `Urgent`.
/// - Returns `f64::NEG_INFINITY` when urgency is `Punt`.
#[must_use]
pub fn composite_score(inputs: &MetricInputs, weights: &CompositeWeights) -> f64 {
    match inputs.urgency {
        Urgency::Urgent => return f64::MAX,
        Urgency::Punt => return f64::NEG_INFINITY,
        Urgency::Default => {}
    }

    let cp = normalize_unit(inputs.critical_path);
    let pr = normalize_unit(inputs.pagerank);
    let bc = normalize_unit(inputs.betweenness);
    let u = urgency_component(inputs.urgency);
    let d = decay_component(inputs.decay_days);

    (weights.alpha * cp)
        + (weights.beta * pr)
        + (weights.gamma * bc)
        + (weights.delta * u)
        + (weights.epsilon * d)
}

/// Min-max normalization that maps raw metric values to `[0, 1]`.
///
/// If all values are equal (including a single-element slice), all outputs are
/// `0.0`.
#[must_use]
pub fn normalize_metric(values: &[f64]) -> Vec<f64> {
    if values.is_empty() {
        return Vec::new();
    }

    let min = values.iter().copied().fold(f64::INFINITY, f64::min);
    let max = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
    let range = max - min;

    if !range.is_finite() || range.abs() <= f64::EPSILON {
        return vec![0.0; values.len()];
    }

    values
        .iter()
        .map(|&value| normalize_unit((value - min) / range))
        .collect()
}

fn normalize_unit(value: f64) -> f64 {
    if !value.is_finite() {
        return 0.0;
    }

    value.clamp(0.0, 1.0)
}

fn urgency_component(urgency: Urgency) -> f64 {
    match urgency {
        Urgency::Urgent => 1.0,
        Urgency::Default => 0.5,
        Urgency::Punt => 0.0,
    }
}

fn decay_component(days_in_doing: f64) -> f64 {
    if !days_in_doing.is_finite() {
        return 0.0;
    }

    normalize_unit(days_in_doing.max(0.0) / DECAY_WINDOW_DAYS)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn assert_approx_eq(actual: f64, expected: f64) {
        let tolerance = 1e-10;
        assert!(
            (actual - expected).abs() <= tolerance,
            "actual ({actual}) != expected ({expected})"
        );
    }

    #[test]
    fn normalize_metric_uses_min_max() {
        let normalized = normalize_metric(&[3.0, 1.0, 5.0]);
        assert_eq!(normalized.len(), 3);

        assert_approx_eq(normalized[0], 0.5);
        assert_approx_eq(normalized[1], 0.0);
        assert_approx_eq(normalized[2], 1.0);
    }

    #[test]
    fn normalize_metric_equal_values_returns_zeroes() {
        let normalized = normalize_metric(&[2.0, 2.0, 2.0]);
        assert_eq!(normalized, vec![0.0, 0.0, 0.0]);
    }

    #[test]
    fn normalize_metric_empty_returns_empty() {
        let normalized = normalize_metric(&[]);
        assert!(normalized.is_empty());
    }

    #[test]
    fn composite_score_returns_max_for_urgent() {
        let score = composite_score(
            &MetricInputs {
                critical_path: 0.1,
                pagerank: 0.2,
                betweenness: 0.3,
                urgency: Urgency::Urgent,
                decay_days: 9.0,
            },
            &CompositeWeights::default(),
        );

        assert_eq!(score, f64::MAX);
    }

    #[test]
    fn composite_score_returns_negative_infinity_for_punt() {
        let score = composite_score(
            &MetricInputs {
                critical_path: 0.9,
                pagerank: 0.8,
                betweenness: 0.7,
                urgency: Urgency::Punt,
                decay_days: 40.0,
            },
            &CompositeWeights::default(),
        );

        assert_eq!(score, f64::NEG_INFINITY);
    }

    #[test]
    fn composite_score_applies_weighted_sum() {
        let score = composite_score(
            &MetricInputs {
                critical_path: 0.8,
                pagerank: 0.4,
                betweenness: 0.6,
                urgency: Urgency::Default,
                decay_days: 7.0,
            },
            &CompositeWeights::default(),
        );

        // 0.25*0.8 + 0.25*0.4 + 0.20*0.6 + 0.15*0.5 + 0.15*0.5
        assert_approx_eq(score, 0.57);
    }

    #[test]
    fn composite_score_clamps_inputs_and_caps_decay() {
        let score = composite_score(
            &MetricInputs {
                critical_path: 5.0,
                pagerank: -1.0,
                betweenness: f64::NAN,
                urgency: Urgency::Default,
                decay_days: 45.0,
            },
            &CompositeWeights::default(),
        );

        // 0.25*1.0 + 0.25*0.0 + 0.20*0.0 + 0.15*0.5 + 0.15*1.0
        assert_approx_eq(score, 0.475);
    }

    #[test]
    fn composite_score_boosts_items_with_more_decay_days() {
        let baseline = composite_score(
            &MetricInputs {
                critical_path: 0.3,
                pagerank: 0.3,
                betweenness: 0.3,
                urgency: Urgency::Default,
                decay_days: 0.0,
            },
            &CompositeWeights::default(),
        );

        let boosted = composite_score(
            &MetricInputs {
                critical_path: 0.3,
                pagerank: 0.3,
                betweenness: 0.3,
                urgency: Urgency::Default,
                decay_days: 14.0,
            },
            &CompositeWeights::default(),
        );

        assert!(boosted > baseline);
    }
}
