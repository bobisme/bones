impl ListView {
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

            // Copy bone ID to clipboard
            KeyCode::Char('y') => {
                if let Some(item) = self.selected_item() {
                    let id = item.item_id.clone();
                    match copy_to_clipboard(&id) {
                        Ok(()) => self.set_status(format!("Copied {id}")),
                        Err(e) => self.set_status(format!("Copy failed: {e}")),
                    }
                }
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
            KeyCode::Char('j') | KeyCode::Down
                if !modal.links.is_empty() => {
                    modal.list_idx = (modal.list_idx + 1).min(modal.links.len() - 1);
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
            let mut updates = vec![
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
            ];

            let current_labels: HashSet<String> = self
                .detail_item
                .as_ref()
                .filter(|detail| detail.id == item_id)
                .map(|detail| detail.labels.iter().cloned().collect())
                .unwrap_or_default();
            let next_labels: HashSet<String> = draft.labels.iter().cloned().collect();

            for label in &draft.labels {
                if !current_labels.contains(label) {
                    updates.push((
                        "labels".to_string(),
                        json!({
                            "action": "add",
                            "label": label,
                        }),
                    ));
                }
            }

            let mut removed_labels: Vec<String> =
                current_labels.difference(&next_labels).cloned().collect();
            removed_labels.sort_unstable();
            for label in removed_labels {
                updates.push((
                    "labels".to_string(),
                    json!({
                        "action": "remove",
                        "label": label,
                    }),
                ));
            }

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
        self.detail_lines_cache.clear();
    }

    /// Rebuild the cached detail lines from the current `detail_item`.
    fn rebuild_detail_lines_cache(&mut self) {
        if let Some(ref detail) = self.detail_item {
            self.detail_lines_cache = detail_lines(detail);
        } else {
            self.detail_lines_cache.clear();
        }
    }

    fn refresh_selected_detail(&mut self) {
        if !self.show_detail {
            return;
        }

        let Some(selected_id) = self.selected_item().map(|item| item.item_id.clone()) else {
            self.detail_item = None;
            self.detail_item_id = None;
            self.detail_lines_cache.clear();
            return;
        };

        if self.detail_item_id.as_deref() == Some(selected_id.as_str()) {
            // Same bone selected — check if its updated_at changed before reloading.
            let cached_updated = self.detail_item.as_ref().map(|d| d.updated_at_us);
            let db_updated = query::try_open_projection(&self.db_path)
                .ok()
                .flatten()
                .and_then(|conn| query::get_item(&conn, &selected_id, false).ok().flatten())
                .map(|item| item.updated_at_us);
            if cached_updated == db_updated && cached_updated.is_some() {
                // Nothing changed — skip reload entirely.
                self.clamp_detail_scroll();
                return;
            }
            if let Ok(detail) = self.load_detail_item(&selected_id) {
                self.detail_item = Some(detail);
                self.rebuild_detail_lines_cache();
            }
            self.clamp_detail_scroll();
            return;
        }

        match self.load_detail_item(&selected_id) {
            Ok(detail) => {
                self.detail_item = Some(detail);
                self.detail_item_id = Some(selected_id);
                self.detail_scroll = 0;
                self.rebuild_detail_lines_cache();
            }
            Err(err) => {
                self.detail_item = None;
                self.detail_item_id = None;
                self.detail_lines_cache.clear();
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

        // Cap at 200 most recent comments to bound memory in the detail pane.
        let mut comments: Vec<DetailComment> =
            query::get_comments(&conn, item_id, Some(200), None)?
                .into_iter()
                .map(|comment| DetailComment {
                    author: comment.author,
                    body: comment.body,
                    created_at_us: comment.created_at_us,
                })
                .collect();
        comments.sort_by_key(|a| a.created_at_us);

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
    pub fn tick(&mut self) -> Result<()> {
        if self.last_refresh.elapsed() >= self.refresh_interval
            && let Err(e) = self.reload()
        {
            self.set_status(format!("DB refresh failed: {e:#}"));
        }

        // Poll for background semantic refinement results.
        if let Some(rx) = &self.semantic_refinement_rx
            && let Ok(refined_ids) = rx.try_recv()
        {
            tracing::debug!(count = refined_ids.len(), "tier-2 refinement applied");
            self.semantic_search_ids = refined_ids;
            self.semantic_refinement_rx = None;
            self.search_refining = false;
            self.apply_filter_and_sort();
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
