//! TUI triage view showing `bn next` recommendations.
//!
//! Displays a ranked list of tasks based on composite scoring (critical path,
//! pagerank, urgency, etc.). Allows quick actions (do, done, skip).

use crate::cmd::triage_support::{self, RankedItem};
use anyhow::{Context, Result};
use bones_core::db::query;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, List, ListItem, ListState, Paragraph, Wrap},
    Frame,
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

pub struct TriageView {
    db_path: PathBuf,
    recommendations: Vec<RankedItem>,
    state: ListState,
    status_msg: Option<(String, Instant)>,
    /// If true, we need to refresh data on next update
    needs_refresh: bool,
}

impl TriageView {
    pub fn new(db_path: PathBuf) -> Result<Self> {
        let mut view = Self {
            db_path,
            recommendations: Vec::new(),
            state: ListState::default(),
            status_msg: None,
            needs_refresh: true,
        };
        view.reload()?;
        Ok(view)
    }

    pub fn reload(&mut self) -> Result<()> {
        let conn = match query::try_open_projection(&self.db_path)? {
            Some(c) => c,
            None => {
                self.recommendations.clear();
                return Ok(());
            }
        };

        // Use current time for decay calculation
        let now_us = chrono::Utc::now().timestamp_micros();
        let snapshot = triage_support::build_triage_snapshot(&conn, now_us)
            .context("build triage snapshot")?;
        
        // Filter to unblocked items (ready for action)
        self.recommendations = snapshot.unblocked_ranked;
        
        if self.state.selected().is_none() && !self.recommendations.is_empty() {
            self.state.select(Some(0));
        }
        
        self.needs_refresh = false;
        Ok(())
    }

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<Option<TriageAction>> {
        match key.code {
            KeyCode::Char('n') | KeyCode::Char('j') | KeyCode::Down => {
                self.select_next();
                Ok(None)
            }
            KeyCode::Char('k') | KeyCode::Up => {
                self.select_prev();
                Ok(None)
            }
            KeyCode::Char('s') => {
                // Skip = just move to next
                self.select_next();
                self.set_status("Skipped".to_string());
                Ok(None)
            }
            KeyCode::Char('d') => {
                 if let Some(item) = self.selected_item() {
                    Ok(Some(TriageAction::Do(item.id.clone())))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Char('D') => {
                 if let Some(item) = self.selected_item() {
                    Ok(Some(TriageAction::Done(item.id.clone())))
                } else {
                    Ok(None)
                }
            }
            KeyCode::Char('c') => {
                Ok(Some(TriageAction::Create))
            }
            KeyCode::Char('r') => {
                self.reload()?;
                self.set_status("Refreshed".to_string());
                Ok(None)
            }
            _ => Ok(None),
        }
    }

    fn select_next(&mut self) {
        let len = self.recommendations.len();
        if len == 0 { return; }
        let i = self.state.selected().map_or(0, |i| if i + 1 >= len { 0 } else { i + 1 });
        self.state.select(Some(i));
    }

    fn select_prev(&mut self) {
        let len = self.recommendations.len();
        if len == 0 { return; }
        let i = self.state.selected().map_or(0, |i| if i == 0 { len - 1 } else { i - 1 });
        self.state.select(Some(i));
    }

    fn selected_item(&self) -> Option<&RankedItem> {
        self.state.selected().and_then(|i| self.recommendations.get(i))
    }

    pub fn set_status(&mut self, msg: String) {
        self.status_msg = Some((msg, Instant::now()));
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Min(0), Constraint::Length(3)])
            .split(area);

        let main_area = chunks[0];
        let status_area = chunks[1];

        // Split main area into list (left) and details (right)
        let content_chunks = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
            .split(main_area);
        
        let list_area = content_chunks[0];
        let detail_area = content_chunks[1];

        self.render_list(frame, list_area);
        self.render_detail(frame, detail_area);
        self.render_status(frame, status_area);
    }

    fn render_list(&mut self, frame: &mut Frame, area: Rect) {
        let items: Vec<ListItem> = self.recommendations
            .iter()
            .map(|item| {
                let score_bar = self.score_bar(item.score);
                let content = Line::from(vec![
                    Span::styled(format!("{:<10} ", item.id), Style::default().fg(Color::Cyan)),
                    Span::styled(format!("{:<6.2} ", item.score), Style::default().fg(Color::Yellow)),
                    Span::raw(&item.title),
                ]);
                ListItem::new(content)
            })
            .collect();

        let list = List::new(items)
            .block(Block::default().borders(Borders::ALL).title(" Recommendations "))
            .highlight_style(Style::default().add_modifier(Modifier::BOLD).fg(Color::White).bg(Color::DarkGray))
            .highlight_symbol("► ");

        frame.render_stateful_widget(list, area, &mut self.state);
    }

    fn render_detail(&self, frame: &mut Frame, area: Rect) {
        let block = Block::default().borders(Borders::ALL).title(" Details ");
        
        if let Some(item) = self.selected_item() {
            let text = vec![
                Line::from(vec![
                    Span::styled("ID: ", Style::default().fg(Color::Cyan)),
                    Span::raw(&item.id),
                    Span::raw("   "),
                    Span::styled("Score: ", Style::default().fg(Color::Yellow)),
                    Span::raw(format!("{:.4}", item.score)),
                ]),
                Line::from(vec![
                    Span::styled("Title: ", Style::default().add_modifier(Modifier::BOLD)),
                    Span::raw(&item.title),
                ]),
                Line::from(""),
                Line::from(Span::styled("Analysis:", Style::default().fg(Color::Green))),
                Line::from(Span::raw(&item.explanation)),
                Line::from(""),
                Line::from(vec![
                    Span::styled("Unblocks: ", Style::default().fg(Color::Blue)),
                    Span::raw(format!("{} active items", item.unblocks_active)),
                ]),
            ];
            
            let p = Paragraph::new(text)
                .block(block)
                .wrap(Wrap { trim: true });
            frame.render_widget(p, area);
        } else {
             let p = Paragraph::new("No item selected")
                .block(block);
             frame.render_widget(p, area);
        }
    }

    fn render_status(&self, frame: &mut Frame, area: Rect) {
        let mut spans = vec![
            Span::styled("n/j", Style::default().fg(Color::Yellow)),
            Span::raw(" next  "),
            Span::styled("s", Style::default().fg(Color::Yellow)),
            Span::raw(" skip  "),
            Span::styled("d", Style::default().fg(Color::Yellow)),
            Span::raw(" do  "),
            Span::styled("D", Style::default().fg(Color::Yellow)),
            Span::raw(" done  "),
            Span::styled("c", Style::default().fg(Color::Yellow)),
            Span::raw(" create  "),
            Span::styled("r", Style::default().fg(Color::Yellow)),
            Span::raw(" refresh"),
        ];

        if let Some((msg, time)) = &self.status_msg {
            if time.elapsed() < Duration::from_secs(3) {
                spans.push(Span::raw("  |  "));
                spans.push(Span::styled(msg, Style::default().fg(Color::Cyan)));
            }
        }

        let p = Paragraph::new(Line::from(spans))
            .block(Block::default().borders(Borders::ALL));
        frame.render_widget(p, area);
    }

    fn score_bar(&self, score: f64) -> String {
        // Simplified bar for now
        let w = 5;
        let filled = (score.clamp(0.0, 1.0) * w as f64).round() as usize;
        "█".repeat(filled)
    }
}

pub enum TriageAction {
    Do(String),
    Done(String),
    Create,
}

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::db::migrations;
    use bones_core::db::project::{Projector, ensure_tracking_table};
    use bones_core::event::data::{CreateData, EventData};
    use bones_core::event::types::EventType;
    use bones_core::event::Event;
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

    fn insert_item(conn: &Connection, id: &str, title: &str, urgency: Urgency) {
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
                urgency,
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
    fn triage_view_shows_recommendations() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_item(&conn, "bn-1", "Urgent Task", Urgency::Urgent);
        insert_item(&conn, "bn-2", "Default Task", Urgency::Default);

        let mut view = TriageView::new(db_path.clone()).unwrap();
        
        // Initial load should have items
        assert_eq!(view.recommendations.len(), 2);
        assert_eq!(view.recommendations[0].id, "bn-1"); // Urgent first
        
        // Initial selection
        assert_eq!(view.state.selected(), Some(0));
    }

    #[test]
    fn triage_view_navigation() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_item(&conn, "bn-1", "Task 1", Urgency::Default);
        insert_item(&conn, "bn-2", "Task 2", Urgency::Default);

        let mut view = TriageView::new(db_path).unwrap();
        
        // Select next
        view.handle_key(KeyEvent::from(KeyCode::Char('j'))).unwrap();
        assert_eq!(view.state.selected(), Some(1));

        // Select next wraps to start
        view.handle_key(KeyEvent::from(KeyCode::Char('j'))).unwrap();
        assert_eq!(view.state.selected(), Some(0));

        // Select prev wraps to end
        view.handle_key(KeyEvent::from(KeyCode::Char('k'))).unwrap();
        assert_eq!(view.state.selected(), Some(1));
    }

    #[test]
    fn triage_view_actions() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_item(&conn, "bn-1", "Task 1", Urgency::Default);

        let mut view = TriageView::new(db_path).unwrap();
        
        // 'd' -> Do
        let action = view.handle_key(KeyEvent::from(KeyCode::Char('d'))).unwrap();
        match action {
            Some(TriageAction::Do(id)) => assert_eq!(id, "bn-1"),
            _ => panic!("Expected Do action"),
        }

        // 'D' -> Done
        let action = view.handle_key(KeyEvent::from(KeyCode::Char('D'))).unwrap();
        match action {
            Some(TriageAction::Done(id)) => assert_eq!(id, "bn-1"),
            _ => panic!("Expected Done action"),
        }

        // 'c' -> Create
        let action = view.handle_key(KeyEvent::from(KeyCode::Char('c'))).unwrap();
        match action {
            Some(TriageAction::Create) => {},
            _ => panic!("Expected Create action"),
        }
    }
}
