//! bones TUI: nested bones list with optional detail pane.

mod actions;
pub mod list;

use crate::tui::list::ListView;
use anyhow::{Context, Result};
use crossterm::{
    event::{self, DisableMouseCapture, EnableMouseCapture, Event, KeyCode, KeyModifiers},
    execute,
    terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
};
use ratatui::{Terminal, backend::CrosstermBackend};
use std::{
    io::{self, IsTerminal, Stdout},
    path::Path,
    time::Duration,
};

struct TuiApp {
    list_view: ListView,
    should_quit: bool,
}

impl TuiApp {
    fn new(project_root: &Path) -> Result<Self> {
        let db_path = project_root.join(".bones/bones.db");
        Ok(Self {
            list_view: ListView::new(db_path)?,
            should_quit: false,
        })
    }

    fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        if ctrl && key.code == KeyCode::Char('c') {
            self.should_quit = true;
            return Ok(());
        }

        self.list_view.handle_key(key)?;
        if self.list_view.should_quit() {
            self.should_quit = true;
        }
        Ok(())
    }

    fn handle_mouse(&mut self, mouse: crossterm::event::MouseEvent) {
        self.list_view.handle_mouse(mouse);
    }

    fn tick(&mut self) -> Result<()> {
        self.list_view.tick()
    }

    fn render(&mut self, frame: &mut ratatui::Frame) {
        self.list_view.render(frame, frame.area());
    }
}

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

/// Run the bones TUI to completion.
pub fn run_tui(project_root: &Path) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        anyhow::bail!(
            "tui requires an interactive terminal (TTY). Use CLI commands like `bn list`, `bn search`, or `bn triage` in non-interactive environments"
        );
    }

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
            match event::read().context("event read")? {
                Event::Key(key) => app.handle_key(key)?,
                Event::Mouse(mouse) => app.handle_mouse(mouse),
                _ => {}
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
