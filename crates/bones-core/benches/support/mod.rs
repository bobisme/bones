#![allow(dead_code)]

use bones_core::event::writer::write_event;
use bones_core::event::{
    AssignAction, AssignData, CommentData, CompactData, CreateData, DeleteData, Event, EventData,
    EventType, LinkData, MoveData, RedactData, SnapshotData, UnlinkData, UpdateData, parse_line,
};
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_core::model::item_id::{ItemId, generate_item_id};
use serde_json::json;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

#[derive(Clone, Copy, Debug)]
pub struct BenchmarkTier {
    pub name: &'static str,
    pub item_count: usize,
    pub event_count: usize,
}

pub const TIER_S: BenchmarkTier = BenchmarkTier {
    name: "S",
    item_count: 1_000,
    event_count: 50_000,
};

pub const TIER_M: BenchmarkTier = BenchmarkTier {
    name: "M",
    item_count: 10_000,
    event_count: 500_000,
};

pub const TIER_L: BenchmarkTier = BenchmarkTier {
    name: "L",
    item_count: 100_000,
    event_count: 5_000_000,
};

pub const TIERS: [BenchmarkTier; 3] = [TIER_S, TIER_M, TIER_L];

#[derive(Debug)]
pub struct SyntheticCorpus {
    pub tier: BenchmarkTier,
    pub seed: u64,
    pub lines: Vec<String>,
    pub bytes_by_event: HashMap<EventType, usize>,
}

#[derive(Clone, Copy, Debug)]
pub struct LatencySummary {
    pub p50: Duration,
    pub p95: Duration,
    pub p99: Duration,
}

#[derive(Clone, Copy, Debug)]
struct Prng(u64);

impl Prng {
    fn new(seed: u64) -> Self {
        Self(seed)
    }

    fn next_u64(&mut self) -> u64 {
        // 64-bit LCG constants from Numerical Recipes.
        self.0 = self
            .0
            .wrapping_mul(6_364_136_223_846_793_005)
            .wrapping_add(1_442_695_040_888_963_407);
        self.0
    }

    fn next_index(&mut self, upper_exclusive: usize) -> usize {
        if upper_exclusive == 0 {
            return 0;
        }
        (self.next_u64() as usize) % upper_exclusive
    }

    fn chance(&mut self, numerator: u64, denominator: u64) -> bool {
        debug_assert!(numerator <= denominator);
        self.next_u64() % denominator < numerator
    }
}

pub fn generate_corpus(tier: BenchmarkTier, seed: u64) -> SyntheticCorpus {
    generate_corpus_with_event_limit(tier, seed, tier.event_count)
}

pub fn generate_corpus_for_bench(tier: BenchmarkTier, seed: u64) -> SyntheticCorpus {
    let max_events = std::env::var("BONES_BENCH_MAX_EVENTS")
        .ok()
        .and_then(|s| s.parse::<usize>().ok())
        .unwrap_or(50_000);
    let event_limit = tier.event_count.min(max_events);
    generate_corpus_with_event_limit(tier, seed, event_limit)
}

pub fn generate_corpus_with_event_limit(
    tier: BenchmarkTier,
    seed: u64,
    event_limit: usize,
) -> SyntheticCorpus {
    let mut prng = Prng::new(seed);
    let item_limit = tier.item_count.min(event_limit.max(1));
    let item_ids = build_item_ids(item_limit);
    let mut last_hash_by_item: Vec<Option<String>> = vec![None; item_limit];

    let mut bytes_by_event: HashMap<EventType, usize> = HashMap::new();
    let mut lines = Vec::with_capacity(event_limit);

    for index in 0..event_limit {
        let event_type = if index < item_limit {
            EventType::Create
        } else {
            sample_mutation_type(&mut prng)
        };

        let item_index = if event_type == EventType::Create && index < item_limit {
            index
        } else {
            prng.next_index(item_limit)
        };

        let item_id = item_ids[item_index].clone();
        let parents = last_hash_by_item[item_index]
            .iter()
            .cloned()
            .collect::<Vec<_>>();

        let mut event = Event {
            wall_ts_us: 1_700_000_000_000_000_i64 + index as i64,
            agent: format!("bench-agent-{}", index % 12),
            itc: format!("itc:AQ.{index}"),
            parents,
            event_type,
            item_id: item_id.clone(),
            data: build_event_data(event_type, &item_id, &item_ids, &mut prng),
            event_hash: String::new(),
        };

        let line_with_newline =
            write_event(&mut event).expect("benchmark corpus generation should serialize");
        let line = line_with_newline.trim_end_matches('\n').to_owned();

        parse_line(&line).expect("generated line must parse as valid TSJSON event");

        *bytes_by_event.entry(event_type).or_insert(0) += line.len() + 1;
        last_hash_by_item[item_index] = Some(event.event_hash);
        lines.push(line);
    }

    SyntheticCorpus {
        tier,
        seed,
        lines,
        bytes_by_event,
    }
}

pub fn sample_latencies(iterations: usize, mut op: impl FnMut()) -> Vec<Duration> {
    let mut samples = Vec::with_capacity(iterations);
    for _ in 0..iterations {
        let start = Instant::now();
        op();
        samples.push(start.elapsed());
    }
    samples
}

pub fn summarize_latencies(samples: &[Duration]) -> LatencySummary {
    assert!(!samples.is_empty(), "at least one sample is required");

    let mut sorted = samples.to_vec();
    sorted.sort_unstable();

    LatencySummary {
        p50: percentile(&sorted, 50),
        p95: percentile(&sorted, 95),
        p99: percentile(&sorted, 99),
    }
}

pub fn bytes_per_event_by_type(corpus: &SyntheticCorpus) -> BTreeMap<String, f64> {
    let mut counts: HashMap<EventType, usize> = HashMap::new();
    for line in &corpus.lines {
        let fields = line.split('\t').collect::<Vec<_>>();
        let event_type: EventType = fields
            .get(4)
            .expect("tsjson must have event type field")
            .parse()
            .expect("event type in generated corpus must parse");
        *counts.entry(event_type).or_insert(0usize) += 1;
    }

    corpus
        .bytes_by_event
        .iter()
        .map(|(event_type, total_bytes)| {
            let count = counts.get(event_type).copied().unwrap_or(1);
            (
                event_type.as_str().to_string(),
                *total_bytes as f64 / count as f64,
            )
        })
        .collect()
}

fn percentile(sorted: &[Duration], percentile: usize) -> Duration {
    let idx = ((sorted.len() - 1) * percentile) / 100;
    sorted[idx]
}

fn build_item_ids(item_count: usize) -> Vec<ItemId> {
    let mut generated = Vec::with_capacity(item_count);

    for index in 0..item_count {
        let id = generate_item_id(&format!("tier-item-{index}"), index, |_| false);
        generated.push(id);
    }

    generated
}

fn sample_mutation_type(prng: &mut Prng) -> EventType {
    let roll = prng.next_u64() % 100;
    match roll {
        0..=27 => EventType::Update,
        28..=42 => EventType::Comment,
        43..=54 => EventType::Move,
        55..=64 => EventType::Assign,
        65..=73 => EventType::Link,
        74..=79 => EventType::Unlink,
        80..=86 => EventType::Compact,
        87..=92 => EventType::Snapshot,
        93..=96 => EventType::Redact,
        _ => EventType::Delete,
    }
}

fn build_event_data(
    event_type: EventType,
    item_id: &ItemId,
    item_ids: &[ItemId],
    prng: &mut Prng,
) -> EventData {
    match event_type {
        EventType::Create => EventData::Create(CreateData {
            title: format!("{}: {}", item_id.as_str(), make_text(prng, 5, 12)),
            kind: sample_kind(prng),
            size: sample_size(prng),
            urgency: sample_urgency(prng),
            labels: sample_labels(prng),
            parent: if prng.chance(1, 20) {
                Some(item_ids[prng.next_index(item_ids.len())].to_string())
            } else {
                None
            },
            causation: None,
            description: Some(sample_description(prng)),
            extra: BTreeMap::new(),
        }),
        EventType::Update => EventData::Update(UpdateData {
            field: if prng.chance(2, 3) {
                "description".to_string()
            } else {
                "labels".to_string()
            },
            value: if prng.chance(2, 3) {
                json!(sample_description(prng))
            } else {
                json!(sample_labels(prng))
            },
            extra: BTreeMap::new(),
        }),
        EventType::Move => EventData::Move(MoveData {
            state: sample_state(prng),
            reason: if prng.chance(1, 2) {
                Some(make_text(prng, 4, 12))
            } else {
                None
            },
            extra: BTreeMap::new(),
        }),
        EventType::Assign => EventData::Assign(AssignData {
            agent: format!("agent-{}", prng.next_u64() % 20),
            action: if prng.chance(3, 4) {
                AssignAction::Assign
            } else {
                AssignAction::Unassign
            },
            extra: BTreeMap::new(),
        }),
        EventType::Comment => EventData::Comment(CommentData {
            body: sample_description(prng),
            extra: BTreeMap::new(),
        }),
        EventType::Link => {
            let target = &item_ids[prng.next_index(item_ids.len())];
            EventData::Link(LinkData {
                target: target.to_string(),
                link_type: if prng.chance(4, 5) {
                    "blocks".to_string()
                } else {
                    "related_to".to_string()
                },
                extra: BTreeMap::new(),
            })
        }
        EventType::Unlink => {
            let target = &item_ids[prng.next_index(item_ids.len())];
            EventData::Unlink(UnlinkData {
                target: target.to_string(),
                link_type: if prng.chance(2, 3) {
                    Some("blocks".to_string())
                } else {
                    None
                },
                extra: BTreeMap::new(),
            })
        }
        EventType::Delete => EventData::Delete(DeleteData {
            reason: Some("cleanup".to_string()),
            extra: BTreeMap::new(),
        }),
        EventType::Compact => EventData::Compact(CompactData {
            summary: make_text(prng, 8, 20),
            extra: BTreeMap::new(),
        }),
        EventType::Snapshot => EventData::Snapshot(SnapshotData {
            state: json!({
                "id": item_id.as_str(),
                "title": make_text(prng, 4, 10),
                "state": "done",
                "labels": sample_labels(prng),
            }),
            extra: BTreeMap::new(),
        }),
        EventType::Redact => EventData::Redact(RedactData {
            target_hash: format!("blake3:{:064x}", prng.next_u64()),
            reason: "synthetic benchmark redaction".to_string(),
            extra: BTreeMap::new(),
        }),
    }
}

fn sample_kind(prng: &mut Prng) -> Kind {
    match prng.next_u64() % 100 {
        0..=74 => Kind::Task,
        75..=89 => Kind::Bug,
        _ => Kind::Goal,
    }
}

fn sample_size(prng: &mut Prng) -> Option<Size> {
    match prng.next_u64() % 8 {
        0 => None,
        1 => Some(Size::Xs),
        2 => Some(Size::S),
        3 => Some(Size::M),
        4 => Some(Size::L),
        5 => Some(Size::Xl),
        6 => Some(Size::Xxl),
        _ => Some(Size::Xxs),
    }
}

fn sample_urgency(prng: &mut Prng) -> Urgency {
    match prng.next_u64() % 100 {
        0..=7 => Urgency::Urgent,
        8..=92 => Urgency::Default,
        _ => Urgency::Punt,
    }
}

fn sample_state(prng: &mut Prng) -> State {
    match prng.next_u64() % 100 {
        0..=48 => State::Doing,
        49..=86 => State::Done,
        87..=95 => State::Open,
        _ => State::Archived,
    }
}

fn sample_labels(prng: &mut Prng) -> Vec<String> {
    const LABELS: [&str; 8] = [
        "backend",
        "frontend",
        "cli",
        "performance",
        "infra",
        "ux",
        "docs",
        "search",
    ];

    let label_count = match prng.next_u64() % 100 {
        0..=59 => 1,
        60..=91 => 2,
        _ => 3,
    };

    let mut labels = Vec::with_capacity(label_count);
    while labels.len() < label_count {
        let label = LABELS[prng.next_index(LABELS.len())].to_string();
        if !labels.contains(&label) {
            labels.push(label);
        }
    }

    labels
}

fn sample_description(prng: &mut Prng) -> String {
    let min_words = sample_desc_word_min(prng);
    let max_words = sample_desc_word_max(prng);
    make_text(prng, min_words, max_words)
}

fn sample_desc_word_min(prng: &mut Prng) -> usize {
    match prng.next_u64() % 100 {
        0..=59 => 12,
        60..=89 => 40,
        _ => 140,
    }
}

fn sample_desc_word_max(prng: &mut Prng) -> usize {
    match prng.next_u64() % 100 {
        0..=59 => 28,
        60..=89 => 110,
        _ => 480,
    }
}

fn make_text(prng: &mut Prng, min_words: usize, max_words: usize) -> String {
    const WORDS: [&str; 24] = [
        "agent",
        "event",
        "graph",
        "latency",
        "projection",
        "snapshot",
        "parser",
        "rebuild",
        "create",
        "update",
        "search",
        "queue",
        "cache",
        "lock",
        "retry",
        "merge",
        "compact",
        "dependency",
        "comment",
        "priority",
        "state",
        "deterministic",
        "benchmark",
        "throughput",
    ];

    let span = max_words.saturating_sub(min_words) + 1;
    let words = min_words + prng.next_index(span);

    let mut out = String::new();
    for i in 0..words {
        if i > 0 {
            out.push(' ');
        }
        out.push_str(WORDS[prng.next_index(WORDS.len())]);
    }
    out
}
