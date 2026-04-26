//! TUI search view.
//!
//! Provides live search with results updating as you type.
//! Calls `bones_search::fusion::scoring::hybrid_search()` for full fusion scoring.
//! Falls back to FTS5-only search if semantic model unavailable.

use anyhow::Result;
use bones_core::config::load_project_config;
use bones_core::db::query;
use bones_search::fusion::{HybridSearchResult, hybrid_search, hybrid_search_fast};
use bones_search::semantic::SemanticModel;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::collections::HashSet;
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct EnrichedResult {
    item: HybridSearchResult,
    title: String,
}

struct DirectIdMatch {
    rank: usize,
    result: EnrichedResult,
}

fn escape_like(query: &str) -> String {
    let mut escaped = String::with_capacity(query.len());
    for ch in query.chars() {
        if matches!(ch, '%' | '_' | '\\') {
            escaped.push('\\');
        }
        escaped.push(ch);
    }
    escaped
}

fn direct_id_matches(
    conn: &rusqlite::Connection,
    query: &str,
    limit: usize,
) -> Result<Vec<DirectIdMatch>> {
    let query = query.trim().to_ascii_lowercase();
    if query.is_empty() || limit == 0 {
        return Ok(Vec::new());
    }

    let escaped = escape_like(&query);
    let contains = format!("%{escaped}%");
    let prefix = format!("{escaped}%");
    let limit = i64::try_from(limit).unwrap_or(i64::MAX);
    let mut stmt = conn.prepare(
        "SELECT item_id, title,
                CASE
                    WHEN lower(item_id) = ?2 THEN 0
                    WHEN lower(item_id) LIKE ?3 ESCAPE '\\' THEN 1
                    ELSE 2
                END AS id_rank
         FROM items
         WHERE is_deleted = 0
           AND lower(item_id) LIKE ?1 ESCAPE '\\'
         ORDER BY id_rank, length(item_id), item_id
         LIMIT ?4",
    )?;

    let rows = stmt.query_map(
        rusqlite::params![contains, query, prefix, limit],
        |row| {
            let item_id: String = row.get(0)?;
            let title: String = row.get(1)?;
            let rank: i64 = row.get(2)?;
            let rank = usize::try_from(rank).unwrap_or(usize::MAX);
            Ok(DirectIdMatch {
                rank,
                result: EnrichedResult {
                    item: HybridSearchResult {
                        item_id,
                        score: 1.0,
                        lexical_score: 1.0,
                        semantic_score: 0.0,
                        structural_score: 0.0,
                        lexical_rank: rank + 1,
                        semantic_rank: 0,
                        structural_rank: 0,
                    },
                    title,
                },
            })
        },
    )?;

    let mut matches = Vec::new();
    for row in rows {
        matches.push(row?);
    }
    Ok(matches)
}

fn merge_direct_id_matches(
    direct: Vec<DirectIdMatch>,
    ranked: Vec<EnrichedResult>,
    limit: usize,
) -> Vec<EnrichedResult> {
    let mut seen = HashSet::new();
    let mut merged = Vec::new();

    // All direct id matches (exact, prefix, or substring) take priority over
    // ranked lexical/semantic results. This matches the list view, where
    // `local_search_rank` orders contains-id above contains-title.
    for item in direct {
        if seen.insert(item.result.item.item_id.clone()) {
            merged.push(item.result);
        }
    }

    for item in ranked {
        if seen.insert(item.item.item_id.clone()) {
            merged.push(item);
        }
    }

    merged.truncate(limit);
    merged
}

pub struct SearchView {
    db_path: PathBuf,
    semantic_model: Option<std::sync::Arc<SemanticModel>>,
    query: String,
    results: Vec<EnrichedResult>,
    state: ListState,
    last_keystroke: Instant,
    debounced_query: String,
    /// Receiver for background semantic refinement results.
    refinement_rx: Option<std::sync::mpsc::Receiver<Vec<EnrichedResult>>>,
}

impl SearchView {
    pub fn new(db_path: PathBuf) -> Result<Self> {
        let semantic_enabled = db_path
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(|root| load_project_config(root).ok())
            .map(|cfg| cfg.search.semantic)
            .unwrap_or(true);
        let semantic_model = if semantic_enabled {
            match SemanticModel::load() {
                Ok(model) => Some(std::sync::Arc::new(model)),
                Err(err) => {
                    tracing::warn!(
                        "semantic model unavailable in TUI search; using lexical+structural only: {err}"
                    );
                    None
                }
            }
        } else {
            None
        };

        let view = Self {
            db_path,
            semantic_model,
            query: String::new(),
            results: Vec::new(),
            state: ListState::default(),
            last_keystroke: Instant::now(),
            debounced_query: String::new(),
            refinement_rx: None,
        };
        Ok(view)
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<Option<SearchAction>> {
        match key.code {
            KeyCode::Char('j') => {
                self.select_next();
                Ok(None)
            }
            KeyCode::Char('k') => {
                self.select_prev();
                Ok(None)
            }
            KeyCode::Char(c) => {
                self.query.push(c);
                self.last_keystroke = Instant::now();
                self.state.select(None);
                Ok(None)
            }
            KeyCode::Backspace => {
                self.query.pop();
                self.last_keystroke = Instant::now();
                self.state.select(None);
                Ok(None)
            }
            KeyCode::Esc => {
                if !self.query.is_empty() {
                    self.query.clear();
                    self.results.clear();
                    self.state.select(None);
                }
                Ok(None)
            }
            KeyCode::Enter => {
                if let Some(res) = self.selected_item() {
                    Ok(Some(SearchAction::Select(res.item.item_id.clone())))
                } else {
                    self.perform_search()?;
                    Ok(None)
                }
            }
            KeyCode::Down => {
                self.select_next();
                Ok(None)
            }
            KeyCode::Up => {
                self.select_prev();
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    pub fn tick(&mut self) -> Result<()> {
        if self.query != self.debounced_query
            && self.last_keystroke.elapsed() > Duration::from_millis(300)
        {
            self.perform_search()?;
            self.debounced_query = self.query.clone();
        }

        // Poll for background semantic refinement results.
        if let Some(rx) = &self.refinement_rx {
            if let Ok(refined) = rx.try_recv() {
                tracing::debug!(count = refined.len(), "search view: tier-2 refinement applied");
                self.results = refined;
                self.refinement_rx = None;
                if !self.results.is_empty() && self.state.selected().is_none() {
                    self.state.select(Some(0));
                }
            }
        }

        Ok(())
    }

    fn perform_search(&mut self) -> Result<()> {
        self.refinement_rx = None;

        if self.query.trim().is_empty() {
            self.results.clear();
            return Ok(());
        }

        let conn = match query::try_open_projection(&self.db_path)? {
            Some(c) => c,
            None => return Ok(()),
        };
        let direct_matches = direct_id_matches(&conn, &self.query, 20)?;

        // Auto-wildcard for simple queries to support "type-ahead" feel
        let effective_query = if !self.query.contains(' ')
            && !self.query.contains('*')
            && !self.query.contains(':')
        {
            format!("{}*", self.query)
        } else {
            self.query.clone()
        };

        // Tier 1: fast search (lexical + structural only).
        let raw_results = match hybrid_search_fast(&effective_query, &conn, 20, 60) {
            Ok(results) => results,
            Err(err) => {
                tracing::warn!("search view fast search failed: {err:#}");
                Vec::new()
            }
        };

        let mut enriched = Vec::with_capacity(raw_results.len());
        for res in raw_results {
            let title = query::get_item(&conn, &res.item_id, false)?
                .map(|item| item.title)
                .unwrap_or_else(|| "Unknown".to_string());
            enriched.push(EnrichedResult { item: res, title });
        }
        self.results = merge_direct_id_matches(direct_matches, enriched, 20);

        if !self.results.is_empty() {
            self.state.select(Some(0));
        } else {
            self.state.select(None);
        }

        // Tier 2: spawn background semantic refinement.
        if let Some(model) = self.semantic_model.clone() {
            let db_path = self.db_path.clone();
            let raw_query = self.query.clone();
            let query_owned = effective_query;
            let (tx, rx) = std::sync::mpsc::channel();
            self.refinement_rx = Some(rx);

            std::thread::spawn(move || {
                let conn = match query::try_open_projection(&db_path) {
                    Ok(Some(c)) => c,
                    _ => return,
                };
                let hits = match hybrid_search(&query_owned, &conn, Some(&model), 20, 60) {
                    Ok(h) => h,
                    Err(e) => {
                        // Leave tier-1 results (already merged with direct id matches
                        // on the foreground) visible rather than overwriting them with
                        // a direct-only fallback.
                        tracing::debug!("search view tier-2 failed: {e:#}");
                        return;
                    }
                };
                let enriched: Vec<EnrichedResult> = hits
                    .into_iter()
                    .filter_map(|res| {
                        let title = query::get_item(&conn, &res.item_id, false)
                            .ok()
                            .flatten()
                            .map(|item| item.title)
                            .unwrap_or_else(|| "Unknown".to_string());
                        Some(EnrichedResult { item: res, title })
                    })
                    .collect();
                let direct = match direct_id_matches(&conn, &raw_query, 20) {
                    Ok(matches) => matches,
                    Err(e) => {
                        tracing::debug!("search view direct ID refinement failed: {e:#}");
                        Vec::new()
                    }
                };
                let _ = tx.send(merge_direct_id_matches(direct, enriched, 20));
            });
        }

        Ok(())
    }

    fn select_next(&mut self) {
        let len = self.results.len();
        if len == 0 {
            return;
        }
        let i = self
            .state
            .selected()
            .map_or(0, |i| if i + 1 >= len { 0 } else { i + 1 });
        self.state.select(Some(i));
    }

    fn select_prev(&mut self) {
        let len = self.results.len();
        if len == 0 {
            return;
        }
        let i = self
            .state
            .selected()
            .map_or(0, |i| if i == 0 { len - 1 } else { i - 1 });
        self.state.select(Some(i));
    }

    fn selected_item(&self) -> Option<&EnrichedResult> {
        self.state.selected().and_then(|i| self.results.get(i))
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(3), Constraint::Min(0)])
            .split(area);

        let input_area = chunks[0];
        let list_area = chunks[1];

        let input = Paragraph::new(self.query.as_str())
            .block(Block::default().borders(Borders::ALL).title(" Search "))
            .style(Style::default().fg(Color::Yellow));
        frame.render_widget(input, input_area);

        let items: Vec<ListItem> = self
            .results
            .iter()
            .map(|res| {
                let content = Line::from(vec![
                    Span::styled(
                        format!("{:<10} ", res.item.item_id),
                        Style::default().fg(Color::Cyan),
                    ),
                    Span::styled(
                        format!("{:.4} ", res.item.score),
                        Style::default().fg(Color::Green),
                    ),
                    Span::raw(&res.title),
                ]);
                ListItem::new(content)
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Results "))
            .highlight_style(
                Style::default()
                    .add_modifier(Modifier::BOLD)
                    .fg(Color::White)
                    .bg(Color::DarkGray),
            )
            .highlight_symbol("► ");

        frame.render_stateful_widget(list, list_area, &mut self.state);
    }
}

pub enum SearchAction {
    Select(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db::migrations;
    use bones_core::db::project::{Projector, ensure_tracking_table};
    use bones_core::event::Event;
    use bones_core::event::data::{CreateData, EventData};
    use bones_core::event::types::EventType;
    use bones_core::model::item::{Kind, Size, Urgency};
    use bones_core::model::item_id::ItemId;
    use rusqlite::Connection;
    use std::collections::BTreeMap;
    use tempfile::tempdir;

    fn setup_db() -> (tempfile::TempDir, PathBuf) {
        let dir = tempdir().expect("tempdir");
        let db_path = dir.path().join("bones.db");
        let mut conn = Connection::open(&db_path).expect("open db");
        migrations::migrate(&mut conn).expect("migrate");
        ensure_tracking_table(&conn).expect("tracking table");
        (dir, db_path)
    }

    fn insert_item(conn: &Connection, id: &str, title: &str) {
        let proj = Projector::new(conn);
        proj.project_event(&Event {
            wall_ts_us: 1000,
            agent: "test".into(),
            itc: "itc:AQ".into(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked(id),
            data: EventData::Create(CreateData {
                title: title.into(),
                kind: Kind::Task,
                size: Some(Size::M),
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: format!("hash:{id}"),
        })
        .unwrap();
    }

    #[test]
    fn search_view_initially_empty() {
        let (_dir, db_path) = setup_db();
        let view = SearchView::new(db_path).unwrap();
        assert!(view.query.is_empty());
        assert!(view.results.is_empty());
    }

    #[test]
    fn search_view_updates_query() {
        let (_dir, db_path) = setup_db();
        let mut view = SearchView::new(db_path).unwrap();

        view.handle_key(KeyEvent::from(KeyCode::Char('a'))).unwrap();
        assert_eq!(view.query, "a");

        view.handle_key(KeyEvent::from(KeyCode::Backspace)).unwrap();
        assert_eq!(view.query, "");
    }

    #[test]
    fn search_view_performs_search() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_item(&conn, "bn-1", "Authentication Bug");

        // Debug: check if item is in items table
        let count: i64 = conn
            .query_row("SELECT count(*) FROM items", [], |r| r.get(0))
            .unwrap();
        println!("Items count: {}", count);

        // Debug: check if item is in items_fts table
        let fts_count: i64 = conn
            .query_row("SELECT count(*) FROM items_fts", [], |r| r.get(0))
            .unwrap();
        println!("FTS count: {}", fts_count);

        let mut view = SearchView::new(db_path.clone()).unwrap();

        // Simulate typing "auth"
        view.query = "auth".to_string();
        view.perform_search().unwrap(); // Force search (skip debounce)

        assert_eq!(view.results.len(), 1);
        assert_eq!(view.results[0].item.item_id, "bn-1");
        assert_eq!(view.results[0].title, "Authentication Bug");
    }

    #[test]
    fn search_view_finds_item_by_full_id() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_item(&conn, "bn-abc123", "Unrelated title");

        let mut view = SearchView::new(db_path.clone()).unwrap();
        view.query = "bn-abc123".to_string();
        view.perform_search().unwrap();

        assert_eq!(view.results.len(), 1);
        assert_eq!(view.results[0].item.item_id, "bn-abc123");
        assert_eq!(view.results[0].title, "Unrelated title");
    }

    #[test]
    fn search_view_finds_item_by_partial_id() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_item(&conn, "bn-abc123", "Unrelated title");

        let mut view = SearchView::new(db_path.clone()).unwrap();
        view.query = "abc123".to_string();
        view.perform_search().unwrap();

        assert_eq!(view.results.len(), 1);
        assert_eq!(view.results[0].item.item_id, "bn-abc123");
    }

    fn make_enriched(id: &str, title: &str) -> EnrichedResult {
        EnrichedResult {
            item: HybridSearchResult {
                item_id: id.into(),
                score: 0.5,
                lexical_score: 0.5,
                semantic_score: 0.0,
                structural_score: 0.0,
                lexical_rank: 1,
                semantic_rank: 0,
                structural_rank: 0,
            },
            title: title.into(),
        }
    }

    fn make_direct(id: &str, title: &str, rank: usize) -> DirectIdMatch {
        DirectIdMatch {
            rank,
            result: make_enriched(id, title),
        }
    }

    #[test]
    fn merge_direct_id_matches_promotes_substring_id_above_ranked() {
        // Direct hit is a rank-2 (substring) id match; ranked hit is a different
        // bone whose title contains the query. The id match should still come first.
        let direct = vec![make_direct("bn-typo", "Authentication bug", 2)];
        let ranked = vec![make_enriched("bn-other", "fix bn-typo issue")];
        let merged = merge_direct_id_matches(direct, ranked, 20);
        let ids: Vec<&str> = merged.iter().map(|r| r.item.item_id.as_str()).collect();
        assert_eq!(ids, vec!["bn-typo", "bn-other"]);
    }

    #[test]
    fn merge_direct_id_matches_dedupes_across_direct_and_ranked() {
        let direct = vec![make_direct("bn-1", "One", 0)];
        let ranked = vec![make_enriched("bn-1", "One"), make_enriched("bn-2", "Two")];
        let merged = merge_direct_id_matches(direct, ranked, 20);
        let ids: Vec<&str> = merged.iter().map(|r| r.item.item_id.as_str()).collect();
        assert_eq!(ids, vec!["bn-1", "bn-2"]);
    }

    #[test]
    fn search_view_selection() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_item(&conn, "bn-1", "First");
        insert_item(&conn, "bn-2", "Second");

        let mut view = SearchView::new(db_path.clone()).unwrap();

        // Search "First"
        view.query = "First".to_string();
        view.perform_search().unwrap();

        assert!(!view.results.is_empty());
        let first_index = view
            .results
            .iter()
            .position(|res| res.item.item_id == "bn-1")
            .expect("bn-1 should be present in search results");
        view.state.select(Some(first_index));

        // Enter -> Select
        let action = view.handle_key(KeyEvent::from(KeyCode::Enter)).unwrap();
        match action {
            Some(SearchAction::Select(id)) => assert_eq!(id, "bn-1"),
            _ => panic!("Expected Select action"),
        }
    }
}
