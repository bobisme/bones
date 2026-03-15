//! Benchmark measuring ORT semantic model memory over time.
//!
//! Tests whether holding an ORT session and running periodic inference
//! causes RSS growth, as the ORT runtime has internal memory pools.

use anyhow::Result;
use bones_search::semantic::SemanticModel;
use std::time::Instant;

fn rss_mb() -> f64 {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: usize = statm
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    (pages * 4096) as f64 / (1024.0 * 1024.0)
}

fn main() -> Result<()> {
    let n_inferences: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(5000);

    eprintln!("=== ORT Semantic Model Memory Benchmark ===");
    eprintln!("Inferences: {n_inferences}");

    let rss_baseline = rss_mb();
    eprintln!("RSS before model load: {rss_baseline:.1} MB");

    let model = match SemanticModel::load() {
        Ok(m) => m,
        Err(e) => {
            eprintln!("Cannot load semantic model: {e}");
            eprintln!("Skipping ORT benchmark.");
            return Ok(());
        }
    };

    let rss_after_load = rss_mb();
    eprintln!("RSS after model load:  {rss_after_load:.1} MB (+{:.1} MB)",
        rss_after_load - rss_baseline);

    // Simulate the TUI's semantic search being triggered periodically.
    // In ward, users might type in the search box which triggers embed() calls.
    let queries = [
        "memory leak in tui rendering",
        "fix authentication bug",
        "implement caching for database queries",
        "refactor event handling pipeline",
        "add unit tests for parser module",
        "optimize build time",
        "update dependencies to latest versions",
        "debug intermittent test failure",
    ];

    let start = Instant::now();
    for i in 0..n_inferences {
        let query = queries[i % queries.len()];
        let _embedding = model.embed(query)?;

        if i % 1000 == 0 {
            let rss_now = rss_mb();
            eprintln!("  inference {i:>5}: RSS = {rss_now:.1} MB (+{:.1} from load)",
                rss_now - rss_after_load);
        }
    }

    let elapsed = start.elapsed();
    let rss_final = rss_mb();
    eprintln!();
    eprintln!("=== Results ===");
    eprintln!("Inferences: {n_inferences}");
    eprintln!("Total time: {elapsed:.2?}");
    eprintln!("Time per inference: {:.2?}", elapsed / n_inferences as u32);
    eprintln!("RSS after load:  {rss_after_load:.1} MB");
    eprintln!("RSS final:       {rss_final:.1} MB");
    eprintln!("RSS growth from inference: {:.1} MB", rss_final - rss_after_load);
    eprintln!("RSS total (model + inference): {:.1} MB", rss_final - rss_baseline);

    Ok(())
}
