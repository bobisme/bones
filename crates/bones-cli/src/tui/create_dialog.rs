//! TUI create dialog with duplicate detection.
//!
//! When the user presses `c` in the triage view (or any view), this overlay
//! appears. As the user types a title, similar existing items are shown below
//! via `hybrid_search`, allowing the user to:
//!
//! - Press **Enter** to create the new item
//! - Press **l** (or Enter on a highlighted similar item) to link/navigate to it
//! - Press **Esc** to cancel

use anyhow::{Context, Result};
use bones_core::db::query;
use bones_search::fusion::hybrid_search;
use crossterm::event::{KeyCode, KeyEvent};
use ratatui::{
    Frame,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Clear, List, ListItem, ListState, Paragraph},
};
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// A similar item candidate shown in the panel.
#[derive(Clone)]
struct SimilarCandidate {
    item_id: String,
    title: String,
    score: f32,
}

/// The action the dialog wants the caller to take.
pub enum DialogAction {
    /// Create a new item with this title.
    Create(String),
    /// Navigate to an existing item (user chose to link instead of create).
    LinkTo(String),
    /// The user cancelled; close the dialog.
    Cancel,
}

/// Overlay dialog for creating a new item with live duplicate detection.
pub struct CreateDialog {
    db_path: PathBuf,
    /// Title the user is typing.
    title: String,
    /// Debounced search results.
    similar: Vec<SimilarCandidate>,
    /// Selection state for the similar items list.
    similar_state: ListState,
    /// When the title was last changed (for debounce).
    last_change: Instant,
    /// The query that was last searched (avoids redundant lookups).
    last_searched: String,
}

impl CreateDialog {
    /// Create a new dialog backed by the given projection database.
    pub fn new(db_path: PathBuf) -> Self {
        Self {
            db_path,
            title: String::new(),
            similar: Vec::new(),
            similar_state: ListState::default(),
            last_change: Instant::now(),
            last_searched: String::new(),
        }
    }

    // -----------------------------------------------------------------------
    // Input handling
    // -----------------------------------------------------------------------

    /// Feed a key event to the dialog.
    ///
    /// Returns `Some(DialogAction)` when the dialog is complete (caller should
    /// close the overlay), or `None` when the user is still typing.
    pub fn handle_key(&mut self, key: KeyEvent) -> Result<Option<DialogAction>> {
        match key.code {
            // Cancel
            KeyCode::Esc => Ok(Some(DialogAction::Cancel)),

            // Confirm create
            KeyCode::Enter => {
                if self.title.trim().is_empty() {
                    // Nothing typed yet — cancel silently
                    Ok(Some(DialogAction::Cancel))
                } else if let Some(idx) = self.similar_state.selected() {
                    if let Some(candidate) = self.similar.get(idx) {
                        // User has a similar item highlighted — they want to link to it
                        Ok(Some(DialogAction::LinkTo(candidate.item_id.clone())))
                    } else {
                        Ok(Some(DialogAction::Create(self.title.trim().to_string())))
                    }
                } else {
                    Ok(Some(DialogAction::Create(self.title.trim().to_string())))
                }
            }

            // 'l' — explicitly link to selected similar item
            KeyCode::Char('l') => {
                if let Some(idx) = self.similar_state.selected() {
                    if let Some(candidate) = self.similar.get(idx) {
                        return Ok(Some(DialogAction::LinkTo(candidate.item_id.clone())));
                    }
                }
                Ok(None)
            }

            // Navigate similar items list
            KeyCode::Down | KeyCode::Char('j') => {
                self.select_next();
                Ok(None)
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.select_prev();
                Ok(None)
            }

            // Backspace
            KeyCode::Backspace => {
                self.title.pop();
                self.last_change = Instant::now();
                self.similar_state.select(None);
                Ok(None)
            }

            // Regular character input
            KeyCode::Char(c) => {
                self.title.push(c);
                self.last_change = Instant::now();
                self.similar_state.select(None);
                Ok(None)
            }

            _ => Ok(None),
        }
    }

    /// Tick — called periodically to trigger debounced search.
    pub fn tick(&mut self) -> Result<()> {
        let debounce = Duration::from_millis(300);
        if self.title != self.last_searched && self.last_change.elapsed() >= debounce {
            self.refresh_similar()?;
        }
        Ok(())
    }

    fn refresh_similar(&mut self) -> Result<()> {
        self.last_searched = self.title.clone();

        if self.title.trim().is_empty() {
            self.similar.clear();
            return Ok(());
        }

        let conn = match query::try_open_projection(&self.db_path)? {
            Some(c) => c,
            None => {
                self.similar.clear();
                return Ok(());
            }
        };

        // Auto-wildcard for prefix-style type-ahead
        let q = if !self.title.contains(' ') && !self.title.contains('*') {
            format!("{}*", self.title)
        } else {
            self.title.clone()
        };

        let raw = hybrid_search(&q, &conn, None, 5, 60).context("hybrid search")?;

        self.similar = raw
            .into_iter()
            .filter_map(|r| {
                let title = query::get_item(&conn, &r.item_id, false).ok()??.title;
                Some(SimilarCandidate {
                    item_id: r.item_id,
                    title,
                    score: r.score,
                })
            })
            .collect();

        Ok(())
    }

    fn select_next(&mut self) {
        let len = self.similar.len();
        if len == 0 {
            return;
        }
        let i = self
            .similar_state
            .selected()
            .map_or(0, |i| if i + 1 >= len { 0 } else { i + 1 });
        self.similar_state.select(Some(i));
    }

    fn select_prev(&mut self) {
        let len = self.similar.len();
        if len == 0 {
            return;
        }
        let i = self
            .similar_state
            .selected()
            .map_or(0, |i| if i == 0 { len - 1 } else { i - 1 });
        self.similar_state.select(Some(i));
    }

    // -----------------------------------------------------------------------
    // Rendering
    // -----------------------------------------------------------------------

    /// Render the dialog as a centered overlay on top of `area`.
    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Dialog dimensions
        let dialog_w: u16 = 70.min(area.width.saturating_sub(4));
        let has_similar = !self.similar.is_empty();
        let dialog_h: u16 = if has_similar {
            5 + self.similar.len() as u16 + 2
        } else {
            5
        };
        let dialog_h = dialog_h.min(area.height.saturating_sub(4));

        let x = area.x + area.width.saturating_sub(dialog_w) / 2;
        let y = area.y + area.height.saturating_sub(dialog_h) / 2;

        let dialog_area = Rect {
            x,
            y,
            width: dialog_w,
            height: dialog_h,
        };

        frame.render_widget(Clear, dialog_area);

        let block = Block::default()
            .borders(Borders::ALL)
            .title(" Create Item ")
            .title_style(
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            )
            .style(Style::default().bg(Color::Black));

        frame.render_widget(block, dialog_area);

        let inner = Rect {
            x: dialog_area.x + 1,
            y: dialog_area.y + 1,
            width: dialog_area.width.saturating_sub(2),
            height: dialog_area.height.saturating_sub(2),
        };

        // Divide inner area: title input (3 lines) + similar list
        let constraints = if has_similar {
            vec![Constraint::Length(3), Constraint::Min(0)]
        } else {
            vec![Constraint::Length(3)]
        };

        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints(constraints)
            .split(inner);

        // Title input
        let title_display = format!("{}_", self.title);
        let title_para = Paragraph::new(title_display.as_str())
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Title ")
                    .border_style(Style::default().fg(Color::Yellow)),
            )
            .style(Style::default().fg(Color::White));
        frame.render_widget(title_para, chunks[0]);

        // Similar items panel
        if has_similar {
            let items: Vec<ListItem> = self
                .similar
                .iter()
                .map(|c| {
                    let line = Line::from(vec![
                        Span::styled(
                            format!("{:<10} ", c.item_id),
                            Style::default().fg(Color::Cyan),
                        ),
                        Span::styled(
                            format!("{:.3} ", c.score as f64),
                            Style::default().fg(Color::Yellow),
                        ),
                        Span::raw(c.title.clone()),
                    ]);
                    ListItem::new(line)
                })
                .collect();

            let list = List::new(items)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" Similar Items (↑↓ navigate, l=link, Enter=create anyway) ")
                        .border_style(Style::default().fg(Color::DarkGray)),
                )
                .highlight_style(
                    Style::default()
                        .bg(Color::DarkGray)
                        .add_modifier(Modifier::BOLD),
                )
                .highlight_symbol("► ");

            frame.render_stateful_widget(list, chunks[1], &mut self.similar_state);
        }

        // Key hint bar at bottom of dialog (rendered in last line of dialog_area)
        if dialog_area.height > 2 {
            let hint_area = Rect {
                x: dialog_area.x + 1,
                y: dialog_area.y + dialog_area.height - 2,
                width: dialog_area.width.saturating_sub(2),
                height: 1,
            };
            let hints = Line::from(vec![
                Span::styled("Enter", Style::default().fg(Color::Yellow)),
                Span::raw(" create  "),
                Span::styled("l", Style::default().fg(Color::Yellow)),
                Span::raw(" link to selected  "),
                Span::styled("Esc", Style::default().fg(Color::Yellow)),
                Span::raw(" cancel"),
            ]);
            frame.render_widget(Paragraph::new(hints), hint_area);
        }
    }

    /// The current title being entered (for external read-back).
    #[cfg(test)]
    pub fn title(&self) -> &str {
        &self.title
    }
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
    fn dialog_starts_empty() {
        let (_dir, db_path) = setup_db();
        let dialog = CreateDialog::new(db_path);
        assert!(dialog.title().is_empty());
        assert!(dialog.similar.is_empty());
    }

    #[test]
    fn dialog_accepts_typed_characters() {
        let (_dir, db_path) = setup_db();
        let mut dialog = CreateDialog::new(db_path);

        dialog
            .handle_key(KeyEvent::from(KeyCode::Char('h')))
            .unwrap();
        dialog
            .handle_key(KeyEvent::from(KeyCode::Char('i')))
            .unwrap();
        assert_eq!(dialog.title(), "hi");
    }

    #[test]
    fn dialog_backspace_removes_char() {
        let (_dir, db_path) = setup_db();
        let mut dialog = CreateDialog::new(db_path);

        dialog
            .handle_key(KeyEvent::from(KeyCode::Char('a')))
            .unwrap();
        dialog
            .handle_key(KeyEvent::from(KeyCode::Char('b')))
            .unwrap();
        dialog
            .handle_key(KeyEvent::from(KeyCode::Backspace))
            .unwrap();
        assert_eq!(dialog.title(), "a");
    }

    #[test]
    fn dialog_esc_cancels() {
        let (_dir, db_path) = setup_db();
        let mut dialog = CreateDialog::new(db_path);
        dialog
            .handle_key(KeyEvent::from(KeyCode::Char('x')))
            .unwrap();

        let action = dialog.handle_key(KeyEvent::from(KeyCode::Esc)).unwrap();
        assert!(matches!(action, Some(DialogAction::Cancel)));
    }

    #[test]
    fn dialog_enter_on_empty_title_cancels() {
        let (_dir, db_path) = setup_db();
        let mut dialog = CreateDialog::new(db_path);

        let action = dialog.handle_key(KeyEvent::from(KeyCode::Enter)).unwrap();
        assert!(matches!(action, Some(DialogAction::Cancel)));
    }

    #[test]
    fn dialog_enter_with_title_creates() {
        let (_dir, db_path) = setup_db();
        let mut dialog = CreateDialog::new(db_path);

        dialog
            .handle_key(KeyEvent::from(KeyCode::Char('N')))
            .unwrap();
        dialog
            .handle_key(KeyEvent::from(KeyCode::Char('e')))
            .unwrap();
        dialog
            .handle_key(KeyEvent::from(KeyCode::Char('w')))
            .unwrap();

        let action = dialog.handle_key(KeyEvent::from(KeyCode::Enter)).unwrap();
        match action {
            Some(DialogAction::Create(title)) => assert_eq!(title, "New"),
            _ => panic!("expected Create action"),
        }
    }

    #[test]
    fn dialog_shows_similar_items_after_search() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_item(&conn, "bn-1", "Authentication Bug Fix");

        let mut dialog = CreateDialog::new(db_path.clone());
        dialog.title = "authentication".to_string();
        dialog.refresh_similar().unwrap();

        assert_eq!(dialog.similar.len(), 1);
        assert_eq!(dialog.similar[0].item_id, "bn-1");
    }

    #[test]
    fn dialog_link_action_returns_selected_item() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_item(&conn, "bn-1", "Authentication Bug Fix");

        let mut dialog = CreateDialog::new(db_path.clone());
        dialog.title = "authentication".to_string();
        dialog.refresh_similar().unwrap();

        // Select the first similar item
        dialog.similar_state.select(Some(0));

        let action = dialog
            .handle_key(KeyEvent::from(KeyCode::Char('l')))
            .unwrap();
        match action {
            Some(DialogAction::LinkTo(id)) => assert_eq!(id, "bn-1"),
            _ => panic!("expected LinkTo action"),
        }
    }

    #[test]
    fn dialog_enter_with_selected_similar_links() {
        let (_dir, db_path) = setup_db();
        let conn = Connection::open(&db_path).unwrap();
        insert_item(&conn, "bn-1", "Authentication Bug Fix");

        let mut dialog = CreateDialog::new(db_path.clone());
        dialog.title = "authentication".to_string();
        dialog.refresh_similar().unwrap();

        // Select the first similar item
        dialog.similar_state.select(Some(0));

        let action = dialog.handle_key(KeyEvent::from(KeyCode::Enter)).unwrap();
        match action {
            Some(DialogAction::LinkTo(id)) => assert_eq!(id, "bn-1"),
            _ => panic!("expected LinkTo when similar item is selected"),
        }
    }
}
