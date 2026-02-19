use std::cell::RefCell;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use serde_json::json;

/// Aggregated timing report across instrumented operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimingReport {
    /// Per-operation timing statistics.
    pub operations: Vec<OpTiming>,
}

/// Timing statistics for a single named operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpTiming {
    /// Human-readable operation name.
    pub name: String,
    /// 50th percentile latency.
    pub p50: Duration,
    /// 95th percentile latency.
    pub p95: Duration,
    /// 99th percentile latency.
    pub p99: Duration,
    /// Number of samples collected for this operation.
    pub count: usize,
}

#[derive(Debug, Clone)]
struct Sample {
    name: String,
    elapsed: Duration,
}

thread_local! {
    static SAMPLES: RefCell<Vec<Sample>> = const { RefCell::new(Vec::new()) };
}

static TIMING_ENABLED: AtomicBool = AtomicBool::new(false);

/// Returns true when `BONES_TIMING` enables timing collection.
///
/// Supported truthy values: `1`, `true`, `yes`, `on` (case-insensitive).
#[must_use]
pub fn timing_enabled_from_env() -> bool {
    std::env::var("BONES_TIMING")
        .ok()
        .is_some_and(|value| is_truthy(value.as_str()))
}

/// Enable or disable timing collection.
pub fn set_timing_enabled(enabled: bool) {
    TIMING_ENABLED.store(enabled, Ordering::Relaxed);
    if !enabled {
        clear_timings();
    }
}

/// Returns true when timing collection is currently enabled.
#[must_use]
pub fn is_timing_enabled() -> bool {
    TIMING_ENABLED.load(Ordering::Relaxed)
}

/// Clears all recorded timings for the current thread.
pub fn clear_timings() {
    SAMPLES.with(|samples| samples.borrow_mut().clear());
}

/// Execute a closure while recording its duration.
///
/// Timing is recorded only when enabled via [`set_timing_enabled`].
pub fn timed<R>(name: &str, f: impl FnOnce() -> R) -> R {
    if !is_timing_enabled() {
        return f();
    }

    let started = Instant::now();
    let result = f();
    record_sample(name, started.elapsed());
    result
}

/// Collect all recorded timings from thread-local storage into a report.
///
/// This drains the current thread's sample buffer.
#[must_use]
pub fn collect_report() -> TimingReport {
    let samples = SAMPLES.with(|samples| std::mem::take(&mut *samples.borrow_mut()));

    let mut grouped: BTreeMap<String, Vec<Duration>> = BTreeMap::new();
    for sample in samples {
        grouped.entry(sample.name).or_default().push(sample.elapsed);
    }

    let operations = grouped
        .into_iter()
        .map(|(name, mut values)| {
            values.sort_unstable();
            let count = values.len();

            OpTiming {
                name,
                p50: percentile(&values, 50),
                p95: percentile(&values, 95),
                p99: percentile(&values, 99),
                count,
            }
        })
        .collect();

    TimingReport { operations }
}

impl TimingReport {
    /// Returns true when no timing samples were recorded.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.operations.is_empty()
    }

    /// Render the timing report as JSON.
    #[must_use]
    pub fn to_json(&self) -> serde_json::Value {
        let operations = self
            .operations
            .iter()
            .map(|op| {
                json!({
                    "name": op.name,
                    "count": op.count,
                    "p50_us": op.p50.as_micros(),
                    "p95_us": op.p95.as_micros(),
                    "p99_us": op.p99.as_micros(),
                })
            })
            .collect::<Vec<_>>();

        json!({ "operations": operations })
    }

    /// Render the timing report as a simple table for terminal output.
    #[must_use]
    pub fn display_table(&self) -> String {
        if self.operations.is_empty() {
            return "No timing samples recorded.".to_string();
        }

        let mut out = String::new();
        out.push_str("operation                    count      p50      p95      p99\n");
        out.push_str("--------------------------------------------------------------\n");

        for op in &self.operations {
            out.push_str(&format!(
                "{:<28} {:>6} {:>8} {:>8} {:>8}\n",
                op.name,
                op.count,
                format_duration(op.p50),
                format_duration(op.p95),
                format_duration(op.p99)
            ));
        }

        out
    }
}

fn record_sample(name: &str, elapsed: Duration) {
    SAMPLES.with(|samples| {
        samples.borrow_mut().push(Sample {
            name: name.to_string(),
            elapsed,
        });
    });
}

fn percentile(sorted: &[Duration], pct: u32) -> Duration {
    if sorted.is_empty() {
        return Duration::ZERO;
    }

    let pct_usize = usize::try_from(pct).unwrap_or(100).min(100);
    let rank = pct_usize.saturating_mul(sorted.len()).saturating_add(99) / 100;
    let index = rank.saturating_sub(1).min(sorted.len().saturating_sub(1));

    sorted[index]
}

fn format_duration(duration: Duration) -> String {
    let micros = duration.as_micros();

    if micros >= 1_000_000 {
        let secs = micros / 1_000_000;
        let millis = (micros % 1_000_000) / 1_000;
        format!("{secs}.{millis:03}s")
    } else if micros >= 1_000 {
        let millis = micros / 1_000;
        let rem = micros % 1_000;
        format!("{millis}.{rem:03}ms")
    } else {
        format!("{micros}µs")
    }
}

fn is_truthy(value: &str) -> bool {
    value.eq_ignore_ascii_case("1")
        || value.eq_ignore_ascii_case("true")
        || value.eq_ignore_ascii_case("yes")
        || value.eq_ignore_ascii_case("on")
}

#[cfg(test)]
mod tests {
    use super::*;

    static TEST_GUARD: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn timed_does_not_record_when_disabled() {
        let _guard = TEST_GUARD.lock().expect("test guard lock");
        set_timing_enabled(false);
        clear_timings();

        let value = timed("disabled", || 7_u8);
        assert_eq!(value, 7);

        let report = collect_report();
        assert!(report.is_empty());
    }

    #[test]
    fn timed_records_when_enabled() {
        let _guard = TEST_GUARD.lock().expect("test guard lock");
        set_timing_enabled(true);
        clear_timings();

        let value = timed("enabled", || 42_u8);
        assert_eq!(value, 42);

        let report = collect_report();
        assert_eq!(report.operations.len(), 1);
        assert_eq!(report.operations[0].name, "enabled");
        assert_eq!(report.operations[0].count, 1);
        assert!(report.operations[0].p50 > Duration::ZERO);

        set_timing_enabled(false);
    }

    #[test]
    fn collect_report_groups_and_sorts_operations() {
        let _guard = TEST_GUARD.lock().expect("test guard lock");
        clear_timings();

        record_sample("query", Duration::from_micros(3_000));
        record_sample("query", Duration::from_micros(1_000));
        record_sample("query", Duration::from_micros(2_000));
        record_sample("replay", Duration::from_micros(5_000));

        let report = collect_report();
        assert_eq!(report.operations.len(), 2);

        let query = report
            .operations
            .iter()
            .find(|op| op.name == "query")
            .expect("query timing should exist");
        assert_eq!(query.count, 3);
        assert_eq!(query.p50, Duration::from_micros(2_000));
        assert_eq!(query.p95, Duration::from_micros(3_000));
        assert_eq!(query.p99, Duration::from_micros(3_000));
    }

    #[test]
    fn truthy_parser_is_case_insensitive() {
        let _guard = TEST_GUARD.lock().expect("test guard lock");

        assert!(is_truthy("TrUe"));
        assert!(is_truthy("1"));
        assert!(is_truthy("YES"));
        assert!(is_truthy("on"));
        assert!(!is_truthy("0"));
        assert!(!is_truthy("false"));
    }

    #[test]
    fn display_table_and_json_have_expected_fields() {
        let _guard = TEST_GUARD.lock().expect("test guard lock");
        clear_timings();

        record_sample("project", Duration::from_micros(1_500));

        let report = collect_report();
        let table = report.display_table();
        assert!(table.contains("operation"));
        assert!(table.contains("project"));

        let json = report.to_json();
        let operations = json
            .get("operations")
            .and_then(serde_json::Value::as_array)
            .expect("operations array should exist");
        assert_eq!(operations.len(), 1);

        let op = &operations[0];
        assert_eq!(
            op.get("name"),
            Some(&serde_json::Value::String("project".to_string()))
        );
        assert_eq!(op.get("count"), Some(&serde_json::Value::from(1)));
        assert!(op.get("p50_us").is_some());
    }
}
