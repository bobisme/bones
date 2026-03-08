//! TUI list view for bones.
//!
//! Provides a full-screen terminal UI with:
//! - Filterable nested bones list with slash search
//! - Right-side detail pane
//! - Key bindings: j/k navigate or scroll, / search, F filter, a add bone, D show/hide done, q quit

#![allow(
    clippy::similar_names,
    clippy::match_same_arms,
    clippy::manual_let_else,
    clippy::too_many_lines,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    clippy::assigning_clones,
    clippy::items_after_statements,
    clippy::option_if_let_else,
    clippy::needless_pass_by_ref_mut,
    clippy::struct_excessive_bools,
    clippy::cast_precision_loss,
    clippy::cast_possible_wrap,
    clippy::map_unwrap_or
)]

use crate::agent;
use anyhow::{Context, Result};
use bones_core::config::load_project_config;
use bones_core::db::query::{self, ItemFilter, QueryItem, SortOrder};
use bones_core::model::item::{Kind, Size, State, Urgency};
use bones_search::fusion::hybrid_search;
use bones_search::semantic::SemanticModel;
use chrono::{DateTime, Local, Utc};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui::{
    layout::{Alignment, Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    symbols::border,
    text::{Line, Span},
    widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, TableState, Wrap},
};
use std::{
    collections::{HashMap, HashSet},
    io::Write as _,
    path::Path,
    path::PathBuf,
    time::{Duration, Instant},
};

use serde_json::json;

use super::actions;

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
    pub const fn is_empty(&self) -> bool {
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
        if let Some(ref state) = self.state
            && item.state != *state
        {
            return false;
        }
        if let Some(ref kind) = self.kind
            && item.kind != *kind
        {
            return false;
        }
        if let Some(ref urgency) = self.urgency
            && item.urgency != *urgency
        {
            return false;
        }
        if let Some(ref label) = self.label
            && !item.labels.iter().any(|l| l.contains(label.as_str()))
        {
            return false;
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
    /// Sort by dependency execution order (blockers before blocked),
    /// using priority as the tie-breaker among ready items.
    #[default]
    Execution,
    /// Sort by priority: urgent → default → punt, then `updated_at` desc.
    Priority,
    /// Sort by `created_at` descending (newest first).
    Created,
    /// Sort by `updated_at` descending (most recently changed first).
    Updated,
    /// Sort by label/tag alphabetically, then by `updated_at` within each group.
    Tags,
}

impl SortField {
    const fn label(self) -> &'static str {
        match self {
            Self::Execution => "execution",
            Self::Priority => "priority",
            Self::Created => "created",
            Self::Updated => "updated",
            Self::Tags => "tags",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Execution => Self::Priority,
            Self::Priority => Self::Created,
            Self::Created => Self::Updated,
            Self::Updated => Self::Tags,
            Self::Tags => Self::Execution,
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
    pub size: Option<String>,
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
            size: qi.size,
            labels,
            created_at_us: qi.created_at_us,
            updated_at_us: qi.updated_at_us,
        }
    }
}

#[derive(Debug, Clone)]
struct DetailComment {
    author: String,
    body: String,
    created_at_us: i64,
}

#[derive(Debug, Clone)]
struct DetailRef {
    id: String,
    title: Option<String>,
}

#[derive(Debug, Clone)]
struct DetailItem {
    id: String,
    title: String,
    description: Option<String>,
    kind: String,
    state: String,
    urgency: String,
    size: Option<String>,
    parent_id: Option<String>,
    labels: Vec<String>,
    assignees: Vec<String>,
    blockers: Vec<DetailRef>,
    blocked: Vec<DetailRef>,
    relationships: Vec<DetailRef>,
    comments: Vec<DetailComment>,
    created_at_us: i64,
    updated_at_us: i64,
}

fn urgency_rank(u: &str) -> u8 {
    match u {
        "urgent" => 0,
        "default" => 1,
        "punt" => 2,
        _ => 3,
    }
}

fn is_related_link(link_type: &str) -> bool {
    matches!(link_type, "related_to" | "related" | "relates")
}

fn load_detail_refs(conn: &rusqlite::Connection, mut ids: Vec<String>) -> Result<Vec<DetailRef>> {
    ids.sort_unstable();
    ids.dedup();
    ids.into_iter()
        .map(|id| {
            let title = query::get_item(conn, &id, false)?.map(|item| item.title);
            Ok(DetailRef { id, title })
        })
        .collect()
}

/// Sort a mutable slice of `WorkItem` by the given `SortField`.
pub fn sort_items(items: &mut [WorkItem], sort: SortField) {
    items.sort_by(|a, b| match sort {
        SortField::Execution => urgency_rank(&a.urgency)
            .cmp(&urgency_rank(&b.urgency))
            .then_with(|| b.updated_at_us.cmp(&a.updated_at_us))
            .then_with(|| a.item_id.cmp(&b.item_id)),
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
        SortField::Tags => {
            let a_tag = a.labels.first().map(String::as_str).unwrap_or("\u{ffff}");
            let b_tag = b.labels.first().map(String::as_str).unwrap_or("\u{ffff}");
            a_tag
                .cmp(b_tag)
                .then_with(|| b.updated_at_us.cmp(&a.updated_at_us))
                .then_with(|| a.item_id.cmp(&b.item_id))
        }
    });
}

fn sort_items_execution(items: &mut Vec<WorkItem>, blocker_map: &HashMap<String, Vec<String>>) {
    if items.is_empty() {
        return;
    }

    let base_order: Vec<String> = items.iter().map(|item| item.item_id.clone()).collect();
    let base_rank: HashMap<String, usize> = base_order
        .iter()
        .enumerate()
        .map(|(idx, id)| (id.clone(), idx))
        .collect();
    let id_set: HashSet<String> = base_order.iter().cloned().collect();

    let mut indegree: HashMap<String, usize> =
        base_order.iter().map(|id| (id.clone(), 0)).collect();
    let mut outgoing: HashMap<String, Vec<String>> = HashMap::new();

    for blocked_id in &base_order {
        if let Some(blockers) = blocker_map.get(blocked_id) {
            for blocker_id in blockers {
                if !id_set.contains(blocker_id) {
                    continue;
                }
                *indegree.entry(blocked_id.clone()).or_insert(0) += 1;
                outgoing
                    .entry(blocker_id.clone())
                    .or_default()
                    .push(blocked_id.clone());
            }
        }
    }

    let mut ready: Vec<String> = base_order
        .iter()
        .filter(|id| indegree.get(*id).copied().unwrap_or(0) == 0)
        .cloned()
        .collect();

    let mut ordered_ids = Vec::with_capacity(base_order.len());
    while let Some(next_id) = ready.first().cloned() {
        ready.remove(0);
        ordered_ids.push(next_id.clone());

        if let Some(children) = outgoing.get(&next_id) {
            for child in children {
                if let Some(deg) = indegree.get_mut(child) {
                    if *deg > 0 {
                        *deg -= 1;
                    }
                    if *deg == 0 {
                        let rank = base_rank.get(child).copied().unwrap_or(usize::MAX);
                        let insert_at = ready
                            .binary_search_by_key(&rank, |id| {
                                base_rank.get(id).copied().unwrap_or(usize::MAX)
                            })
                            .unwrap_or_else(|idx| idx);
                        ready.insert(insert_at, child.clone());
                    }
                }
            }
        }
    }

    if ordered_ids.len() < base_order.len() {
        for id in &base_order {
            if !ordered_ids.iter().any(|seen| seen == id) {
                ordered_ids.push(id.clone());
            }
        }
    }

    let mut by_id: HashMap<String, WorkItem> = items
        .drain(..)
        .map(|item| (item.item_id.clone(), item))
        .collect();
    for item_id in ordered_ids {
        if let Some(item) = by_id.remove(&item_id) {
            items.push(item);
        }
    }
}

fn load_blocker_map(conn: &rusqlite::Connection) -> Result<HashMap<String, Vec<String>>> {
    let mut stmt = conn.prepare(
        "SELECT item_id, depends_on_item_id
         FROM item_dependencies
         WHERE link_type IN ('blocks', 'blocked_by')
         ORDER BY item_id, depends_on_item_id",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    let mut map: HashMap<String, Vec<String>> = HashMap::new();
    for row in rows {
        let (item_id, blocker_id) = row?;
        map.entry(item_id).or_default().push(blocker_id);
    }

    for blockers in map.values_mut() {
        blockers.sort_unstable();
        blockers.dedup();
    }

    Ok(map)
}

fn build_hierarchy_order(
    sorted_items: Vec<WorkItem>,
    parent_map: &HashMap<String, Option<String>>,
) -> (Vec<WorkItem>, Vec<usize>) {
    if sorted_items.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let sorted_ids: Vec<String> = sorted_items.iter().map(|i| i.item_id.clone()).collect();
    let id_set: HashSet<String> = sorted_ids.iter().cloned().collect();

    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    let mut roots: Vec<String> = Vec::new();

    for item_id in &sorted_ids {
        let parent_id = parent_map.get(item_id).cloned().flatten();
        if let Some(parent_id) = parent_id {
            if id_set.contains(&parent_id) {
                children.entry(parent_id).or_default().push(item_id.clone());
            } else {
                roots.push(item_id.clone());
            }
        } else {
            roots.push(item_id.clone());
        }
    }

    let mut by_id: HashMap<String, WorkItem> = sorted_items
        .into_iter()
        .map(|item| (item.item_id.clone(), item))
        .collect();
    let mut visited: HashSet<String> = HashSet::new();
    let mut ordered = Vec::new();
    let mut depths = Vec::new();

    fn visit(
        item_id: &str,
        depth: usize,
        children: &HashMap<String, Vec<String>>,
        by_id: &mut HashMap<String, WorkItem>,
        visited: &mut HashSet<String>,
        ordered: &mut Vec<WorkItem>,
        depths: &mut Vec<usize>,
    ) {
        if !visited.insert(item_id.to_string()) {
            return;
        }

        if let Some(item) = by_id.remove(item_id) {
            ordered.push(item);
            depths.push(depth);
        }

        if let Some(kids) = children.get(item_id) {
            for child in kids {
                visit(child, depth + 1, children, by_id, visited, ordered, depths);
            }
        }
    }

    for root in &roots {
        visit(
            root,
            0,
            &children,
            &mut by_id,
            &mut visited,
            &mut ordered,
            &mut depths,
        );
    }

    for item_id in &sorted_ids {
        if !visited.contains(item_id) {
            visit(
                item_id,
                0,
                &children,
                &mut by_id,
                &mut visited,
                &mut ordered,
                &mut depths,
            );
        }
    }

    (ordered, depths)
}

fn build_dependency_order(
    sorted_items: Vec<WorkItem>,
    blocker_map: &HashMap<String, Vec<String>>,
    parent_map: &HashMap<String, Option<String>>,
) -> (Vec<WorkItem>, Vec<usize>) {
    if sorted_items.is_empty() {
        return (Vec::new(), Vec::new());
    }

    let sorted_ids: Vec<String> = sorted_items
        .iter()
        .map(|item| item.item_id.clone())
        .collect();
    let id_set: HashSet<String> = sorted_ids.iter().cloned().collect();
    let base_rank: HashMap<String, usize> = sorted_ids
        .iter()
        .enumerate()
        .map(|(idx, id)| (id.clone(), idx))
        .collect();

    // Build parent-child tree from the parent_map (hierarchy relationships).
    // An item whose parent_id points to an item in the current set is a
    // hierarchy child.
    let mut hierarchy_children: HashMap<String, Vec<String>> = HashMap::new();
    let mut has_hierarchy_parent: HashSet<String> = HashSet::new();
    for item_id in &sorted_ids {
        if let Some(Some(pid)) = parent_map.get(item_id)
            && id_set.contains(pid)
        {
            hierarchy_children
                .entry(pid.clone())
                .or_default()
                .push(item_id.clone());
            has_hierarchy_parent.insert(item_id.clone());
        }
    }
    // Sort hierarchy children by their execution rank so they appear in
    // the right relative order under their parent.
    for kids in hierarchy_children.values_mut() {
        kids.sort_by_key(|id| base_rank.get(id).copied().unwrap_or(usize::MAX));
    }

    // Build a lookup: item_id -> parent_id (for items that have a hierarchy parent).
    let mut item_parent: HashMap<String, String> = HashMap::new();
    for item_id in &sorted_ids {
        if let Some(Some(pid)) = parent_map.get(item_id)
            && id_set.contains(pid)
        {
            item_parent.insert(item_id.clone(), pid.clone());
        }
    }

    // Build dependency nesting.  Items with a hierarchy parent can still nest
    // under a blocker *if that blocker shares the same hierarchy parent* (i.e.
    // both are siblings under the same goal).  This preserves intra-phase
    // dependency indentation while keeping cross-phase items grouped under
    // their parent goal.
    let mut primary_blocker: HashMap<String, String> = HashMap::new();
    for blocked_id in &sorted_ids {
        let Some(blockers) = blocker_map.get(blocked_id) else {
            continue;
        };

        let blocked_parent = item_parent.get(blocked_id);

        let chosen = blockers
            .iter()
            .filter(|blocker_id| {
                if !id_set.contains((*blocker_id).as_str()) {
                    return false;
                }
                // If blocked item has a hierarchy parent, only nest under a
                // blocker that shares the same parent (sibling dependency).
                if let Some(bp) = blocked_parent {
                    let blocker_parent = item_parent.get((*blocker_id).as_str());
                    return blocker_parent == Some(bp);
                }
                true
            })
            .min_by_key(|blocker_id| {
                base_rank
                    .get((*blocker_id).as_str())
                    .copied()
                    .unwrap_or(usize::MAX)
            })
            .cloned();

        if let Some(blocker_id) = chosen {
            primary_blocker.insert(blocked_id.clone(), blocker_id);
        }
    }

    // Merge dependency children and hierarchy children into one tree.
    let mut children: HashMap<String, Vec<String>> = HashMap::new();
    for (blocked_id, blocker_id) in &primary_blocker {
        children
            .entry(blocker_id.clone())
            .or_default()
            .push(blocked_id.clone());
    }
    for dep_children in children.values_mut() {
        dep_children.sort_by_key(|item_id| {
            base_rank
                .get(item_id.as_str())
                .copied()
                .unwrap_or(usize::MAX)
        });
    }
    // Layer hierarchy children on top.  Only add children that are NOT already
    // nested under a sibling blocker (those are reachable via the dependency
    // tree within the parent group).
    for (parent_id, kids) in &hierarchy_children {
        let entry = children.entry(parent_id.clone()).or_default();
        let mut top_kids: Vec<String> = kids
            .iter()
            .filter(|kid| !primary_blocker.contains_key((*kid).as_str()))
            .cloned()
            .collect();
        top_kids.append(entry);
        *entry = top_kids;
    }

    // A root is any item that is neither a dependency child nor a hierarchy
    // child.
    let roots: Vec<String> = sorted_ids
        .iter()
        .filter(|item_id| {
            !primary_blocker.contains_key((*item_id).as_str())
                && !has_hierarchy_parent.contains((*item_id).as_str())
        })
        .cloned()
        .collect();

    let mut by_id: HashMap<String, WorkItem> = sorted_items
        .into_iter()
        .map(|item| (item.item_id.clone(), item))
        .collect();
    let mut visited: HashSet<String> = HashSet::new();
    let mut ordered = Vec::new();
    let mut depths = Vec::new();

    fn visit(
        item_id: &str,
        depth: usize,
        children: &HashMap<String, Vec<String>>,
        by_id: &mut HashMap<String, WorkItem>,
        visited: &mut HashSet<String>,
        ordered: &mut Vec<WorkItem>,
        depths: &mut Vec<usize>,
    ) {
        if !visited.insert(item_id.to_string()) {
            return;
        }

        if let Some(item) = by_id.remove(item_id) {
            ordered.push(item);
            depths.push(depth);
        }

        if let Some(direct) = children.get(item_id) {
            for child_id in direct {
                visit(
                    child_id,
                    depth + 1,
                    children,
                    by_id,
                    visited,
                    ordered,
                    depths,
                );
            }
        }
    }

    for root_id in &roots {
        visit(
            root_id,
            0,
            &children,
            &mut by_id,
            &mut visited,
            &mut ordered,
            &mut depths,
        );
    }

    for item_id in &sorted_ids {
        if !visited.contains(item_id) {
            visit(
                item_id,
                0,
                &children,
                &mut by_id,
                &mut visited,
                &mut ordered,
                &mut depths,
            );
        }
    }

    (ordered, depths)
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
    /// Create-bone modal is open.
    CreateModal,
    /// Comment/close/reopen note modal is open.
    NoteModal,
    /// Help overlay is open.
    Help,
    /// Filter popup is open.
    FilterPopup,
    /// Filter popup: editing a text field (label).
    FilterLabel,
    /// Blocker/link picker modal is open.
    BlockerModal,
    /// Edit-link modal is open.
    EditLinkModal,
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
    const fn next(self) -> Self {
        match self {
            Self::State => Self::Kind,
            Self::Kind => Self::Urgency,
            Self::Urgency => Self::Label,
            Self::Label => Self::State,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::State => Self::Label,
            Self::Kind => Self::State,
            Self::Urgency => Self::Kind,
            Self::Label => Self::Urgency,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
enum CreateField {
    #[default]
    Title,
    Description,
    Kind,
    Size,
    Urgency,
    Labels,
}

impl CreateField {
    const fn next(self) -> Self {
        match self {
            Self::Title => Self::Description,
            Self::Description => Self::Kind,
            Self::Kind => Self::Size,
            Self::Size => Self::Urgency,
            Self::Urgency => Self::Labels,
            Self::Labels => Self::Title,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Title => Self::Labels,
            Self::Description => Self::Title,
            Self::Kind => Self::Description,
            Self::Size => Self::Kind,
            Self::Urgency => Self::Size,
            Self::Labels => Self::Urgency,
        }
    }
}

#[derive(Debug, Clone)]
struct CreateDraft {
    title: String,
    description: Option<String>,
    kind: String,
    size: Option<String>,
    urgency: String,
    labels: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CreateAction {
    None,
    Submit,
    Cancel,
    OpenEditor,
}

#[derive(Debug, Clone)]
struct CreateModalState {
    focus: CreateField,
    title: String,
    title_cursor: usize,
    description: Vec<String>,
    desc_row: usize,
    desc_col: usize,
    kind_idx: usize,
    size_idx: usize,
    urgency_idx: usize,
    labels: String,
    labels_cursor: usize,
}

impl Default for CreateModalState {
    fn default() -> Self {
        Self {
            focus: CreateField::Title,
            title: String::new(),
            title_cursor: 0,
            description: vec![String::new()],
            desc_row: 0,
            desc_col: 0,
            kind_idx: 0,
            size_idx: 0,
            urgency_idx: 0,
            labels: String::new(),
            labels_cursor: 0,
        }
    }
}

impl CreateModalState {
    fn from_detail(detail: &DetailItem) -> Self {
        let mut modal = Self::default();
        modal.title = detail.title.clone();
        modal.title_cursor = char_len(&modal.title);
        modal.description = detail
            .description
            .as_deref()
            .map(|d| {
                d.lines()
                    .map(std::string::ToString::to_string)
                    .collect::<Vec<_>>()
            })
            .filter(|lines| !lines.is_empty())
            .unwrap_or_else(|| vec![String::new()]);
        modal.desc_row = modal.description.len().saturating_sub(1);
        modal.desc_col = char_len(&modal.description[modal.desc_row]);
        modal.kind_idx = match detail.kind.as_str() {
            "goal" => 1,
            "bug" => 2,
            _ => 0,
        };
        modal.size_idx = Self::size_index(detail.size.as_deref());
        modal.urgency_idx = Self::urgency_index(&detail.urgency);
        modal.labels = detail.labels.join(", ");
        modal.labels_cursor = char_len(&modal.labels);
        modal
    }

    const fn kind(&self) -> &str {
        match self.kind_idx {
            0 => "task",
            1 => "goal",
            2 => "bug",
            _ => "task",
        }
    }

    const fn size_options() -> [&'static str; 6] {
        ["(none)", "xs", "s", "m", "l", "xl"]
    }

    fn size_index(size: Option<&str>) -> usize {
        match size {
            Some("xs") => 1,
            Some("s") => 2,
            Some("m") => 3,
            Some("l") => 4,
            Some("xl") => 5,
            _ => 0,
        }
    }

    fn size(&self) -> Option<String> {
        if self.size_idx == 0 {
            None
        } else {
            Some(Self::size_options()[self.size_idx].to_string())
        }
    }

    const fn urgency_options() -> [&'static str; 3] {
        ["none", "urgent", "punted"]
    }

    fn urgency_index(urgency: &str) -> usize {
        match urgency {
            "urgent" => 1,
            "punt" => 2,
            _ => 0,
        }
    }

    const fn urgency_raw(&self) -> &'static str {
        match self.urgency_idx {
            1 => "urgent",
            2 => "punt",
            _ => "default",
        }
    }

    const fn urgency_display(&self) -> &'static str {
        Self::urgency_options()[self.urgency_idx]
    }

    fn can_submit(&self) -> bool {
        !self.title.trim().is_empty()
    }

    fn labels_vec(&self) -> Vec<String> {
        self.labels
            .split(',')
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .collect()
    }

    fn description_value(&self) -> Option<String> {
        let text = self.description.join("\n");
        if text.trim().is_empty() {
            None
        } else {
            Some(text)
        }
    }

    fn build_draft(&self) -> CreateDraft {
        CreateDraft {
            title: self.title.trim().to_string(),
            description: self.description_value(),
            kind: self.kind().to_string(),
            size: self.size(),
            urgency: self.urgency_raw().to_string(),
            labels: self.labels_vec(),
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> CreateAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);

        match key.code {
            KeyCode::Esc => return CreateAction::Cancel,
            KeyCode::Char('s') if ctrl => {
                if self.can_submit() {
                    return CreateAction::Submit;
                }
                return CreateAction::None;
            }
            KeyCode::Enter if ctrl => {
                if self.can_submit() {
                    return CreateAction::Submit;
                }
                return CreateAction::None;
            }
            KeyCode::Char('g') if ctrl => {
                if matches!(self.focus, CreateField::Title | CreateField::Description) {
                    return CreateAction::OpenEditor;
                }
                return CreateAction::None;
            }
            KeyCode::BackTab => {
                self.focus = self.focus.prev();
                return CreateAction::None;
            }
            KeyCode::Tab if shift => {
                self.focus = self.focus.prev();
                return CreateAction::None;
            }
            KeyCode::Tab => {
                self.focus = self.focus.next();
                return CreateAction::None;
            }
            _ => {}
        }

        match self.focus {
            CreateField::Title => {
                if key.code == KeyCode::Enter {
                    self.focus = CreateField::Description;
                } else {
                    Self::edit_single_line(&mut self.title, &mut self.title_cursor, key);
                }
            }
            CreateField::Description => {
                self.edit_description(key);
            }
            CreateField::Kind => match key.code {
                KeyCode::Left | KeyCode::Up | KeyCode::Char('h' | 'k') => {
                    self.kind_idx = self.kind_idx.saturating_sub(1);
                }
                KeyCode::Right | KeyCode::Down | KeyCode::Char('l' | 'j') => {
                    self.kind_idx = (self.kind_idx + 1).min(2);
                }
                KeyCode::Char('t') => self.kind_idx = 0,
                KeyCode::Char('g') => self.kind_idx = 1,
                KeyCode::Char('b') => self.kind_idx = 2,
                _ => {}
            },
            CreateField::Size => match key.code {
                KeyCode::Left | KeyCode::Up | KeyCode::Char('h' | 'k') => {
                    self.size_idx = self.size_idx.saturating_sub(1);
                }
                KeyCode::Right | KeyCode::Down | KeyCode::Char('j') => {
                    self.size_idx = (self.size_idx + 1).min(Self::size_options().len() - 1);
                }
                KeyCode::Char('n') => self.size_idx = 0,
                KeyCode::Char('z') => self.size_idx = 1,
                KeyCode::Char('x') => self.size_idx = 2,
                KeyCode::Char('s') => self.size_idx = 3,
                KeyCode::Char('m') => self.size_idx = 4,
                KeyCode::Char('l') => self.size_idx = 5,
                _ => {}
            },
            CreateField::Urgency => match key.code {
                KeyCode::Left | KeyCode::Up | KeyCode::Char('h' | 'k') => {
                    self.urgency_idx = self.urgency_idx.saturating_sub(1);
                }
                KeyCode::Right | KeyCode::Down | KeyCode::Char('j') => {
                    self.urgency_idx =
                        (self.urgency_idx + 1).min(Self::urgency_options().len() - 1);
                }
                KeyCode::Char('n') => self.urgency_idx = 0,
                KeyCode::Char('u') => self.urgency_idx = 1,
                KeyCode::Char('p') => self.urgency_idx = 2,
                _ => {}
            },
            CreateField::Labels => {
                Self::edit_single_line(&mut self.labels, &mut self.labels_cursor, key);
            }
        }

        CreateAction::None
    }

    fn edit_single_line(text: &mut String, cursor: &mut usize, key: KeyEvent) {
        let _ = edit_single_line_readline(text, cursor, key);
    }

    fn edit_description(&mut self, key: KeyEvent) {
        edit_multiline(
            &mut self.description,
            &mut self.desc_row,
            &mut self.desc_col,
            key,
        );
    }

    fn handle_paste(&mut self, text: &str) {
        match self.focus {
            CreateField::Title => {
                insert_single_line_text(&mut self.title, &mut self.title_cursor, text);
            }
            CreateField::Description => paste_multiline_text(
                &mut self.description,
                &mut self.desc_row,
                &mut self.desc_col,
                text,
            ),
            CreateField::Labels => {
                insert_single_line_text(&mut self.labels, &mut self.labels_cursor, text);
            }
            _ => {}
        }
    }
}

/// Open `$EDITOR` (falling back to `vi`) with `initial` content.
///
/// Suspends the TUI's raw-mode/alt-screen, launches the editor, then
/// re-enters raw-mode/alt-screen.  Returns the edited text on success,
/// or `None` if the editor exited with a non-zero status.
fn open_in_editor(initial: &str) -> anyhow::Result<Option<String>> {
    use crossterm::{
        event::{DisableMouseCapture, EnableMouseCapture},
        execute,
        terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode},
    };

    let editor = std::env::var("EDITOR")
        .or_else(|_| std::env::var("VISUAL"))
        .unwrap_or_else(|_| "vi".to_string());

    let tmp_path = std::env::temp_dir().join(format!("bones-edit-{}.md", std::process::id()));
    {
        let mut f = std::fs::File::create(&tmp_path)?;
        f.write_all(initial.as_bytes())?;
    }

    disable_raw_mode()?;
    execute!(std::io::stdout(), LeaveAlternateScreen, DisableMouseCapture)?;

    let status = std::process::Command::new(&editor).arg(&tmp_path).status();

    enable_raw_mode()?;
    execute!(std::io::stdout(), EnterAlternateScreen, EnableMouseCapture)?;

    match status {
        Ok(s) if s.success() => {
            let content = std::fs::read_to_string(&tmp_path).unwrap_or_default();
            let _ = std::fs::remove_file(&tmp_path);
            Ok(Some(content))
        }
        _ => {
            let _ = std::fs::remove_file(&tmp_path);
            Ok(None)
        }
    }
}

enum NoteAction {
    None,
    Submit,
    Cancel,
    OpenEditor,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NoteMode {
    Comment,
    Transition { target: State, reopen: bool },
}

#[derive(Debug, Clone)]
struct NoteModalState {
    mode: NoteMode,
    lines: Vec<String>,
    row: usize,
    col: usize,
}

impl NoteModalState {
    fn comment() -> Self {
        Self {
            mode: NoteMode::Comment,
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    fn transition(target: State, reopen: bool) -> Self {
        Self {
            mode: NoteMode::Transition { target, reopen },
            lines: vec![String::new()],
            row: 0,
            col: 0,
        }
    }

    fn handle_key(&mut self, key: KeyEvent) -> NoteAction {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        match key.code {
            KeyCode::Esc => NoteAction::Cancel,
            KeyCode::Char('s') if ctrl => {
                if self.text().trim().is_empty() {
                    NoteAction::None
                } else {
                    NoteAction::Submit
                }
            }
            KeyCode::Enter if ctrl => {
                if self.text().trim().is_empty() {
                    NoteAction::None
                } else {
                    NoteAction::Submit
                }
            }
            KeyCode::Char('g') if ctrl => NoteAction::OpenEditor,
            _ => {
                edit_multiline(&mut self.lines, &mut self.row, &mut self.col, key);
                NoteAction::None
            }
        }
    }

    fn text(&self) -> String {
        self.lines.join("\n")
    }

    fn handle_paste(&mut self, text: &str) {
        paste_multiline_text(&mut self.lines, &mut self.row, &mut self.col, text);
    }
}

/// Which relationship the blocker modal will create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum BlockerRelType {
    /// Current bone blocks the selected bone.
    Blocks,
    /// Current bone is blocked by the selected bone.
    BlockedBy,
    /// Current bone becomes a child of the selected bone.
    ChildOf,
    /// Selected bone becomes a child of the current bone.
    ParentOf,
}

impl BlockerRelType {
    const fn label(self) -> &'static str {
        match self {
            Self::Blocks => "Blocks",
            Self::BlockedBy => "Blocked by",
            Self::ChildOf => "Child of",
            Self::ParentOf => "Parent of",
        }
    }

    const fn next(self) -> Self {
        match self {
            Self::Blocks => Self::BlockedBy,
            Self::BlockedBy => Self::ChildOf,
            Self::ChildOf => Self::ParentOf,
            Self::ParentOf => Self::Blocks,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Blocks => Self::ParentOf,
            Self::BlockedBy => Self::Blocks,
            Self::ChildOf => Self::BlockedBy,
            Self::ParentOf => Self::ChildOf,
        }
    }
}

struct BlockerModalState {
    rel_type: BlockerRelType,
    search: String,
    search_cursor: usize,
    /// All active items (excluding the current bone).
    items: Vec<(String, String)>,
    /// Index into the filtered view.
    list_idx: usize,
    /// Whether the search field is focused (accepts all character input).
    search_focused: bool,
}

impl BlockerModalState {
    const fn new(items: Vec<(String, String)>) -> Self {
        Self {
            rel_type: BlockerRelType::Blocks,
            search: String::new(),
            search_cursor: 0,
            items,
            list_idx: 0,
            search_focused: false,
        }
    }

    fn filtered(&self) -> Vec<&(String, String)> {
        let q = self.search.to_ascii_lowercase();
        if q.is_empty() {
            self.items.iter().collect()
        } else {
            self.items
                .iter()
                .filter(|(id, title)| {
                    id.to_ascii_lowercase().contains(&q) || title.to_ascii_lowercase().contains(&q)
                })
                .collect()
        }
    }

    fn selected_item(&self) -> Option<&(String, String)> {
        let filtered = self.filtered();
        filtered.get(self.list_idx).copied()
    }
}

// ---------------------------------------------------------------------------
// Edit-link modal types
// ---------------------------------------------------------------------------

/// Direction of a link relative to the current bone.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LinkDirection {
    /// The link event is recorded on the current bone (`item_id = current`).
    Outgoing,
    /// The link event is recorded on the peer bone (`item_id = peer`).
    Incoming,
}

/// Display type for a link in the edit-link modal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EditLinkType {
    Blocks,
    BlockedBy,
    Related,
    /// Current bone is a child of the peer (parent relationship).
    ChildOf,
    /// Peer bone is a child of the current bone.
    ParentOf,
}

impl EditLinkType {
    const fn label(self) -> &'static str {
        match self {
            Self::Blocks => "Blocks",
            Self::BlockedBy => "Blocked by",
            Self::Related => "Related",
            Self::ChildOf => "Child of",
            Self::ParentOf => "Parent of",
        }
    }

    /// Cycle to next type. Parent/child types only cycle among themselves;
    /// link types (Blocks/BlockedBy/Related) cycle among themselves.
    const fn next(self) -> Self {
        match self {
            Self::Blocks => Self::BlockedBy,
            Self::BlockedBy => Self::Related,
            Self::Related => Self::Blocks,
            Self::ChildOf => Self::ParentOf,
            Self::ParentOf => Self::ChildOf,
        }
    }

    const fn prev(self) -> Self {
        match self {
            Self::Blocks => Self::Related,
            Self::BlockedBy => Self::Blocks,
            Self::Related => Self::BlockedBy,
            Self::ChildOf => Self::ParentOf,
            Self::ParentOf => Self::ChildOf,
        }
    }
}

/// A single link row in the edit-link modal.
#[derive(Debug, Clone)]
struct EditableLink {
    peer_id: String,
    peer_title: Option<String>,
    /// Original link type as stored in the event model.
    original_type: String,
    /// Original direction relative to the current bone.
    original_direction: LinkDirection,
    /// Current (proposed) display type.
    current_type: EditLinkType,
    /// Whether this link is marked for deletion.
    deleted: bool,
}

impl EditableLink {
    /// Whether this link has been changed from its original state.
    fn is_changed(&self) -> bool {
        self.deleted || self.display_type_for_original() != self.current_type
    }

    /// Compute the display type that corresponds to the original link.
    fn display_type_for_original(&self) -> EditLinkType {
        if self.original_type == "parent" {
            match self.original_direction {
                LinkDirection::Outgoing => EditLinkType::ChildOf,
                LinkDirection::Incoming => EditLinkType::ParentOf,
            }
        } else if is_related_link(&self.original_type) {
            EditLinkType::Related
        } else {
            match self.original_direction {
                LinkDirection::Outgoing => EditLinkType::BlockedBy,
                LinkDirection::Incoming => EditLinkType::Blocks,
            }
        }
    }
}

/// State for the edit-link modal.
struct EditLinkModalState {
    /// The bone whose links we are editing.
    item_id: String,
    /// Editable link rows.
    links: Vec<EditableLink>,
    /// Currently selected row index.
    list_idx: usize,
}

fn edit_multiline(lines: &mut Vec<String>, row: &mut usize, col: &mut usize, key: KeyEvent) {
    if lines.is_empty() {
        lines.push(String::new());
    }
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    if matches!(key.code, KeyCode::Char('j')) && key.modifiers.contains(KeyModifiers::SHIFT) {
        insert_newline(lines, row, col);
        return;
    }

    if ctrl {
        match key.code {
            KeyCode::Char('a') => {
                *col = 0;
                return;
            }
            KeyCode::Char('e') => {
                *col = char_len(&lines[*row]);
                return;
            }
            KeyCode::Char('h') => {
                backspace_multiline(lines, row, col);
                return;
            }
            KeyCode::Char('d') => {
                delete_multiline(lines, row, col);
                return;
            }
            KeyCode::Char('w') => {
                delete_prev_word_in_line(&mut lines[*row], col);
                return;
            }
            KeyCode::Char('u') => {
                let start = byte_index_at_char(&lines[*row], 0);
                let end = byte_index_at_char(&lines[*row], *col);
                lines[*row].replace_range(start..end, "");
                *col = 0;
                return;
            }
            KeyCode::Char('k') => {
                let start = byte_index_at_char(&lines[*row], *col);
                lines[*row].replace_range(start.., "");
                return;
            }
            _ => {}
        }
    }

    if alt {
        match key.code {
            KeyCode::Char('b') => {
                *col = prev_word_boundary(&lines[*row], *col);
                return;
            }
            KeyCode::Char('f') => {
                *col = next_word_boundary(&lines[*row], *col);
                return;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Left => {
            if *col > 0 {
                *col -= 1;
            } else if *row > 0 {
                *row -= 1;
                *col = char_len(&lines[*row]);
            }
        }
        KeyCode::Right => {
            let line_len = char_len(&lines[*row]);
            if *col < line_len {
                *col += 1;
            } else if *row + 1 < lines.len() {
                *row += 1;
                *col = 0;
            }
        }
        KeyCode::Up => {
            if *row > 0 {
                *row -= 1;
                *col = (*col).min(char_len(&lines[*row]));
            }
        }
        KeyCode::Down => {
            if *row + 1 < lines.len() {
                *row += 1;
                *col = (*col).min(char_len(&lines[*row]));
            }
        }
        KeyCode::Home => *col = 0,
        KeyCode::End => *col = char_len(&lines[*row]),
        KeyCode::Enter => insert_newline(lines, row, col),
        KeyCode::Backspace => {
            backspace_multiline(lines, row, col);
        }
        KeyCode::Delete => delete_multiline(lines, row, col),
        KeyCode::Char('\n' | '\r') => insert_newline(lines, row, col),
        KeyCode::Char(c) => {
            if !ctrl && !alt {
                insert_char_at(&mut lines[*row], *col, c);
                *col += 1;
            }
        }
        _ => {}
    }
}

fn is_word_char(ch: char) -> bool {
    ch.is_alphanumeric() || matches!(ch, '_' | '-')
}

fn prev_word_boundary(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() || cursor == 0 {
        return 0;
    }

    let mut idx = cursor.min(chars.len());
    while idx > 0 && !is_word_char(chars[idx - 1]) {
        idx -= 1;
    }
    while idx > 0 && is_word_char(chars[idx - 1]) {
        idx -= 1;
    }
    idx
}

fn next_word_boundary(text: &str, cursor: usize) -> usize {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return 0;
    }

    let mut idx = cursor.min(chars.len());
    while idx < chars.len() && !is_word_char(chars[idx]) {
        idx += 1;
    }
    while idx < chars.len() && is_word_char(chars[idx]) {
        idx += 1;
    }
    idx
}

fn delete_prev_word_in_line(text: &mut String, cursor: &mut usize) {
    if *cursor == 0 {
        return;
    }
    let start = prev_word_boundary(text, *cursor);
    let start_byte = byte_index_at_char(text, start);
    let end_byte = byte_index_at_char(text, *cursor);
    text.replace_range(start_byte..end_byte, "");
    *cursor = start;
}

fn insert_newline(lines: &mut Vec<String>, row: &mut usize, col: &mut usize) {
    let split_at = byte_index_at_char(&lines[*row], *col);
    let tail = lines[*row].split_off(split_at);
    *row += 1;
    *col = 0;
    lines.insert(*row, tail);
}

fn backspace_multiline(lines: &mut Vec<String>, row: &mut usize, col: &mut usize) {
    if *col > 0 {
        let remove_idx = *col - 1;
        remove_char_at(&mut lines[*row], remove_idx);
        *col = remove_idx;
    } else if *row > 0 {
        let current = lines.remove(*row);
        *row -= 1;
        *col = char_len(&lines[*row]);
        lines[*row].push_str(&current);
    }
}

fn delete_multiline(lines: &mut Vec<String>, row: &mut usize, col: &mut usize) {
    let line_len = char_len(&lines[*row]);
    if *col < line_len {
        remove_char_at(&mut lines[*row], *col);
    } else if *row + 1 < lines.len() {
        let next = lines.remove(*row + 1);
        lines[*row].push_str(&next);
    }
}

fn normalize_paste_text(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

fn insert_single_line_text(text: &mut String, cursor: &mut usize, pasted: &str) {
    let flattened = normalize_paste_text(pasted).replace('\n', " ");
    if flattened.is_empty() {
        return;
    }
    let idx = byte_index_at_char(text, *cursor);
    text.insert_str(idx, &flattened);
    *cursor += flattened.chars().count();
}

fn paste_multiline_text(lines: &mut Vec<String>, row: &mut usize, col: &mut usize, pasted: &str) {
    if pasted.is_empty() {
        return;
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    for ch in normalize_paste_text(pasted).chars() {
        if ch == '\n' {
            insert_newline(lines, row, col);
        } else {
            insert_char_at(&mut lines[*row], *col, ch);
            *col += 1;
        }
    }
}

fn edit_single_line_readline(text: &mut String, cursor: &mut usize, key: KeyEvent) -> bool {
    let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
    let alt = key.modifiers.contains(KeyModifiers::ALT);

    if ctrl {
        match key.code {
            KeyCode::Char('a') => {
                *cursor = 0;
                return false;
            }
            KeyCode::Char('e') => {
                *cursor = char_len(text);
                return false;
            }
            KeyCode::Char('h') => {
                if *cursor > 0 {
                    let remove_idx = *cursor - 1;
                    remove_char_at(text, remove_idx);
                    *cursor = remove_idx;
                    return true;
                }
                return false;
            }
            KeyCode::Char('d') => {
                let before = text.len();
                remove_char_at(text, *cursor);
                return text.len() != before;
            }
            KeyCode::Char('w') => {
                let before = text.len();
                delete_prev_word_in_line(text, cursor);
                return text.len() != before;
            }
            KeyCode::Char('u') => {
                let start = byte_index_at_char(text, 0);
                let end = byte_index_at_char(text, *cursor);
                text.replace_range(start..end, "");
                *cursor = 0;
                return true;
            }
            KeyCode::Char('k') => {
                let start = byte_index_at_char(text, *cursor);
                text.replace_range(start.., "");
                return true;
            }
            _ => {}
        }
    }

    if alt {
        match key.code {
            KeyCode::Char('b') => {
                *cursor = prev_word_boundary(text, *cursor);
                return false;
            }
            KeyCode::Char('f') => {
                *cursor = next_word_boundary(text, *cursor);
                return false;
            }
            _ => {}
        }
    }

    match key.code {
        KeyCode::Left => *cursor = cursor.saturating_sub(1),
        KeyCode::Right => *cursor = (*cursor + 1).min(char_len(text)),
        KeyCode::Home => *cursor = 0,
        KeyCode::End => *cursor = char_len(text),
        KeyCode::Backspace => {
            if *cursor > 0 {
                let remove_idx = *cursor - 1;
                remove_char_at(text, remove_idx);
                *cursor = remove_idx;
                return true;
            }
        }
        KeyCode::Delete => {
            let before = text.len();
            remove_char_at(text, *cursor);
            return text.len() != before;
        }
        KeyCode::Char(c) => {
            if !ctrl && !alt && !matches!(c, '\n' | '\r') {
                insert_char_at(text, *cursor, c);
                *cursor += 1;
                return true;
            }
        }
        _ => {}
    }
    false
}

fn char_len(value: &str) -> usize {
    value.chars().count()
}

fn byte_index_at_char(value: &str, char_idx: usize) -> usize {
    value
        .char_indices()
        .nth(char_idx)
        .map_or(value.len(), |(idx, _)| idx)
}

fn insert_char_at(value: &mut String, char_idx: usize, ch: char) {
    let idx = byte_index_at_char(value, char_idx);
    value.insert(idx, ch);
}

fn remove_char_at(value: &mut String, char_idx: usize) {
    if char_idx >= char_len(value) {
        return;
    }
    let start = byte_index_at_char(value, char_idx);
    let end = byte_index_at_char(value, char_idx + 1);
    value.replace_range(start..end, "");
}

fn with_cursor_marker(value: &str, char_idx: usize) -> String {
    let cursor = char_idx.min(char_len(value));
    let mut out = String::new();
    let mut inserted = false;
    for (idx, ch) in value.chars().enumerate() {
        if idx == cursor {
            out.push('|');
            inserted = true;
        }
        out.push(ch);
    }
    if !inserted {
        out.push('|');
    }
    out
}

fn with_cursor_spans(value: &str, char_idx: usize, base_style: Style) -> Vec<Span<'static>> {
    let chars: Vec<char> = value.chars().collect();
    let cursor = char_idx.min(chars.len());
    let cursor_style = base_style.add_modifier(Modifier::REVERSED);

    let mut spans = Vec::with_capacity(chars.len() + 1);
    for (idx, ch) in chars.iter().enumerate() {
        let style = if idx == cursor {
            cursor_style
        } else {
            base_style
        };
        spans.push(Span::styled(ch.to_string(), style));
    }

    if cursor == chars.len() {
        spans.push(Span::styled(" ".to_string(), cursor_style));
    }

    spans
}

fn with_cursor_line(value: &str, char_idx: usize, base_style: Style) -> Line<'static> {
    Line::from(with_cursor_spans(value, char_idx, base_style))
}

/// Main application state for the TUI list view.
pub struct ListView {
    /// Path to the bones projection database.
    db_path: PathBuf,
    /// Project root path.
    project_root: PathBuf,
    /// Agent name used for mutations from TUI.
    agent: String,
    /// All items loaded from the projection (unfiltered, unsorted for display).
    all_items: Vec<WorkItem>,
    /// Items after filtering and sorting — this is what the table shows.
    visible_items: Vec<WorkItem>,
    /// Parallel depths for each row in `visible_items`.
    visible_depths: Vec<usize>,
    /// First index in `visible_items` where done/archived items start.
    done_start_idx: Option<usize>,
    /// Parent relationship map from `item_id -> parent_id`.
    parent_map: HashMap<String, Option<String>>,
    /// Blocking dependency map from `blocked_item_id -> [blocker_item_id...]`.
    blocker_map: HashMap<String, Vec<String>>,
    /// Semantic model used for slash search.
    semantic_model: Option<SemanticModel>,
    /// Ranked IDs returned by semantic/hybrid slash search.
    semantic_search_ids: Vec<String>,
    /// Whether semantic search executed successfully for the active query.
    semantic_search_active: bool,
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
    /// Cursor position within `search_buf`.
    search_cursor: usize,
    /// Query value before entering Search mode (for Esc cancel).
    search_prev_query: String,
    /// Buffer for the label filter being typed in the popup.
    label_buf: String,
    /// Cursor position within `label_buf`.
    label_cursor: usize,
    /// Current focus inside the filter popup.
    filter_field: FilterField,
    /// Whether to quit.
    should_quit: bool,
    /// Last refresh timestamp (for status bar).
    last_refresh: Instant,
    /// Background auto-refresh interval.
    refresh_interval: Duration,
    /// Whether a status message should be shown temporarily.
    status_msg: Option<(String, Instant)>,
    /// Most recent tracing ERROR captured from the log sink (shown in red).
    error_msg: Option<(String, Instant)>,
    /// Whether the right-side detail pane is open.
    show_detail: bool,
    /// Whether done/archived bones are shown.
    show_done: bool,
    /// Split percentage for list/detail panes.
    split_percent: u16,
    /// Current detail-pane vertical scroll offset.
    detail_scroll: u16,
    /// Geometry used for mouse interactions.
    list_area: Rect,
    /// Geometry used for mouse interactions.
    detail_area: Rect,
    /// Whether split drag is active.
    split_resize_active: bool,
    /// Cached detail content for the selected item.
    detail_item: Option<DetailItem>,
    /// Item ID currently loaded into `detail_item`.
    detail_item_id: Option<String>,
    /// Create-bone modal state when open.
    create_modal: Option<CreateModalState>,
    /// Item being edited in create modal; None means create mode.
    create_modal_edit_item_id: Option<String>,
    /// Comment/close/reopen note modal state when open.
    note_modal: Option<NoteModalState>,
    /// Blocker/link picker modal state when open.
    blocker_modal: Option<BlockerModalState>,
    /// Edit-link modal state when open.
    edit_link_modal: Option<EditLinkModalState>,
    /// Help overlay filter query.
    help_query: String,
    /// Cursor position within `help_query`.
    help_cursor: usize,
    /// Set after an external editor is closed; the run loop should clear the
    /// terminal so the TUI repaints cleanly.
    pub needs_terminal_refresh: bool,
}

impl ListView {
    /// Create a new list view, loading items from the given database.
    pub fn new(db_path: PathBuf) -> Result<Self> {
        let project_root = db_path
            .parent()
            .and_then(Path::parent)
            .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf);
        let agent = agent::require_agent(None).unwrap_or_else(|_| "tui".to_string());

        let semantic_enabled = db_path
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(|root| load_project_config(root).ok())
            .is_none_or(|cfg| cfg.search.semantic);
        let semantic_model = if semantic_enabled {
            match SemanticModel::load() {
                Ok(model) => Some(model),
                Err(err) => {
                    tracing::warn!(
                        "semantic model unavailable in bones TUI slash search; using lexical+structural only: {err}"
                    );
                    None
                }
            }
        } else {
            None
        };

        let mut view = Self {
            db_path,
            project_root,
            agent,
            all_items: Vec::new(),
            visible_items: Vec::new(),
            visible_depths: Vec::new(),
            done_start_idx: None,
            parent_map: HashMap::new(),
            blocker_map: HashMap::new(),
            semantic_model,
            semantic_search_ids: Vec::new(),
            semantic_search_active: false,
            filter: FilterState::default(),
            sort: SortField::default(),
            table_state: TableState::default(),
            input_mode: InputMode::default(),
            search_buf: String::new(),
            search_cursor: 0,
            search_prev_query: String::new(),
            label_buf: String::new(),
            label_cursor: 0,
            filter_field: FilterField::default(),
            should_quit: false,
            last_refresh: Instant::now(),
            refresh_interval: Duration::from_secs(2),
            status_msg: None,
            error_msg: None,
            show_detail: false,
            show_done: false,
            split_percent: 40,
            detail_scroll: 0,
            list_area: Rect::default(),
            detail_area: Rect::default(),
            split_resize_active: false,
            detail_item: None,
            detail_item_id: None,
            create_modal: None,
            create_modal_edit_item_id: None,
            note_modal: None,
            blocker_modal: None,
            edit_link_modal: None,
            help_query: String::new(),
            help_cursor: 0,
            needs_terminal_refresh: false,
        };
        if let Err(e) = view.reload() {
            view.set_status(format!("DB load failed: {e:#}"));
        }
        Ok(view)
    }

    /// Load (or reload) all items from the projection database.
    pub fn reload(&mut self) -> Result<()> {
        let conn = if let Some(c) = query::try_open_projection(&self.db_path)? {
            c
        } else {
            self.all_items.clear();
            self.visible_items.clear();
            self.visible_depths.clear();
            self.done_start_idx = None;
            self.parent_map.clear();
            self.blocker_map.clear();
            self.detail_item = None;
            self.detail_item_id = None;
            self.detail_scroll = 0;
            self.last_refresh = Instant::now();
            return Ok(());
        };

        let filter = ItemFilter {
            include_deleted: false,
            sort: SortOrder::UpdatedDesc,
            ..Default::default()
        };

        let raw_items = query::list_items(&conn, &filter).context("list_items")?;
        self.parent_map.clear();
        self.blocker_map = load_blocker_map(&conn).unwrap_or_default();
        self.all_items = raw_items
            .into_iter()
            .map(|qi| {
                self.parent_map
                    .insert(qi.item_id.clone(), qi.parent_id.clone());
                let labels = query::get_labels(&conn, &qi.item_id)
                    .unwrap_or_default()
                    .into_iter()
                    .map(|l| l.label)
                    .collect();
                WorkItem::from_query(qi, labels)
            })
            .collect();

        let _ = self.refresh_semantic_search_ids();
        self.apply_filter_and_sort();
        self.last_refresh = Instant::now();
        Ok(())
    }

    fn refresh_semantic_search_ids(&mut self) -> Result<()> {
        self.semantic_search_ids.clear();
        self.semantic_search_active = false;
        let query = self.filter.search_query.trim();
        if query.is_empty() {
            return Ok(());
        }

        let conn = match query::try_open_projection(&self.db_path)? {
            Some(c) => c,
            None => return Ok(()),
        };

        let effective_query =
            if !query.contains(' ') && !query.contains('*') && !query.contains(':') {
                format!("{query}*")
            } else {
                query.to_string()
            };

        let hits = hybrid_search(
            &effective_query,
            &conn,
            self.semantic_model.as_ref(),
            200,
            60,
        )
        .context("bones slash search failed")?;
        self.semantic_search_ids = hits.into_iter().map(|hit| hit.item_id).collect();
        self.semantic_search_active = true;
        Ok(())
    }

    /// Recompute `visible_items` from `all_items` using the current filter and sort.
    fn apply_filter_and_sort(&mut self) {
        let mut base_filter = self.filter.clone();
        base_filter.search_query.clear();
        let mut filtered = base_filter.apply(&self.all_items);

        let query_active = !self.filter.search_query.trim().is_empty();
        if query_active {
            if self.semantic_search_active {
                let rank_index: HashMap<&str, usize> = self
                    .semantic_search_ids
                    .iter()
                    .enumerate()
                    .map(|(idx, item_id)| (item_id.as_str(), idx))
                    .collect();
                filtered.retain(|bone| rank_index.contains_key(bone.item_id.as_str()));
                filtered.sort_by(|a, b| {
                    rank_index
                        .get(a.item_id.as_str())
                        .cmp(&rank_index.get(b.item_id.as_str()))
                        .then_with(|| a.item_id.cmp(&b.item_id))
                });
            } else {
                let q = self.filter.search_query.to_ascii_lowercase();
                filtered.retain(|bone| {
                    bone.title.to_ascii_lowercase().contains(&q)
                        || bone.item_id.to_ascii_lowercase().contains(&q)
                });
            }
        }

        let mut active_items = Vec::new();
        let mut done_items = Vec::new();
        for item in filtered {
            if item.state == "done" || item.state == "archived" {
                done_items.push(item);
            } else {
                active_items.push(item);
            }
        }

        if !query_active {
            match self.sort {
                SortField::Execution => {
                    sort_items(&mut active_items, SortField::Priority);
                    sort_items_execution(&mut active_items, &self.blocker_map);
                }
                _ => sort_items(&mut active_items, self.sort),
            }
        }

        let (mut ordered, mut depths) = if !query_active && self.sort == SortField::Execution {
            build_dependency_order(active_items, &self.blocker_map, &self.parent_map)
        } else {
            build_hierarchy_order(active_items, &self.parent_map)
        };
        self.done_start_idx = None;
        if self.show_done && !done_items.is_empty() {
            // Show completed bones newest-first (reverse close order approximation).
            done_items.sort_by(|a, b| {
                b.updated_at_us
                    .cmp(&a.updated_at_us)
                    .then_with(|| a.item_id.cmp(&b.item_id))
            });
            self.done_start_idx = Some(ordered.len());
            depths.extend(std::iter::repeat_n(0, done_items.len()));
            ordered.extend(done_items);
        }

        self.visible_items = ordered;
        self.visible_depths = depths;

        // Clamp selection into valid range.
        let len = self.visible_items.len();
        match self.table_state.selected() {
            Some(_) if len == 0 => self.table_state.select(None),
            Some(i) if i >= len => self.table_state.select(Some(len.saturating_sub(1))),
            None if len > 0 => self.table_state.select(Some(0)),
            _ => {}
        }

        self.refresh_selected_detail();
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
        self.refresh_selected_detail();
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
        self.refresh_selected_detail();
    }

    fn select_first(&mut self) {
        if !self.visible_items.is_empty() {
            self.table_state.select(Some(0));
            self.refresh_selected_detail();
        }
    }

    fn select_last(&mut self) {
        let len = self.visible_items.len();
        if len > 0 {
            self.table_state.select(Some(len - 1));
            self.refresh_selected_detail();
        }
    }

    /// Currently selected item (if any).
    pub fn selected_item(&self) -> Option<&WorkItem> {
        self.table_state
            .selected()
            .and_then(|i| self.visible_items.get(i))
    }

    const fn detail_visible_height(&self) -> usize {
        self.detail_area.height.saturating_sub(2) as usize
    }

    fn max_detail_scroll(&self) -> u16 {
        if !self.show_detail {
            return 0;
        }
        let Some(detail) = self.detail_item.as_ref() else {
            return 0;
        };
        let viewport_h = self.detail_visible_height();
        if viewport_h == 0 {
            return 0;
        }
        let wrap_w = self.detail_area.width.saturating_sub(2).max(1) as usize;
        let total_lines = detail_lines(detail)
            .iter()
            .map(|line| {
                let width: usize = line
                    .spans
                    .iter()
                    .map(|span| span.content.chars().count())
                    .sum();
                width.max(1).div_ceil(wrap_w)
            })
            .sum::<usize>();

        total_lines
            .saturating_sub(viewport_h)
            .min(u16::MAX as usize) as u16
    }

    fn clamp_detail_scroll(&mut self) {
        let max_scroll = self.max_detail_scroll();
        if self.detail_scroll > max_scroll {
            self.detail_scroll = max_scroll;
        }
    }

    fn scroll_detail_by(&mut self, delta: i32) {
        let max_scroll = i32::from(self.max_detail_scroll());
        let next = i32::from(self.detail_scroll)
            .saturating_add(delta)
            .clamp(0, max_scroll);
        self.detail_scroll = next as u16;
    }

    fn table_row_count(&self) -> usize {
        self.visible_items.len() + usize::from(self.done_start_idx.is_some())
    }

    const fn table_row_from_visible_index(&self, visible_idx: usize) -> usize {
        match self.done_start_idx {
            Some(done_idx) if visible_idx >= done_idx => visible_idx + 1,
            _ => visible_idx,
        }
    }

    const fn visible_index_from_table_row(&self, table_row: usize) -> Option<usize> {
        match self.done_start_idx {
            Some(done_idx) if table_row == done_idx => None,
            Some(done_idx) if table_row > done_idx => Some(table_row - 1),
            _ => Some(table_row),
        }
    }

    // -----------------------------------------------------------------------
    // Key event handling
    // -----------------------------------------------------------------------

    pub fn handle_key(&mut self, key: KeyEvent) -> Result<()> {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);

        match self.input_mode {
            InputMode::Search => self.handle_search_key(key),
            InputMode::CreateModal => self.handle_create_modal_key(key)?,
            InputMode::NoteModal => self.handle_note_modal_key(key)?,
            InputMode::BlockerModal => self.handle_blocker_modal_key(key)?,
            InputMode::EditLinkModal => self.handle_edit_link_modal_key(key)?,
            InputMode::Help => self.handle_help_key(key),
            InputMode::FilterPopup => self.handle_filter_popup_key(key),
            InputMode::FilterLabel => self.handle_filter_label_key(key),
            InputMode::Normal => self.handle_normal_key(key, ctrl),
        }

        Ok(())
    }

    pub fn handle_paste(&mut self, text: &str) {
        match self.input_mode {
            InputMode::CreateModal => {
                if let Some(modal) = self.create_modal.as_mut() {
                    modal.handle_paste(text);
                }
            }
            InputMode::NoteModal => {
                if let Some(modal) = self.note_modal.as_mut() {
                    modal.handle_paste(text);
                }
            }
            InputMode::Search => {
                insert_single_line_text(&mut self.search_buf, &mut self.search_cursor, text);
                self.filter.search_query = self.search_buf.clone();
                let _ = self.refresh_semantic_search_ids();
                self.apply_filter_and_sort();
            }
            InputMode::FilterLabel => {
                insert_single_line_text(&mut self.label_buf, &mut self.label_cursor, text);
            }
            InputMode::Help => {
                insert_single_line_text(&mut self.help_query, &mut self.help_cursor, text);
            }
            InputMode::BlockerModal => {
                if let Some(modal) = self.blocker_modal.as_mut() {
                    let prev = modal.search.clone();
                    insert_single_line_text(&mut modal.search, &mut modal.search_cursor, text);
                    if modal.search != prev {
                        modal.list_idx = 0;
                    }
                }
            }
            InputMode::FilterPopup | InputMode::Normal | InputMode::EditLinkModal => {}
        }
    }

    fn handle_normal_key(&mut self, key: KeyEvent, ctrl: bool) {
        match key.code {
            // Quit
            KeyCode::Char('q') => self.should_quit = true,
            KeyCode::Char('c') if ctrl => self.should_quit = true,

            // Navigation
            KeyCode::Char('j') | KeyCode::Down => {
                if self.show_detail {
                    self.scroll_detail_by(1);
                } else {
                    self.select_next();
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                if self.show_detail {
                    self.scroll_detail_by(-1);
                } else {
                    self.select_prev();
                }
            }
            KeyCode::Char('g') | KeyCode::Home => {
                if self.show_detail {
                    self.detail_scroll = 0;
                } else {
                    self.select_first();
                }
            }
            KeyCode::Char('G') | KeyCode::End => {
                if self.show_detail {
                    self.detail_scroll = self.max_detail_scroll();
                } else {
                    self.select_last();
                }
            }

            // Page scroll
            KeyCode::PageDown | KeyCode::Char('d') => {
                if self.show_detail {
                    self.scroll_detail_by(10);
                } else {
                    for _ in 0..10 {
                        self.select_next();
                    }
                }
            }
            KeyCode::Char('f') => {
                if self.show_detail {
                    self.scroll_detail_by(10);
                } else {
                    for _ in 0..10 {
                        self.select_next();
                    }
                }
            }
            KeyCode::PageUp | KeyCode::Char('u') => {
                if self.show_detail {
                    self.scroll_detail_by(-10);
                } else {
                    for _ in 0..10 {
                        self.select_prev();
                    }
                }
            }

            KeyCode::Char('b') => {
                if self.show_detail {
                    self.scroll_detail_by(-10);
                } else {
                    for _ in 0..10 {
                        self.select_prev();
                    }
                }
            }

            // 'L' (shift+l) in detail pane opens the blocker/link picker.
            KeyCode::Char('L') if self.show_detail => {
                self.open_blocker_modal();
            }

            // 'E' (shift+e) in detail pane opens the edit-link modal.
            KeyCode::Char('E') if self.show_detail => {
                self.open_edit_link_modal();
            }

            // Open detail pane for current selection.
            KeyCode::Enter | KeyCode::Char('l') | KeyCode::Right => {
                self.open_detail();
            }

            // Close detail pane.
            KeyCode::Char('h') | KeyCode::Left if self.show_detail => {
                self.close_detail();
            }

            // Search
            KeyCode::Char('/') => {
                self.search_prev_query = self.filter.search_query.clone();
                self.search_buf = self.filter.search_query.clone();
                self.search_cursor = char_len(&self.search_buf);
                self.input_mode = InputMode::Search;
            }

            // Create modal.
            KeyCode::Char('a') => {
                self.open_create_modal();
            }

            // Edit selected bone from detail pane.
            KeyCode::Char('e') if self.show_detail => {
                self.open_edit_modal();
            }

            // Add comment from detail pane.
            KeyCode::Char('c') if self.show_detail => {
                self.note_modal = Some(NoteModalState::comment());
                self.input_mode = InputMode::NoteModal;
            }

            // Close or reopen from detail pane with comment.
            KeyCode::Char('x') if self.show_detail => {
                self.open_transition_modal();
            }

            // Help overlay.
            KeyCode::Char('?') => {
                self.help_query.clear();
                self.help_cursor = 0;
                self.input_mode = InputMode::Help;
            }

            // Filter popup
            KeyCode::Char('F') => {
                self.label_buf = self.filter.label.clone().unwrap_or_default();
                self.label_cursor = char_len(&self.label_buf);
                self.filter_field = FilterField::default();
                self.input_mode = InputMode::FilterPopup;
            }

            // Cycle sort order
            KeyCode::Char('s') if !self.show_detail => {
                self.sort = self.sort.next();
                self.apply_filter_and_sort();
                self.set_status(format!("Sort: {}", self.sort.label()));
            }

            // Toggle done/archived visibility.
            KeyCode::Char('D') => {
                self.show_done = !self.show_done;
                self.apply_filter_and_sort();
                let done_count = self
                    .all_items
                    .iter()
                    .filter(|bone| bone.state == "done" || bone.state == "archived")
                    .count();
                self.set_status(format!(
                    "Done bones {} ({done_count} total)",
                    if self.show_done { "shown" } else { "hidden" }
                ));
            }

            // Clear filter
            KeyCode::Esc => {
                if self.show_detail {
                    self.close_detail();
                } else if !self.filter.is_empty() {
                    self.filter = FilterState::default();
                    self.semantic_search_ids.clear();
                    self.apply_filter_and_sort();
                    self.set_status("Filters cleared".to_string());
                }
            }

            _ => {}
        }
    }

    fn open_create_modal(&mut self) {
        self.create_modal = Some(CreateModalState::default());
        self.create_modal_edit_item_id = None;
        self.input_mode = InputMode::CreateModal;
    }

    fn open_edit_modal(&mut self) {
        let Some(detail) = self.detail_item.as_ref() else {
            self.set_status("No bone selected".to_string());
            return;
        };
        self.create_modal = Some(CreateModalState::from_detail(detail));
        self.create_modal_edit_item_id = Some(detail.id.clone());
        self.input_mode = InputMode::CreateModal;
    }

    fn open_blocker_modal(&mut self) {
        let Some(ref detail) = self.detail_item else {
            self.set_status("No bone selected".to_string());
            return;
        };
        let current_id = detail.id.clone();

        let Ok(Some(conn)) = query::try_open_projection(&self.db_path) else {
            self.set_status("Cannot open DB for blocker modal".to_string());
            return;
        };

        let all = match query::list_items(
            &conn,
            &ItemFilter {
                include_deleted: false,
                sort: SortOrder::UpdatedDesc,
                ..Default::default()
            },
        ) {
            Ok(items) => items,
            Err(e) => {
                self.set_status(format!("Error loading items: {e}"));
                return;
            }
        };

        let items: Vec<(String, String)> = all
            .into_iter()
            .filter(|item| {
                item.item_id != current_id && item.state != "done" && item.state != "archived"
            })
            .map(|item| (item.item_id, item.title))
            .collect();

        self.blocker_modal = Some(BlockerModalState::new(items));
        self.input_mode = InputMode::BlockerModal;
    }

    fn handle_blocker_modal_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(modal) = self.blocker_modal.as_mut() else {
            self.input_mode = InputMode::Normal;
            return Ok(());
        };

        match key.code {
            KeyCode::Esc => {
                if modal.search_focused {
                    modal.search.clear();
                    modal.search_cursor = 0;
                    modal.list_idx = 0;
                    modal.search_focused = false;
                } else {
                    self.blocker_modal = None;
                    self.input_mode = InputMode::Normal;
                }
            }
            KeyCode::Char('/') if !modal.search_focused => {
                modal.search_focused = true;
            }
            KeyCode::Enter if modal.search_focused => {
                modal.search_focused = false;
            }
            KeyCode::Left if !modal.search_focused => {
                modal.rel_type = modal.rel_type.prev();
            }
            KeyCode::Right | KeyCode::Tab if !modal.search_focused => {
                modal.rel_type = modal.rel_type.next();
            }
            KeyCode::Down if !modal.search_focused => {
                let count = modal.filtered().len();
                if count > 0 {
                    modal.list_idx = (modal.list_idx + 1).min(count - 1);
                }
            }
            KeyCode::Char('j') if !modal.search_focused => {
                let count = modal.filtered().len();
                if count > 0 {
                    modal.list_idx = (modal.list_idx + 1).min(count - 1);
                }
            }
            KeyCode::Up if !modal.search_focused => {
                modal.list_idx = modal.list_idx.saturating_sub(1);
            }
            KeyCode::Char('k') if !modal.search_focused => {
                modal.list_idx = modal.list_idx.saturating_sub(1);
            }
            KeyCode::Enter => {
                let selected = modal.selected_item().map(|(id, _)| id.clone());
                let rel_type = modal.rel_type;
                self.blocker_modal = None;
                self.input_mode = InputMode::Normal;
                if let Some(target_id) = selected {
                    self.submit_blocker_link(rel_type, &target_id)?;
                }
            }
            _ => {
                let prev = modal.search.clone();
                edit_single_line_readline(&mut modal.search, &mut modal.search_cursor, key);
                if modal.search != prev {
                    modal.list_idx = 0;
                }
            }
        }
        Ok(())
    }

    fn submit_blocker_link(&mut self, rel_type: BlockerRelType, target_id: &str) -> Result<()> {
        let Some(ref detail) = self.detail_item else {
            return Ok(());
        };
        let current_id = detail.id.clone();

        match rel_type {
            BlockerRelType::Blocks => {
                // current blocks target: link event on target (blocked), target=current (blocker)
                actions::add_link(
                    &self.project_root,
                    &self.db_path,
                    &self.agent,
                    target_id,
                    &current_id,
                    "blocks",
                )?;
                self.set_status(format!("{current_id} blocks {target_id}"));
            }
            BlockerRelType::BlockedBy => {
                // current blocked by target: link event on current (blocked), target=target (blocker)
                actions::add_link(
                    &self.project_root,
                    &self.db_path,
                    &self.agent,
                    &current_id,
                    target_id,
                    "blocks",
                )?;
                self.set_status(format!("{target_id} blocks {current_id}"));
            }
            BlockerRelType::ChildOf => {
                actions::set_parent(
                    &self.project_root,
                    &self.db_path,
                    &self.agent,
                    &current_id,
                    target_id,
                )?;
                self.set_status(format!("{current_id} is now child of {target_id}"));
            }
            BlockerRelType::ParentOf => {
                actions::set_parent(
                    &self.project_root,
                    &self.db_path,
                    &self.agent,
                    target_id,
                    &current_id,
                )?;
                self.set_status(format!("{target_id} is now child of {current_id}"));
            }
        }
        self.reload()?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Edit-link modal
    // -----------------------------------------------------------------------

    fn open_edit_link_modal(&mut self) {
        let Some(ref detail) = self.detail_item else {
            self.set_status("No bone selected".to_string());
            return;
        };
        let item_id = detail.id.clone();

        let Ok(Some(conn)) = query::try_open_projection(&self.db_path) else {
            self.set_status("Cannot open DB for edit-link modal".to_string());
            return;
        };

        let mut links = Vec::new();

        // Dependencies: current bone depends on peer (link recorded on current,
        // target = peer). For "blocks" type this means "current is blocked by peer".
        if let Ok(deps) = query::get_dependencies(&conn, &item_id) {
            for dep in deps {
                let title = query::get_item(&conn, &dep.depends_on_item_id, false)
                    .ok()
                    .flatten()
                    .map(|i| i.title);
                links.push(EditableLink {
                    peer_id: dep.depends_on_item_id,
                    peer_title: title,
                    original_type: dep.link_type.clone(),
                    original_direction: LinkDirection::Outgoing,
                    current_type: if is_related_link(&dep.link_type) {
                        EditLinkType::Related
                    } else {
                        EditLinkType::BlockedBy
                    },
                    deleted: false,
                });
            }
        }

        // Dependents: peer depends on current (link recorded on peer,
        // target = current). For "blocks" type this means "current blocks peer".
        if let Ok(deps) = query::get_dependents(&conn, &item_id) {
            for dep in deps {
                let title = query::get_item(&conn, &dep.item_id, false)
                    .ok()
                    .flatten()
                    .map(|i| i.title);
                links.push(EditableLink {
                    peer_id: dep.item_id,
                    peer_title: title,
                    original_type: dep.link_type.clone(),
                    original_direction: LinkDirection::Incoming,
                    current_type: if is_related_link(&dep.link_type) {
                        EditLinkType::Related
                    } else {
                        EditLinkType::Blocks
                    },
                    deleted: false,
                });
            }
        }

        // Parent relationship: current bone is a child of parent.
        if let Some(ref parent_id) = detail.parent_id {
            let title = query::get_item(&conn, parent_id, false)
                .ok()
                .flatten()
                .map(|i| i.title);
            links.push(EditableLink {
                peer_id: parent_id.clone(),
                peer_title: title,
                original_type: "parent".to_string(),
                original_direction: LinkDirection::Outgoing,
                current_type: EditLinkType::ChildOf,
                deleted: false,
            });
        }

        // Children: bones that have current bone as their parent.
        if let Ok(all_items) = query::list_items(
            &conn,
            &ItemFilter {
                include_deleted: false,
                sort: SortOrder::UpdatedDesc,
                ..Default::default()
            },
        ) {
            for child in all_items {
                if child.parent_id.as_deref() == Some(&*item_id) {
                    links.push(EditableLink {
                        peer_id: child.item_id.clone(),
                        peer_title: Some(child.title),
                        original_type: "parent".to_string(),
                        original_direction: LinkDirection::Incoming,
                        current_type: EditLinkType::ParentOf,
                        deleted: false,
                    });
                }
            }
        }

        if links.is_empty() {
            self.set_status("No links to edit".to_string());
            return;
        }

        self.edit_link_modal = Some(EditLinkModalState {
            item_id,
            links,
            list_idx: 0,
        });
        self.input_mode = InputMode::EditLinkModal;
    }

    fn handle_edit_link_modal_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(modal) = self.edit_link_modal.as_mut() else {
            self.input_mode = InputMode::Normal;
            return Ok(());
        };

        match key.code {
            KeyCode::Esc => {
                self.edit_link_modal = None;
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                if !modal.links.is_empty() {
                    modal.list_idx = (modal.list_idx + 1).min(modal.links.len() - 1);
                }
            }
            KeyCode::Char('k') | KeyCode::Up => {
                modal.list_idx = modal.list_idx.saturating_sub(1);
            }
            KeyCode::Left => {
                if let Some(link) = modal.links.get_mut(modal.list_idx)
                    && !link.deleted
                {
                    link.current_type = link.current_type.prev();
                }
            }
            KeyCode::Right | KeyCode::Tab => {
                if let Some(link) = modal.links.get_mut(modal.list_idx)
                    && !link.deleted
                {
                    link.current_type = link.current_type.next();
                }
            }
            KeyCode::Char('d') => {
                if let Some(link) = modal.links.get_mut(modal.list_idx) {
                    link.deleted = !link.deleted;
                }
            }
            KeyCode::Enter => {
                // Collect changes, then apply.
                let item_id = modal.item_id.clone();
                let changes: Vec<EditableLink> = modal
                    .links
                    .iter()
                    .filter(|l| l.is_changed())
                    .cloned()
                    .collect();
                self.edit_link_modal = None;
                self.input_mode = InputMode::Normal;
                self.apply_edit_link_changes(&item_id, &changes)?;
            }
            _ => {}
        }
        Ok(())
    }

    fn apply_edit_link_changes(
        &mut self,
        current_id: &str,
        changes: &[EditableLink],
    ) -> Result<()> {
        let mut count = 0usize;
        for link in changes {
            let is_hierarchy = link.original_type == "parent";

            if is_hierarchy {
                // Parent/child relationships use the `parent` field, not link events.
                match link.original_direction {
                    LinkDirection::Outgoing => {
                        // Current is child of peer — clear current's parent.
                        if link.deleted || link.current_type == EditLinkType::ParentOf {
                            actions::clear_parent(
                                &self.project_root,
                                &self.db_path,
                                &self.agent,
                                current_id,
                            )?;
                        }
                        if !link.deleted && link.current_type == EditLinkType::ParentOf {
                            // Flip: peer becomes child of current.
                            actions::set_parent(
                                &self.project_root,
                                &self.db_path,
                                &self.agent,
                                &link.peer_id,
                                current_id,
                            )?;
                        }
                    }
                    LinkDirection::Incoming => {
                        // Peer is child of current — clear peer's parent.
                        if link.deleted || link.current_type == EditLinkType::ChildOf {
                            actions::clear_parent(
                                &self.project_root,
                                &self.db_path,
                                &self.agent,
                                &link.peer_id,
                            )?;
                        }
                        if !link.deleted && link.current_type == EditLinkType::ChildOf {
                            // Flip: current becomes child of peer.
                            actions::set_parent(
                                &self.project_root,
                                &self.db_path,
                                &self.agent,
                                current_id,
                                &link.peer_id,
                            )?;
                        }
                    }
                }
            } else {
                // Link-based relationships (blocks, related_to).
                // First, remove the original link.
                match link.original_direction {
                    LinkDirection::Outgoing => {
                        actions::remove_link(
                            &self.project_root,
                            &self.db_path,
                            &self.agent,
                            current_id,
                            &link.peer_id,
                            Some(&link.original_type),
                        )?;
                    }
                    LinkDirection::Incoming => {
                        actions::remove_link(
                            &self.project_root,
                            &self.db_path,
                            &self.agent,
                            &link.peer_id,
                            current_id,
                            Some(&link.original_type),
                        )?;
                    }
                }

                // If not deleted, add the new link.
                if !link.deleted {
                    match link.current_type {
                        EditLinkType::BlockedBy => {
                            actions::add_link(
                                &self.project_root,
                                &self.db_path,
                                &self.agent,
                                current_id,
                                &link.peer_id,
                                "blocks",
                            )?;
                        }
                        EditLinkType::Blocks => {
                            actions::add_link(
                                &self.project_root,
                                &self.db_path,
                                &self.agent,
                                &link.peer_id,
                                current_id,
                                "blocks",
                            )?;
                        }
                        EditLinkType::Related => {
                            actions::add_link(
                                &self.project_root,
                                &self.db_path,
                                &self.agent,
                                current_id,
                                &link.peer_id,
                                "related_to",
                            )?;
                        }
                        EditLinkType::ChildOf => {
                            actions::set_parent(
                                &self.project_root,
                                &self.db_path,
                                &self.agent,
                                current_id,
                                &link.peer_id,
                            )?;
                        }
                        EditLinkType::ParentOf => {
                            actions::set_parent(
                                &self.project_root,
                                &self.db_path,
                                &self.agent,
                                &link.peer_id,
                                current_id,
                            )?;
                        }
                    }
                }
            }
            count += 1;
        }

        if count > 0 {
            self.set_status(format!("Updated {count} link(s)"));
            self.reload()?;
        }
        Ok(())
    }

    fn open_transition_modal(&mut self) {
        let Some(detail) = self.detail_item.as_ref() else {
            self.set_status("No bone selected".to_string());
            return;
        };

        let modal = match detail.state.as_str() {
            "done" | "archived" => Some(NoteModalState::transition(State::Open, true)),
            "open" | "doing" => Some(NoteModalState::transition(State::Done, false)),
            _ => None,
        };
        if let Some(modal) = modal {
            self.note_modal = Some(modal);
            self.input_mode = InputMode::NoteModal;
        }
    }

    fn handle_create_modal_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(modal) = self.create_modal.as_mut() else {
            self.input_mode = InputMode::Normal;
            return Ok(());
        };

        let action = modal.handle_key(key);
        match action {
            CreateAction::None => {}
            CreateAction::Cancel => {
                self.create_modal = None;
                self.create_modal_edit_item_id = None;
                self.input_mode = InputMode::Normal;
            }
            CreateAction::Submit => {
                let draft = modal.build_draft();
                self.create_modal = None;
                self.input_mode = InputMode::Normal;
                self.create_from_draft(draft)?;
            }
            CreateAction::OpenEditor => {
                let modal = self.create_modal.as_mut().unwrap();
                let initial = match modal.focus {
                    CreateField::Title => modal.title.clone(),
                    CreateField::Description => modal.description.join("\n"),
                    _ => String::new(),
                };
                if let Ok(Some(edited)) = open_in_editor(&initial) {
                    let modal = self.create_modal.as_mut().unwrap();
                    match modal.focus {
                        CreateField::Title => {
                            modal.title = edited.trim_end_matches('\n').to_string();
                            modal.title_cursor = modal.title.chars().count();
                        }
                        CreateField::Description => {
                            modal.description = edited.lines().map(str::to_owned).collect();
                            if modal.description.is_empty() {
                                modal.description.push(String::new());
                            }
                            modal.desc_row = modal.description.len().saturating_sub(1);
                            modal.desc_col = modal
                                .description
                                .last()
                                .map(|l| l.chars().count())
                                .unwrap_or(0);
                        }
                        _ => {}
                    }
                }
                self.needs_terminal_refresh = true;
            }
        }
        Ok(())
    }

    fn create_from_draft(&mut self, draft: CreateDraft) -> Result<()> {
        let kind = match draft.kind.as_str() {
            "goal" => Kind::Goal,
            "bug" => Kind::Bug,
            _ => Kind::Task,
        };
        let size = draft
            .size
            .as_deref()
            .and_then(|raw| raw.parse::<Size>().ok());
        let urgency = match draft.urgency.as_str() {
            "urgent" => Urgency::Urgent,
            "punt" => Urgency::Punt,
            _ => Urgency::Default,
        };

        let editing_id = self.create_modal_edit_item_id.take();
        let was_edit = editing_id.is_some();
        let id = if let Some(item_id) = editing_id {
            let updates = vec![
                ("title".to_string(), json!(draft.title)),
                (
                    "description".to_string(),
                    match draft.description {
                        Some(text) => json!(text),
                        None => json!(null),
                    },
                ),
                ("kind".to_string(), json!(kind.to_string())),
                (
                    "size".to_string(),
                    match size {
                        Some(sz) => json!(sz.to_string()),
                        None => json!(null),
                    },
                ),
                ("urgency".to_string(), json!(draft.urgency)),
                ("labels".to_string(), json!(draft.labels)),
            ];
            actions::update_item_fields(
                &self.project_root,
                &self.db_path,
                &self.agent,
                &item_id,
                &updates,
            )?;
            item_id
        } else {
            actions::create_item(
                &self.project_root,
                &self.db_path,
                &self.agent,
                &draft.title,
                draft.description,
                kind,
                size,
                urgency,
                draft.labels,
            )?
        };

        self.reload()?;
        if let Some(index) = self
            .visible_items
            .iter()
            .position(|item| item.item_id == id)
        {
            self.table_state.select(Some(index));
        }
        if self.show_detail {
            self.open_detail();
        }
        self.set_status(if was_edit {
            format!("Updated bone {id}")
        } else {
            format!("Created bone {id}")
        });
        Ok(())
    }

    fn handle_note_modal_key(&mut self, key: KeyEvent) -> Result<()> {
        let Some(modal) = self.note_modal.as_mut() else {
            self.input_mode = InputMode::Normal;
            return Ok(());
        };

        match modal.handle_key(key) {
            NoteAction::None => {}
            NoteAction::Cancel => {
                self.note_modal = None;
                self.input_mode = InputMode::Normal;
            }
            NoteAction::OpenEditor => {
                let current = modal.text();
                if let Ok(Some(edited)) = open_in_editor(&current) {
                    let modal = self.note_modal.as_mut().unwrap();
                    modal.lines = edited.lines().map(str::to_owned).collect();
                    if modal.lines.is_empty() {
                        modal.lines.push(String::new());
                    }
                    modal.row = modal.lines.len().saturating_sub(1);
                    modal.col = modal.lines.last().map(|l| l.chars().count()).unwrap_or(0);
                }
                self.needs_terminal_refresh = true;
            }
            NoteAction::Submit => {
                let body = modal.text();
                let mode = modal.mode;
                self.note_modal = None;
                self.input_mode = InputMode::Normal;

                let Some(item_id) = self.selected_item().map(|item| item.item_id.clone()) else {
                    return Ok(());
                };
                actions::add_comment(
                    &self.project_root,
                    &self.db_path,
                    &self.agent,
                    &item_id,
                    &body,
                )?;
                if let NoteMode::Transition { target, reopen } = mode {
                    actions::move_item_state(
                        &self.project_root,
                        &self.db_path,
                        &self.agent,
                        &item_id,
                        target,
                        Some(body),
                        reopen,
                    )?;
                }
                self.reload()?;
                self.set_status(format!("Saved note on {item_id}"));
            }
        }
        Ok(())
    }

    fn handle_help_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.help_query.clear();
                self.help_cursor = 0;
                self.input_mode = InputMode::Normal;
            }
            _ => {
                let _ = edit_single_line_readline(&mut self.help_query, &mut self.help_cursor, key);
            }
        }
    }

    fn handle_search_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.search_buf = self.search_prev_query.clone();
                self.search_cursor = char_len(&self.search_prev_query);
                self.filter.search_query = self.search_prev_query.clone();
                let _ = self.refresh_semantic_search_ids();
                self.apply_filter_and_sort();
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Enter => {
                self.filter.search_query = self.search_buf.clone();
                let _ = self.refresh_semantic_search_ids();
                self.apply_filter_and_sort();
                self.input_mode = InputMode::Normal;
            }
            _ => {
                let changed =
                    edit_single_line_readline(&mut self.search_buf, &mut self.search_cursor, key);
                if changed {
                    self.filter.search_query = self.search_buf.clone();
                    let _ = self.refresh_semantic_search_ids();
                    self.apply_filter_and_sort();
                }
            }
        }
    }

    fn handle_filter_popup_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc | KeyCode::Char('q') => {
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Char('F') => {
                // Pressing 'F' again applies and closes
                self.commit_label_filter();
                self.apply_filter_and_sort();
                self.input_mode = InputMode::Normal;
            }
            KeyCode::Enter => {
                if self.filter_field == FilterField::Label {
                    // Enter on the label field -> edit mode
                    self.label_cursor = char_len(&self.label_buf);
                    self.input_mode = InputMode::FilterLabel;
                } else {
                    // Enter elsewhere -> apply and close
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
            KeyCode::Right | KeyCode::Char('l' | ' ') => {
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
            _ => {
                let _ = edit_single_line_readline(&mut self.label_buf, &mut self.label_cursor, key);
            }
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
                self.label_cursor = char_len(&self.label_buf);
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

    fn open_detail(&mut self) {
        self.show_detail = true;
        self.detail_scroll = 0;
        self.refresh_selected_detail();
        self.clamp_detail_scroll();
    }

    fn close_detail(&mut self) {
        self.show_detail = false;
        self.detail_scroll = 0;
        self.detail_item = None;
        self.detail_item_id = None;
    }

    fn refresh_selected_detail(&mut self) {
        if !self.show_detail {
            return;
        }

        let Some(selected_id) = self.selected_item().map(|item| item.item_id.clone()) else {
            self.detail_item = None;
            self.detail_item_id = None;
            return;
        };

        if self.detail_item_id.as_deref() == Some(selected_id.as_str()) {
            if let Ok(detail) = self.load_detail_item(&selected_id) {
                self.detail_item = Some(detail);
            }
            self.clamp_detail_scroll();
            return;
        }

        match self.load_detail_item(&selected_id) {
            Ok(detail) => {
                self.detail_item = Some(detail);
                self.detail_item_id = Some(selected_id);
                self.detail_scroll = 0;
            }
            Err(err) => {
                self.detail_item = None;
                self.detail_item_id = None;
                self.set_status(format!("detail load error: {err}"));
            }
        }
        self.clamp_detail_scroll();
    }

    fn load_detail_item(&self, item_id: &str) -> Result<DetailItem> {
        let conn = query::try_open_projection(&self.db_path)?
            .ok_or_else(|| anyhow::anyhow!("projection database not found"))?;

        let item = query::get_item(&conn, item_id, false)?
            .ok_or_else(|| anyhow::anyhow!("bone '{item_id}' not found"))?;

        let labels = query::get_labels(&conn, item_id)?
            .into_iter()
            .map(|label| label.label)
            .collect();

        let assignees = query::get_assignees(&conn, item_id)?
            .into_iter()
            .map(|assignee| assignee.agent)
            .collect();

        let mut blocker_ids = Vec::new();
        let mut blocked_ids = Vec::new();
        let mut relationship_ids = Vec::new();

        for dep in query::get_dependencies(&conn, item_id)? {
            if is_related_link(&dep.link_type) {
                relationship_ids.push(dep.depends_on_item_id);
            } else {
                blocker_ids.push(dep.depends_on_item_id);
            }
        }

        for dep in query::get_dependents(&conn, item_id)? {
            if is_related_link(&dep.link_type) {
                relationship_ids.push(dep.item_id);
            } else {
                blocked_ids.push(dep.item_id);
            }
        }

        let blockers = load_detail_refs(&conn, blocker_ids)?;
        let blocked = load_detail_refs(&conn, blocked_ids)?;
        let relationships = load_detail_refs(&conn, relationship_ids)?;

        let mut comments: Vec<DetailComment> = query::get_comments(&conn, item_id, None, None)?
            .into_iter()
            .map(|comment| DetailComment {
                author: comment.author,
                body: comment.body,
                created_at_us: comment.created_at_us,
            })
            .collect();
        comments.sort_by(|a, b| a.created_at_us.cmp(&b.created_at_us));

        Ok(DetailItem {
            id: item.item_id,
            title: item.title,
            description: item.description,
            kind: item.kind,
            state: item.state,
            urgency: item.urgency,
            size: item.size,
            parent_id: item.parent_id,
            labels,
            assignees,
            blockers,
            blocked,
            relationships,
            comments,
            created_at_us: item.created_at_us,
            updated_at_us: item.updated_at_us,
        })
    }

    pub fn set_status(&mut self, msg: String) {
        self.status_msg = Some((msg, Instant::now()));
    }

    /// Returns true if the list view has been asked to quit (e.g. 'q' key).
    pub const fn should_quit(&self) -> bool {
        self.should_quit
    }

    /// Render the list view into `area` within the given frame.
    pub fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        render_into(frame, self, area);
    }

    pub fn tick(&mut self) -> Result<()> {
        if self.last_refresh.elapsed() >= self.refresh_interval
            && let Err(e) = self.reload()
        {
            self.set_status(format!("DB refresh failed: {e:#}"));
        }
        self.clamp_detail_scroll();
        // Pick up any ERROR events captured by the TUI log layer.
        if let Some(msg) = crate::telemetry::tui_drain_errors().into_iter().last() {
            self.error_msg = Some((msg, Instant::now()));
        }
        Ok(())
    }

    pub fn handle_mouse(&mut self, mouse: MouseEvent) {
        if self.input_mode != InputMode::Normal {
            return;
        }

        match mouse.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let x = mouse.column;
                let y = mouse.row;

                if self.show_detail && self.is_on_split_handle(x, y) {
                    self.split_resize_active = true;
                    self.update_split_from_mouse(x);
                    return;
                }

                self.split_resize_active = false;

                if self.list_area.contains((x, y).into()) {
                    let row_y = y.saturating_sub(self.list_area.y.saturating_add(1));
                    let table_idx = row_y as usize;
                    if table_idx < self.table_row_count()
                        && let Some(idx) = self.visible_index_from_table_row(table_idx)
                    {
                        self.table_state.select(Some(idx));
                        self.open_detail();
                    }
                }
            }
            MouseEventKind::Drag(MouseButton::Left) if self.split_resize_active => {
                self.update_split_from_mouse(mouse.column);
            }
            MouseEventKind::Up(_) => {
                self.split_resize_active = false;
            }
            MouseEventKind::ScrollDown => {
                if self.show_detail {
                    self.scroll_detail_by(3);
                } else {
                    self.select_next();
                }
            }
            MouseEventKind::ScrollUp => {
                if self.show_detail {
                    self.scroll_detail_by(-3);
                } else {
                    self.select_prev();
                }
            }
            _ => {}
        }
    }

    fn is_on_split_handle(&self, x: u16, y: u16) -> bool {
        if !self.show_detail || self.list_area.width == 0 || self.detail_area.width == 0 {
            return false;
        }

        let top = self.list_area.y.min(self.detail_area.y);
        let bottom = self
            .list_area
            .y
            .saturating_add(self.list_area.height)
            .max(self.detail_area.y.saturating_add(self.detail_area.height));
        if y < top || y >= bottom {
            return false;
        }

        let list_right = self
            .list_area
            .x
            .saturating_add(self.list_area.width.saturating_sub(1));
        let detail_left = self.detail_area.x;
        x == list_right || x == detail_left
    }

    fn update_split_from_mouse(&mut self, x: u16) {
        if !self.show_detail {
            return;
        }
        let total_width = self.list_area.width.saturating_add(self.detail_area.width);
        if total_width == 0 {
            return;
        }

        let content_left = self.list_area.x;
        let content_right = content_left.saturating_add(total_width.saturating_sub(1));
        let clamped_x = x.clamp(content_left, content_right);
        let left_width = clamped_x.saturating_sub(content_left).saturating_add(1);
        let raw_percent = ((u32::from(left_width) * 100) / u32::from(total_width)) as u16;
        self.split_percent = raw_percent.clamp(25, 75);
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

fn urgency_label(urgency: &str) -> &str {
    match urgency {
        "default" => "none",
        "punt" => "punted",
        other => other,
    }
}

fn kind_state_icon(kind: &str, state: &str) -> &'static str {
    let done = state == "done";
    match kind {
        "task" => {
            if done {
                "▶"
            } else {
                "▷"
            }
        }
        "goal" => {
            if done {
                "◆"
            } else {
                "◇"
            }
        }
        "bug" => {
            if done {
                "●"
            } else {
                "⊘"
            }
        }
        _ => {
            if done {
                "●"
            } else {
                "◦"
            }
        }
    }
}

fn icon_color(kind: &str, state: &str) -> Color {
    if state == "done" {
        return Color::DarkGray;
    }
    if state == "doing" {
        return Color::Yellow;
    }
    match kind {
        "bug" => Color::Red,
        "goal" => Color::Cyan,
        _ => Color::Green,
    }
}

fn title_style_for_urgency(urgency: &str) -> Style {
    match urgency {
        "urgent" => Style::default().add_modifier(Modifier::BOLD),
        "punt" => Style::default().add_modifier(Modifier::ITALIC | Modifier::DIM),
        _ => Style::default(),
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

fn size_marker(size: &str) -> &str {
    match size {
        "xs" => "⠁",
        "s" => "⠉",
        "m" => "⠛",
        "l" => "⠿",
        "xl" => "⣿",
        _ => size,
    }
}

/// Build one table `Row` from a `WorkItem` and hierarchy depth.
fn build_row(item: &WorkItem, depth: usize, width: u16, is_selected: bool) -> Row<'static> {
    let indent = "  ".repeat(depth);
    let icon = kind_state_icon(&item.kind, &item.state);
    let labels_full = item
        .labels
        .iter()
        .map(|label| format!("[{label}]"))
        .collect::<Vec<_>>()
        .join(" ");
    let available = width.saturating_sub((depth as u16) * 2 + 2) as usize;
    let id_prefix = format!("{} ", item.item_id);
    let size_prefix = item
        .size
        .as_ref()
        .map(|size| format!("{} ", size_marker(size)))
        .unwrap_or_default();
    let text_budget = available
        .saturating_sub(id_prefix.chars().count())
        .saturating_sub(size_prefix.chars().count());
    let title_min = 20_usize.min(text_budget);
    let label_max = text_budget.saturating_sub(title_min).min(24);
    let label_with_gap = if labels_full.is_empty() || label_max == 0 {
        String::new()
    } else {
        truncate(&format!(" {labels_full}"), label_max)
    };
    let title_budget = text_budget.saturating_sub(label_with_gap.chars().count());
    let title = truncate(&item.title, title_budget);

    let id_style = if is_selected {
        Style::default()
    } else {
        Style::default().fg(Color::DarkGray)
    };

    let cell = Cell::from(Line::from(vec![
        Span::raw(indent),
        Span::styled(
            icon.to_string(),
            Style::default().fg(icon_color(&item.kind, &item.state)),
        ),
        Span::raw(" "),
        Span::styled(item.item_id.clone(), id_style),
        Span::raw(" "),
        Span::styled(size_prefix, Style::default().fg(Color::Cyan)),
        Span::styled(title, title_style_for_urgency(&item.urgency)),
        Span::styled(label_with_gap, Style::default().fg(Color::Yellow)),
    ]));
    Row::new([cell])
}

fn done_separator_text(width: u16) -> String {
    let label = " Done ";
    let total = width.max(label.len() as u16) as usize;
    if total <= label.len() {
        return label.to_string();
    }
    let side = total - label.len();
    let left = side / 2;
    let right = side - left;
    format!("{}{}{}", "─".repeat(left), label, "─".repeat(right))
}

fn micros_to_local_datetime(us: i64) -> String {
    DateTime::<Utc>::from_timestamp_micros(us)
        .map(|ts| {
            ts.with_timezone(&Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| us.to_string())
}

fn push_ref_section(
    lines: &mut Vec<Line<'static>>,
    heading: &str,
    refs: &[DetailRef],
    heading_color: Color,
) {
    if refs.is_empty() {
        return;
    }
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        format!("{heading}:"),
        Style::default()
            .fg(heading_color)
            .add_modifier(Modifier::BOLD),
    )]));
    for item in refs {
        let mut spans = vec![
            Span::styled("  └─ ", Style::default().fg(Color::DarkGray)),
            Span::styled(item.id.clone(), Style::default().fg(Color::Cyan)),
        ];
        if let Some(title) = &item.title {
            spans.push(Span::raw("  "));
            spans.push(Span::styled(
                title.clone(),
                Style::default().fg(Color::White),
            ));
        }
        lines.push(Line::from(spans));
    }
}

fn detail_lines(detail: &DetailItem) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    lines.push(Line::from(vec![Span::styled(
        detail.title.clone(),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("ID: ", Style::default().fg(Color::DarkGray)),
        Span::styled(detail.id.clone(), Style::default().fg(Color::Cyan)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Type: ", Style::default().fg(Color::DarkGray)),
        Span::raw(detail.kind.clone()),
        Span::raw("  "),
        Span::styled("State: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            detail.state.clone(),
            Style::default().fg(state_color(&detail.state)),
        ),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Urgency: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            urgency_label(&detail.urgency).to_string(),
            Style::default().fg(urgency_color(&detail.urgency)),
        ),
    ]));
    if let Some(size) = &detail.size {
        lines.push(Line::from(vec![
            Span::styled("Size: ", Style::default().fg(Color::DarkGray)),
            Span::raw(size.clone()),
        ]));
    }
    if let Some(parent_id) = &detail.parent_id {
        lines.push(Line::from(vec![
            Span::styled("Parent: ", Style::default().fg(Color::DarkGray)),
            Span::raw(parent_id.clone()),
        ]));
    }
    if !detail.labels.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Labels: ", Style::default().fg(Color::DarkGray)),
            Span::styled(detail.labels.join(", "), Style::default().fg(Color::Yellow)),
        ]));
    }
    if !detail.assignees.is_empty() {
        lines.push(Line::from(vec![
            Span::styled("Assignees: ", Style::default().fg(Color::DarkGray)),
            Span::raw(detail.assignees.join(", ")),
        ]));
    }
    if let Some(description) = &detail.description {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            "Description",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]));
        lines.push(Line::from(""));
        lines.extend(super::markdown::markdown_to_lines(description));
    }
    if !detail.comments.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("Comments ({})", detail.comments.len()),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]));
        for comment in &detail.comments {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(comment.author.clone(), Style::default().fg(Color::Cyan)),
                Span::raw("  "),
                Span::styled(
                    micros_to_local_datetime(comment.created_at_us),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            lines.extend(super::markdown::markdown_to_lines(&comment.body));
        }
    }

    push_ref_section(&mut lines, "Blocked by", &detail.blockers, Color::LightRed);
    push_ref_section(&mut lines, "Blocks", &detail.blocked, Color::LightCyan);
    push_ref_section(&mut lines, "Related", &detail.relationships, Color::Magenta);

    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("Created: ", Style::default().fg(Color::DarkGray)),
        Span::raw(micros_to_local_datetime(detail.created_at_us)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Updated: ", Style::default().fg(Color::DarkGray)),
        Span::raw(micros_to_local_datetime(detail.updated_at_us)),
    ]));

    lines
}

fn render_detail_panel(frame: &mut ratatui::Frame<'_>, app: &ListView, area: Rect) {
    let border_style = Style::default().fg(Color::Green);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(border_style)
        .title(" Detail ")
        .title_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(area);
    frame.render_widget(block, area);

    if let Some(detail) = &app.detail_item {
        frame.render_widget(
            Paragraph::new(detail_lines(detail))
                .scroll((app.detail_scroll, 0))
                .wrap(Wrap { trim: false }),
            inner,
        );
    } else {
        frame.render_widget(
            Paragraph::new(Line::from(vec![Span::styled(
                "No bone selected",
                Style::default().fg(Color::DarkGray),
            )])),
            inner,
        );
    }
}

fn render_create_modal(frame: &mut ratatui::Frame<'_>, app: &ListView, area: Rect) {
    let Some(modal) = app.create_modal.as_ref() else {
        return;
    };

    let modal_width = area.width.saturating_sub(4).min(80);
    let modal_height = area.height.saturating_sub(4).min(20);
    let x = area.x + area.width.saturating_sub(modal_width) / 2;
    let y = area.y + area.height.saturating_sub(modal_height) / 2;
    let modal_area = Rect::new(x, y, modal_width, modal_height);

    frame.render_widget(Clear, modal_area);

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(3),
            Constraint::Min(5),
            Constraint::Length(3),
        ])
        .split(modal_area);

    let title_focused = modal.focus == CreateField::Title;
    let title_border = if title_focused {
        Color::Green
    } else {
        Color::DarkGray
    };
    let title_block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(title_border))
        .title(" Title ")
        .title_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    let title_text = if title_focused {
        with_cursor_line(
            &modal.title,
            modal.title_cursor,
            Style::default().fg(Color::White),
        )
    } else {
        Line::from(modal.title.clone())
    };
    // Scroll so cursor stays visible (1 inner row, width minus borders)
    let title_inner_w = chunks[0].width.saturating_sub(2) as usize;
    let title_col_offset = if modal.title_cursor >= title_inner_w {
        (modal.title_cursor - title_inner_w + 1) as u16
    } else {
        0
    };
    frame.render_widget(
        Paragraph::new(title_text)
            .block(title_block)
            .scroll((0, title_col_offset)),
        chunks[0],
    );

    let desc_focused = modal.focus == CreateField::Description;
    let desc_border = if desc_focused {
        Color::Green
    } else {
        Color::DarkGray
    };
    let desc_title = if desc_focused {
        " Description --- Press <tab> to switch fields "
    } else {
        " Description "
    };
    let desc_lines: Vec<Line<'static>> = modal
        .description
        .iter()
        .enumerate()
        .map(|(row, line)| {
            if desc_focused && row == modal.desc_row {
                with_cursor_line(line, modal.desc_col, Style::default().fg(Color::White))
            } else {
                Line::from(line.clone())
            }
        })
        .collect();
    // Scroll description so cursor row/col stay visible
    let desc_inner_h = chunks[1].height.saturating_sub(2) as usize;
    let desc_inner_w = chunks[1].width.saturating_sub(2) as usize;
    let desc_row_offset = if desc_focused && modal.desc_row >= desc_inner_h {
        (modal.desc_row - desc_inner_h + 1) as u16
    } else {
        0
    };
    let desc_col_offset = if desc_focused && modal.desc_col >= desc_inner_w {
        (modal.desc_col - desc_inner_w + 1) as u16
    } else {
        0
    };
    frame.render_widget(
        Paragraph::new(desc_lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(border::ROUNDED)
                    .border_style(Style::default().fg(desc_border))
                    .title(desc_title)
                    .title_style(
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
            )
            .scroll((desc_row_offset, desc_col_offset)),
        chunks[1],
    );

    let type_focused = modal.focus == CreateField::Kind;
    let size_focused = modal.focus == CreateField::Size;
    let urgency_focused = modal.focus == CreateField::Urgency;
    let labels_focused = modal.focus == CreateField::Labels;
    let options_border = if type_focused || size_focused || urgency_focused || labels_focused {
        Color::Green
    } else {
        Color::DarkGray
    };
    let action_verb = if app.create_modal_edit_item_id.is_some() {
        "save"
    } else {
        "create"
    };
    let options_block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(options_border))
        .title(format!(" Options --- Press <ctrl+s> to {action_verb} "))
        .title_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    let options_inner = options_block.inner(chunks[2]);
    frame.render_widget(options_block, chunks[2]);

    let type_style = if type_focused {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let size_style = if size_focused {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };
    let urgency_style = if urgency_focused {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::REVERSED | Modifier::BOLD)
    } else {
        Style::default().fg(Color::White)
    };

    let labels_style = if labels_focused {
        Style::default().fg(Color::Green)
    } else {
        Style::default().fg(Color::White)
    };

    let mut options_spans = vec![
        Span::styled("Type: ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!(" {} ", modal.kind()), type_style),
        Span::raw("   "),
        Span::styled("Size: ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            format!(" {} ", modal.size().unwrap_or_else(|| "(none)".to_string())),
            size_style,
        ),
        Span::raw("   "),
        Span::styled("Urgency: ", Style::default().fg(Color::DarkGray)),
        Span::styled(format!(" {} ", modal.urgency_display()), urgency_style),
        Span::raw("   "),
        Span::styled("Labels: ", Style::default().fg(Color::DarkGray)),
    ];
    if labels_focused {
        options_spans.extend(with_cursor_spans(
            &modal.labels,
            modal.labels_cursor,
            labels_style,
        ));
    } else if modal.labels.is_empty() {
        options_spans.push(Span::styled("(none)".to_string(), labels_style));
    } else {
        options_spans.push(Span::styled(modal.labels.clone(), labels_style));
    }
    let options_line = Line::from(options_spans);
    frame.render_widget(Paragraph::new(options_line), options_inner);
}

fn render_note_modal(frame: &mut ratatui::Frame<'_>, app: &ListView, area: Rect) {
    let Some(modal) = app.note_modal.as_ref() else {
        return;
    };

    let modal_width = area.width.saturating_sub(8).min(96);
    let modal_height = area.height.saturating_sub(6).min(20);
    let x = area.x + area.width.saturating_sub(modal_width) / 2;
    let y = area.y + area.height.saturating_sub(modal_height) / 2;
    let modal_area = Rect::new(x, y, modal_width, modal_height);

    frame.render_widget(Clear, modal_area);

    let title = match modal.mode {
        NoteMode::Comment => " Comment ",
        NoteMode::Transition { target, .. } if target == State::Open => " Reopen Reason ",
        NoteMode::Transition { .. } => " Completion Note ",
    };

    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(modal_area);

    let lines: Vec<Line<'static>> = modal
        .lines
        .iter()
        .enumerate()
        .map(|(row, line)| {
            if row == modal.row {
                with_cursor_line(line, modal.col, Style::default().fg(Color::White))
            } else {
                Line::from(line.clone())
            }
        })
        .collect();
    // Scroll note so cursor row/col stay visible
    let note_inner_h = chunks[0].height.saturating_sub(2) as usize;
    let note_inner_w = chunks[0].width.saturating_sub(2) as usize;
    let note_row_offset = if modal.row >= note_inner_h {
        (modal.row - note_inner_h + 1) as u16
    } else {
        0
    };
    let note_col_offset = if modal.col >= note_inner_w {
        (modal.col - note_inner_w + 1) as u16
    } else {
        0
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .border_set(border::ROUNDED)
                    .border_style(Style::default().fg(Color::Green))
                    .title(title)
                    .title_style(
                        Style::default()
                            .fg(Color::White)
                            .add_modifier(Modifier::BOLD),
                    ),
            )
            .scroll((note_row_offset, note_col_offset)),
        chunks[0],
    );

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Ctrl+S", Style::default().fg(Color::Cyan)),
            Span::styled(" submit  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::styled(" cancel", Style::default().fg(Color::DarkGray)),
        ])),
        chunks[1],
    );
}

fn render_blocker_modal(frame: &mut ratatui::Frame<'_>, app: &ListView, area: Rect) {
    let Some(modal) = app.blocker_modal.as_ref() else {
        return;
    };

    let modal_width = area.width.saturating_sub(8).min(80);
    let modal_height = area.height.saturating_sub(6).min(24);
    let x = area.x + area.width.saturating_sub(modal_width) / 2;
    let y = area.y + area.height.saturating_sub(modal_height) / 2;
    let modal_area = Rect::new(x, y, modal_width, modal_height);

    frame.render_widget(Clear, modal_area);

    // Outer block.
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Add Link ")
        .title_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    // Split inner into: relation row (1), search row (1), list (rest), footer (1).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(2),
            Constraint::Length(1),
        ])
        .split(inner);

    // Relation type row.
    let rel_spans = vec![
        Span::styled("Type: ", Style::default().fg(Color::DarkGray)),
        Span::styled("◄ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            modal.rel_type.label(),
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        ),
        Span::styled(" ►", Style::default().fg(Color::DarkGray)),
    ];
    frame.render_widget(Paragraph::new(Line::from(rel_spans)), chunks[0]);

    // Search row.
    let search_label_style = if modal.search_focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    let search_spans = if modal.search_focused {
        with_cursor_spans(
            &modal.search,
            modal.search_cursor,
            Style::default().fg(Color::White),
        )
    } else if modal.search.is_empty() {
        vec![Span::styled(
            "(press / to search)",
            Style::default().fg(Color::DarkGray),
        )]
    } else {
        vec![Span::styled(
            modal.search.as_str(),
            Style::default().fg(Color::White),
        )]
    };
    let mut search_line_spans = vec![Span::styled("Search: ", search_label_style)];
    search_line_spans.extend(search_spans);
    frame.render_widget(Paragraph::new(Line::from(search_line_spans)), chunks[1]);

    // Item list.
    let filtered = modal.filtered();
    let list_height = chunks[2].height as usize;
    let scroll = if modal.list_idx >= list_height {
        modal.list_idx - list_height + 1
    } else {
        0
    };
    let rows: Vec<Row<'_>> = filtered
        .iter()
        .enumerate()
        .skip(scroll)
        .take(list_height)
        .map(|(i, (id, title))| {
            let selected = i == modal.list_idx;
            let id_style = if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::DarkGray)
            };
            let title_style = if selected {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::Gray)
            };
            let prefix = if selected { "> " } else { "  " };
            Row::new(vec![
                Cell::from(format!("{prefix}{id}")).style(id_style),
                Cell::from(title.clone()).style(title_style),
            ])
        })
        .collect();

    let col_id_w = modal_width.saturating_sub(4) / 4;
    let col_title_w = modal_width.saturating_sub(4).saturating_sub(col_id_w);
    frame.render_widget(
        Table::new(
            rows,
            [Constraint::Length(col_id_w), Constraint::Min(col_title_w)],
        )
        .block(Block::default().borders(Borders::TOP)),
        chunks[2],
    );

    // Footer.
    let footer_spans = if modal.search_focused {
        vec![
            Span::styled("type to search", Style::default().fg(Color::DarkGray)),
            Span::styled("  Enter", Style::default().fg(Color::Cyan)),
            Span::styled(": confirm  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::styled(": clear", Style::default().fg(Color::DarkGray)),
        ]
    } else {
        vec![
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::styled(": add  ", Style::default().fg(Color::DarkGray)),
            Span::styled("/", Style::default().fg(Color::Cyan)),
            Span::styled(": search  ", Style::default().fg(Color::DarkGray)),
            Span::styled("j/k", Style::default().fg(Color::Cyan)),
            Span::styled(": navigate  ", Style::default().fg(Color::DarkGray)),
            Span::styled("←/→", Style::default().fg(Color::Cyan)),
            Span::styled(": type  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::styled(": cancel", Style::default().fg(Color::DarkGray)),
        ]
    };
    frame.render_widget(Paragraph::new(Line::from(footer_spans)), chunks[3]);
}

fn render_edit_link_modal(frame: &mut ratatui::Frame<'_>, app: &ListView, area: Rect) {
    let Some(modal) = app.edit_link_modal.as_ref() else {
        return;
    };

    let modal_width = area.width.saturating_sub(8).min(80);
    let modal_height = area.height.saturating_sub(6).min(24);
    let x = area.x + area.width.saturating_sub(modal_width) / 2;
    let y = area.y + area.height.saturating_sub(modal_height) / 2;
    let modal_area = Rect::new(x, y, modal_width, modal_height);

    frame.render_widget(Clear, modal_area);

    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Edit Links ")
        .title_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(modal_area);
    frame.render_widget(block, modal_area);

    // Split inner into: list (rest), footer (1).
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(2), Constraint::Length(1)])
        .split(inner);

    let list_height = chunks[0].height as usize;
    let scroll = if modal.list_idx >= list_height {
        modal.list_idx - list_height + 1
    } else {
        0
    };

    let col_id_w = 12u16;
    let col_type_w = 16u16;
    let col_title_w = modal_width
        .saturating_sub(4)
        .saturating_sub(col_id_w)
        .saturating_sub(col_type_w);

    let rows: Vec<Row<'_>> = modal
        .links
        .iter()
        .enumerate()
        .skip(scroll)
        .take(list_height)
        .map(|(i, link)| {
            let selected = i == modal.list_idx;
            let changed = link.is_changed();

            let base_fg = if link.deleted {
                Color::DarkGray
            } else if changed {
                Color::Yellow
            } else {
                Color::Gray
            };

            let id_style = if selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(base_fg)
            };
            let title_style = if selected && !link.deleted {
                Style::default()
                    .fg(Color::White)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(base_fg)
            };

            let prefix = if selected { "> " } else { "  " };
            let title_text = link
                .peer_title
                .as_deref()
                .unwrap_or("(untitled)")
                .to_string();

            let type_text = if link.deleted {
                "DELETED".to_string()
            } else if selected {
                format!("\u{25c4} {} \u{25ba}", link.current_type.label())
            } else {
                link.current_type.label().to_string()
            };

            let type_style = if link.deleted {
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD)
            } else if changed {
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(base_fg)
            };

            Row::new(vec![
                Cell::from(format!("{prefix}{}", link.peer_id)).style(id_style),
                Cell::from(title_text).style(title_style),
                Cell::from(type_text).style(type_style),
            ])
        })
        .collect();

    frame.render_widget(
        Table::new(
            rows,
            [
                Constraint::Length(col_id_w),
                Constraint::Min(col_title_w),
                Constraint::Length(col_type_w),
            ],
        )
        .block(Block::default().borders(Borders::TOP)),
        chunks[0],
    );

    // Footer.
    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::styled(": save  ", Style::default().fg(Color::DarkGray)),
            Span::styled("d", Style::default().fg(Color::Cyan)),
            Span::styled(": delete  ", Style::default().fg(Color::DarkGray)),
            Span::styled("\u{2190}/\u{2192}", Style::default().fg(Color::Cyan)),
            Span::styled(": type  ", Style::default().fg(Color::DarkGray)),
            Span::styled("j/k", Style::default().fg(Color::Cyan)),
            Span::styled(": navigate  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::styled(": cancel", Style::default().fg(Color::DarkGray)),
        ])),
        chunks[1],
    );
}

fn help_hotkeys() -> Vec<(&'static str, &'static str, &'static str)> {
    vec![
        ("j/k", "list", "move selection"),
        ("f", "list", "page down"),
        ("b/u", "list", "page up"),
        ("enter/l", "list", "open detail pane"),
        ("s", "list", "cycle list sort"),
        ("a", "list", "add bone"),
        ("F", "list", "open filter popup"),
        ("D", "list", "toggle done visibility"),
        ("/", "global", "search bones"),
        ("?", "global", "open help overlay"),
        ("q", "global", "quit tui"),
        ("j/k", "detail", "scroll detail pane"),
        ("f", "detail", "page detail pane down"),
        ("u", "detail", "page detail pane up"),
        ("h/esc", "detail", "close detail pane"),
        ("e", "detail", "edit selected bone"),
        ("c", "detail", "add comment"),
        ("L", "detail", "add link/blocker/parent"),
        ("E", "detail", "edit/remove links"),
        ("x", "detail", "done/reopen with note"),
        ("Tab", "create", "next field"),
        ("Shift+Tab", "create", "previous field"),
        ("Ctrl+S", "create", "save/create bone"),
        ("Esc", "create", "cancel create/edit"),
        ("Ctrl+S", "note", "save note"),
        ("Esc", "note", "cancel note"),
        ("Tab", "filter", "next filter field"),
        ("Enter", "filter", "apply/confirm"),
        ("Esc", "filter", "close filter popup"),
    ]
}

fn render_help_overlay(frame: &mut ratatui::Frame<'_>, app: &ListView, area: Rect) {
    let popup_w = area.width.saturating_sub(8).min(96);
    let popup_h = area.height.saturating_sub(6).min(28);
    let x = area.x + area.width.saturating_sub(popup_w) / 2;
    let y = area.y + area.height.saturating_sub(popup_h) / 2;
    let popup = Rect::new(x, y, popup_w, popup_h);

    frame.render_widget(Clear, popup);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_set(border::ROUNDED)
        .border_style(Style::default().fg(Color::Green))
        .title(" Hotkeys ")
        .title_style(
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        );
    let inner = block.inner(popup);
    frame.render_widget(block, popup);

    let rows_area = Rect {
        x: inner.x,
        y: inner.y + 2,
        width: inner.width,
        height: inner.height.saturating_sub(3),
    };

    let query = app.help_query.to_ascii_lowercase();
    let mut lines: Vec<Line<'static>> = help_hotkeys()
        .into_iter()
        .filter(|(key, ctx, desc)| {
            query.is_empty()
                || key.to_ascii_lowercase().contains(&query)
                || ctx.to_ascii_lowercase().contains(&query)
                || desc.to_ascii_lowercase().contains(&query)
        })
        .map(|(key, ctx, desc)| {
            Line::from(vec![
                Span::styled(format!("{key:10}"), Style::default().fg(Color::Cyan)),
                Span::styled(format!("{ctx:8}"), Style::default().fg(Color::Cyan)),
                Span::styled(desc.to_string(), Style::default().fg(Color::White)),
            ])
        })
        .collect();
    if lines.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            "No hotkeys match the current filter",
            Style::default().fg(Color::DarkGray),
        )]));
    }

    let mut query_spans = vec![Span::styled(
        "Filter: ",
        Style::default().fg(Color::DarkGray),
    )];
    query_spans.extend(with_cursor_spans(
        &app.help_query,
        app.help_cursor,
        Style::default().fg(Color::White),
    ));
    let query_line = Line::from(query_spans);
    frame.render_widget(Paragraph::new(query_line), Rect { height: 1, ..inner });

    frame.render_widget(Paragraph::new(lines), rows_area);

    frame.render_widget(
        Paragraph::new(Line::from(vec![
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
            Span::styled(" close  ", Style::default().fg(Color::DarkGray)),
            Span::styled("Type", Style::default().fg(Color::Cyan)),
            Span::styled(" search hotkeys", Style::default().fg(Color::DarkGray)),
        ])),
        Rect {
            x: inner.x,
            y: inner.y + inner.height.saturating_sub(1),
            width: inner.width,
            height: 1,
        },
    );
}

/// Render the list view into a specific area of the frame.
fn render_into(frame: &mut ratatui::Frame<'_>, app: &mut ListView, area: Rect) {
    // Layout: content + status bar.
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Min(3), Constraint::Length(1)])
        .split(area);

    let content_area = chunks[0];
    let status_area = chunks[1];

    let content_chunks = if app.show_detail {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([
                Constraint::Percentage(app.split_percent),
                Constraint::Percentage(100 - app.split_percent),
            ])
            .split(content_area)
    } else {
        Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Percentage(100), Constraint::Percentage(0)])
            .split(content_area)
    };

    let table_area = content_chunks[0];
    let detail_area = content_chunks[1];
    app.list_area = table_area;
    app.detail_area = detail_area;
    app.clamp_detail_scroll();

    let body_width = table_area.width.saturating_sub(4).max(10);
    let widths = [Constraint::Min(10)];

    let mut rows: Vec<Row<'static>> = Vec::with_capacity(app.table_row_count());
    for (index, item) in app.visible_items.iter().enumerate() {
        if app.done_start_idx == Some(index) {
            rows.push(Row::new([Cell::from(Line::from(vec![Span::styled(
                done_separator_text(body_width),
                Style::default().fg(Color::DarkGray),
            )]))]));
        }
        let depth = app.visible_depths.get(index).copied().unwrap_or(0);
        let is_selected = app.table_state.selected() == Some(index);
        rows.push(build_row(item, depth, body_width, is_selected));
    }

    let block_title = match app.input_mode {
        InputMode::Search => format!(
            " bones — search: {} ",
            with_cursor_marker(&app.search_buf, app.search_cursor)
        ),
        _ => format!(
            " bones — {} of {} bones  [sort: {}] ",
            app.visible_items.len(),
            app.all_items.len(),
            app.sort.label()
        ),
    };

    let list_border_style = if app.show_detail {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default().fg(Color::Green)
    };
    let list_title_style = if app.show_detail {
        Style::default()
            .fg(Color::DarkGray)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD)
    };

    let table = Table::new(rows, widths)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_set(border::ROUNDED)
                .border_style(list_border_style)
                .title(block_title)
                .title_style(list_title_style),
        )
        .row_highlight_style(
            Style::default()
                .bg(Color::DarkGray)
                .add_modifier(Modifier::BOLD),
        )
        .highlight_symbol(" ");

    let selected_visible = app.table_state.selected();
    let mut render_state = app.table_state.clone();
    render_state.select(selected_visible.map(|idx| app.table_row_from_visible_index(idx)));
    frame.render_stateful_widget(table, table_area, &mut render_state);
    app.table_state = render_state;
    app.table_state.select(selected_visible);

    if app.show_detail && detail_area.width > 0 {
        render_detail_panel(frame, app, detail_area);
    }

    // -----------------------------------------------------------------------
    // Status bar
    // -----------------------------------------------------------------------
    let status_text = build_status_bar(app, status_area.width);
    let status_paragraph = Paragraph::new(status_text).alignment(Alignment::Left);
    frame.render_widget(status_paragraph, status_area);

    // -----------------------------------------------------------------------
    // Filter popup overlay
    // -----------------------------------------------------------------------
    if app.input_mode == InputMode::FilterPopup || app.input_mode == InputMode::FilterLabel {
        render_filter_popup(frame, app, area);
    }
    if app.input_mode == InputMode::CreateModal {
        render_create_modal(frame, app, area);
    }
    if app.input_mode == InputMode::NoteModal {
        render_note_modal(frame, app, area);
    }
    if app.input_mode == InputMode::BlockerModal {
        render_blocker_modal(frame, app, area);
    }
    if app.input_mode == InputMode::EditLinkModal {
        render_edit_link_modal(frame, app, area);
    }
    if app.input_mode == InputMode::Help {
        render_help_overlay(frame, app, area);
    }
}

/// Build the status bar line from current filter state.
fn build_status_bar(app: &ListView, width: u16) -> Line<'static> {
    // Errors take highest priority: show in red for 10 seconds.
    if let Some((ref msg, at)) = app.error_msg
        && at.elapsed() < Duration::from_secs(10)
    {
        return Line::from(vec![Span::styled(
            format!("error: {msg}"),
            Style::default().fg(Color::Red),
        )]);
    }

    // Show a transient status message if recent (< 3 seconds).
    if let Some((ref msg, at)) = app.status_msg
        && at.elapsed() < Duration::from_secs(3)
    {
        return Line::from(vec![Span::styled(
            msg.clone(),
            Style::default().fg(Color::Cyan),
        )]);
    }

    let mut spans: Vec<Span<'static>> = Vec::new();

    let key_style = Style::default().fg(Color::Cyan);
    let label_style = Style::default().fg(Color::White);
    let val_style = Style::default().fg(Color::Cyan);
    let dim_style = Style::default().fg(Color::DarkGray);

    // Left padding
    spans.push(Span::raw(" "));

    match app.input_mode {
        InputMode::Search => {
            spans.push(Span::styled("esc", key_style));
            spans.push(Span::styled(" cancel  ", dim_style));
            spans.push(Span::styled("enter", key_style));
            spans.push(Span::styled(" confirm", dim_style));
        }
        InputMode::CreateModal => {
            spans.push(Span::styled("tab", key_style));
            spans.push(Span::styled(" next field  ", dim_style));
            spans.push(Span::styled("shift+tab", key_style));
            spans.push(Span::styled(" prev field  ", dim_style));
            spans.push(Span::styled("ctrl+s", key_style));
            spans.push(Span::styled(" save  ", dim_style));
            spans.push(Span::styled("esc", key_style));
            spans.push(Span::styled(" cancel", dim_style));
        }
        InputMode::NoteModal => {
            spans.push(Span::styled("ctrl+s", key_style));
            spans.push(Span::styled(" submit note  ", dim_style));
            spans.push(Span::styled("esc", key_style));
            spans.push(Span::styled(" cancel", dim_style));
        }
        InputMode::BlockerModal => {
            spans.push(Span::styled("enter", key_style));
            spans.push(Span::styled(": add  ", dim_style));
            spans.push(Span::styled("j/k", key_style));
            spans.push(Span::styled(": navigate  ", dim_style));
            spans.push(Span::styled("←/→", key_style));
            spans.push(Span::styled(": type  ", dim_style));
            spans.push(Span::styled("esc", key_style));
            spans.push(Span::styled(": cancel", dim_style));
        }
        InputMode::EditLinkModal => {
            spans.push(Span::styled("enter", key_style));
            spans.push(Span::styled(": save  ", dim_style));
            spans.push(Span::styled("d", key_style));
            spans.push(Span::styled(": delete  ", dim_style));
            spans.push(Span::styled("←/→", key_style));
            spans.push(Span::styled(": type  ", dim_style));
            spans.push(Span::styled("j/k", key_style));
            spans.push(Span::styled(": navigate  ", dim_style));
            spans.push(Span::styled("esc", key_style));
            spans.push(Span::styled(": cancel", dim_style));
        }
        InputMode::FilterPopup | InputMode::FilterLabel => {
            spans.push(Span::styled("tab", key_style));
            spans.push(Span::styled(" move field  ", dim_style));
            spans.push(Span::styled("←/→", key_style));
            spans.push(Span::styled(" change value  ", dim_style));
            spans.push(Span::styled("enter", key_style));
            spans.push(Span::styled(" apply/edit  ", dim_style));
            spans.push(Span::styled("esc", key_style));
            spans.push(Span::styled(" close", dim_style));
        }
        InputMode::Help => {
            spans.push(Span::styled("type", key_style));
            spans.push(Span::styled(" search keys  ", dim_style));
            spans.push(Span::styled("backspace", key_style));
            spans.push(Span::styled(" delete char  ", dim_style));
            spans.push(Span::styled("esc", key_style));
            spans.push(Span::styled(" close help", dim_style));
        }
        InputMode::Normal => {
            if !app.filter.is_empty() {
                spans.push(Span::styled("FILTERS: ", label_style));
                if let Some(ref s) = app.filter.state {
                    spans.push(Span::styled(format!("state={s} "), val_style));
                }
                if let Some(ref k) = app.filter.kind {
                    spans.push(Span::styled(format!("kind={k} "), val_style));
                }
                if let Some(ref u) = app.filter.urgency {
                    spans.push(Span::styled(
                        format!("urgency={} ", urgency_label(u)),
                        val_style,
                    ));
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

            let hints = if app.show_detail {
                vec![
                    ("j/k", "scroll"),
                    ("f/b", "page"),
                    ("h/esc", "close"),
                    ("e", "edit"),
                    ("c", "comment"),
                    ("x", "done/reopen"),
                    ("?", "help"),
                    ("q", "quit"),
                ]
            } else {
                vec![
                    ("j/k", "nav"),
                    ("f/b", "page"),
                    ("enter/l", "open"),
                    ("a", "add"),
                    ("F", "filter"),
                    ("s", "sort"),
                    (
                        "D",
                        if app.show_done {
                            "hide done"
                        } else {
                            "show done"
                        },
                    ),
                    ("?", "help"),
                    ("q", "quit"),
                ]
            };
            for (key, desc) in &hints {
                spans.push(Span::styled((*key).to_string(), key_style));
                spans.push(Span::styled(format!(" {desc}  "), dim_style));
            }
        }
    }

    let version = format!("bones {}", env!("CARGO_PKG_VERSION"));
    let right_part = format!("{version} "); // trailing space for right padding
    let left_len: usize = spans.iter().map(|span| span.content.chars().count()).sum();
    let right_len = right_part.chars().count();
    if (width as usize) > left_len + right_len + 1 {
        spans.push(Span::raw(" ".repeat(width as usize - left_len - right_len)));
    } else {
        spans.push(Span::raw("  "));
    }
    spans.push(Span::styled(right_part, dim_style));

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
        );
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

        let val_display = match (*field, value.as_deref()) {
            (FilterField::Urgency, Some(v)) => urgency_label(v),
            (_, Some(v)) => v,
            (_, None) => "(any)",
        };
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
            if editing {
                String::new()
            } else {
                "(any)".to_string()
            }
        } else {
            app.label_buf.clone()
        };
        let mut line_spans = vec![
            Span::styled(prefix.to_string(), focused_style),
            Span::styled("Label  ".to_string(), label_style),
            Span::styled(": ".to_string(), dim_style),
        ];
        if editing && is_focused {
            line_spans.extend(with_cursor_spans(&val_display, app.label_cursor, val_style));
        } else {
            line_spans.push(Span::styled(val_display, val_style));
        }
        line_spans.push(if editing {
            Span::styled("  type to edit, Enter done".to_string(), dim_style)
        } else {
            Span::styled("  Enter to edit".to_string(), dim_style)
        });
        let line = Line::from(line_spans);
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
            Span::styled("Tab", Style::default().fg(Color::Cyan)),
            Span::styled("/", dim_style),
            Span::styled("Shift+Tab", Style::default().fg(Color::Cyan)),
            Span::styled(" navigate  ", dim_style),
            Span::styled("Enter", Style::default().fg(Color::Cyan)),
            Span::styled(" apply  ", dim_style),
            Span::styled("Esc", Style::default().fg(Color::Cyan)),
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
            size: None,
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

    #[test]
    fn execution_sort_places_blockers_before_blocked_items() {
        let mut items = vec![
            make_item(
                "bn-39t",
                "Urgent blocked",
                "open",
                "task",
                "urgent",
                vec![],
                100,
                300,
            ),
            make_item(
                "bn-22v",
                "Prerequisite",
                "open",
                "task",
                "default",
                vec![],
                100,
                100,
            ),
        ];
        let mut blocker_map = HashMap::new();
        blocker_map.insert("bn-39t".to_string(), vec!["bn-22v".to_string()]);

        // Seed tie-break order similarly to runtime before execution ordering.
        sort_items(&mut items, SortField::Priority);
        sort_items_execution(&mut items, &blocker_map);

        assert_eq!(items[0].item_id, "bn-22v");
        assert_eq!(items[1].item_id, "bn-39t");
    }

    #[test]
    fn hierarchy_orders_children_beneath_parent() {
        let mut items = vec![
            make_item(
                "bn-001",
                "Parent",
                "open",
                "goal",
                "default",
                vec![],
                100,
                300,
            ),
            make_item(
                "bn-002",
                "Child",
                "open",
                "task",
                "default",
                vec![],
                100,
                200,
            ),
            make_item(
                "bn-003",
                "Sibling",
                "open",
                "task",
                "default",
                vec![],
                100,
                100,
            ),
        ];
        sort_items(&mut items, SortField::Updated);
        let mut parent_map = HashMap::new();
        parent_map.insert("bn-001".to_string(), None);
        parent_map.insert("bn-002".to_string(), Some("bn-001".to_string()));
        parent_map.insert("bn-003".to_string(), None);

        let (ordered, depths) = build_hierarchy_order(items, &parent_map);
        let ordered_ids: Vec<String> = ordered.into_iter().map(|item| item.item_id).collect();
        assert_eq!(ordered_ids, vec!["bn-001", "bn-002", "bn-003"]);
        assert_eq!(depths, vec![0, 1, 0]);
    }

    #[test]
    fn dependency_order_nests_blocked_under_blocker_chain() {
        let mut items = vec![
            make_item("bn-ccc", "C", "open", "task", "default", vec![], 100, 100),
            make_item("bn-bbb", "B", "open", "task", "default", vec![], 100, 200),
            make_item("bn-aaa", "A", "open", "task", "default", vec![], 100, 300),
        ];
        let blocker_map = HashMap::from([
            ("bn-bbb".to_string(), vec!["bn-aaa".to_string()]),
            ("bn-ccc".to_string(), vec!["bn-bbb".to_string()]),
        ]);

        sort_items(&mut items, SortField::Priority);
        sort_items_execution(&mut items, &blocker_map);
        let parent_map = HashMap::new();
        let (ordered, depths) = build_dependency_order(items, &blocker_map, &parent_map);
        let ordered_ids: Vec<String> = ordered.into_iter().map(|item| item.item_id).collect();

        assert_eq!(ordered_ids, vec!["bn-aaa", "bn-bbb", "bn-ccc"]);
        assert_eq!(depths, vec![0, 1, 2]);
    }

    #[test]
    fn dependency_order_groups_children_under_parent_goals() {
        // Phase I goal blocks Phase II goal (dependency edge).
        // Tasks A and B are children of Phase I (parent edge).
        // Task C is a child of Phase II (parent edge).
        // Task D has no parent.
        //
        // Expected: Phase I, then its children (A, B), then Phase II,
        // then its child (C), then D.
        let mut items = vec![
            make_item(
                "bn-p1",
                "Phase I",
                "open",
                "goal",
                "default",
                vec![],
                100,
                500,
            ),
            make_item(
                "bn-p2",
                "Phase II",
                "open",
                "goal",
                "default",
                vec![],
                100,
                400,
            ),
            make_item(
                "bn-a",
                "Task A",
                "open",
                "task",
                "default",
                vec![],
                100,
                300,
            ),
            make_item(
                "bn-b",
                "Task B",
                "open",
                "task",
                "default",
                vec![],
                100,
                200,
            ),
            make_item(
                "bn-c",
                "Task C",
                "open",
                "task",
                "default",
                vec![],
                100,
                150,
            ),
            make_item(
                "bn-d",
                "Task D",
                "open",
                "task",
                "default",
                vec![],
                100,
                100,
            ),
        ];

        // Phase II is blocked by Phase I.
        let blocker_map = HashMap::from([("bn-p2".to_string(), vec!["bn-p1".to_string()])]);

        // Tasks A, B are children of Phase I; Task C is child of Phase II.
        let parent_map = HashMap::from([
            ("bn-p1".to_string(), None),
            ("bn-p2".to_string(), None),
            ("bn-a".to_string(), Some("bn-p1".to_string())),
            ("bn-b".to_string(), Some("bn-p1".to_string())),
            ("bn-c".to_string(), Some("bn-p2".to_string())),
            ("bn-d".to_string(), None),
        ]);

        sort_items(&mut items, SortField::Priority);
        sort_items_execution(&mut items, &blocker_map);
        let (ordered, depths) = build_dependency_order(items, &blocker_map, &parent_map);
        let ordered_ids: Vec<String> = ordered.iter().map(|item| item.item_id.clone()).collect();

        // Phase I is a root; its hierarchy children (A, B) nest under it at depth 1.
        // Phase II is a dependency child of Phase I, at depth 1.
        // Task C nests under Phase II at depth 2.
        // Task D is a root with no parent.
        assert_eq!(
            ordered_ids,
            vec!["bn-p1", "bn-a", "bn-b", "bn-p2", "bn-c", "bn-d"],
            "parent goals should appear before their children"
        );
        // Phase I=0, A=1, B=1, Phase II=1, C=2, D=0
        assert_eq!(depths, vec![0, 1, 1, 1, 2, 0]);
    }

    #[test]
    fn kind_state_icons_fill_only_done() {
        assert_eq!(kind_state_icon("task", "open"), "▷");
        assert_eq!(kind_state_icon("task", "done"), "▶");
        assert_eq!(kind_state_icon("goal", "open"), "◇");
        assert_eq!(kind_state_icon("goal", "done"), "◆");
        assert_eq!(kind_state_icon("bug", "open"), "⊘");
        assert_eq!(kind_state_icon("bug", "done"), "●");
    }

    #[test]
    fn icon_color_doing_is_yellow() {
        assert_eq!(icon_color("task", "doing"), Color::Yellow);
        assert_eq!(icon_color("bug", "doing"), Color::Yellow);
        assert_eq!(icon_color("goal", "doing"), Color::Yellow);
        assert_eq!(icon_color("task", "done"), Color::DarkGray);
        assert_eq!(icon_color("task", "open"), Color::Green);
        assert_eq!(icon_color("bug", "open"), Color::Red);
        assert_eq!(icon_color("goal", "open"), Color::Cyan);
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
        let start = SortField::Execution;
        let s1 = start.next();
        let s2 = s1.next();
        let s3 = s2.next();
        let s4 = s3.next();
        let s5 = s4.next();
        assert_eq!(s1, SortField::Priority);
        assert_eq!(s2, SortField::Created);
        assert_eq!(s3, SortField::Updated);
        assert_eq!(s4, SortField::Tags);
        assert_eq!(s5, SortField::Execution);
    }

    // -----------------------------------------------------------------------
    // ListView navigation (no DB needed — operate on pre-loaded data)
    // -----------------------------------------------------------------------

    fn make_list_view() -> ListView {
        let mut view = ListView {
            db_path: PathBuf::from("/nonexistent"),
            project_root: PathBuf::from("/nonexistent"),
            agent: "tui-test".to_string(),
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
            visible_depths: Vec::new(),
            done_start_idx: None,
            parent_map: HashMap::new(),
            blocker_map: HashMap::new(),
            semantic_model: None,
            semantic_search_ids: Vec::new(),
            semantic_search_active: false,
            filter: FilterState::default(),
            sort: SortField::default(),
            table_state: TableState::default(),
            input_mode: InputMode::Normal,
            search_buf: String::new(),
            search_cursor: 0,
            search_prev_query: String::new(),
            label_buf: String::new(),
            label_cursor: 0,
            filter_field: FilterField::default(),
            should_quit: false,
            last_refresh: Instant::now(),
            refresh_interval: Duration::from_secs(2),
            status_msg: None,
            error_msg: None,
            show_detail: false,
            show_done: false,
            split_percent: 40,
            detail_scroll: 0,
            list_area: Rect::default(),
            detail_area: Rect::default(),
            split_resize_active: false,
            detail_item: None,
            detail_item_id: None,
            create_modal: None,
            create_modal_edit_item_id: None,
            note_modal: None,
            blocker_modal: None,
            edit_link_modal: None,
            help_query: String::new(),
            help_cursor: 0,
            needs_terminal_refresh: false,
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
        assert_eq!(view.table_state.selected(), Some(1)); // stays at last visible
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
        // Default execution sort still keeps bn-001 before bn-002 for this fixture.
        assert_eq!(item.item_id, "bn-002");
    }

    #[test]
    fn list_view_empty_items_no_selection() {
        let mut view = ListView {
            db_path: PathBuf::from("/nonexistent"),
            project_root: PathBuf::from("/nonexistent"),
            agent: "tui-test".to_string(),
            all_items: Vec::new(),
            visible_items: Vec::new(),
            visible_depths: Vec::new(),
            done_start_idx: None,
            parent_map: HashMap::new(),
            blocker_map: HashMap::new(),
            semantic_model: None,
            semantic_search_ids: Vec::new(),
            semantic_search_active: false,
            filter: FilterState::default(),
            sort: SortField::default(),
            table_state: TableState::default(),
            input_mode: InputMode::Normal,
            search_buf: String::new(),
            search_cursor: 0,
            search_prev_query: String::new(),
            label_buf: String::new(),
            label_cursor: 0,
            filter_field: FilterField::default(),
            should_quit: false,
            last_refresh: Instant::now(),
            refresh_interval: Duration::from_secs(2),
            status_msg: None,
            error_msg: None,
            show_detail: false,
            show_done: false,
            split_percent: 40,
            detail_scroll: 0,
            list_area: Rect::default(),
            detail_area: Rect::default(),
            split_resize_active: false,
            detail_item: None,
            detail_item_id: None,
            create_modal: None,
            create_modal_edit_item_id: None,
            note_modal: None,
            blocker_modal: None,
            edit_link_modal: None,
            help_query: String::new(),
            help_cursor: 0,
            needs_terminal_refresh: false,
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
        assert_eq!(view.sort, SortField::Execution);
        view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.sort, SortField::Priority);
        view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.sort, SortField::Created);
        view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.sort, SortField::Updated);
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
        assert_eq!(view.visible_items.len(), 2); // done remains hidden by default
    }

    #[test]
    fn list_view_f_key_pages_down() {
        let mut view = make_list_view();
        view.handle_key(KeyEvent::new(KeyCode::Char('f'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.table_state.selected(), Some(1));
        assert_eq!(view.input_mode, InputMode::Normal);
    }

    #[test]
    fn list_view_upper_f_opens_filter_popup() {
        let mut view = make_list_view();
        view.handle_key(KeyEvent::new(KeyCode::Char('F'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.input_mode, InputMode::FilterPopup);
    }

    #[test]
    fn list_view_a_opens_create_modal() {
        let mut view = make_list_view();
        view.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.input_mode, InputMode::CreateModal);
        assert!(view.create_modal.is_some());
    }

    #[test]
    fn list_view_detail_mode_jk_scrolls_not_selects() {
        let mut view = make_list_view();
        assert_eq!(view.table_state.selected(), Some(0));

        view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(view.show_detail);

        view.handle_key(KeyEvent::new(KeyCode::Char('j'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.table_state.selected(), Some(0));
        assert_eq!(view.detail_scroll, 0);

        view.handle_key(KeyEvent::new(KeyCode::Char('k'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.table_state.selected(), Some(0));
        assert_eq!(view.detail_scroll, 0);
    }

    #[test]
    fn list_view_detail_mode_does_not_cycle_sort() {
        let mut view = make_list_view();
        assert_eq!(view.sort, SortField::Execution);

        view.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE))
            .unwrap();
        assert!(view.show_detail);

        view.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::NONE))
            .unwrap();
        assert_eq!(view.sort, SortField::Execution);
    }

    #[test]
    fn list_view_d_toggles_done_visibility() {
        let mut view = make_list_view();
        assert!(!view.show_done);
        assert_eq!(view.visible_items.len(), 2);

        view.handle_key(KeyEvent::new(KeyCode::Char('D'), KeyModifiers::NONE))
            .unwrap();
        assert!(view.show_done);
        assert_eq!(view.visible_items.len(), 3);
    }

    #[test]
    fn list_view_done_separator_index_and_mapping() {
        let mut view = make_list_view();
        view.show_done = true;
        view.apply_filter_and_sort();

        assert_eq!(view.done_start_idx, Some(2));
        assert_eq!(view.table_row_count(), 4);
        assert_eq!(view.visible_index_from_table_row(2), None);
        assert_eq!(view.table_row_from_visible_index(2), 3);
    }

    #[test]
    fn list_view_done_items_show_newest_first() {
        let mut view = make_list_view();
        view.all_items = vec![
            make_item("bn-open", "Open", "open", "task", "default", vec![], 10, 10),
            make_item(
                "bn-done-a",
                "Done A",
                "done",
                "task",
                "default",
                vec![],
                10,
                20,
            ),
            make_item(
                "bn-done-b",
                "Done B",
                "done",
                "task",
                "default",
                vec![],
                10,
                30,
            ),
        ];
        view.show_done = true;
        view.apply_filter_and_sort();

        assert_eq!(view.done_start_idx, Some(1));
        assert_eq!(view.visible_items[0].item_id, "bn-open");
        assert_eq!(view.visible_items[1].item_id, "bn-done-b");
        assert_eq!(view.visible_items[2].item_id, "bn-done-a");
    }

    #[test]
    fn detail_scroll_clamps_to_content_end() {
        let mut view = make_list_view();
        view.show_detail = true;
        view.detail_area = Rect::new(0, 0, 20, 5);
        view.detail_item = Some(DetailItem {
            id: "bn-001".to_string(),
            title: "A long detail title that wraps in narrow panes".to_string(),
            description: Some("line one\nline two\nline three\nline four\nline five".to_string()),
            kind: "task".to_string(),
            state: "open".to_string(),
            urgency: "default".to_string(),
            size: None,
            parent_id: None,
            labels: vec![],
            assignees: vec![],
            blockers: vec![],
            blocked: vec![],
            relationships: vec![],
            comments: vec![],
            created_at_us: 0,
            updated_at_us: 0,
        });
        view.detail_scroll = 999;

        let max = view.max_detail_scroll();
        view.clamp_detail_scroll();
        assert_eq!(view.detail_scroll, max);
    }

    #[test]
    fn create_modal_description_accepts_newlines() {
        let mut modal = CreateModalState::default();
        modal.focus = CreateField::Description;

        assert_eq!(
            modal.handle_key(KeyEvent::new(KeyCode::Char('a'), KeyModifiers::NONE)),
            CreateAction::None
        );
        assert_eq!(
            modal.handle_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)),
            CreateAction::None
        );
        assert_eq!(
            modal.handle_key(KeyEvent::new(KeyCode::Char('b'), KeyModifiers::NONE)),
            CreateAction::None
        );

        assert_eq!(modal.description, vec!["a".to_string(), "b".to_string()]);
    }

    #[test]
    fn create_modal_ctrl_s_requires_title() {
        let mut modal = CreateModalState::default();
        assert_eq!(
            modal.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            CreateAction::None
        );

        assert_eq!(
            modal.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE)),
            CreateAction::None
        );
        assert_eq!(
            modal.handle_key(KeyEvent::new(KeyCode::Char('s'), KeyModifiers::CONTROL)),
            CreateAction::Submit
        );
    }
}
