//! Large-repo performance benchmarks.
//!
//! Measures the real end-to-end performance of bones with a Tier M synthetic
//! corpus: 10k items / 500k events (capped to `BONES_BENCH_MAX_EVENTS` for
//! CI speed, defaulting to 50k events).
//!
//! # SLO targets (Tier M corpus)
//!
//! | operation        | target  |
//! |------------------|---------|
//! | `bn list` open   | < 200ms |
//! | incremental apply (10 new events) | < 50ms |
//! | full rebuild     | < 5s    |
//!
//! Run with:
//! ```sh
//! cargo bench --bench large_repo
//! BONES_BENCH_MAX_EVENTS=500000 cargo bench --bench large_repo  # full Tier M
//! ```

mod support;

use bones_core::db::incremental::incremental_apply;
use bones_core::db::query::{ItemFilter, SortOrder, list_items, try_open_projection};
use bones_core::db::rebuild;
use bones_core::event::writer::write_event;
use bones_core::event::{Event, EventData, EventType};
use bones_core::event::data::{AssignAction, AssignData, CommentData, CreateData, MoveData};
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_core::model::item_id::ItemId;
use bones_core::shard::ShardManager;
use criterion::{BenchmarkId, Criterion, black_box, criterion_group, criterion_main};
use std::collections::BTreeMap;
use support::{TIER_M, sample_latencies, summarize_latencies};
use tempfile::TempDir;

// ---------------------------------------------------------------------------
// Fixture: synthetic large-repo corpus
// ---------------------------------------------------------------------------

/// The number of items in the Tier M corpus for this bench.
/// Controlled by `BONES_BENCH_ITEMS` env var (default: 10_000).
fn bench_item_count() -> usize {
    std::env::var("BONES_BENCH_ITEMS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(TIER_M.item_count)
}

/// The number of events to generate per item.
/// Controls total event count = items × events_per_item.
fn events_per_item() -> usize {
    std::env::var("BONES_BENCH_EVENTS_PER_ITEM")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(5)
}

/// Generate and append synthetic events directly, bypassing the corpus
/// generator's `parse_line` assertion which has a known issue with some
/// generated item IDs.
fn write_synthetic_events(shard_mgr: &ShardManager, year: i32, month: u32, item_count: usize, epi: usize) {
    // Generate item IDs using a simple deterministic scheme that always
    // produces valid ItemIds.
    let item_ids: Vec<ItemId> = (0..item_count)
        .map(|i| ItemId::new_unchecked(format!("bn-{i:04x}")))
        .collect();

    let agents = ["agent-alpha", "agent-beta", "agent-gamma", "agent-delta", "agent-epsilon"];

    for (i, id) in item_ids.iter().enumerate() {
        let base_ts = 1_700_000_000_000_000_i64 + i as i64 * 1000;
        let agent = agents[i % agents.len()];

        // Create event.
        let mut create = Event {
            wall_ts_us: base_ts,
            agent: agent.to_string(),
            itc: format!("itc:AQ.{}", base_ts),
            parents: vec![],
            event_type: EventType::Create,
            item_id: id.clone(),
            data: EventData::Create(CreateData {
                title: format!("Synthetic item {i:05} for large-repo bench"),
                kind: if i % 10 == 0 { Kind::Bug } else { Kind::Task },
                size: Some(Size::M),
                urgency: if i % 20 == 0 { Urgency::Urgent } else { Urgency::Default },
                labels: vec!["bench".to_string()],
                parent: None,
                causation: None,
                description: Some(format!("Description for bench item {i}")),
                extra: BTreeMap::new(),
            }),
            event_hash: String::new(),
        };
        let line = write_event(&mut create).expect("write create event");
        shard_mgr.append_raw(year, month, &line).expect("append create");

        // Generate `epi - 1` mutation events per item.
        for e in 1..epi {
            let ts = base_ts + e as i64;
            let mut mutation = Event {
                wall_ts_us: ts,
                agent: agents[(i + e) % agents.len()].to_string(),
                itc: format!("itc:AQ.{ts}"),
                parents: vec![create.event_hash.clone()],
                event_type: match e % 4 {
                    0 => EventType::Move,
                    1 => EventType::Comment,
                    2 => EventType::Assign,
                    _ => EventType::Move,
                },
                item_id: id.clone(),
                data: match e % 4 {
                    0 => EventData::Move(MoveData {
                        state: if e % 8 == 0 { State::Doing } else { State::Open },
                        reason: None,
                        extra: BTreeMap::new(),
                    }),
                    1 => EventData::Comment(CommentData {
                        body: format!("Bench comment {e} on item {i}"),
                        extra: BTreeMap::new(),
                    }),
                    2 => EventData::Assign(AssignData {
                        agent: agents[e % agents.len()].to_string(),
                        action: AssignAction::Assign,
                        extra: BTreeMap::new(),
                    }),
                    _ => EventData::Move(MoveData {
                        state: State::Open,
                        reason: None,
                        extra: BTreeMap::new(),
                    }),
                },
                event_hash: String::new(),
            };
            let line = write_event(&mut mutation).expect("write mutation event");
            shard_mgr.append_raw(year, month, &line).expect("append mutation");
        }
    }
}

/// Write corpus events to a real shard directory and build a SQLite projection.
///
/// Returns the temp directory (must be kept alive) and the paths to:
/// - `.bones/events/` (events_dir)
/// - `.bones/bones.db` (db_path)
fn build_projection_fixture() -> (TempDir, std::path::PathBuf, std::path::PathBuf) {
    let dir = TempDir::new().expect("tempdir");
    let bones_dir = dir.path().join(".bones");
    let events_dir = bones_dir.join("events");
    let db_path = bones_dir.join("bones.db");

    let shard_mgr = ShardManager::new(&bones_dir);
    shard_mgr.ensure_dirs().expect("ensure dirs");
    shard_mgr.init().expect("init shard");

    let item_count = bench_item_count();
    let epi = events_per_item();

    let (year, month) = shard_mgr.active_shard().expect("active shard").expect("some");
    write_synthetic_events(&shard_mgr, year, month, item_count, epi);

    // Build the initial projection.
    rebuild::rebuild(&events_dir, &db_path).expect("initial rebuild");

    (dir, events_dir, db_path)
}

// ---------------------------------------------------------------------------
// Benchmarks
// ---------------------------------------------------------------------------

fn bench_list_open_items(c: &mut Criterion) {
    let (_dir, _events_dir, db_path) = build_projection_fixture();

    let mut group = c.benchmark_group("large_repo");
    group.sample_size(20);

    group.bench_with_input(
        BenchmarkId::new("list_open_items", TIER_M.name),
        &db_path,
        |b, db_path| {
            b.iter(|| {
                let conn = try_open_projection(db_path)
                    .expect("open db")
                    .expect("projection exists");
                let filter = ItemFilter {
                    state: Some("open".to_string()),
                    limit: Some(50),
                    sort: SortOrder::UpdatedDesc,
                    ..Default::default()
                };
                let items = list_items(&conn, &filter).expect("list items");
                black_box(items.len())
            });
        },
    );

    group.finish();
}

fn bench_incremental_apply(c: &mut Criterion) {
    let (_dir, events_dir, db_path) = build_projection_fixture();

    // The shard manager roots in the .bones dir (parent of events_dir).
    let bones_dir = events_dir.parent().expect("bones dir");
    let shard_mgr = ShardManager::new(bones_dir);
    let (year, month) = shard_mgr
        .active_shard()
        .expect("active shard")
        .expect("some");

    let mut group = c.benchmark_group("large_repo");
    group.sample_size(20);

    // Use a fixed set of 10 new comment events to append each iter.
    let new_events: Vec<String> = (0..10)
        .map(|i| {
            let mut e = Event {
                wall_ts_us: 2_000_000_000_000_000_i64 + i,
                agent: "bench-new-agent".to_string(),
                itc: format!("itc:AQ.inc.{i}"),
                parents: vec![],
                event_type: EventType::Comment,
                item_id: ItemId::new_unchecked(format!("bn-{i:04x}")),
                data: EventData::Comment(CommentData {
                    body: format!("Incremental bench comment {i}"),
                    extra: BTreeMap::new(),
                }),
                event_hash: String::new(),
            };
            write_event(&mut e).expect("write new event")
        })
        .collect();

    group.bench_with_input(
        BenchmarkId::new("incremental_apply_10_new_events", TIER_M.name),
        &new_events,
        |b, lines| {
            b.iter(|| {
                // Append 10 new events.
                for line in lines.iter() {
                    shard_mgr
                        .append_raw(year, month, line)
                        .expect("append new event");
                }

                // Apply incrementally.
                let report = incremental_apply(&events_dir, &db_path, false)
                    .expect("incremental apply");
                black_box(report.events_applied)
            });
        },
    );

    group.finish();
}

fn bench_full_rebuild(c: &mut Criterion) {
    let (_dir, events_dir, db_path) = build_projection_fixture();

    let mut group = c.benchmark_group("large_repo");
    group.sample_size(10);

    group.bench_function(BenchmarkId::new("full_rebuild", TIER_M.name), |b| {
        b.iter(|| {
            let report = rebuild::rebuild(&events_dir, &db_path).expect("rebuild");
            black_box(report.event_count)
        });
    });

    group.finish();
}

// ---------------------------------------------------------------------------
// SLO latency report
// ---------------------------------------------------------------------------

fn emit_large_repo_slo_report() {
    let (_dir, events_dir, db_path) = build_projection_fixture();
    let bones_dir = events_dir.parent().expect("bones dir");
    let shard_mgr = ShardManager::new(bones_dir);
    let (year, month) = shard_mgr
        .active_shard()
        .expect("active shard")
        .expect("some");

    // Measure list latency.
    let list_samples = sample_latencies(50, || {
        let conn = try_open_projection(&db_path)
            .expect("open db")
            .expect("projection exists");
        let filter = ItemFilter {
            state: Some("open".to_string()),
            limit: Some(50),
            sort: SortOrder::UpdatedDesc,
            ..Default::default()
        };
        let items = list_items(&conn, &filter).expect("list items");
        black_box(items.len());
    });
    let list_stats = summarize_latencies(&list_samples);

    let target_200ms = std::time::Duration::from_millis(200);
    let list_slo_pass = list_stats.p99 < target_200ms;

    eprintln!(
        "SLO tier={} op=list_open p50={:?} p95={:?} p99={:?} target=200ms {}",
        TIER_M.name,
        list_stats.p50,
        list_stats.p95,
        list_stats.p99,
        if list_slo_pass { "PASS" } else { "FAIL" },
    );

    // Measure incremental apply (10 new events).
    let new_events: Vec<String> = (0..10)
        .map(|i| {
            let mut e = Event {
                wall_ts_us: 3_000_000_000_000_000_i64 + i as i64,
                agent: "bench-slo-agent".to_string(),
                itc: format!("itc:AQ.slo.{i}"),
                parents: vec![],
                event_type: EventType::Comment,
                item_id: ItemId::new_unchecked(format!("bn-{i:04x}")),
                data: EventData::Comment(CommentData {
                    body: format!("SLO bench comment {i}"),
                    extra: BTreeMap::new(),
                }),
                event_hash: String::new(),
            };
            write_event(&mut e).expect("write new event")
        })
        .collect();

    let inc_samples = sample_latencies(30, || {
        for line in &new_events {
            shard_mgr
                .append_raw(year, month, line)
                .expect("append");
        }
        let report = incremental_apply(&events_dir, &db_path, false).expect("incremental");
        black_box(report.events_applied);
    });
    let inc_stats = summarize_latencies(&inc_samples);

    let inc_slo_pass = inc_stats.p99 < std::time::Duration::from_millis(50);
    eprintln!(
        "SLO tier={} op=incremental_apply_10 p50={:?} p95={:?} p99={:?} target=50ms {}",
        TIER_M.name,
        inc_stats.p50,
        inc_stats.p95,
        inc_stats.p99,
        if inc_slo_pass { "PASS" } else { "FAIL" },
    );

    // Measure full rebuild latency.
    let rebuild_samples = sample_latencies(5, || {
        let report = rebuild::rebuild(&events_dir, &db_path).expect("rebuild");
        black_box(report.event_count);
    });
    let rebuild_stats = summarize_latencies(&rebuild_samples);
    eprintln!(
        "SLO tier={} op=full_rebuild p50={:?} p95={:?} p99={:?}",
        TIER_M.name, rebuild_stats.p50, rebuild_stats.p95, rebuild_stats.p99
    );
}

fn bench_all(c: &mut Criterion) {
    emit_large_repo_slo_report();
    bench_list_open_items(c);
    bench_incremental_apply(c);
    bench_full_rebuild(c);
}

criterion_group!(benches, bench_all);
criterion_main!(benches);
