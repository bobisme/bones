//! `bn dedup` — bulk duplicate detection across open items.
//!
//! Scans all open items, builds a sparse similarity graph using BM25 prefiltering
//! and fusion scoring, then reports duplicate clusters.

use crate::output::{CliError, OutputMode, render, render_error};
use bones_core::config::load_project_config;
use bones_core::db::{fts, query};
use bones_search::find_duplicates;
use bones_search::fusion::scoring::SearchConfig;
use bones_triage::graph::RawGraph;
use clap::Args;
use serde::Serialize;
use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::io::Write;

const PREFILTER_LIMIT: u32 = 25;
const PREFILTER_SCORE_FLOOR: f64 = 0.30;

#[derive(Args, Debug)]
#[command(
    about = "Bulk duplicate detection across open items",
    long_about = "Scan all open items to find likely duplicate clusters.\n\n\
                  Uses FTS5 BM25 as a first-pass filter, then fusion scoring to\
                  confirm likely duplicate links.",
    after_help = "EXAMPLES:\n    # Scan with default threshold\n    bn dedup\n\n\
                  # More permissive threshold\n    bn dedup --threshold 0.60\n\n\
                  # Limit groups\n    bn dedup --limit 20\n\n\
                  # Machine-readable output\n    bn dedup --json"
)]
pub struct DedupArgs {
    /// Minimum fusion score to consider an edge as duplicate.
    #[arg(long, default_value = "0.70")]
    pub threshold: f64,

    /// Maximum number of duplicate groups to report.
    #[arg(long)]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize)]
pub struct GroupItem {
    pub id: String,
    pub title: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct DedupGroupOutput {
    pub group: Vec<String>,
    pub score: f64,
    pub items: Vec<GroupItem>,
}

#[derive(Debug, Clone, PartialEq)]
struct DupGroup {
    items: Vec<String>,
    max_score: f64,
}

pub fn run_dedup(
    args: &DedupArgs,
    output: OutputMode,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
    let threshold = args.threshold.clamp(0.0, 1.0);
    let db_path = project_root.join(".bones/bones.db");

    let conn = match query::try_open_projection(&db_path)? {
        Some(c) => c,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    "projection database not found",
                    "run `bn rebuild` to initialize the projection",
                    "projection_missing",
                ),
            )?;
            anyhow::bail!("projection not found");
        }
    };

    let open_items = query::list_items(
        &conn,
        &query::ItemFilter {
            state: Some("open".to_string()),
            ..Default::default()
        },
    )?;

    if open_items.is_empty() {
        let empty: Vec<DedupGroupOutput> = Vec::new();
        return render(output, &empty, render_human);
    }

    let open_set: HashSet<String> = open_items.iter().map(|i| i.item_id.clone()).collect();
    let titles: HashMap<String, String> = open_items
        .iter()
        .map(|i| (i.item_id.clone(), i.title.clone()))
        .collect();

    let cfg = load_project_config(project_root).unwrap_or_default();
    let search_cfg = SearchConfig {
        rrf_k: 60,
        likely_duplicate_threshold: cfg.search.duplicate_threshold as f32,
        possibly_related_threshold: 0.70,
        maybe_related_threshold: 0.50,
    };

    let dependency_graph = RawGraph::from_sqlite(&conn)
        .map(|raw| raw.graph)
        .unwrap_or_else(|err| {
            tracing::warn!("unable to load dependency graph for dedup: {err}");
            petgraph::graph::DiGraph::new()
        });
    let mut pair_scores: HashMap<(String, String), f64> = HashMap::new();

    for item in &open_items {
        let query_text = build_query_text(&item.title, item.description.as_deref());
        if query_text.trim().is_empty() {
            continue;
        }

        let pre_hits = fts::search_bm25(&conn, &query_text, PREFILTER_LIMIT).unwrap_or_default();
        if pre_hits.is_empty() {
            continue;
        }

        let source_rank = pre_hits
            .iter()
            .find(|h| h.item_id == item.item_id)
            .map(|h| h.rank)
            .unwrap_or_else(|| {
                pre_hits
                    .iter()
                    .map(|h| h.rank)
                    .fold(f64::INFINITY, f64::min)
            });

        let mut candidate_ids: HashSet<String> = HashSet::new();
        for hit in &pre_hits {
            if hit.item_id == item.item_id || !open_set.contains(&hit.item_id) {
                continue;
            }
            let normalized = normalized_bm25(hit.rank, source_rank);
            if normalized >= PREFILTER_SCORE_FLOOR {
                candidate_ids.insert(hit.item_id.clone());
            }
        }

        if candidate_ids.is_empty() {
            continue;
        }

        let limit = candidate_ids.len().max(10);
        let Ok(candidates) = find_duplicates(
            &query_text,
            &conn,
            &dependency_graph,
            &search_cfg,
            false,
            limit,
        ) else {
            continue;
        };

        for cand in candidates {
            if cand.item_id == item.item_id || !candidate_ids.contains(&cand.item_id) {
                continue;
            }
            let score = cand.composite_score as f64;
            if score < threshold {
                continue;
            }
            let pair = canonical_pair(&item.item_id, &cand.item_id);
            pair_scores
                .entry(pair)
                .and_modify(|s| *s = s.max(score))
                .or_insert(score);
        }
    }

    let nodes: Vec<String> = open_set.iter().cloned().collect();
    let mut groups = cluster_groups(&nodes, &pair_scores, threshold);
    groups.sort_by(|a, b| {
        b.max_score
            .partial_cmp(&a.max_score)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.items.cmp(&b.items))
    });

    if let Some(limit) = args.limit {
        groups.truncate(limit);
    }

    let rendered: Vec<DedupGroupOutput> = groups
        .into_iter()
        .map(|g| DedupGroupOutput {
            group: g.items.clone(),
            score: g.max_score,
            items: g
                .items
                .into_iter()
                .map(|id| GroupItem {
                    title: titles.get(&id).cloned().unwrap_or_default(),
                    id,
                })
                .collect(),
        })
        .collect();

    render(output, &rendered, render_human)
}

fn render_human(groups: &Vec<DedupGroupOutput>, w: &mut dyn Write) -> std::io::Result<()> {
    if groups.is_empty() {
        writeln!(w, "No duplicate groups found.")?;
        return Ok(());
    }

    writeln!(w, "{} duplicate group(s):", groups.len())?;
    for (i, group) in groups.iter().enumerate() {
        writeln!(w, "\n{}. score {:.2}", i + 1, group.score)?;
        for item in &group.items {
            writeln!(w, "  - {}  {}", item.id, item.title)?;
        }
    }
    Ok(())
}

fn build_query_text(title: &str, description: Option<&str>) -> String {
    match description.map(str::trim).filter(|s| !s.is_empty()) {
        Some(desc) => format!("{title} {desc}"),
        None => title.to_string(),
    }
}

fn normalized_bm25(rank: f64, source_rank: f64) -> f64 {
    if source_rank < 0.0 {
        (rank / source_rank).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn canonical_pair(a: &str, b: &str) -> (String, String) {
    if a <= b {
        (a.to_string(), b.to_string())
    } else {
        (b.to_string(), a.to_string())
    }
}

fn cluster_groups(
    nodes: &[String],
    pair_scores: &HashMap<(String, String), f64>,
    threshold: f64,
) -> Vec<DupGroup> {
    let mut parent: Vec<usize> = (0..nodes.len()).collect();
    let mut rank: Vec<u8> = vec![0; nodes.len()];
    let mut index: HashMap<&str, usize> = HashMap::new();
    for (i, id) in nodes.iter().enumerate() {
        index.insert(id.as_str(), i);
    }

    fn find(parent: &mut [usize], x: usize) -> usize {
        if parent[x] != x {
            let root = find(parent, parent[x]);
            parent[x] = root;
        }
        parent[x]
    }

    fn union(parent: &mut [usize], rank: &mut [u8], a: usize, b: usize) {
        let mut ra = find(parent, a);
        let mut rb = find(parent, b);
        if ra == rb {
            return;
        }
        if rank[ra] < rank[rb] {
            std::mem::swap(&mut ra, &mut rb);
        }
        parent[rb] = ra;
        if rank[ra] == rank[rb] {
            rank[ra] += 1;
        }
    }

    for ((a, b), score) in pair_scores {
        if *score < threshold {
            continue;
        }
        let Some(&ia) = index.get(a.as_str()) else {
            continue;
        };
        let Some(&ib) = index.get(b.as_str()) else {
            continue;
        };
        union(&mut parent, &mut rank, ia, ib);
    }

    let mut members: BTreeMap<usize, BTreeSet<String>> = BTreeMap::new();
    for (i, id) in nodes.iter().enumerate() {
        let root = find(&mut parent, i);
        members.entry(root).or_default().insert(id.clone());
    }

    let mut out = Vec::new();
    for set in members.values() {
        if set.len() < 2 {
            continue;
        }
        let items: Vec<String> = set.iter().cloned().collect();
        let mut max_score: f64 = 0.0;
        for i in 0..items.len() {
            for j in (i + 1)..items.len() {
                if let Some(score) = pair_scores.get(&canonical_pair(&items[i], &items[j])) {
                    max_score = max_score.max(*score);
                }
            }
        }
        out.push(DupGroup { items, max_score });
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_pair_is_order_independent() {
        assert_eq!(
            canonical_pair("bn-2", "bn-1"),
            ("bn-1".into(), "bn-2".into())
        );
        assert_eq!(
            canonical_pair("bn-1", "bn-2"),
            ("bn-1".into(), "bn-2".into())
        );
    }

    #[test]
    fn bm25_normalization_handles_degenerate_source() {
        assert_eq!(normalized_bm25(-3.0, -5.0), 0.6);
        assert_eq!(normalized_bm25(-3.0, 0.0), 0.0);
    }

    #[test]
    fn cluster_deduplicates_bidirectional_pairs() {
        let nodes = vec!["a".into(), "b".into(), "c".into()];
        let mut edges = HashMap::new();
        edges.insert(canonical_pair("a", "b"), 0.91);
        // Reverse direction writes same canonical key in production path.
        edges.insert(canonical_pair("b", "a"), 0.91);

        let groups = cluster_groups(&nodes, &edges, 0.7);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].items, vec!["a", "b"]);
    }

    #[test]
    fn cluster_transitive_closure() {
        let nodes = vec!["a".into(), "b".into(), "c".into()];
        let mut edges = HashMap::new();
        edges.insert(canonical_pair("a", "b"), 0.82);
        edges.insert(canonical_pair("b", "c"), 0.88);

        let groups = cluster_groups(&nodes, &edges, 0.7);
        assert_eq!(groups.len(), 1);
        assert_eq!(groups[0].items, vec!["a", "b", "c"]);
        assert!((groups[0].max_score - 0.88).abs() < 1e-9);
    }

    #[test]
    fn cluster_respects_threshold() {
        let nodes = vec!["a".into(), "b".into()];
        let mut edges = HashMap::new();
        edges.insert(canonical_pair("a", "b"), 0.69);

        let groups = cluster_groups(&nodes, &edges, 0.7);
        assert!(groups.is_empty());
    }

    #[test]
    fn build_query_text_uses_description_when_present() {
        assert_eq!(
            build_query_text("title", Some("desc")),
            "title desc".to_string()
        );
        assert_eq!(build_query_text("title", Some("  ")), "title".to_string());
    }
}
