//! TUI search view.
//!
//! Provides live search with results updating as you type.
//! Calls `bones_search::fusion::scoring::hybrid_search()` for full fusion scoring.
//! Falls back to FTS5-only search if semantic model unavailable.

use anyhow::{Context, Result};
use bones_core::config::load_project_config;
use bones_core::db::query;
use bones_search::fusion::{HybridSearchResult, hybrid_search};
use bones_search::semantic::SemanticModel;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph},
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

struct EnrichedResult {
    item: HybridSearchResult,
    title: String,
}

pub struct SearchView {
    db_path: PathBuf,
    semantic_model: Option<SemanticModel>,
    query: String,
    results: Vec<EnrichedResult>,
    state: ListState,
    last_keystroke: Instant,
    debounced_query: String,
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
                Ok(model) => Some(model),
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
        Ok(())
    }

    fn perform_search(&mut self) -> Result<()> {
        if self.query.trim().is_empty() {
            self.results.clear();
            return Ok(());
        }

        let conn = match query::try_open_projection(&self.db_path)? {
            Some(c) => c,
            None => return Ok(()),
        };

        // Auto-wildcard for simple queries to support "type-ahead" feel
        let effective_query = if !self.query.contains(' ')
            && !self.query.contains('*')
            && !self.query.contains(':')
        {
            format!("{}*", self.query)
        } else {
            self.query.clone()
        };

        let raw_results = hybrid_search(
            &effective_query,
            &conn,
            self.semantic_model.as_ref(),
            20,
            60,
        )
        .context("search failed")?;

        let mut enriched = Vec::with_capacity(raw_results.len());
        for res in raw_results {
            let title = query::get_item(&conn, &res.item_id, false)?
                .map(|item| item.title)
                .unwrap_or_else(|| "Unknown".to_string());
            enriched.push(EnrichedResult { item: res, title });
        }
        self.results = enriched;

        if !self.results.is_empty() {
            self.state.select(Some(0));
        } else {
            self.state.select(None);
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
    fn search_view_selection() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_item(&conn, "bn-1", "First");
        insert_item(&conn, "bn-2", "Second");

        let mut view = SearchView::new(db_path.clone()).unwrap();

        // Search "First"
        view.query = "First".to_string();
        view.perform_search().unwrap();

        assert_eq!(view.results.len(), 1);
        assert_eq!(view.state.selected(), Some(0));

        // Enter -> Select
        let action = view.handle_key(KeyEvent::from(KeyCode::Enter)).unwrap();
        match action {
            Some(SearchAction::Select(id)) => assert_eq!(id, "bn-1"),
            _ => panic!("Expected Select action"),
        }
    }
}
