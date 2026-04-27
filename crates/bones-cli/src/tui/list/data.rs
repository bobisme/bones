impl ListView {
    /// Create a new list view, loading items from the given database.
    pub fn new(db_path: PathBuf) -> Result<Self> {
        let project_root = db_path
            .parent()
            .and_then(Path::parent)
            .map_or_else(|| PathBuf::from("."), std::path::Path::to_path_buf);
        let agent = agent::require_agent(None).unwrap_or_else(|_| "tui".to_string());
        if let Err(e) = validate::validate_agent(&agent) {
            anyhow::bail!("invalid agent '{}': {}", e.value, e.reason);
        }

        let semantic_enabled = db_path
            .parent()
            .and_then(std::path::Path::parent)
            .and_then(|root| load_project_config(root).ok())
            .is_none_or(|cfg| cfg.search.semantic);
        let semantic_model = if semantic_enabled {
            match SemanticModel::load() {
                Ok(model) => Some(std::sync::Arc::new(model)),
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
            semantic_refinement_rx: None,
            semantic_search_gen: 0,
            last_searched_query: String::new(),
            search_refining: false,
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
            detail_lines_cache: Vec::new(),
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
            self.detail_lines_cache.clear();
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

        // Only re-run search if the query changed since the last search.
        // Auto-refresh reloads the item list but shouldn't re-trigger search
        // (it causes a visible flash as results clear and re-populate).
        let query_changed = self.filter.search_query.trim() != self.last_searched_query;
        if query_changed {
            let _ = self.refresh_semantic_search_ids();
        }
        self.apply_filter_and_sort();
        self.last_refresh = Instant::now();
        Ok(())
    }

    fn apply_filter_and_sort(&mut self) {
        let mut base_filter = self.filter.clone();
        base_filter.search_query.clear();
        let mut filtered = base_filter.apply(&self.all_items);

        let query_active = !self.filter.search_query.trim().is_empty();
        if query_active {
            let q = self.filter.search_query.trim().to_ascii_lowercase();
            if self.semantic_search_active {
                let rank_index: HashMap<&str, usize> = self
                    .semantic_search_ids
                    .iter()
                    .enumerate()
                    .map(|(idx, item_id)| (item_id.as_str(), idx))
                    .collect();
                filtered.retain(|bone| {
                    rank_index.contains_key(bone.item_id.as_str())
                        || local_search_rank(bone, &q).is_some()
                });
                filtered.sort_unstable_by(|a, b| {
                    search_sort_key(a, &q, &rank_index)
                        .cmp(&search_sort_key(b, &q, &rank_index))
                        .then_with(|| a.item_id.cmp(&b.item_id))
                });
            } else {
                filtered.retain(|bone| local_search_rank(bone, &q).is_some());
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

        let (mut ordered, mut depths) = if query_active && self.semantic_search_active {
            // Search results are already ranked; preserve flat order.
            let len = active_items.len();
            (active_items, vec![0; len])
        } else if !query_active && self.sort == SortField::Execution {
            build_dependency_order(active_items, &self.blocker_map, &self.parent_map)
        } else {
            build_hierarchy_order(active_items, &self.parent_map)
        };
        self.done_start_idx = None;
        if self.show_done && !done_items.is_empty() {
            // Show completed bones newest-first (reverse close order approximation).
            done_items.sort_unstable_by(|a, b| {
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
        if !self.show_detail || self.detail_lines_cache.is_empty() {
            return 0;
        }
        let viewport_h = self.detail_visible_height();
        if viewport_h == 0 {
            return 0;
        }
        let wrap_w = self.detail_area.width.saturating_sub(2).max(1) as usize;
        let total_lines = self
            .detail_lines_cache
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

}
