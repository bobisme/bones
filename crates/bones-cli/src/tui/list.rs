//! TUI list view for bones work items.
//!
//! Provides a full-screen terminal UI with:
//! - Filterable, sortable item list
//! - Status bar showing item count and active filters
//! - Key bindings: j/k navigate, / search, f filter, s sort, r refresh, q quit

use anyhow::{Context, Result};
use bones_core::db::query::{self, ItemFilter, QueryItem, SortOrder};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState},
};
use std::{
    path::PathBuf,
    time::{Duration, Instant},
};

// ---------------------------------------------------------------------------
// Data types
// ---------------------------------------------------------------------------

/// Filter criteria applied to the item list.
#[derive(Debug, Clone, Default)]
pub struct FilterState {
    /// Filter by lifecycle state (open, doing, done, archived).
    pub state: Option<String>,
    /// Filter by item kind (task, goal, bug).
    pub kind: Option<String>,
    /// Filter by label (substring match on the label string).
    pub label: Option<String>,
    /// Filter by urgency (urgent, default, punt).
    pub urgency: Option<String>,
    /// Free-text search query (matches against title via substring).
    pub search_query: String,
}

impl FilterState {
    /// Returns true if no filter criteria are active.
    pub fn is_empty(&self) -> bool {
        self.state.is_none()
            && self.kind.is_none()
            && self.label.is_none()
            && self.urgency.is_none()
            && self.search_query.is_empty()
    }

    /// Apply this filter to a list of items.
    ///
    /// Returns a new vec containing only items that match all active criteria.
    pub fn apply(&self, items: &[WorkItem]) -> Vec<WorkItem> {
        items
            .iter()
            .filter(|item| self.matches(item))
            .cloned()
            .collect()
    }

    /// Returns true if the item satisfies all active filter criteria.
    pub fn matches(&self, item: &WorkItem) -> bool {
        if let Some(ref state) = self.state {
            if item.state != *state {
                return false;
            }
        }
        if let Some(ref kind) = self.kind {
            if item.kind != *kind {
                return false;
            }
        }
        if let Some(ref urgency) = self.urgency {
            if item.urgency != *urgency {
                return false;
            }
        }
        if let Some(ref label) = self.label {
            if !item.labels.iter().any(|l| l.contains(label.as_str())) {
                return false;
            }
        }
        if !self.search_query.is_empty() {
            let q = self.search_query.to_ascii_lowercase();
            if !item.title.to_ascii_lowercase().contains(&q)
                && !item.item_id.to_ascii_lowercase().contains(&q)
            {
                return false;
            }
        }
        true
    }
}

/// Sort field for the item list.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum SortField {
    /// Sort by priority: urgent → default → punt, then updated_at desc.
    Priority,
    /// Sort by created_at descending (newest first).
    Created,
    /// Sort by updated_at descending (most recently changed first).
    #[default]
    Updated,
}

impl SortField {
    fn label(self) -> &'static str {
        match self {
            Self::Priority => "priority",
            Self::Created => "created",
            Self::Updated => "updated",
        }
    }

    fn next(self) -> Self {
        match self {
            Self::Priority => Self::Created,
            Self::Created => Self::Updated,
            Self::Updated => Self::Priority,
        }
    }
}

/// A single item held in memory by the list view.
#[derive(Debug, Clone)]
pub struct WorkItem {
    pub item_id: String,
    pub title: String,
    pub kind: String,
    pub state: String,
    pub urgency: String,
    pub labels: Vec<String>,
    pub created_at_us: i64,
    pub updated_at_us: i64,
}

impl WorkItem {
    /// Construct from a `QueryItem` plus its label list.
    pub fn from_query(qi: QueryItem, labels: Vec<String>) -> Self {
        Self {
            item_id: qi.item_id,
            title: qi.title,
            kind: qi.kind,
            state: qi.state,
            urgency: qi.urgency,
            labels,
            created_at_us: qi.created_at_us,
            updated_at_us: qi.updated_at_us,
        }
    }
}

fn urgency_rank(u: &str) -> u8 {
    match u {
        "urgent" => 0,
        "default" => 1,
        "punt" => 2,
        _ => 3,
    }
}

/// Sort a mutable slice of `WorkItem` by the given `SortField`.
pub fn sort_items(items: &mut [WorkItem], sort: SortField) {
    items.sort_by(|a, b| match sort {
        SortField::Priority => urgency_rank(&a.urgency)
            .cmp(&urgency_rank(&b.urgency))
            .then_with(|| b.updated_at_us.cmp(&a.updated_at_us))
            .then_with(|| a.item_id.cmp(&b.item_id)),
        SortField::Created => b
            .created_at_us
            .cmp(&a.created_at_us)
            .then_with(|| a.item_id.cmp(&b.item_id)),
        SortField::Updated => b
            .updated_at_us
            .cmp(&a.updated_at_us)
            .then_with(|| a.item_id.cmp(&b.item_id)),
    });
}

// ---------------------------------------------------------------------------
// Application input modes
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum InputMode {
    #[default]
    Normal,
    /// User is typing a search query.
    Search,
    /// Filter popup is open.
    FilterPopup,
    /// Filter popup: editing a text field (label).
    FilterLabel,
}

// ---------------------------------------------------------------------------
// Application state
// ---------------------------------------------------------------------------

/// Current focus inside the filter popup.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum FilterField {
    #[default]
    State,
    Kind,
    Urgency,
    Label,
}

impl FilterField {
    fn next(self) -> Self {
        match self {
            Self::State => Self::Kind,
            Self::Kind => Self::Urgency,
            Self::Urgency => Self::Label,
            Self::Label => Self::State,
        }
    }

    fn prev(self) -> Self {
        match self {
            Self::State => Self::Label,
            Self::Kind => Self::State,
            Self::Urgency => Self::Kind,
            Self::Label => Self::Urgency,
        }
    }
}

/// Main application state for the TUI list view.
pub struct ListView {
    /// Path to the bones projection database.
    db_path: PathBuf,
    /// All items loaded from the projection (unfiltered, unsorted for display).
    all_items: Vec<WorkItem>,
    /// Items after filtering and sorting — this is what the table shows.
    visible_items: Vec<WorkItem>,
    /// Current filter criteria.
    pub filter: FilterState,
    /// Current sort order.
    pub sort: SortField,
    /// Table navigation state (selected row index in `visible_items`).
    table_state: TableState,
    /// Current input mode.
    input_mode: InputMode,
    /// Buffer for the search query being typed.
    search_buf: String,
    /// Buffer for the label filter being typed in the popup.
    label_buf: String,
    /// Current focus inside the filter popup.
    filter_field: FilterField,
    /// Whether to quit.
    should_quit: bool,
    /// Last refresh timestamp (for status bar).
    last_refresh: Instant,
    /// Whether a status message should be shown temporarily.
    status_msg: Option<(String, Instant)>,
}

impl ListView {
    /// Create a new list view, loading items from the given database.
    pub fn new(db_path: PathBuf) -> Result<Self> {
        let mut view = Self {
            db_path,
            all_items: Vec::new(),
            visible_items: Vec::new(),
            filter: FilterState::default(),
            sort: SortField::default(),
            table_state: TableState::default(),
            input_mode: InputMode::default(),
            search_buf: String::new(),
            label_buf: String::new(),
            filter_field: FilterField::default(),
            should_quit: false,
            last_refresh: Instant::now(),
            status_msg: None,
        };
        view.reload()?;
        Ok(view)
    }

    /// Load (or reload) all items from the projection database.
    pub fn reload(&mut self) -> Result<()> {
        let conn = match query::try_open_projection(&self.db_path)? {
            Some(c) => c,
            None => {
                self.all_items.clear();
                self.visible_items.clear();
                self.last_refresh = Instant::now();
                return Ok(());
            }
        };

        let filter = ItemFilter {
            include_deleted: false,
            sort: SortOrder::UpdatedDesc,
            ..Default::default()
        };

        let raw_items = query::list_items(&conn, &filter).context("list_items")?;
        self.all_items = raw_items
            .into_iter()
            .map(|qi| {
                let labels = query::get_labels(&conn, &qi.item_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|l| l.label)
                    .collect();
                WorkItem::from_query(qi, labels)
            })
            .collect();

        self.apply_filter_and_sort();
        self.last_refresh = Instant::now();
        Ok(())
    }

    /// Recompute `visible_items` from `all_items` using the current filter and sort.
    fn apply_filter_and_sort(&mut self) {
        let mut filtered = self.filter.apply(&self.all_items);
        sort_items(&mut filtered, self.sort);
        self.visible_items = filtered;

        // Clamp selection into valid range.
        let len = self.visible_items.len();
        match self.table_state.selected() {
            Some(i) if len == 0 => self.table_state.select(None),
            Some(i) if i >= len => self.table_state.select(Some(len.saturating_sub(1))),
            None if len > 0 => self.table_state.select(Some(0)),
            _ => {}
        }
    }

    // -----------------------------------------------------------------------
    // Navigation
    // -----------------------------------------------------------------------

    fn select_next(&mut self) {
        let len = self.visible_items.len();
        if len == 0 {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map_or(0, |i| if i + 1 >= len { len - 1 } else { i + 1 });
        self.table_state.select(Some(i));
    }

    fn select_prev(&mut self) {
        let len = self.visible_items.len();
        if len == 0 {
            return;
        }
        let i = self
            .table_state
            .selected()
            .map_or(0, |i| i.saturating_sub(1));
        self.table_state.select(Some(i));
    }

    fn select_first(&mut self) {
        if !self.visible_items.is_empty() {
            self.table_state.select(Some(0));
        }
    }

    fn select_last(&mut self) {
        let len = self.visible_items.len();
        if len > 0 {
            self.table_state.select(Some(len - 1));
        }
    }

    /// Currently selected item (if any).
    pub fn selected_item(&self) -> Option<&WorkItem> {
        self.table_state
            .selected()
            .and_then(|i| self.visible_items.get(i))
    }

    // -----------------------------------------------------------------------
    // Key event handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match self.input_mode {
            InputMode::Search => self.handle_search_key(key),
            InputMode::FilterPopup => self.handle_filter_popup_key(key),
            InputMode::FilterLabel => self.handle_filter_label_key(key),
            InputMode::Normal => self.handle_normal_key(key, ctrl),
        }

        Ok(())
    }

    fn handle_normal_key(&mut self, key: KeyEvent, ctrl: bool) {
        match key.code {
            // Quit
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,

            // Navigation
            KeyCode::Char('j') | KeyCode::Down => self.select_next(),
            KeyCode::Char('k') | KeyCode::Up => self.select_prev(),
            KeyCode::Char('g') | KeyCode::Home => self.select_first(),
            KeyCode::Char('G') | KeyCode::End => self.select_last(),

            // Page scroll
            KeyCode::PageDown | KeyCode::Char('d') => {
                for _ in 0..10 {
                    self.select_next();
                }
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                for _ in 0..10 {
                    self.select_prev();
                }
            }

            // Enter: surface the current selection in the status line
            KeyCode::Enter => {
                if let Some(item) = self.selected_item() {
                    let id = item.item_id.clone();
                    self.set_status(format!("Selected: {id}"));
                }
            }

            // Search
            KeyCode::Char('/') => {
                self.search_buf = self.filter.search_query.clone();
                self.input_mode = InputMode::Search;
            }

            // Filter popup
            KeyCode::Char('f') => {
                self.label_buf = self.filter.label.clone().unwrap_or_default();
                self.filter_field = FilterField::default();
                self.input_mode = InputMode::FilterPopup;
            }

            // Cycle sort order
            KeyCode::Char('s') => {
                self.sort = self.sort.next();
                self.apply_filter_and_sort();
                self.set_status(format!("Sort: {}", self.sort.label()));
            }

            // Refresh
            KeyCode::Char('r') => {
                if let Err(e) = self.reload() {
                    self.set_status(format!("Reload error: {e}"));
                } else {
                    self.set_status(format!("Refreshed — {} items", self.all_items.len()));
                }
            }

            // Clear filter
            KeyCode::Esc => {
                if !self.filter.is_empty() {
                    self.filter = FilterState::default();
                    self.apply_filter_and_sort();
                    self.set_status("Filters cleared".to_string());
                }
            }

            _ => {}
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                // Cancel search — restore previous query
                self.search_buf = self.filter.search_query.clone();
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Enter => {
                // Confirm search
                self.filter.search_query = self.search_buf.clone();
                self.apply_filter_and_sort();
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Backspace => {
                self.search_buf.pop();
                self.filter.search_query = self.search_buf.clone();
                self.apply_filter_and_sort();
            }
            KeyCode::Char(c) => {
                self.search_buf.push(c);
                self.filter.search_query = self.search_buf.clone();
                self.apply_filter_and_sort();
            }
            _ => {}
        }
    }

    fn handle_filter_popup_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Char('f') => {
                // Pressing 'f' again applies and closes
                self.commit_label_filter();
                self.apply_filter_and_sort();
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Enter => {
                if self.filter_field == FilterField::Label {
                    // Enter on the label field → edit mode
                    self.input_mode = InputMode::FilterLabel;
                } else {
                    // Enter elsewhere → apply and close
                    self.commit_label_filter();
                    self.apply_filter_and_sort();
                    self.input_mode = InputMode::Normal;
                }
            }
            // Navigate fields
            KeyCode::Tab | KeyCode::Down | KeyCode::Char('j') => {
                self.filter_field = self.filter_field.next();
            }
            KeyCode::BackTab | KeyCode::Up | KeyCode::Char('k') => {
                self.filter_field = self.filter_field.prev();
            }
            // Cycle enum values for state/kind/urgency, or enter text for label
            KeyCode::Right | KeyCode::Char('l') | KeyCode::Char(' ') => {
                self.cycle_filter_field_forward();
            }
            KeyCode::Left | KeyCode::Char('h') => {
                self.cycle_filter_field_backward();
            }
            _ => {}
        }
    }

    /// Commit the label buffer to the active filter.
    fn commit_label_filter(&mut self) {
        self.filter.label = if self.label_buf.trim().is_empty() {
            None
        } else {
            Some(self.label_buf.trim().to_string())
        };
    }

    fn handle_filter_label_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Enter => {
                self.input_mode = InputMode::FilterPopup;
            }
            KeyCode::Backspace => {
                self.label_buf.pop();
            }
            KeyCode::Char(c) => {
                self.label_buf.push(c);
            }
            _ => {}
        }
    }

    fn cycle_filter_field_forward(&mut self) {
        match self.filter_field {
            FilterField::State => {
                self.filter.state = cycle_option(
                    self.filter.state.as_deref(),
                    &["open", "doing", "done", "archived"],
                );
                self.apply_filter_and_sort();
            }
            FilterField::Kind => {
                self.filter.kind =
                    cycle_option(self.filter.kind.as_deref(), &["task", "goal", "bug"]);
                self.apply_filter_and_sort();
            }
            FilterField::Urgency => {
                self.filter.urgency = cycle_option(
                    self.filter.urgency.as_deref(),
                    &["urgent", "default", "punt"],
                );
                self.apply_filter_and_sort();
            }
            FilterField::Label => {
                self.input_mode = InputMode::FilterLabel;
            }
        }
    }

    fn cycle_filter_field_backward(&mut self) {
        match self.filter_field {
            FilterField::State => {
                self.filter.state = cycle_option_rev(
                    self.filter.state.as_deref(),
                    &["open", "doing", "done", "archived"],
                );
                self.apply_filter_and_sort();
            }
            FilterField::Kind => {
                self.filter.kind =
                    cycle_option_rev(self.filter.kind.as_deref(), &["task", "goal", "bug"]);
                self.apply_filter_and_sort();
            }
            FilterField::Urgency => {
                self.filter.urgency = cycle_option_rev(
                    self.filter.urgency.as_deref(),
                    &["urgent", "default", "punt"],
                );
                self.apply_filter_and_sort();
            }
            FilterField::Label => {}
        }
    }

    pub fn set_status(&mut self, msg: String) {
        self.status_msg = Some((msg, Instant::now()));
    }

    /// Returns true if the list view has been asked to quit (e.g. 'q' key).
    pub fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// Render the list view into `area` within the given frame.
    pub fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        render_into(frame, self, area);
    }
}

// ---------------------------------------------------------------------------
// Cycle helpers for filter popup
// ---------------------------------------------------------------------------

/// Cycle through `options`, wrapping around.
///
/// `current = None` → first option; last option → `None` (clear filter).
fn cycle_option(current: Option<&str>, options: &[&str]) -> Option<String> {
    match current {
        None => options.first().map(|s| (*s).to_string()),
        Some(c) => {
            let pos = options.iter().position(|&s| s == c);
            match pos {
                None => None,
                Some(p) if p + 1 >= options.len() => None,
                Some(p) => Some(options[p + 1].to_string()),
            }
        }
    }
}

fn cycle_option_rev(current: Option<&str>, options: &[&str]) -> Option<String> {
    match current {
        None => options.last().map(|s| (*s).to_string()),
        Some(c) => {
            let pos = options.iter().position(|&s| s == c);
            match pos {
                None | Some(0) => None,
                Some(p) => Some(options[p - 1].to_string()),
            }
        }
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Color for a state value.
fn state_color(state: &str) -> Color {
    match state {
        "open" => Color::Cyan,
        "doing" => Color::Green,
        "done" => Color::DarkGray,
        "archived" => Color::DarkGray,
        _ => Color::White,
    }
}

/// Color for an urgency value.
fn urgency_color(urgency: &str) -> Color {
    match urgency {
        "urgent" => Color::Red,
        "default" => Color::White,
        "punt" => Color::DarkGray,
        _ => Color::White,
    }
}

/// Color for a kind value.
fn kind_color(kind: &str) -> Color {
    match kind {
        "bug" => Color::Red,
        "goal" => Color::Cyan,
        "task" => Color::White,
        _ => Color::White,
    }
}

/// Truncate a string to at most `max_chars`, appending '…' if truncated.
fn truncate(s: &str, max_chars: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max_chars {
        s.to_string()
    } else if max_chars == 0 {
        String::new()
    } else {
        let truncated: String = chars[..max_chars.saturating_sub(1)].iter().collect();
        format!("{truncated}…")
    }
}

/// Build one table `Row` from a `WorkItem`.
fn build_row(item: &WorkItem, title_width: u16) -> Row<'static> {
    let id_cell = Cell::from(item.item_id.clone());
    let title_cell = Cell::from(truncate(&item.title, title_width as usize));
    let state_cell =
        Cell::from(item.state.clone()).style(Style::default().fg(state_color(&item.state)));
    let kind_cell =
        Cell::from(item.kind.clone()).style(Style::default().fg(kind_color(&item.kind)));
    let urgency_cell =
        Cell::from(item.urgency.clone()).style(Style::default().fg(urgency_color(&item.urgency)));
    let labels_str = item.labels.join(", ");
    let labels_cell = Cell::from(labels_str);

    Row::new([
        id_cell,
        title_cell,
        state_cell,
        kind_cell,
        urgency_cell,
        labels_cell,
    ])
}

/// Render the list view into a specific area of the frame.
fn render_into(frame: &mut ratatui::Frame<'_>, app: &mut ListView, area: Rect) {
    // Layout: main table + status bar (3 lines at bottom).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(3)])
        .split(area);

    let table_area = chunks[0];
    let status_area = chunks[1];

    // Fixed column widths; title gets remaining space.
    let id_w: u16 = 14;
    let state_w: u16 = 10;
    let kind_w: u16 = 6;
    let urgency_w: u16 = 9;
    let labels_w: u16 = 24;
    let borders: u16 = 2; // left + right border
    let separators: u16 = 5; // between 6 columns
    let total_fixed = id_w + state_w + kind_w + urgency_w + labels_w + borders + separators;
    let title_w = area.width.saturating_sub(total_fixed).max(10);

    let widths = [
        Constraint::Length(id_w),
        Constraint::Length(title_w),
        Constraint::Length(state_w),
        Constraint::Length(kind_w),
        Constraint::Length(urgency_w),
        Constraint::Length(labels_w),
    ];

    let header_cells = ["ID", "TITLE", "STATE", "KIND", "URGENCY", "LABELS"].map(|h| {
        Cell::from(h).style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
    });
    let header = Row::new(header_cells).height(1).bottom_margin(0);

    let rows: Vec<Row<'static>> = app
        .visible_items
        .iter()
        .map(|item| build_row(item, title_w))
        .collect();

    let block_title = match app.input_mode {
        InputMode::Search => format!(" bones — search: {} ", app.search_buf),
        _ => format!(
            " bones — {} of {} items  [sort: {}] ",
            app.visible_items.len(),
            app.all_items.len(),
            app.sort.label()
        ),
    };

    let table = Table::new(rows, widths)
        .header(header)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(block_title)
                .title_style(Style::default().add_modifier(Modifier::BOLD)),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol("► ");

    frame.render_stateful_widget(table, table_area, &mut app.table_state);

    // -----------------------------------------------------------------------
    // Status bar
    // -----------------------------------------------------------------------
    let status_text = build_status_bar(app);
    let status_paragraph = Paragraph::new(status_text)
        .block(Block::default().borders(Borders::ALL))
        .alignment(Alignment::Left);
    frame.render_widget(status_paragraph, status_area);

    // -----------------------------------------------------------------------
    // Filter popup overlay
    // -----------------------------------------------------------------------
    if app.input_mode == InputMode::FilterPopup || app.input_mode == InputMode::FilterLabel {
        render_filter_popup(frame, app, area);
    }
}

/// Build the status bar line from current filter state.
fn build_status_bar(app: &ListView) -> Line<'static> {
    // Show a transient status message if recent (< 3 seconds).
    if let Some((ref msg, at)) = app.status_msg {
        if at.elapsed() < Duration::from_secs(3) {
            return Line::from(vec![Span::styled(
                msg.clone(),
                Style::default().fg(Color::Cyan),
            )]);
        }
    }

    let mut spans: Vec<Span<'static>> = Vec::new();

    let key_style = Style::default()
        .fg(Color::Yellow)
        .add_modifier(Modifier::BOLD);
    let label_style = Style::default().fg(Color::White);
    let val_style = Style::default().fg(Color::Cyan);
    let dim_style = Style::default().fg(Color::DarkGray);

    match app.input_mode {
        InputMode::Search => {
            spans.push(Span::styled("ESC", key_style));
            spans.push(Span::styled(" cancel  ", dim_style));
            spans.push(Span::styled("ENTER", key_style));
            spans.push(Span::styled(" confirm search", dim_style));
        }
        _ => {
            // Filter summary
            if !app.filter.is_empty() {
                spans.push(Span::styled("FILTERS: ", label_style));
                if let Some(ref s) = app.filter.state {
                    spans.push(Span::styled(format!("state={s} "), val_style));
                }
                if let Some(ref k) = app.filter.kind {
                    spans.push(Span::styled(format!("kind={k} "), val_style));
                }
                if let Some(ref u) = app.filter.urgency {
                    spans.push(Span::styled(format!("urgency={u} "), val_style));
                }
                if let Some(ref l) = app.filter.label {
                    spans.push(Span::styled(format!("label={l} "), val_style));
                }
                if !app.filter.search_query.is_empty() {
                    spans.push(Span::styled(
                        format!("search={} ", app.filter.search_query),
                        val_style,
                    ));
                }
                spans.push(Span::styled("  ", dim_style));
            }

            // Key hints
            let hints = [
                ("j/k", "nav"),
                ("/", "search"),
                ("f", "filter"),
                ("s", "sort"),
                ("r", "refresh"),
                ("ESC", "clear"),
                ("q", "quit"),
            ];
            for (key, desc) in &hints {
                spans.push(Span::styled((*key).to_string(), key_style));
                spans.push(Span::styled(format!(" {desc}  "), dim_style));
            }
        }
    }

    Line::from(spans)
}

/// Render the filter configuration popup.
fn render_filter_popup(frame: &mut ratatui::Frame<'_>, app: &ListView, area: Rect) {
    // Center the popup.
    let popup_w: u16 = 52;
    let popup_h: u16 = 12;
    let x = area.x + area.width.saturating_sub(popup_w) / 2;
    let y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup_area = Rect {
        x,
        y,
        width: popup_w.min(area.width),
        height: popup_h.min(area.height),
    };

    frame.render_widget(Clear, popup_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Filter ")
        .title_style(
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(Color::Black));
    frame.render_widget(block, popup_area);

    // Build inner area.
    let inner = Rect {
        x: popup_area.x + 1,
        y: popup_area.y + 1,
        width: popup_area.width.saturating_sub(2),
        height: popup_area.height.saturating_sub(2),
    };

    let focused_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let normal_style = Style::default().fg(Color::White);
    let dim_style = Style::default().fg(Color::DarkGray);
    let val_style = Style::default().fg(Color::Green);

    let fields = [
        (FilterField::State, "State ", &app.filter.state),
        (FilterField::Kind, "Kind  ", &app.filter.kind),
        (FilterField::Urgency, "Urgency", &app.filter.urgency),
    ];

    for (i, (field, label, value)) in fields.iter().enumerate() {
        let row_y = inner.y + i as u16;
        if row_y >= inner.y + inner.height {
            break;
        }
        let row_area = Rect {
            y: row_y,
            height: 1,
            ..inner
        };

        let is_focused = app.filter_field == *field;
        let label_style = if is_focused {
            focused_style
        } else {
            normal_style
        };
        let prefix = if is_focused { "► " } else { "  " };

        let val_display = value.as_deref().unwrap_or("(any)");
        let line = Line::from(vec![
            Span::styled(prefix.to_string(), focused_style),
            Span::styled((*label).to_string(), label_style),
            Span::styled(": ".to_string(), dim_style),
            Span::styled(val_display.to_string(), val_style),
            Span::styled("  ←/→ cycle".to_string(), dim_style),
        ]);
        frame.render_widget(Paragraph::new(line), row_area);
    }

    // Label field
    let label_row_y = inner.y + 3;
    if label_row_y < inner.y + inner.height {
        let is_focused = app.filter_field == FilterField::Label;
        let label_style = if is_focused {
            focused_style
        } else {
            normal_style
        };
        let prefix = if is_focused { "► " } else { "  " };
        let editing = app.input_mode == InputMode::FilterLabel;
        let val_display = if app.label_buf.is_empty() {
            "(any)".to_string()
        } else {
            app.label_buf.clone()
        };
        let cursor = if editing && is_focused { "_" } else { "" };
        let line = Line::from(vec![
            Span::styled(prefix.to_string(), focused_style),
            Span::styled("Label  ".to_string(), label_style),
            Span::styled(": ".to_string(), dim_style),
            Span::styled(format!("{val_display}{cursor}"), val_style),
            if editing {
                Span::styled("  type to edit, Enter done".to_string(), dim_style)
            } else {
                Span::styled("  Enter to edit".to_string(), dim_style)
            },
        ]);
        frame.render_widget(
            Paragraph::new(line),
            Rect {
                y: label_row_y,
                height: 1,
                ..inner
            },
        );
    }

    // Footer hints
    let footer_y = inner.y + inner.height.saturating_sub(2);
    if footer_y < inner.y + inner.height {
        let hints = Line::from(vec![
            Span::styled("Tab", Style::default().fg(Color::Yellow)),
            Span::styled("/", dim_style),
            Span::styled("Shift+Tab", Style::default().fg(Color::Yellow)),
            Span::styled(" navigate  ", dim_style),
            Span::styled("Enter", Style::default().fg(Color::Yellow)),
            Span::styled(" apply  ", dim_style),
            Span::styled("Esc", Style::default().fg(Color::Yellow)),
            Span::styled(" cancel", dim_style),
        ]);
        frame.render_widget(
            Paragraph::new(hints),
            Rect {
                y: footer_y,
                height: 1,
                ..inner
            },
        );
    }
}

// ---------------------------------------------------------------------------
// Unit tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // WorkItem helpers
    // -----------------------------------------------------------------------

    fn make_item(
        id: &str,
        title: &str,
        state: &str,
        kind: &str,
        urgency: &str,
        labels: Vec<&str>,
        created: i64,
        updated: i64,
    ) -> WorkItem {
        WorkItem {
            item_id: id.to_string(),
            title: title.to_string(),
            kind: kind.to_string(),
            state: state.to_string(),
            urgency: urgency.to_string(),
            labels: labels.into_iter().map(String::from).collect(),
            created_at_us: created,
            updated_at_us: updated,
        }
    }

    // -----------------------------------------------------------------------
    // FilterState tests
    // -----------------------------------------------------------------------

    #[test]
    fn filter_state_empty_matches_all() {
        let filter = FilterState::default();
        let item = make_item(
            "bn-001",
            "hello",
            "open",
            "task",
            "default",
            vec![],
            100,
            200,
        );
        assert!(filter.matches(&item));
        assert!(filter.is_empty());
    }

    #[test]
    fn filter_state_by_state() {
        let mut filter = FilterState::default();
        filter.state = Some("open".to_string());

        let open = make_item(
            "bn-001",
            "open item",
            "open",
            "task",
            "default",
            vec![],
            100,
            200,
        );
        let doing = make_item(
            "bn-002",
            "doing item",
            "doing",
            "task",
            "default",
            vec![],
            100,
            200,
        );

        assert!(filter.matches(&open));
        assert!(!filter.matches(&doing));
    }

    #[test]
    fn filter_state_by_kind() {
        let mut filter = FilterState::default();
        filter.kind = Some("bug".to_string());

        let bug = make_item(
            "bn-001",
            "a bug",
            "open",
            "bug",
            "default",
            vec![],
            100,
            200,
        );
        let task = make_item(
            "bn-002",
            "a task",
            "open",
            "task",
            "default",
            vec![],
            100,
            200,
        );

        assert!(filter.matches(&bug));
        assert!(!filter.matches(&task));
    }

    #[test]
    fn filter_state_by_urgency() {
        let mut filter = FilterState::default();
        filter.urgency = Some("urgent".to_string());

        let urgent = make_item(
            "bn-001",
            "urgent",
            "open",
            "task",
            "urgent",
            vec![],
            100,
            200,
        );
        let default = make_item(
            "bn-002",
            "default",
            "open",
            "task",
            "default",
            vec![],
            100,
            200,
        );

        assert!(filter.matches(&urgent));
        assert!(!filter.matches(&default));
    }

    #[test]
    fn filter_state_by_label() {
        let mut filter = FilterState::default();
        filter.label = Some("backend".to_string());

        let with_label = make_item(
            "bn-001",
            "item",
            "open",
            "task",
            "default",
            vec!["backend", "auth"],
            100,
            200,
        );
        let without_label = make_item(
            "bn-002",
            "item2",
            "open",
            "task",
            "default",
            vec![],
            100,
            200,
        );

        assert!(filter.matches(&with_label));
        assert!(!filter.matches(&without_label));
    }

    #[test]
    fn filter_state_by_label_partial_match() {
        let mut filter = FilterState::default();
        filter.label = Some("area".to_string());

        let item = make_item(
            "bn-001",
            "item",
            "open",
            "task",
            "default",
            vec!["area:backend"],
            100,
            200,
        );
        assert!(filter.matches(&item));
    }

    #[test]
    fn filter_state_by_search_query() {
        let mut filter = FilterState::default();
        filter.search_query = "auth".to_string();

        let matching = make_item(
            "bn-001",
            "Fix authentication bug",
            "open",
            "task",
            "default",
            vec![],
            100,
            200,
        );
        let non_matching = make_item(
            "bn-002",
            "Update documentation",
            "open",
            "task",
            "default",
            vec![],
            100,
            200,
        );

        assert!(filter.matches(&matching));
        assert!(!filter.matches(&non_matching));
    }

    #[test]
    fn filter_state_search_case_insensitive() {
        let mut filter = FilterState::default();
        filter.search_query = "AUTH".to_string();

        let item = make_item(
            "bn-001",
            "fix auth timeout",
            "open",
            "task",
            "default",
            vec![],
            100,
            200,
        );
        assert!(filter.matches(&item));
    }

    #[test]
    fn filter_state_search_matches_item_id() {
        let mut filter = FilterState::default();
        filter.search_query = "bn-001".to_string();

        let item = make_item(
            "bn-001",
            "unrelated title",
            "open",
            "task",
            "default",
            vec![],
            100,
            200,
        );
        assert!(filter.matches(&item));
    }

    #[test]
    fn filter_state_combined_and_semantics() {
        let mut filter = FilterState::default();
        filter.state = Some("open".to_string());
        filter.urgency = Some("urgent".to_string());

        // Matches both criteria
        let both = make_item("bn-001", "x", "open", "task", "urgent", vec![], 100, 200);
        // Only one matches
        let only_state = make_item("bn-002", "x", "open", "task", "default", vec![], 100, 200);
        let only_urgency = make_item("bn-003", "x", "doing", "task", "urgent", vec![], 100, 200);

        assert!(filter.matches(&both));
        assert!(!filter.matches(&only_state));
        assert!(!filter.matches(&only_urgency));
    }

    #[test]
    fn filter_apply_returns_matching_subset() {
        let filter = FilterState {
            state: Some("open".to_string()),
            ..Default::default()
        };
        let items = vec![
            make_item(
                "bn-001",
                "open",
                "open",
                "task",
                "default",
                vec![],
                100,
                200,
            ),
            make_item(
                "bn-002",
                "doing",
                "doing",
                "task",
                "default",
                vec![],
                101,
                201,
            ),
            make_item("bn-003", "open2", "open", "bug", "urgent", vec![], 102, 202),
        ];
        let result = filter.apply(&items);
        assert_eq!(result.len(), 2);
        assert!(result.iter().all(|i| i.state == "open"));
    }

    // -----------------------------------------------------------------------
    // Sort tests
    // -----------------------------------------------------------------------

    #[test]
    fn sort_priority_orders_urgent_first() {
        let mut items = vec![
            make_item("bn-001", "punt", "open", "task", "punt", vec![], 100, 200),
            make_item(
                "bn-002",
                "default",
                "open",
                "task",
                "default",
                vec![],
                100,
                200,
            ),
            make_item(
                "bn-003",
                "urgent",
                "open",
                "task",
                "urgent",
                vec![],
                100,
                200,
            ),
        ];
        sort_items(&mut items, SortField::Priority);
        assert_eq!(items[0].urgency, "urgent");
        assert_eq!(items[1].urgency, "default");
        assert_eq!(items[2].urgency, "punt");
    }

    #[test]
    fn sort_updated_desc_orders_newest_first() {
        let mut items = vec![
            make_item("bn-001", "old", "open", "task", "default", vec![], 100, 100),
            make_item("bn-002", "new", "open", "task", "default", vec![], 100, 300),
            make_item("bn-003", "mid", "open", "task", "default", vec![], 100, 200),
        ];
        sort_items(&mut items, SortField::Updated);
        assert_eq!(items[0].updated_at_us, 300);
        assert_eq!(items[1].updated_at_us, 200);
        assert_eq!(items[2].updated_at_us, 100);
    }

    #[test]
    fn sort_created_desc_orders_newest_first() {
        let mut items = vec![
            make_item(
                "bn-001",
                "oldest",
                "open",
                "task",
                "default",
                vec![],
                100,
                500,
            ),
            make_item(
                "bn-002",
                "newest",
                "open",
                "task",
                "default",
                vec![],
                300,
                500,
            ),
            make_item(
                "bn-003",
                "middle",
                "open",
                "task",
                "default",
                vec![],
                200,
                500,
            ),
        ];
        sort_items(&mut items, SortField::Created);
        assert_eq!(items[0].created_at_us, 300);
        assert_eq!(items[1].created_at_us, 200);
        assert_eq!(items[2].created_at_us, 100);
    }

    #[test]
    fn sort_stable_tie_breaks_by_id() {
        let mut items = vec![
            make_item("bn-zzz", "z", "open", "task", "default", vec![], 100, 200),
            make_item("bn-aaa", "a", "open", "task", "default", vec![], 100, 200),
        ];
        sort_items(&mut items, SortField::Updated);
        assert_eq!(items[0].item_id, "bn-aaa");
        assert_eq!(items[1].item_id, "bn-zzz");
    }

    #[test]
    fn sort_priority_tie_breaks_by_updated_then_id() {
        let mut items = vec![
            make_item("bn-002", "b", "open", "task", "urgent", vec![], 100, 100),
            make_item("bn-001", "a", "open", "task", "urgent", vec![], 100, 200),
        ];
        sort_items(&mut items, SortField::Priority);
        // Both urgent; bn-001 has higher updated_at_us, so comes first
        assert_eq!(items[0].item_id, "bn-001");
    }

    // -----------------------------------------------------------------------
    // Cycle option tests
    // -----------------------------------------------------------------------

    #[test]
    fn cycle_option_from_none_goes_to_first() {
        let result = cycle_option(None, &["open", "doing", "done"]);
        assert_eq!(result.as_deref(), Some("open"));
    }

    #[test]
    fn cycle_option_from_last_goes_to_none() {
        let result = cycle_option(Some("done"), &["open", "doing", "done"]);
        assert_eq!(result, None);
    }

    #[test]
    fn cycle_option_advances_forward() {
        let result = cycle_option(Some("open"), &["open", "doing", "done"]);
        assert_eq!(result.as_deref(), Some("doing"));
    }

    #[test]
    fn cycle_option_rev_from_none_goes_to_last() {
        let result = cycle_option_rev(None, &["open", "doing", "done"]);
        assert_eq!(result.as_deref(), Some("done"));
    }

    #[test]
    fn cycle_option_rev_from_first_goes_to_none() {
        let result = cycle_option_rev(Some("open"), &["open", "doing", "done"]);
        assert_eq!(result, None);
    }

    #[test]
    fn cycle_option_rev_advances_backward() {
        let result = cycle_option_rev(Some("done"), &["open", "doing", "done"]);
        assert_eq!(result.as_deref(), Some("doing"));
    }

    // -----------------------------------------------------------------------
    // Truncate tests
    // -----------------------------------------------------------------------

    #[test]
    fn truncate_short_string_unchanged() {
        assert_eq!(truncate("hello", 10), "hello");
    }

    #[test]
    fn truncate_exact_length_unchanged() {
        assert_eq!(truncate("hello", 5), "hello");
    }

    #[test]
    fn truncate_long_string_gets_ellipsis() {
        let result = truncate("hello world", 8);
        assert!(result.ends_with('…'));
        let char_len: usize = result.chars().count();
        assert_eq!(char_len, 8);
    }

    #[test]
    fn truncate_zero_width_returns_empty() {
        assert_eq!(truncate("hello", 0), "");
    }

    // -----------------------------------------------------------------------
    // SortField cycling
    // -----------------------------------------------------------------------

    #[test]
    fn sort_field_cycles_through_all_variants() {
        let start = SortField::Priority;
        let s1 = start.next();
        let s2 = s1.next();
        let s3 = s2.next();
        assert_eq!(s1, SortField::Created);
        assert_eq!(s2, SortField::Updated);
        assert_eq!(s3, SortField::Priority);
    }

    // -----------------------------------------------------------------------
    // ListView navigation (no DB needed — operate on pre-loaded data)
    // -----------------------------------------------------------------------

    fn make_list_view() -> ListView {
        let mut view = ListView {
            db_path: PathBuf::from("/nonexistent"),
            all_items: vec![
                make_item(
                    "bn-001",
                    "First",
                    "open",
                    "task",
                    "urgent",
                    vec![],
                    100,
                    300,
                ),
                make_item(
                    "bn-002",
                    "Second",
                    "doing",
                    "task",
                    "default",
                    vec![],
                    200,
                    200,
                ),
                make_item(
                    "bn-003",
                    "Third",
                    "done",
                    "bug",
                    "punt",
                    vec!["fix"],
                    300,
                    100,
                ),
            ],
            visible_items: Vec::new(),
            filter: FilterState::default(),
            sort: SortField::default(),
            table_state: TableState::default(),
            input_mode: InputMode::Normal,
            search_buf: String::new(),
            label_buf: String::new(),
            filter_field: FilterField::default(),
            should_quit: false,
            last_refresh: Instant::now(),
            status_msg: None,
        };
        view.apply_filter_and_sort();
        view
    }

    #[test]
    fn list_view_initial_selection_is_first_item() {
        let view = make_list_view();
        assert_eq!(view.table_state.selected(), Some(0));
    }

    #[test]
    fn list_view_select_next_advances() {
        let mut view = make_list_view();
        view.select_next();
        assert_eq!(view.table_state.selected(), Some(1));
    }

    #[test]
    fn list_view_select_next_does_not_wrap_at_end() {
        let mut view = make_list_view();
        view.select_last();
        view.select_next();
        assert_eq!(view.table_state.selected(), Some(2)); // stays at last
    }

    #[test]
    fn list_view_select_prev_does_not_wrap_at_start() {
        let mut view = make_list_view();
        view.select_first();
        view.select_prev();
        assert_eq!(view.table_state.selected(), Some(0)); // stays at first
    }

    #[test]
    fn list_view_filter_reduces_visible_items() {
        let mut view = make_list_view();
        view.filter.state = Some("open".to_string());
        view.apply_filter_and_sort();
        assert_eq!(view.visible_items.len(), 1);
        assert_eq!(view.visible_items[0].item_id, "bn-001");
    }

    #[test]
    fn list_view_filter_clamp_selection_after_filter() {
        let mut view = make_list_view();
        view.select_last(); // index 2
        view.filter.state = Some("open".to_string());
        view.apply_filter_and_sort();
        // Only 1 item left; selection should clamp to 0
        assert_eq!(view.table_state.selected(), Some(0));
    }

    #[test]
    fn list_view_selected_item_returns_correct_item() {
        let mut view = make_list_view();
        view.select_next();
        let item = view.selected_item().expect("item");
        // After UpdatedDesc sort, order should be bn-001 (300) > bn-002 (200) > bn-003 (100)
        assert_eq!(item.item_id, "bn-002");
    }

    #[test]
    fn list_view_empty_items_no_selection() {
        let mut view = ListView {
            db_path: PathBuf::from("/nonexistent"),
            all_items: Vec::new(),
            visible_items: Vec::new(),
            filter: FilterState::default(),
            sort: SortField::default(),
            table_state: TableState::default(),
            input_mode: InputMode::Normal,
            search_buf: String::new(),
            label_buf: String::new(),
            filter_field: FilterField::default(),
            should_quit: false,
            last_refresh: Instant::now(),
            status_msg: None,
        };
        view.apply_filter_and_sort();
        assert_eq!(view.table_state.selected(), None);
    }

    #[test]
    fn list_view_q_key_quits() {
        let mut view = make_list_view();
        view.handle_key(KeyEvent::new(KeyCode::Char('q'), KeyModifiers::NONE))
            .unwrap();
        assert!(view.should_quit);
    }

    #[test]
    fn list_view_s_key_cycles_sort() {
        let mut view = make_list_view();
        assert_eq!(view.sort, SortField::Updated);
        view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.sort, SortField::Priority);
        view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.sort, SortField::Created);
    }

    #[test]
    fn list_view_search_mode_enters_and_filters() {
        let mut view = make_list_view();
        // Start search
        view.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.input_mode, InputMode::Search);

        // Type characters
        for c in "First".chars() {
            view.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE))
                .unwrap();
        }
        assert_eq!(view.filter.search_query, "First");
        assert_eq!(view.visible_items.len(), 1);
    }

    #[test]
    fn list_view_search_esc_cancels() {
        let mut view = make_list_view();
        view.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE))
            .unwrap();
        view.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE))
            .unwrap();
        // Cancel restores original search_query
        view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.input_mode, InputMode::Normal);
    }

    #[test]
    fn list_view_esc_clears_filters() {
        let mut view = make_list_view();
        view.filter.state = Some("open".to_string());
        view.apply_filter_and_sort();
        assert_eq!(view.visible_items.len(), 1);

        view.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE))
            .unwrap();
        assert!(view.filter.is_empty());
        assert_eq!(view.visible_items.len(), 3); // all restored
    }

    #[test]
    fn list_view_f_key_opens_filter_popup() {
        let mut view = make_list_view();
        view.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.input_mode, InputMode::FilterPopup);
    }
}
