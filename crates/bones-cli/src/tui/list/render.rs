impl ListView {
    pub fn render(&mut self, frame: &mut ratatui::Frame<'_>, area: Rect) {
        render_into(frame, self, area);
    }

}

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

    if app.detail_item.is_some() && !app.detail_lines_cache.is_empty() {
        frame.render_widget(
            Paragraph::new(app.detail_lines_cache.clone())
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
        ("y", "global", "copy bone ID to clipboard"),
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

    let refining_indicator = if app.search_refining {
        const SPINNER: &[char] = &['|', '/', '-', '\\'];
        let tick = (app.last_refresh.elapsed().as_millis() / 150) as usize;
        let ch = SPINNER[tick % SPINNER.len()];
        format!(" [{ch}]")
    } else {
        String::new()
    };

    let block_title = match app.input_mode {
        InputMode::Search => format!(
            " bones — search: {}{refining_indicator} ",
            with_cursor_marker(&app.search_buf, app.search_cursor)
        ),
        _ if !app.filter.search_query.is_empty() => format!(
            " bones — {} results for \"{}\"{refining_indicator} ",
            app.visible_items.len(),
            app.filter.search_query,
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

