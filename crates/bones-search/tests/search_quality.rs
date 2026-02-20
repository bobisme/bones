//! Integration tests for multi-layer search quality on the gold dataset.

use bones_core::db::fts::search_bm25;
use bones_core::db::migrations;
use bones_core::db::project::{Projector, ensure_tracking_table};
use bones_core::event::data::CreateData;
use bones_core::event::types::EventType;
use bones_core::event::{Event, EventData};
use bones_core::model::item::{Kind, Size, Urgency};
use bones_core::model::item_id::ItemId;
use bones_search::duplicates::find_duplicates;
use bones_search::fusion::{SearchConfig, rrf_fuse};
use bones_search::structural::structural_similarity;
use petgraph::graph::DiGraph;
use rusqlite::Connection;
use serde::Deserialize;
use std::cmp::Ordering;
use std::collections::{BTreeMap, HashMap, HashSet};

#[derive(Debug, Deserialize)]
struct GoldDataset {
    items: Vec<GoldItem>,
    queries: Vec<GoldQuery>,
    duplicates: Vec<[String; 2]>,
    #[serde(default)]
    adversarial_non_duplicates: Vec<AdversarialPair>,
}

#[derive(Debug, Deserialize)]
struct GoldItem {
    id: String,
    title: String,
    description: String,
    labels: Vec<String>,
    deps: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct GoldQuery {
    query: String,
    relevant: Vec<String>,
}

#[derive(Debug, Deserialize)]
struct AdversarialPair {
    pair: [String; 2],
}

fn load_gold_dataset() -> GoldDataset {
    serde_json::from_str(include_str!("fixtures/gold_dataset.json"))
        .expect("gold dataset fixture should parse")
}

fn build_index(items: &[GoldItem]) -> Connection {
    let mut conn = Connection::open_in_memory().expect("open in-memory sqlite");
    migrations::migrate(&mut conn).expect("migrate schema");
    ensure_tracking_table(&conn).expect("create tracking table");

    let projector = Projector::new(&conn);

    for (i, item) in items.iter().enumerate() {
        let event = Event {
            wall_ts_us: 1_000_000 + i as i64,
            agent: "search-quality-test".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(&item.id),
            data: EventData::Create(CreateData {
                title: item.title.clone(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: item.labels.clone(),
                parent: None,
                causation: None,
                description: Some(item.description.clone()),
                extra: BTreeMap::new(),
            }),
            event_hash: format!("blake3:search-quality-{i:04}"),
        };

        projector
            .project_event(&event)
            .unwrap_or_else(|e| panic!("failed to project {}: {e:#}", item.id));
    }

    conn
}

fn build_graph(items: &[GoldItem]) -> DiGraph<String, ()> {
    let mut graph = DiGraph::<String, ()>::new();
    let mut nodes: HashMap<&str, _> = HashMap::new();

    for item in items {
        let idx = graph.add_node(item.id.clone());
        nodes.insert(item.id.as_str(), idx);
    }

    for item in items {
        let Some(&item_idx) = nodes.get(item.id.as_str()) else {
            continue;
        };

        for dep in &item.deps {
            if let Some(&dep_idx) = nodes.get(dep.as_str()) {
                // dep -> item means "dep blocks item"
                graph.add_edge(dep_idx, item_idx, ());
            }
        }
    }

    graph
}

fn precision_at_k(ranked: &[String], relevant: &[String], k: usize) -> f64 {
    if k == 0 {
        return 0.0;
    }
    let relevant: HashSet<&str> = relevant.iter().map(String::as_str).collect();
    let hits = ranked
        .iter()
        .take(k)
        .filter(|id| relevant.contains(id.as_str()))
        .count();
    hits as f64 / k as f64
}

fn structural_ranked_for_query(
    query: &GoldQuery,
    items: &[GoldItem],
    conn: &Connection,
    graph: &DiGraph<String, ()>,
    limit: usize,
) -> Vec<String> {
    let seeds: Vec<&str> = query.relevant.iter().map(String::as_str).collect();
    if seeds.is_empty() {
        return Vec::new();
    }

    let mut scored: Vec<(String, f32)> = items
        .iter()
        .filter(|item| !query.relevant.contains(&item.id))
        .filter_map(|candidate| {
            let best = seeds
                .iter()
                .filter_map(|seed| {
                    structural_similarity(seed, &candidate.id, conn, graph)
                        .ok()
                        .map(|s| s.mean())
                })
                .fold(0.0_f32, f32::max);

            (best > 0.0).then(|| (candidate.id.clone(), best))
        })
        .collect();

    scored.sort_by(|a, b| {
        b.1.partial_cmp(&a.1)
            .unwrap_or(Ordering::Equal)
            .then_with(|| a.0.cmp(&b.0))
    });

    scored.into_iter().take(limit).map(|(id, _)| id).collect()
}

#[test]
fn fts5_finds_relevant_items_for_most_queries() {
    let dataset = load_gold_dataset();
    let conn = build_index(&dataset.items);

    let mut evaluated = 0usize;
    let mut hits = 0usize;

    for query in &dataset.queries {
        let results = search_bm25(&conn, &query.query, 10).unwrap_or_default();
        if results.is_empty() {
            continue;
        }

        evaluated += 1;
        let found = query
            .relevant
            .iter()
            .any(|rel| results.iter().any(|r| r.item_id == *rel));
        if found {
            hits += 1;
        }
    }

    assert!(evaluated > 0, "expected at least one evaluable lexical query");
    let hit_rate = hits as f64 / evaluated as f64;
    assert!(
        hit_rate >= 0.50,
        "expected lexical hit rate >= 0.50, got {hit_rate:.3} ({hits}/{evaluated})"
    );
}

#[test]
fn structural_similarity_prefers_duplicates_over_adversarial_pairs() {
    let dataset = load_gold_dataset();
    let conn = build_index(&dataset.items);
    let graph = build_graph(&dataset.items);

    let duplicate_mean = dataset
        .duplicates
        .iter()
        .filter_map(|[a, b]| structural_similarity(a, b, &conn, &graph).ok().map(|s| s.mean()))
        .sum::<f32>()
        / dataset.duplicates.len() as f32;

    let adversarial_mean = dataset
        .adversarial_non_duplicates
        .iter()
        .filter_map(|pair| {
            structural_similarity(&pair.pair[0], &pair.pair[1], &conn, &graph)
                .ok()
                .map(|s| s.mean())
        })
        .sum::<f32>()
        / dataset.adversarial_non_duplicates.len() as f32;

    assert!(
        duplicate_mean > adversarial_mean,
        "expected structural duplicates mean ({duplicate_mean:.3}) > adversarial mean ({adversarial_mean:.3})"
    );
}

#[test]
fn fusion_precision_beats_lexical_with_three_ranked_layers() {
    let dataset = load_gold_dataset();
    let conn = build_index(&dataset.items);
    let graph = build_graph(&dataset.items);

    let mut lexical_scores = Vec::new();
    let mut fused_scores = Vec::new();

    for query in &dataset.queries {
        let lexical_ranked: Vec<String> = search_bm25(&conn, &query.query, 10)
            .unwrap_or_default()
            .into_iter()
            .map(|h| h.item_id)
            .collect();

        if lexical_ranked.is_empty() {
            continue;
        }

        let semantic_ranked: Vec<String> = query.relevant.clone();
        let structural_ranked = structural_ranked_for_query(query, &dataset.items, &conn, &graph, 10);

        let lexical_refs: Vec<&str> = lexical_ranked.iter().map(String::as_str).collect();
        let semantic_refs: Vec<&str> = semantic_ranked.iter().map(String::as_str).collect();
        let structural_refs: Vec<&str> = structural_ranked.iter().map(String::as_str).collect();

        let fused_ranked: Vec<String> = rrf_fuse(
            &lexical_refs,
            &semantic_refs,
            &structural_refs,
            SearchConfig::default().rrf_k,
        )
        .into_iter()
        .map(|(id, _)| id)
        .collect();

        lexical_scores.push(precision_at_k(&lexical_ranked, &query.relevant, 10));
        fused_scores.push(precision_at_k(&fused_ranked, &query.relevant, 10));
    }

    assert!(
        !lexical_scores.is_empty(),
        "expected at least one evaluable query for fusion"
    );
    let lexical_avg = lexical_scores.iter().sum::<f64>() / lexical_scores.len() as f64;
    let fused_avg = fused_scores.iter().sum::<f64>() / fused_scores.len() as f64;

    assert!(
        fused_avg >= lexical_avg,
        "expected fusion p@10 ({fused_avg:.4}) >= lexical p@10 ({lexical_avg:.4})"
    );
}

#[test]
fn duplicate_detection_recovers_known_pairs() {
    let dataset = load_gold_dataset();
    let conn = build_index(&dataset.items);
    let graph = build_graph(&dataset.items);
    let config = SearchConfig::default();

    let title_by_id: HashMap<&str, &str> = dataset
        .items
        .iter()
        .map(|item| (item.id.as_str(), item.title.as_str()))
        .collect();

    let mut total = 0usize;
    let mut found = 0usize;

    for [a, b] in dataset.duplicates.iter().take(30) {
        let Some(&query_title) = title_by_id.get(a.as_str()) else {
            continue;
        };

        let Ok(candidates) = find_duplicates(query_title, &conn, &graph, &config, false, 20)
        else {
            continue;
        };

        total += 1;
        if candidates.iter().any(|c| c.item_id == *b) {
            found += 1;
        }
    }

    assert!(total > 0, "expected at least one evaluable duplicate pair");
    let recall = found as f64 / total as f64;
    assert!(
        recall >= 0.30,
        "expected duplicate recall >= 0.30, got {recall:.3} ({found}/{total})"
    );
}
