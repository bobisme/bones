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
            semantic_refinement_rx: None,
            semantic_search_gen: 0,
            last_searched_query: String::new(),
            search_refining: false,
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
            semantic_refinement_rx: None,
            semantic_search_gen: 0,
            last_searched_query: String::new(),
            search_refining: false,
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
