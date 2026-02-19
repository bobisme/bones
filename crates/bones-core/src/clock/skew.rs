use std::time::{SystemTime, UNIX_EPOCH};

/// Warning emitted when clock skew is detected.
#[derive(Debug, Clone)]
pub struct ClockSkewWarning {
    /// The event timestamp that triggered the warning.
    pub event_ts: u64,
    /// The current wall-clock time.
    pub wall_ts: u64,
    /// The detected skew in seconds (positive = event is in the future,
    /// negative = event is significantly in the past).
    pub skew_secs: i64,
    /// The threshold that was exceeded.
    pub threshold_secs: u64,
    /// Human-readable warning message.
    pub message: String,
}

/// Default skew threshold in seconds (5 minutes).
pub const DEFAULT_SKEW_THRESHOLD_SECS: u64 = 300;

/// Get the current wall-clock time as Unix epoch seconds.
pub fn wall_clock_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards")
        .as_secs()
}

/// Check for clock skew between an event's timestamp and the current wall time.
/// Returns Some(warning) if the absolute difference exceeds `threshold_secs`.
///
/// Called during event write (before appending to shard) to warn the user.
/// Events are NEVER rejected due to clock skew — ITC ordering is authoritative.
pub fn check_clock_skew(
    event_ts: u64,
    wall_ts: u64,
    threshold_secs: u64,
) -> Option<ClockSkewWarning> {
    let skew_secs = event_ts as i64 - wall_ts as i64;
    let abs_skew = skew_secs.abs() as u64;

    if abs_skew > threshold_secs {
        let direction = if skew_secs > 0 { "future" } else { "past" };
        let message = format!(
            "Clock skew detected: event is {} seconds in the {}, threshold is {} seconds",
            abs_skew, direction, threshold_secs
        );

        Some(ClockSkewWarning {
            event_ts,
            wall_ts,
            skew_secs,
            threshold_secs,
            message,
        })
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_no_skew() {
        let wall = 1000;
        let event = 1050;
        assert!(check_clock_skew(event, wall, 100).is_none());
    }

    #[test]
    fn test_future_skew() {
        let wall = 1000;
        let event = 1200;
        let warning = check_clock_skew(event, wall, 100).unwrap();
        assert_eq!(warning.skew_secs, 200);
        assert!(warning.message.contains("future"));
    }

    #[test]
    fn test_past_skew() {
        let wall = 1000;
        let event = 800;
        let warning = check_clock_skew(event, wall, 100).unwrap();
        assert_eq!(warning.skew_secs, -200);
        assert!(warning.message.contains("past"));
    }
}
