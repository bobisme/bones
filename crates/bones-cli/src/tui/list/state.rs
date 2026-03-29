// ---------------------------------------------------------------------------
// Clipboard
// ---------------------------------------------------------------------------

/// Copy text to the system clipboard using platform-native tools.
///
/// macOS: `pbcopy`
/// Linux: tries `wl-copy` (Wayland), then `xclip`, then `xsel`.
fn copy_to_clipboard(text: &str) -> Result<(), String> {
    use std::process::{Command, Stdio};

    let candidates: &[&[&str]] = if cfg!(target_os = "macos") {
        &[&["pbcopy"]]
    } else {
        &[
            &["wl-copy"],
            &["xclip", "-selection", "clipboard"],
            &["xsel", "--clipboard", "--input"],
        ]
    };

    for args in candidates {
        let prog = args[0];
        let extra = &args[1..];
        if let Ok(mut child) = Command::new(prog)
            .args(extra)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
        {
            if let Some(stdin) = child.stdin.as_mut() {
                use std::io::Write;
                let _ = stdin.write_all(text.as_bytes());
            }
            if child.wait().is_ok_and(|s| s.success()) {
                return Ok(());
            }
        }
    }

    Err("no clipboard tool found (install xclip, xsel, or wl-copy)".to_string())
}

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
    semantic_model: Option<std::sync::Arc<SemanticModel>>,
    /// Ranked IDs returned by semantic/hybrid slash search.
    semantic_search_ids: Vec<String>,
    /// Whether semantic search executed successfully for the active query.
    semantic_search_active: bool,
    /// Receiver for background semantic refinement results.
    semantic_refinement_rx: Option<std::sync::mpsc::Receiver<Vec<String>>>,
    /// Generation counter to discard stale background results.
    semantic_search_gen: u64,
    /// Query that was last searched (to avoid re-triggering on auto-refresh).
    last_searched_query: String,
    /// Whether a background semantic refinement is in progress.
    search_refining: bool,
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
    /// Cached rendered lines for the detail pane (invalidated when `detail_item` changes).
    detail_lines_cache: Vec<Line<'static>>,
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

