//! Text wrapping and mouse selection support for the TUI detail pane.
//!
//! The detail pane used to hand its lines to `Paragraph::wrap`, which meant
//! ratatui decided where the soft breaks landed and the app had no way to map a
//! screen cell back to a character in the text. Wrapping here instead gives a
//! 1:1 mapping between cached lines and screen rows, which is what click-drag
//! selection (and an accurate scroll extent) needs.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use unicode_width::UnicodeWidthChar as _;

/// A single display row: one wrapped line plus whether it continues the
/// previous row (a soft break) rather than starting a new source line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WrappedLine {
    /// Styled content for this display row.
    pub line: Line<'static>,
    /// True when this row was produced by a soft wrap of the previous row.
    pub continuation: bool,
}

/// A caret position inside the wrapped buffer.
///
/// `col` is a character index into the row's plain text, not a screen column,
/// so wide characters count once.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct Pos {
    /// Index into the wrapped-line buffer.
    pub line: usize,
    /// Character index within that line.
    pub col: usize,
}

/// An in-progress or completed selection, stored as the two endpoints the user
/// produced. `anchor` is where the drag started, so it may follow `cursor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Selection {
    /// Where the drag started.
    pub anchor: Pos,
    /// Where the pointer is now (or ended).
    pub cursor: Pos,
}

impl Selection {
    /// Create a collapsed selection at `pos`.
    pub const fn new(pos: Pos) -> Self {
        Self {
            anchor: pos,
            cursor: pos,
        }
    }

    /// Endpoints in document order.
    pub fn ordered(&self) -> (Pos, Pos) {
        if self.anchor <= self.cursor {
            (self.anchor, self.cursor)
        } else {
            (self.cursor, self.anchor)
        }
    }

    /// True when the selection covers no characters.
    pub fn is_empty(&self) -> bool {
        self.anchor == self.cursor
    }
}

/// Plain text of a line, with styling dropped.
pub fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect()
}

/// Character count of a line.
pub fn line_char_len(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.chars().count())
        .sum()
}

/// Display width of a character, treating control characters as zero-width.
fn char_width(c: char) -> usize {
    c.width().unwrap_or(0)
}

/// Map a screen column offset (0-based, relative to the text area) to a
/// character index in `line`.
///
/// A click past the end of the text clamps to the end, which is what makes
/// dragging off the right edge select the whole row.
pub fn col_from_screen_x(line: &Line<'_>, x: usize) -> usize {
    let mut screen = 0usize;
    let mut chars = 0usize;
    for span in &line.spans {
        for c in span.content.chars() {
            let w = char_width(c);
            // Land on this character while the click falls inside its cells.
            if x < screen + w.max(1) {
                return chars;
            }
            screen += w;
            chars += 1;
        }
    }
    chars
}

/// Wrap `lines` to `width` display columns, preserving span styles.
///
/// Breaks on whitespace when possible and falls back to a hard break for words
/// longer than the target width. The whitespace consumed by a soft break stays
/// at the end of the preceding row so that rejoining continuation rows
/// reproduces the original text; those trailing cells are past the viewport and
/// render as blanks.
pub fn wrap_lines(lines: &[Line<'static>], width: usize) -> Vec<WrappedLine> {
    let width = width.max(1);
    let mut out = Vec::with_capacity(lines.len());

    for line in lines {
        let chunks = wrap_single(line, width);
        for (idx, chunk) in chunks.into_iter().enumerate() {
            out.push(WrappedLine {
                line: chunk,
                continuation: idx > 0,
            });
        }
    }

    out
}

/// Styled character, used as the intermediate form while wrapping.
struct StyledChar {
    ch: char,
    style: Style,
}

/// Wrap one source line into one or more display rows.
fn wrap_single(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let chars: Vec<StyledChar> = line
        .spans
        .iter()
        .flat_map(|span| {
            let style = span.style;
            span.content.chars().map(move |ch| StyledChar { ch, style })
        })
        .collect();

    if chars.is_empty() {
        return vec![Line::from(Vec::new())];
    }

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut start = 0usize;

    while start < chars.len() {
        let end = break_at(&chars, start, width);
        rows.push(build_line(&chars[start..end], line.style, line.alignment));
        start = end;
    }

    if rows.is_empty() {
        rows.push(Line::from(Vec::new()));
    }
    rows
}

/// Find where the row starting at `start` should end.
///
/// No characters are ever dropped: whitespace at a soft break stays on the row
/// it terminates, overflowing the width if need be. Those cells are blank, so
/// the overflow is invisible, and it keeps continuation rows losslessly
/// rejoinable.
fn break_at(chars: &[StyledChar], start: usize, width: usize) -> usize {
    let mut screen = 0usize;
    let mut idx = start;
    // Index just past the most recent whitespace character that fit.
    let mut last_break: Option<usize> = None;

    while idx < chars.len() {
        let w = char_width(chars[idx].ch);
        // Always take at least one character so a too-wide char can't stall.
        if screen + w > width && idx > start {
            break;
        }
        screen += w;
        idx += 1;
        if chars[idx - 1].ch.is_whitespace() {
            last_break = Some(idx);
        }
    }

    if idx >= chars.len() {
        return chars.len();
    }

    // The row filled up exactly at a word end: absorb the following whitespace
    // run rather than pushing it onto the next row.
    if chars[idx].ch.is_whitespace() {
        while idx < chars.len() && chars[idx].ch.is_whitespace() {
            idx += 1;
        }
        return idx;
    }

    // Otherwise back up to the last word boundary so words stay intact.
    if let Some(brk) = last_break
        && brk > start
    {
        return brk;
    }

    // A single word longer than the row: hard break.
    idx
}

/// Rebuild a `Line` from styled characters, merging runs that share a style.
fn build_line(
    chars: &[StyledChar],
    line_style: Style,
    alignment: Option<ratatui::layout::Alignment>,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut current: Option<Style> = None;

    for sc in chars {
        match current {
            Some(style) if style == sc.style => buf.push(sc.ch),
            Some(style) => {
                spans.push(Span::styled(std::mem::take(&mut buf), style));
                buf.push(sc.ch);
                current = Some(sc.style);
            }
            None => {
                buf.push(sc.ch);
                current = Some(sc.style);
            }
        }
    }
    if let Some(style) = current {
        spans.push(Span::styled(buf, style));
    }

    let mut out = Line::from(spans).style(line_style);
    out.alignment = alignment;
    out
}

/// Extract the selected text from the wrapped buffer.
///
/// Rows joined across a soft wrap are concatenated directly; rows that start a
/// new source line are joined with a newline. That way copying a wrapped
/// paragraph yields the paragraph, not the viewport's line breaks.
pub fn selection_text(lines: &[WrappedLine], selection: Selection) -> String {
    let (start, end) = selection.ordered();
    if start == end || lines.is_empty() {
        return String::new();
    }

    let last = lines.len().saturating_sub(1);
    let start_line = start.line.min(last);
    let end_line = end.line.min(last);

    let mut out = String::new();
    for idx in start_line..=end_line {
        let Some(wrapped) = lines.get(idx) else {
            break;
        };
        let text: Vec<char> = line_text(&wrapped.line).chars().collect();
        let from = if idx == start_line {
            start.col.min(text.len())
        } else {
            0
        };
        let to = if idx == end_line {
            end.col.min(text.len())
        } else {
            text.len()
        };

        if idx > start_line && !wrapped.continuation {
            out.push('\n');
        }
        if from < to {
            out.extend(&text[from..to]);
        }
    }

    out
}

/// Style applied to selected cells.
fn selection_style() -> Style {
    Style::default().bg(Color::Blue).fg(Color::White)
}

/// Return `line` with characters in `[from, to)` restyled as selected.
pub fn highlight_line(line: &Line<'static>, from: usize, to: usize) -> Line<'static> {
    if from >= to {
        return line.clone();
    }

    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut idx = 0usize;

    for span in &line.spans {
        let len = span.content.chars().count();
        let span_start = idx;
        let span_end = idx + len;
        idx = span_end;

        if span_end <= from || span_start >= to {
            spans.push(span.clone());
            continue;
        }

        let chars: Vec<char> = span.content.chars().collect();
        let sel_from = from.saturating_sub(span_start);
        let sel_to = (to - span_start).min(len);

        if sel_from > 0 {
            spans.push(Span::styled(
                chars[..sel_from].iter().collect::<String>(),
                span.style,
            ));
        }
        spans.push(Span::styled(
            chars[sel_from..sel_to].iter().collect::<String>(),
            selection_style(),
        ));
        if sel_to < len {
            spans.push(Span::styled(
                chars[sel_to..].iter().collect::<String>(),
                span.style,
            ));
        }
    }

    let mut out = Line::from(spans).style(line.style);
    out.alignment = line.alignment;
    out
}

/// Compute the selected character range for a given wrapped-line index.
///
/// Returns `None` when the line falls outside the selection.
pub fn line_selection_range(
    selection: Selection,
    line_idx: usize,
    line_len: usize,
) -> Option<(usize, usize)> {
    let (start, end) = selection.ordered();
    if start == end || line_idx < start.line || line_idx > end.line {
        return None;
    }

    let from = if line_idx == start.line {
        start.col.min(line_len)
    } else {
        0
    };
    let to = if line_idx == end.line {
        end.col.min(line_len)
    } else {
        line_len
    };

    if from >= to { None } else { Some((from, to)) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn plain(lines: &[WrappedLine]) -> Vec<String> {
        lines.iter().map(|w| line_text(&w.line)).collect()
    }

    #[test]
    fn wraps_on_word_boundaries() {
        let lines = vec![Line::from("the quick brown fox jumps")];
        let wrapped = wrap_lines(&lines, 10);
        assert_eq!(plain(&wrapped), vec!["the quick ", "brown fox ", "jumps"]);
        assert_eq!(
            wrapped.iter().map(|w| w.continuation).collect::<Vec<_>>(),
            vec![false, true, true]
        );
    }

    #[test]
    fn hard_breaks_words_longer_than_width() {
        let lines = vec![Line::from("supercalifragilistic")];
        let wrapped = wrap_lines(&lines, 8);
        assert_eq!(plain(&wrapped), vec!["supercal", "ifragili", "stic"]);
    }

    #[test]
    fn preserves_empty_lines() {
        let lines = vec![Line::from("a"), Line::from(""), Line::from("b")];
        let wrapped = wrap_lines(&lines, 10);
        assert_eq!(plain(&wrapped), vec!["a", "", "b"]);
        assert!(wrapped.iter().all(|w| !w.continuation));
    }

    #[test]
    fn preserves_span_styles_across_a_wrap() {
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let lines = vec![Line::from(vec![
            Span::styled("hello ", bold),
            Span::raw("world again"),
        ])];
        let wrapped = wrap_lines(&lines, 8);
        assert_eq!(plain(&wrapped), vec!["hello ", "world ", "again"]);
        assert_eq!(wrapped[0].line.spans[0].style, bold);
        assert_eq!(wrapped[1].line.spans[0].style, Style::default());
    }

    #[test]
    fn wide_chars_respect_display_width() {
        // Each CJK char is two cells wide, so only three fit in a width of 6.
        let lines = vec![Line::from("日本語対応")];
        let wrapped = wrap_lines(&lines, 6);
        assert_eq!(plain(&wrapped), vec!["日本語", "対応"]);
    }

    #[test]
    fn screen_x_maps_through_wide_chars() {
        let line = Line::from("日本a");
        assert_eq!(col_from_screen_x(&line, 0), 0);
        assert_eq!(col_from_screen_x(&line, 1), 0);
        assert_eq!(col_from_screen_x(&line, 2), 1);
        assert_eq!(col_from_screen_x(&line, 3), 1);
        assert_eq!(col_from_screen_x(&line, 4), 2);
        // Past the end clamps to the line length.
        assert_eq!(col_from_screen_x(&line, 99), 3);
    }

    #[test]
    fn selection_text_rejoins_soft_wraps() {
        let lines = vec![Line::from("the quick brown fox jumps")];
        let wrapped = wrap_lines(&lines, 10);
        let sel = Selection {
            anchor: Pos { line: 0, col: 4 },
            cursor: Pos { line: 2, col: 5 },
        };
        assert_eq!(selection_text(&wrapped, sel), "quick brown fox jumps");
    }

    #[test]
    fn selection_text_keeps_hard_line_breaks() {
        let lines = vec![Line::from("alpha"), Line::from("beta")];
        let wrapped = wrap_lines(&lines, 40);
        let sel = Selection {
            anchor: Pos { line: 0, col: 0 },
            cursor: Pos { line: 1, col: 4 },
        };
        assert_eq!(selection_text(&wrapped, sel), "alpha\nbeta");
    }

    #[test]
    fn selection_text_handles_reversed_drag() {
        let lines = vec![Line::from("alpha beta")];
        let wrapped = wrap_lines(&lines, 40);
        let sel = Selection {
            anchor: Pos { line: 0, col: 10 },
            cursor: Pos { line: 0, col: 6 },
        };
        assert_eq!(selection_text(&wrapped, sel), "beta");
    }

    #[test]
    fn empty_selection_yields_no_text() {
        let lines = vec![Line::from("alpha")];
        let wrapped = wrap_lines(&lines, 40);
        let sel = Selection::new(Pos { line: 0, col: 2 });
        assert!(sel.is_empty());
        assert_eq!(selection_text(&wrapped, sel), "");
    }

    #[test]
    fn highlight_splits_a_span_into_three() {
        let line = Line::from("abcdef");
        let out = highlight_line(&line, 2, 4);
        assert_eq!(
            out.spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<Vec<_>>(),
            vec!["ab", "cd", "ef"]
        );
        assert_eq!(out.spans[1].style, selection_style());
        assert_eq!(out.spans[0].style, Style::default());
    }

    #[test]
    fn highlight_spanning_multiple_spans() {
        let line = Line::from(vec![Span::raw("abc"), Span::raw("def")]);
        let out = highlight_line(&line, 1, 5);
        let texts: Vec<&str> = out.spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(texts.concat(), "abcdef");
        // "a" | "bc" | "de" | "f"
        assert_eq!(texts, vec!["a", "bc", "de", "f"]);
    }

    #[test]
    fn line_selection_range_covers_middle_lines_fully() {
        let sel = Selection {
            anchor: Pos { line: 0, col: 3 },
            cursor: Pos { line: 2, col: 2 },
        };
        assert_eq!(line_selection_range(sel, 0, 10), Some((3, 10)));
        assert_eq!(line_selection_range(sel, 1, 10), Some((0, 10)));
        assert_eq!(line_selection_range(sel, 2, 10), Some((0, 2)));
        assert_eq!(line_selection_range(sel, 3, 10), None);
    }

    #[test]
    fn wrapped_rows_never_exceed_width_ignoring_trailing_space() {
        let lines = vec![Line::from(
            "a bb ccc dddd eeeee ffffff ggggggg hhhhhhhh iiiiiiiii",
        )];
        for width in 2..30usize {
            for w in wrap_lines(&lines, width) {
                let text = line_text(&w.line);
                let visible: usize = text.trim_end().chars().map(char_width).sum();
                assert!(
                    visible <= width,
                    "width {width}: row {text:?} is {visible} cells"
                );
            }
        }
    }

    #[test]
    fn wrapping_is_lossless() {
        let source = "the quick brown fox jumps over the lazy dog";
        let lines = vec![Line::from(source)];
        for width in 3..40usize {
            let wrapped = wrap_lines(&lines, width);
            let rejoined: String = wrapped.iter().map(|w| line_text(&w.line)).collect();
            assert_eq!(rejoined, source, "width {width} lost or added characters");
        }
    }
}
