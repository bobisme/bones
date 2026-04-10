//! Benchmark measuring TUI memory behavior during simulated tick cycles.
//!
//! Since bones-cli is a binary crate, we exercise the exact same code paths
//! the TUI uses: query::list_items, query::get_comments, query::get_labels,
//! and pulldown_cmark markdown rendering (same as tui::markdown).
//!
//! Allocator A/B testing:
//! ```bash
//! cargo bench --bench tui_memory                          # system allocator
//! cargo bench --bench tui_memory --features jemalloc      # tikv-jemallocator
//! cargo bench --bench tui_memory --features mimalloc      # mimalloc
//! ```
//! Feature-gated `#[global_allocator]` must live in the bench binary too —
//! benches are separate compilation units and do not inherit the allocator
//! selected by `crates/bones-cli/src/main.rs`.

#[cfg(feature = "jemalloc")]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

#[cfg(feature = "mimalloc")]
#[global_allocator]
static ALLOC: mimalloc::MiMalloc = mimalloc::MiMalloc;

#[cfg(feature = "jemalloc")]
const ALLOCATOR_NAME: &str = "jemalloc";
#[cfg(feature = "mimalloc")]
const ALLOCATOR_NAME: &str = "mimalloc";
#[cfg(not(any(feature = "jemalloc", feature = "mimalloc")))]
const ALLOCATOR_NAME: &str = "system";

use anyhow::Result;
use bones_core::db::query::{self, ItemFilter, SortOrder};
use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use rusqlite::Connection;
use std::time::Instant;

/// Read current RSS from /proc/self/statm (Linux-specific).
fn rss_bytes() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: usize = statm
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    pages * 4096
}

fn rss_mb() -> f64 {
    rss_bytes() as f64 / (1024.0 * 1024.0)
}

/// Simulate what detail_lines() does: parse markdown from every comment into
/// owned String spans. This is the exact allocation pattern the TUI uses.
fn simulate_detail_lines(comments: &[(String, String, i64)]) -> Vec<String> {
    let mut lines = Vec::new();
    for (author, body, _ts) in comments {
        lines.push(format!("{author}  2025-01-15 10:30:00"));
        // Parse markdown into spans (same as tui::markdown::markdown_to_lines)
        let options = Options::ENABLE_STRIKETHROUGH;
        let parser = Parser::new_ext(body, options);
        for event in parser {
            match event {
                Event::Text(text) => lines.push(text.to_string()),
                Event::Code(code) => lines.push(code.to_string()),
                Event::SoftBreak | Event::HardBreak => lines.push(String::new()),
                Event::Start(Tag::Heading { .. }) => {}
                Event::End(TagEnd::Heading(_)) => lines.push(String::new()),
                Event::End(TagEnd::Paragraph) => lines.push(String::new()),
                Event::Start(Tag::CodeBlock(_)) => {}
                Event::End(TagEnd::CodeBlock) => lines.push(String::new()),
                _ => {}
            }
        }
    }
    lines
}

/// Create a synthetic bones.db with `n_items` bones, each having `comments_per_item` comments.
fn create_synthetic_db(
    path: &std::path::Path,
    n_items: usize,
    comments_per_item: usize,
) -> Result<()> {
    let conn = Connection::open(path)?;

    conn.execute_batch(
        "CREATE TABLE IF NOT EXISTS items (
            item_id TEXT PRIMARY KEY,
            title TEXT NOT NULL,
            description TEXT,
            kind TEXT NOT NULL,
            state TEXT NOT NULL,
            urgency TEXT NOT NULL DEFAULT 'default',
            size TEXT,
            parent_id TEXT,
            compact_summary TEXT,
            snapshot_json TEXT,
            is_deleted INTEGER NOT NULL DEFAULT 0,
            deleted_at_us INTEGER,
            search_labels TEXT NOT NULL DEFAULT '',
            created_at_us INTEGER NOT NULL,
            updated_at_us INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS item_labels (
            item_id TEXT NOT NULL,
            label TEXT NOT NULL,
            created_at_us INTEGER NOT NULL,
            PRIMARY KEY (item_id, label)
        );
        CREATE TABLE IF NOT EXISTS item_assignees (
            item_id TEXT NOT NULL,
            agent TEXT NOT NULL,
            created_at_us INTEGER NOT NULL,
            PRIMARY KEY (item_id, agent)
        );
        CREATE TABLE IF NOT EXISTS item_dependencies (
            item_id TEXT NOT NULL,
            depends_on_item_id TEXT NOT NULL,
            link_type TEXT NOT NULL,
            created_at_us INTEGER NOT NULL,
            PRIMARY KEY (item_id, depends_on_item_id, link_type)
        );
        CREATE TABLE IF NOT EXISTS item_comments (
            comment_id INTEGER PRIMARY KEY AUTOINCREMENT,
            item_id TEXT NOT NULL,
            event_hash TEXT NOT NULL UNIQUE,
            author TEXT NOT NULL,
            body TEXT NOT NULL,
            created_at_us INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS projection_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            schema_version INTEGER NOT NULL,
            last_event_offset INTEGER NOT NULL DEFAULT 0,
            last_event_hash TEXT,
            last_rebuild_at_us INTEGER NOT NULL DEFAULT 0
        );
        CREATE TABLE IF NOT EXISTS item_embeddings (
            item_id TEXT PRIMARY KEY,
            embedding BLOB NOT NULL,
            text_hash TEXT NOT NULL,
            model_version TEXT NOT NULL,
            updated_at_us INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS semantic_meta (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            model_name TEXT NOT NULL,
            embedding_dim INTEGER NOT NULL,
            updated_at_us INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS event_redactions (
            target_event_hash TEXT PRIMARY KEY,
            item_id TEXT NOT NULL,
            reason TEXT NOT NULL,
            redacted_by TEXT NOT NULL,
            redacted_at_us INTEGER NOT NULL
        );
        CREATE TABLE IF NOT EXISTS projected_events (
            rowid INTEGER PRIMARY KEY,
            event_offset INTEGER NOT NULL UNIQUE,
            event_hash TEXT NOT NULL UNIQUE
        );
        CREATE INDEX IF NOT EXISTS idx_items_state_urgency_updated ON items(state, urgency, updated_at_us DESC);
        CREATE INDEX IF NOT EXISTS idx_item_comments_item_created ON item_comments(item_id, created_at_us DESC);
        INSERT OR IGNORE INTO projection_meta (id, schema_version, last_event_offset, last_rebuild_at_us)
        VALUES (1, 9, 0, 0);",
    )?;

    let base_ts = 1_700_000_000_000_000i64;
    let mut stmt = conn.prepare(
        "INSERT INTO items (item_id, title, description, kind, state, urgency, created_at_us, updated_at_us)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
    )?;
    for i in 0..n_items {
        let id = format!("bn-bench{i:04}");
        let title = format!("Benchmark bone {i}: implement feature with detailed requirements");
        let desc = format!(
            "## Overview\n\nThis bone tracks feature {i}.\n\n\
             ### Requirements\n\n- First with **bold**\n- Second with `code`\n\
             - Third\n\n```rust\nfn example() {{\n    println!(\"hello\");\n}}\n```"
        );
        let state = if i % 5 == 0 {
            "done"
        } else if i % 3 == 0 {
            "doing"
        } else {
            "open"
        };
        let ts = base_ts + (i as i64) * 1_000_000;
        stmt.execute(rusqlite::params![
            id,
            title,
            desc,
            "task",
            state,
            "default",
            ts,
            ts + 60_000_000
        ])?;
    }

    let comment_templates = [
        "Starting work on this bone. Will investigate the codebase first.\n\n## Plan\n\n1. Read existing code\n2. Identify changes needed\n3. Implement\n4. Test",
        "**Progress update**: Found the relevant files:\n\n- `src/main.rs` - entry point\n- `src/lib.rs` - core logic\n- `src/utils.rs` - helpers\n\n```rust\nfn process(input: &str) -> Result<Output> {\n    let parsed = parse(input)?;\n    transform(parsed)\n}\n```",
        "Ran into an issue with the `transform` function. The current implementation doesn't handle edge cases:\n\n> The input validation assumes ASCII-only strings, but we need UTF-8 support.\n\nWorking on a fix now.",
        "Fixed the UTF-8 issue. Here's the diff summary:\n\n- Changed `str::len()` to `str::chars().count()` in 3 places\n- Added tests for multibyte characters\n- Updated documentation\n\n```diff\n- let len = input.len();\n+ let len = input.chars().count();\n```",
        "All tests passing. Running `cargo test` output:\n\n```\nrunning 47 tests\ntest test_ascii ... ok\ntest test_utf8 ... ok\ntest test_empty ... ok\n...\ntest result: ok. 47 passed; 0 failed\n```\n\nReady for review.",
    ];

    let mut comment_stmt = conn.prepare(
        "INSERT INTO item_comments (item_id, event_hash, author, body, created_at_us)
         VALUES (?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut hash_counter = 0u64;
    for i in 0..n_items {
        let id = format!("bn-bench{i:04}");
        for j in 0..comments_per_item {
            let body = comment_templates[j % comment_templates.len()];
            let ts = base_ts + (i as i64) * 1_000_000 + (j as i64) * 100_000;
            let hash = format!("hash-{hash_counter:012x}");
            hash_counter += 1;
            let author = if j % 2 == 0 {
                "ward-dev"
            } else {
                "ward-worker-1"
            };
            comment_stmt.execute(rusqlite::params![id, hash, author, body, ts])?;
        }
    }

    let mut label_stmt = conn
        .prepare("INSERT INTO item_labels (item_id, label, created_at_us) VALUES (?1, ?2, ?3)")?;
    for i in 0..n_items {
        let id = format!("bn-bench{i:04}");
        let ts = base_ts + (i as i64) * 1_000_000;
        if i % 3 == 0 {
            label_stmt.execute(rusqlite::params![id, "backend", ts])?;
        }
        if i % 4 == 0 {
            label_stmt.execute(rusqlite::params![id, "perf", ts])?;
        }
    }

    drop(stmt);
    drop(comment_stmt);
    drop(label_stmt);
    conn.close().map_err(|(_, e)| e)?;
    Ok(())
}

fn main() -> Result<()> {
    // Parse args: tui_memory [n_items] [comments_per_item]
    let args: Vec<String> = std::env::args().collect();
    let n_items: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(750);
    let comments_per_item: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(15);

    let tmp = tempfile::TempDir::new()?;
    let bones_dir = tmp.path().join(".bones");
    std::fs::create_dir_all(&bones_dir)?;
    let db_path = bones_dir.join("bones.db");

    eprintln!("=== TUI Memory Benchmark ===");
    eprintln!("Allocator: {ALLOCATOR_NAME}");
    eprintln!("Items: {n_items}, Comments/item: {comments_per_item}");
    eprintln!("Total comments: {}", n_items * comments_per_item);
    eprintln!();

    create_synthetic_db(&db_path, n_items, comments_per_item)?;
    let db_size = std::fs::metadata(&db_path)?.len();
    eprintln!("DB size: {:.1} MB", db_size as f64 / (1024.0 * 1024.0));

    let rss_baseline = rss_mb();
    eprintln!("RSS baseline: {rss_baseline:.1} MB");
    eprintln!();

    // =========================================================================
    // BENCHMARK 1: Full reload cycle (list_items + get_labels for all items)
    // This is what tick() -> reload() does every 2 seconds
    // =========================================================================
    eprintln!("=== Benchmark 1: reload() — list_items + get_labels ===");
    let n_reloads = 500;
    let rss_before = rss_mb();
    let start = Instant::now();

    for i in 0..n_reloads {
        let conn = Connection::open(&db_path)?;
        let filter = ItemFilter {
            include_deleted: false,
            sort: SortOrder::UpdatedDesc,
            ..Default::default()
        };
        let raw_items = query::list_items(&conn, &filter)?;
        // Simulate WorkItem construction with label loading (what reload does)
        let _work_items: Vec<_> = raw_items
            .into_iter()
            .map(|qi| {
                let labels: Vec<String> = query::get_labels(&conn, &qi.item_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|l| l.label)
                    .collect();
                (qi.item_id, qi.title, qi.state, labels)
            })
            .collect();

        if i % 100 == 0 {
            eprintln!("  reload {i:>4}: RSS = {:.1} MB", rss_mb());
        }
    }

    let reload_time = start.elapsed();
    let rss_after_reload = rss_mb();
    eprintln!(
        "Time: {reload_time:.2?} ({:.2?}/iter)",
        reload_time / n_reloads
    );
    eprintln!(
        "RSS: {rss_before:.1} -> {rss_after_reload:.1} MB (+{:.1} MB)",
        rss_after_reload - rss_before
    );
    eprintln!();

    // =========================================================================
    // BENCHMARK 2: load_detail_item — loads all comments for selected bone
    // Called every 2s in refresh_selected_detail when detail pane is open
    // =========================================================================
    eprintln!("=== Benchmark 2: load_detail_item — comment loading ===");
    let target_id = "bn-bench0000";
    let n_detail_loads = 500;
    let rss_before = rss_mb();
    let start = Instant::now();

    for i in 0..n_detail_loads {
        let conn = Connection::open(&db_path)?;
        let _item = query::get_item(&conn, target_id, false)?;
        let _labels = query::get_labels(&conn, target_id)?;
        let _assignees = query::get_assignees(&conn, target_id)?;
        let _deps = query::get_dependencies(&conn, target_id)?;
        let _dependents = query::get_dependents(&conn, target_id)?;
        // This is the big one — loads ALL comments with no limit
        let comments: Vec<_> = query::get_comments(&conn, target_id, None, None)?
            .into_iter()
            .map(|c| (c.author, c.body, c.created_at_us))
            .collect();

        if i % 100 == 0 {
            eprintln!(
                "  detail {i:>4}: RSS = {:.1} MB, comments = {}",
                rss_mb(),
                comments.len()
            );
        }
    }

    let detail_time = start.elapsed();
    let rss_after_detail = rss_mb();
    eprintln!(
        "Time: {detail_time:.2?} ({:.2?}/iter)",
        detail_time / n_detail_loads
    );
    eprintln!(
        "RSS: {rss_before:.1} -> {rss_after_detail:.1} MB (+{:.1} MB)",
        rss_after_detail - rss_before
    );
    eprintln!();

    // =========================================================================
    // BENCHMARK 3: detail_lines — markdown rendering of all comments
    // Called in max_detail_scroll (every clamp) AND render (every frame)
    // With 100ms poll rate = 10 calls/sec, plus extra in clamp_detail_scroll
    // =========================================================================
    eprintln!("=== Benchmark 3: detail_lines — markdown rendering ===");
    let conn = Connection::open(&db_path)?;
    let comments: Vec<_> = query::get_comments(&conn, target_id, None, None)?
        .into_iter()
        .map(|c| (c.author, c.body, c.created_at_us))
        .collect();
    drop(conn);

    let n_renders = 5000; // ~500s at 10/sec
    let rss_before = rss_mb();
    let start = Instant::now();

    for i in 0..n_renders {
        let lines = simulate_detail_lines(&comments);
        // Simulate what max_detail_scroll does: measure line widths
        let _total: usize = lines
            .iter()
            .map(|l| l.chars().count().max(1).div_ceil(80))
            .sum();

        if i % 1000 == 0 {
            eprintln!(
                "  render {i:>5}: RSS = {:.1} MB, lines = {}",
                rss_mb(),
                lines.len()
            );
        }
    }

    let render_time = start.elapsed();
    let rss_after_render = rss_mb();
    eprintln!(
        "Time: {render_time:.2?} ({:.2?}/iter)",
        render_time / n_renders
    );
    eprintln!(
        "RSS: {rss_before:.1} -> {rss_after_render:.1} MB (+{:.1} MB)",
        rss_after_render - rss_before
    );
    eprintln!();

    // =========================================================================
    // BENCHMARK 4: Combined tick cycle (reload + detail + 2x render)
    // This is the actual hot loop pattern every 2 seconds
    // =========================================================================
    eprintln!("=== Benchmark 4: Full tick cycle (reload + detail + render) ===");
    let n_cycles = 500;
    let rss_before = rss_mb();
    let start = Instant::now();

    for i in 0..n_cycles {
        // Step 1: reload (list_items + get_labels)
        let conn = Connection::open(&db_path)?;
        let filter = ItemFilter {
            include_deleted: false,
            sort: SortOrder::UpdatedDesc,
            ..Default::default()
        };
        let raw_items = query::list_items(&conn, &filter)?;
        let _work_items: Vec<_> = raw_items
            .into_iter()
            .map(|qi| {
                let labels: Vec<String> = query::get_labels(&conn, &qi.item_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|l| l.label)
                    .collect();
                (qi.item_id, qi.title, labels)
            })
            .collect();

        // Step 2: load_detail_item
        let comments: Vec<_> = query::get_comments(&conn, target_id, None, None)?
            .into_iter()
            .map(|c| (c.author, c.body, c.created_at_us))
            .collect();
        drop(conn);

        // Step 3: clamp_detail_scroll -> detail_lines (call 1)
        let lines = simulate_detail_lines(&comments);
        let _total: usize = lines
            .iter()
            .map(|l| l.chars().count().max(1).div_ceil(80))
            .sum();
        drop(lines);

        // Step 4: render -> detail_lines (call 2)
        let lines = simulate_detail_lines(&comments);
        drop(lines);

        if i % 100 == 0 {
            eprintln!("  cycle {i:>4}: RSS = {:.1} MB", rss_mb());
        }
    }

    let cycle_time = start.elapsed();
    let rss_after_cycle = rss_mb();
    eprintln!(
        "Time: {cycle_time:.2?} ({:.2?}/iter)",
        cycle_time / n_cycles
    );
    eprintln!(
        "RSS: {rss_before:.1} -> {rss_after_cycle:.1} MB (+{:.1} MB)",
        rss_after_cycle - rss_before
    );
    eprintln!();

    // =========================================================================
    // Summary
    // =========================================================================
    let rss_final = rss_mb();
    eprintln!("=== SUMMARY ===");
    eprintln!("RSS baseline:     {rss_baseline:.1} MB");
    eprintln!("RSS final:        {rss_final:.1} MB");
    eprintln!("RSS total growth: {:.1} MB", rss_final - rss_baseline);
    eprintln!();
    eprintln!("Per-tick breakdown ({n_items} items, {comments_per_item} comments/item):");
    eprintln!("  reload:       {:.2?}", reload_time / n_reloads);
    eprintln!("  detail load:  {:.2?}", detail_time / n_detail_loads);
    eprintln!("  md render:    {:.2?}", render_time / n_renders);
    eprintln!("  full cycle:   {:.2?}", cycle_time / n_cycles);

    Ok(())
}
