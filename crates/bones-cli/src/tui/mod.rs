//! TUI module — multi-view interactive terminal UI for bones.
//!
//! Provides three views accessible from the main `bn tui` command:
//!
//! - **List** (default): filterable, sortable item list (see [`list`]).
//! - **Triage** (`t` from list): ranked recommendations from `bn next` (see [`triage`]).
//! - **Search** (`s` from list or `/` in triage): live full-text search (see [`search`]).
//!
//! Key bindings for view switching (from any view):
//! - `t` — switch to Triage view
//! - `S` — switch to Search view  
//! - `l` — return to List view
//! - `q` / `Esc` — quit (from List) or return to List (from Triage/Search)

pub mod actions;
pub mod create_dialog;
pub mod list;
pub mod search;
pub mod triage;

use crate::agent;
use crate::tui::create_dialog::{CreateDialog, DialogAction};
use crate::tui::list::ListView;
use crate::tui::search::{SearchAction, SearchView};
use crate::tui::triage::{TriageAction, TriageView};
use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyEvent, KeyModifiers,
    },
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{
    Terminal,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Tabs},
};
use std::{
    io::{self, Stdout},
    path::Path,
    time::Duration,
};

// ---------------------------------------------------------------------------
// Active view enum
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum ActiveView {
    #[default]
    List,
    Triage,
    Search,
}

impl ActiveView {
    fn tab_index(self) -> usize {
        match self {
            Self::List => 0,
            Self::Triage => 1,
            Self::Search => 2,
        }
    }
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

struct TuiApp {
    db_path: std::path::PathBuf,
    project_root: std::path::PathBuf,
    list_view: ListView,
    triage_view: TriageView,
    search_view: SearchView,
    create_dialog: Option<CreateDialog>,
    active: ActiveView,
    should_quit: bool,
}

impl TuiApp {
    fn new(project_root: &Path) -> Result<Self> {
        let db_path = project_root.join(".bones/bones.db");
        Ok(Self {
            db_path: db_path.clone(),
            project_root: project_root.to_path_buf(),
            list_view: ListView::new(db_path.clone())?,
            triage_view: TriageView::new(db_path.clone())?,
            search_view: SearchView::new(db_path.clone())?,
            create_dialog: None,
            active: ActiveView::default(),
            should_quit: false,
        })
    }

    fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        // Global quit: Ctrl+C from any view
        if ctrl && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Ok(());
        }

        // If create dialog is open, route keys to it
        if let Some(ref mut dialog) = self.create_dialog {
            let action = dialog.handle_key(key)?;
            match action {
                None => {}
                Some(DialogAction::Cancel) => {
                    self.create_dialog = None;
                }
                Some(DialogAction::Create(title)) => {
                    let agent = agent::require_agent(None).unwrap_or_else(|_| "tui".to_string());
                    match actions::create_task(&self.project_root, &self.db_path, &agent, &title) {
                        Ok(id) => {
                            // Reload triage and list after creation
                            let _ = self.triage_view.reload();
                            let _ = self.list_view.reload();
                            self.triage_view.set_status(format!("Created: {id}"));
                        }
                        Err(e) => {
                            self.triage_view.set_status(format!("Create failed: {e}"));
                        }
                    }
                    self.create_dialog = None;
                }
                Some(DialogAction::LinkTo(id)) => {
                    // User chose to link/navigate to an existing item
                    self.create_dialog = None;
                    self.triage_view.set_status(format!("Selected existing: {id}"));
                }
            }
            return Ok(());
        }

        // Global view-switching shortcuts
        match key.code {
            KeyCode::Char('t') if self.active != ActiveView::Triage => {
                self.active = ActiveView::Triage;
                let _ = self.triage_view.reload();
                return Ok(());
            }
            KeyCode::Char('S') if self.active != ActiveView::Search => {
                self.active = ActiveView::Search;
                return Ok(());
            }
            KeyCode::Char('l') if self.active != ActiveView::List => {
                self.active = ActiveView::List;
                return Ok(());
            }
            _ => {}
        }

        // Route to active view
        match self.active {
            ActiveView::List => {
                self.list_view.handle_key(key)?;
                // Check if list view signals quit
                if self.list_view.should_quit() {
                    self.should_quit = true;
                }
            }
            ActiveView::Triage => {
                if let Some(action) = self.triage_view.handle_key(key)? {
                    self.handle_triage_action(action)?;
                }
            }
            ActiveView::Search => {
                if let Some(action) = self.search_view.handle_key(key)? {
                    self.handle_search_action(action);
                }
            }
        }

        Ok(())
    }

    fn handle_triage_action(&mut self, action: TriageAction) -> Result<()> {
        let agent = agent::require_agent(None).unwrap_or_else(|_| "tui".to_string());
        match action {
            TriageAction::Do(id) => {
                match actions::do_item(&self.project_root, &self.db_path, &agent, &id) {
                    Ok(()) => {
                        let _ = self.triage_view.reload();
                        self.triage_view.set_status(format!("{id} → doing"));
                    }
                    Err(e) => {
                        self.triage_view.set_status(format!("Error: {e}"));
                    }
                }
            }
            TriageAction::Done(id) => {
                match actions::done_item(&self.project_root, &self.db_path, &agent, &id) {
                    Ok(()) => {
                        let _ = self.triage_view.reload();
                        self.triage_view.set_status(format!("{id} → done ✓"));
                    }
                    Err(e) => {
                        self.triage_view.set_status(format!("Error: {e}"));
                    }
                }
            }
            TriageAction::Create => {
                self.create_dialog = Some(CreateDialog::new(self.db_path.clone()));
            }
        }
        Ok(())
    }

    fn handle_search_action(&mut self, action: SearchAction) {
        match action {
            SearchAction::Select(id) => {
                // Navigate back to list view and highlight the selected item
                self.active = ActiveView::List;
                self.list_view.set_status(format!("Selected: {id}"));
            }
        }
    }

    fn tick(&mut self) -> Result<()> {
        if self.active == ActiveView::Search {
            self.search_view.tick()?;
        }
        if let Some(ref mut dialog) = self.create_dialog {
            dialog.tick()?;
        }
        Ok(())
    }

    fn render(&mut self, frame: &mut ratatui::Frame) {
        let area = frame.area();

        // Tab bar (2 lines) + content
        let chunks = Layout::default()
            .direction(Direction::Vertical)
            .constraints([Constraint::Length(2), Constraint::Min(0)])
            .split(area);

        let tab_area = chunks[0];
        let content_area = chunks[1];

        // --- Tab bar ---
        let tab_titles = vec![
            Line::from(vec![
                Span::styled("l ", Style::default().fg(Color::DarkGray)),
                Span::raw("List"),
            ]),
            Line::from(vec![
                Span::styled("t ", Style::default().fg(Color::DarkGray)),
                Span::raw("Triage"),
            ]),
            Line::from(vec![
                Span::styled("S ", Style::default().fg(Color::DarkGray)),
                Span::raw("Search"),
            ]),
        ];
        let tabs = Tabs::new(tab_titles)
            .block(Block::default().borders(Borders::BOTTOM))
            .select(self.active.tab_index())
            .highlight_style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
            .divider(Span::raw("  "));
        frame.render_widget(tabs, tab_area);

        // --- Content area ---
        match self.active {
            ActiveView::List => {
                // Render list view (doesn't draw its own frame)
                self.list_view.render(frame, content_area);
            }
            ActiveView::Triage => {
                self.triage_view.render(frame, content_area);
            }
            ActiveView::Search => {
                self.search_view.render(frame, content_area);
            }
        }

        // --- Create dialog overlay ---
        if let Some(ref mut dialog) = self.create_dialog {
            dialog.render(frame, area);
        }
    }
}

// ---------------------------------------------------------------------------
// Terminal setup / teardown
// ---------------------------------------------------------------------------

fn setup_terminal() -> Result<Terminal<CrosstermBackend<Stdout>>> {
    enable_raw_mode().context("enable raw mode")?;
    let mut stdout = io::stdout();
    execute!(stdout, EnterAlternateScreen, EnableMouseCapture).context("enter alternate screen")?;
    let backend = CrosstermBackend::new(stdout);
    Terminal::new(backend).context("create terminal")
}

fn restore_terminal(terminal: &mut Terminal<CrosstermBackend<Stdout>>) -> Result<()> {
    disable_raw_mode().context("disable raw mode")?;
    execute!(
        terminal.backend_mut(),
        LeaveAlternateScreen,
        DisableMouseCapture
    )
    .context("leave alternate screen")?;
    terminal.show_cursor().context("show cursor")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Public entry points
// ---------------------------------------------------------------------------

/// Run the multi-view TUI to completion.
///
/// Starts in the List view. Press `t` for Triage, `S` for Search, `l` to
/// return to List, `q` to quit.
pub fn run_tui(project_root: &Path) -> Result<()> {
    let mut app = TuiApp::new(project_root)?;
    let mut terminal = setup_terminal()?;
    let result = run_loop(&mut terminal, &mut app);
    restore_terminal(&mut terminal)?;
    result
}

fn run_loop(terminal: &mut Terminal<CrosstermBackend<Stdout>>, app: &mut TuiApp) -> Result<()> {
    let tick_rate = Duration::from_millis(100);

    loop {
        terminal.draw(|frame| app.render(frame))?;

        if event::poll(tick_rate).context("event poll")? {
            if let Event::Key(key) = event::read().context("event read")? {
                app.handle_key(key)?;
            }
        } else {
            app.tick()?;
        }

        if app.should_quit {
            break;
        }
    }

    Ok(())
}
