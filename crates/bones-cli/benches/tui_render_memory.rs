//! Benchmark measuring ratatui rendering memory behavior.
//!
//! Exercises the actual Terminal::draw() -> Paragraph::new().wrap() path
//! that the TUI uses on every frame (10x/sec), to check whether the
//! ratatui rendering pipeline causes RSS growth over many frames.

use pulldown_cmark::{Event as MdEvent, Options, Parser, Tag, TagEnd};
use ratatui::{
    Terminal,
    backend::TestBackend,
    layout::Rect,
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block, Borders, Paragraph, Wrap},
};
use std::time::Instant;

fn rss_bytes() -> usize {
    let statm = std::fs::read_to_string("/proc/self/statm").unwrap_or_default();
    let pages: usize = statm
        .split_whitespace()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(0);
    pages * 4096
}

fn rss_mb() -> f64 {
    rss_bytes() as f64 / (1024.0 * 1024.0)
}

/// Build detail_lines exactly as the TUI does: markdown-rendered comments
/// with styled Spans and owned Strings.
fn build_detail_lines(comments: &[(String, String, i64)]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();

    // Header
    lines.push(Line::from(vec![Span::styled(
        "Benchmark bone: implement feature with detailed requirements".to_string(),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(""));
    lines.push(Line::from(vec![
        Span::styled("ID: ".to_string(), Style::default().fg(Color::DarkGray)),
        Span::styled("bn-bench0000".to_string(), Style::default().fg(Color::Cyan)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Type: ".to_string(), Style::default().fg(Color::DarkGray)),
        Span::raw("task".to_string()),
        Span::raw("  ".to_string()),
        Span::styled("State: ".to_string(), Style::default().fg(Color::DarkGray)),
        Span::styled("open".to_string(), Style::default().fg(Color::Green)),
    ]));

    // Description with markdown
    lines.push(Line::from(""));
    lines.push(Line::from(vec![Span::styled(
        "Description".to_string(),
        Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    )]));
    lines.push(Line::from(""));
    lines.extend(markdown_to_lines(
        "## Overview\n\nThis bone tracks feature implementation.\n\n\
         ### Requirements\n\n- First with **bold**\n- Second with `code`\n\
         - Third\n\n```rust\nfn example() {\n    println!(\"hello\");\n}\n```",
    ));

    // Comments section
    if !comments.is_empty() {
        lines.push(Line::from(""));
        lines.push(Line::from(vec![Span::styled(
            format!("Comments ({})", comments.len()),
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )]));
        for (author, body, _ts) in comments {
            lines.push(Line::from(""));
            lines.push(Line::from(vec![
                Span::styled(author.clone(), Style::default().fg(Color::Cyan)),
                Span::raw("  ".to_string()),
                Span::styled(
                    "2025-01-15 10:30:00".to_string(),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            lines.extend(markdown_to_lines(body));
        }
    }

    lines
}

fn markdown_to_lines(input: &str) -> Vec<Line<'static>> {
    let options = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(input, options);
    let mut lines = Vec::new();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut style = Style::default();

    for event in parser {
        match event {
            MdEvent::Start(Tag::Heading { .. }) => {
                style = style.fg(Color::White).add_modifier(Modifier::BOLD);
            }
            MdEvent::End(TagEnd::Heading(_)) => {
                style = Style::default();
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
                lines.push(Line::from(""));
            }
            MdEvent::Start(Tag::Strong) => {
                style = style.add_modifier(Modifier::BOLD);
            }
            MdEvent::End(TagEnd::Strong) => {
                style = style.remove_modifier(Modifier::BOLD);
            }
            MdEvent::Start(Tag::CodeBlock(_)) => {
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
                lines.push(Line::from(""));
            }
            MdEvent::End(TagEnd::CodeBlock) => {
                lines.push(Line::from(""));
            }
            MdEvent::End(TagEnd::Paragraph) => {
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
                lines.push(Line::from(""));
            }
            MdEvent::Text(text) => {
                spans.push(Span::styled(text.to_string(), style));
            }
            MdEvent::Code(code) => {
                spans.push(Span::styled(
                    code.to_string(),
                    Style::default().fg(Color::Green),
                ));
            }
            MdEvent::SoftBreak | MdEvent::HardBreak => {
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
            }
            MdEvent::Start(Tag::Item) => {
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
                spans.push(Span::styled(
                    "  \u{2022} ".to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            MdEvent::End(TagEnd::Item) => {
                if !spans.is_empty() {
                    lines.push(Line::from(std::mem::take(&mut spans)));
                }
            }
            _ => {}
        }
    }
    if !spans.is_empty() {
        lines.push(Line::from(spans));
    }
    lines
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let n_comments: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(15);
    let n_frames: u32 = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(50_000);

    // Build synthetic comments (same as main benchmark)
    let comment_templates = [
        "Starting work on this bone. Will investigate the codebase first.\n\n## Plan\n\n1. Read existing code\n2. Identify changes needed\n3. Implement\n4. Test",
        "**Progress update**: Found the relevant files:\n\n- `src/main.rs` - entry point\n- `src/lib.rs` - core logic\n- `src/utils.rs` - helpers\n\n```rust\nfn process(input: &str) -> Result<Output> {\n    let parsed = parse(input)?;\n    transform(parsed)\n}\n```",
        "Ran into an issue with the `transform` function. The current implementation doesn't handle edge cases:\n\n> The input validation assumes ASCII-only strings, but we need UTF-8 support.\n\nWorking on a fix now.",
        "Fixed the UTF-8 issue. Here's the diff summary:\n\n- Changed `str::len()` to `str::chars().count()` in 3 places\n- Added tests for multibyte characters\n- Updated documentation",
        "All tests passing. Running `cargo test` output:\n\n```\nrunning 47 tests\ntest test_ascii ... ok\ntest test_utf8 ... ok\ntest test_empty ... ok\n...\ntest result: ok. 47 passed; 0 failed\n```\n\nReady for review.",
    ];

    let comments: Vec<(String, String, i64)> = (0..n_comments)
        .map(|j| {
            let author = if j % 2 == 0 {
                "ward-dev".to_string()
            } else {
                "ward-worker-1".to_string()
            };
            let body = comment_templates[j % comment_templates.len()].to_string();
            (author, body, 1_700_000_000_000_000 + (j as i64) * 100_000)
        })
        .collect();

    eprintln!("=== Ratatui Render Memory Benchmark ===");
    eprintln!("Comments: {n_comments}, Frames: {n_frames}");

    // Create a TestBackend terminal (120x50, typical terminal size)
    let backend = TestBackend::new(120, 50);
    let mut terminal = Terminal::new(backend).unwrap();

    let rss_baseline = rss_mb();
    eprintln!("RSS baseline: {rss_baseline:.1} MB");

    // === BENCHMARK: Render frames with Paragraph + Wrap ===
    // This is what happens 10x/sec in the real TUI
    let start = Instant::now();
    let mut detail_scroll: u16 = 0;

    for i in 0..n_frames {
        terminal
            .draw(|frame| {
                let area = frame.area();
                // Split into list (left) and detail (right) like the real TUI
                let list_width = area.width * 60 / 100;
                let detail_width = area.width - list_width;

                let detail_area = Rect::new(list_width, area.y, detail_width, area.height);

                // Build detail_lines fresh (same as real TUI - no caching)
                let lines = build_detail_lines(&comments);

                // Render with wrap (same as render_detail_panel)
                let block = Block::default().borders(Borders::ALL).title(" Detail ");
                let inner = block.inner(detail_area);
                frame.render_widget(block, detail_area);
                frame.render_widget(
                    Paragraph::new(lines)
                        .scroll((detail_scroll, 0))
                        .wrap(Wrap { trim: false }),
                    inner,
                );

                // Also render a fake list on the left (creates more Cells)
                let list_area = Rect::new(area.x, area.y, list_width, area.height);
                let mut list_lines = Vec::new();
                for j in 0..40_u16 {
                    list_lines.push(Line::from(vec![
                        Span::styled(format!("bn-bench{j:04} "), Style::default().fg(Color::Cyan)),
                        Span::raw(format!("Benchmark bone {j}: feature implementation")),
                    ]));
                }
                frame.render_widget(
                    Paragraph::new(list_lines)
                        .block(Block::default().borders(Borders::ALL).title(" Bones ")),
                    list_area,
                );
            })
            .unwrap();

        // Simulate scroll cycling (varies allocation patterns)
        detail_scroll = (detail_scroll + 1) % 50;

        if i % 10_000 == 0 {
            let rss_now = rss_mb();
            let elapsed = start.elapsed();
            eprintln!(
                "  frame {i:>6}: RSS = {rss_now:.1} MB (+{:.1}), elapsed = {elapsed:.1?}",
                rss_now - rss_baseline
            );
        }
    }

    let elapsed = start.elapsed();
    let rss_final = rss_mb();
    eprintln!();
    eprintln!("=== Results ===");
    eprintln!("Frames: {n_frames}");
    eprintln!("Total time: {elapsed:.2?}");
    eprintln!("Time per frame: {:.2?}", elapsed / n_frames);
    eprintln!("RSS baseline: {rss_baseline:.1} MB");
    eprintln!("RSS final:    {rss_final:.1} MB");
    eprintln!(
        "RSS growth:   {:.1} MB ({:.1}%)",
        rss_final - rss_baseline,
        if rss_baseline > 0.0 {
            (rss_final - rss_baseline) / rss_baseline * 100.0
        } else {
            0.0
        }
    );
    eprintln!(
        "Frames per second: {:.0}",
        n_frames as f64 / elapsed.as_secs_f64()
    );
}
