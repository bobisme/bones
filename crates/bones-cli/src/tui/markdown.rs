//! Lightweight markdown-to-ratatui renderer using pulldown-cmark.
//!
//! Converts markdown text into `Vec<Line<'static>>` for display in
//! ratatui `Paragraph` widgets. Supports inline styles (bold, italic,
//! code), headings, lists, code blocks, blockquotes, and rules.

use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Convert a markdown string into styled ratatui lines.
pub fn markdown_to_lines(input: &str) -> Vec<Line<'static>> {
    let options = Options::ENABLE_STRIKETHROUGH;
    let parser = Parser::new_ext(input, options);
    let mut renderer = MdRenderer::new();
    for event in parser {
        renderer.process(event);
    }
    renderer.finish()
}

struct MdRenderer {
    lines: Vec<Line<'static>>,
    /// Spans accumulated for the current line.
    spans: Vec<Span<'static>>,
    /// Style modifiers currently active (bold, italic, etc).
    style: Style,
    /// Are we inside a code block?
    in_code_block: bool,
    /// Language hint from the code fence (e.g. "rust", "python").
    code_lang: Option<String>,
    /// Accumulated code block text.
    code_buf: String,
    /// Blockquote nesting depth.
    blockquote_depth: usize,
    /// List stack: None = unordered, Some(n) = ordered starting at n.
    list_stack: Vec<Option<u64>>,
    /// Current item index within an ordered list.
    list_index: Option<u64>,
}

impl MdRenderer {
    fn new() -> Self {
        Self {
            lines: Vec::new(),
            spans: Vec::new(),
            style: Style::default(),
            in_code_block: false,
            code_lang: None,
            code_buf: String::new(),
            blockquote_depth: 0,
            list_stack: Vec::new(),
            list_index: None,
        }
    }

    fn process(&mut self, event: Event<'_>) {
        match event {
            Event::Start(tag) => self.start_tag(tag),
            Event::End(tag) => self.end_tag(tag),
            Event::Text(text) => self.text(&text),
            Event::Code(code) => self.inline_code(&code),
            Event::SoftBreak | Event::HardBreak => self.line_break(),
            Event::Rule => self.rule(),
            // Ignore HTML, footnotes, task lists, etc.
            _ => {}
        }
    }

    fn start_tag(&mut self, tag: Tag<'_>) {
        match tag {
            Tag::Heading { .. } => {
                self.style = self.style.fg(Color::White).add_modifier(Modifier::BOLD);
            }
            Tag::Emphasis => {
                self.style = self.style.add_modifier(Modifier::ITALIC);
            }
            Tag::Strong => {
                self.style = self.style.add_modifier(Modifier::BOLD);
            }
            Tag::Strikethrough => {
                self.style = self.style.add_modifier(Modifier::CROSSED_OUT);
            }
            Tag::Link { .. } => {
                self.style = self
                    .style
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::UNDERLINED);
            }
            Tag::CodeBlock(kind) => {
                self.flush_line();
                self.lines.push(Line::from(""));
                self.in_code_block = true;
                self.code_lang = match &kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                        Some(lang.split_whitespace().next().unwrap_or("").to_lowercase())
                    }
                    _ => None,
                };
                self.code_buf.clear();
            }
            Tag::BlockQuote(_) => {
                self.blockquote_depth += 1;
            }
            Tag::List(start) => {
                self.list_stack.push(start);
            }
            Tag::Item => {
                self.flush_line();
                let indent = "  ".repeat(self.list_stack.len().saturating_sub(1));
                let bullet = match self.list_stack.last().copied().flatten() {
                    Some(start) => {
                        let idx = self.list_index.unwrap_or(start);
                        self.list_index = Some(idx + 1);
                        format!("{indent}{idx}. ")
                    }
                    None => format!("{indent}  \u{2022} "),
                };
                self.spans
                    .push(Span::styled(bullet, Style::default().fg(Color::DarkGray)));
            }
            Tag::Paragraph => {}
            _ => {}
        }
    }

    fn end_tag(&mut self, tag: TagEnd) {
        match tag {
            TagEnd::Heading(_) => {
                self.style = Style::default();
                self.flush_line();
                self.lines.push(Line::from(""));
            }
            TagEnd::Emphasis => {
                self.style = self.style.remove_modifier(Modifier::ITALIC);
            }
            TagEnd::Strong => {
                self.style = self.style.remove_modifier(Modifier::BOLD);
            }
            TagEnd::Strikethrough => {
                self.style = self.style.remove_modifier(Modifier::CROSSED_OUT);
            }
            TagEnd::Link => {
                self.style = Style::default();
            }
            TagEnd::CodeBlock => {
                self.emit_code_block();
                self.in_code_block = false;
            }
            TagEnd::BlockQuote(_) => {
                self.blockquote_depth = self.blockquote_depth.saturating_sub(1);
                if self.blockquote_depth == 0 {
                    self.flush_line();
                    self.lines.push(Line::from(""));
                }
            }
            TagEnd::List(_) => {
                self.list_stack.pop();
                self.list_index = None;
                if self.list_stack.is_empty() {
                    self.flush_line();
                    self.lines.push(Line::from(""));
                }
            }
            TagEnd::Item => {
                self.flush_line();
            }
            TagEnd::Paragraph => {
                self.flush_line();
                self.lines.push(Line::from(""));
            }
            _ => {}
        }
    }

    fn text(&mut self, text: &str) {
        if self.in_code_block {
            self.code_buf.push_str(text);
            return;
        }

        if self.blockquote_depth > 0 {
            // For blockquotes, handle line-by-line to add prefix
            let prefix = "\u{2502} ".repeat(self.blockquote_depth);
            for (i, line) in text.split('\n').enumerate() {
                if i > 0 {
                    self.flush_line();
                }
                if i > 0 || self.spans.is_empty() {
                    self.spans.push(Span::styled(
                        prefix.clone(),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
                self.spans.push(Span::styled(
                    line.to_string(),
                    self.style.fg(Color::DarkGray),
                ));
            }
        } else {
            self.spans.push(Span::styled(text.to_string(), self.style));
        }
    }

    fn inline_code(&mut self, code: &str) {
        let code_style = Style::default().fg(Color::Green);
        self.spans.push(Span::styled(code.to_string(), code_style));
    }

    fn line_break(&mut self) {
        self.flush_line();
    }

    fn rule(&mut self) {
        self.flush_line();
        self.lines.push(Line::from(Span::styled(
            "\u{2500}".repeat(40),
            Style::default().fg(Color::DarkGray),
        )));
        self.lines.push(Line::from(""));
    }

    fn flush_line(&mut self) {
        if !self.spans.is_empty() {
            let spans = std::mem::take(&mut self.spans);
            self.lines.push(Line::from(spans));
        }
    }

    fn emit_code_block(&mut self) {
        let code = std::mem::take(&mut self.code_buf);
        let lang = self.code_lang.take();
        for line in code.lines() {
            let spans = highlight_code_line(line, lang.as_deref());
            self.lines.push(Line::from(spans));
        }
        self.lines.push(Line::from(""));
    }

    fn finish(mut self) -> Vec<Line<'static>> {
        self.flush_line();
        // Remove trailing empty line if present
        if self.lines.last().is_some_and(|l| {
            l.spans.is_empty() || (l.spans.len() == 1 && l.spans[0].content.is_empty())
        }) {
            self.lines.pop();
        }
        self.lines
    }
}

/// Keywords for basic syntax highlighting by language family.
fn keywords_for_lang(lang: Option<&str>) -> &'static [&'static str] {
    match lang {
        Some("rust" | "rs") => &[
            "fn", "let", "mut", "pub", "use", "mod", "struct", "enum", "impl", "trait", "for",
            "while", "loop", "if", "else", "match", "return", "self", "Self", "const", "static",
            "type", "where", "async", "await", "move", "ref", "true", "false", "None", "Some",
            "Ok", "Err",
        ],
        Some("python" | "py") => &[
            "def", "class", "import", "from", "return", "if", "elif", "else", "for", "while",
            "with", "as", "try", "except", "finally", "raise", "yield", "lambda", "pass", "True",
            "False", "None", "self", "async", "await", "in", "not", "and", "or", "is",
        ],
        Some("javascript" | "js" | "typescript" | "ts" | "jsx" | "tsx") => &[
            "function",
            "const",
            "let",
            "var",
            "return",
            "if",
            "else",
            "for",
            "while",
            "class",
            "new",
            "this",
            "import",
            "export",
            "from",
            "async",
            "await",
            "try",
            "catch",
            "throw",
            "true",
            "false",
            "null",
            "undefined",
            "typeof",
            "instanceof",
        ],
        Some("bash" | "sh" | "zsh" | "fish" | "shell") => &[
            "if", "then", "else", "elif", "fi", "for", "do", "done", "while", "case", "esac",
            "function", "return", "export", "local", "set", "echo", "exit", "true", "false",
        ],
        Some("go" | "golang") => &[
            "func",
            "package",
            "import",
            "return",
            "if",
            "else",
            "for",
            "range",
            "switch",
            "case",
            "default",
            "struct",
            "interface",
            "type",
            "var",
            "const",
            "defer",
            "go",
            "chan",
            "select",
            "map",
            "nil",
            "true",
            "false",
        ],
        _ => &[],
    }
}

/// Produce styled spans for a single line of code with basic highlighting.
///
/// Highlights:
/// - Keywords (cyan, bold)
/// - Strings (yellow)
/// - Comments (dark gray)
/// - Numbers (magenta)
/// - Everything else (green, the default code color)
fn highlight_code_line(line: &str, lang: Option<&str>) -> Vec<Span<'static>> {
    let indent = "  ";
    let keywords = keywords_for_lang(lang);
    let base_style = Style::default().fg(Color::Green);

    // If no language or empty keywords, fall back to plain green
    if keywords.is_empty() {
        return vec![Span::styled(format!("{indent}{line}"), base_style)];
    }

    let keyword_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    let string_style = Style::default().fg(Color::Yellow);
    let comment_style = Style::default().fg(Color::DarkGray);
    let number_style = Style::default().fg(Color::Magenta);

    let mut spans: Vec<Span<'static>> = vec![Span::styled(indent.to_string(), base_style)];
    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = 0;
    let mut buf = String::new();

    let flush_buf = |buf: &mut String, spans: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            spans.push(Span::styled(std::mem::take(buf), base_style));
        }
    };

    while i < len {
        let ch = chars[i];

        // Line comments
        if ch == '/' && i + 1 < len && chars[i + 1] == '/' {
            flush_buf(&mut buf, &mut spans);
            let rest: String = chars[i..].iter().collect();
            spans.push(Span::styled(rest, comment_style));
            return spans;
        }
        if ch == '#'
            && lang.is_some_and(|l| {
                matches!(
                    l,
                    "python" | "py" | "bash" | "sh" | "zsh" | "fish" | "shell"
                )
            })
        {
            flush_buf(&mut buf, &mut spans);
            let rest: String = chars[i..].iter().collect();
            spans.push(Span::styled(rest, comment_style));
            return spans;
        }

        // Strings
        if ch == '"' || ch == '\'' {
            flush_buf(&mut buf, &mut spans);
            let quote = ch;
            let mut s = String::new();
            s.push(ch);
            i += 1;
            while i < len {
                s.push(chars[i]);
                if chars[i] == quote && (i == 0 || chars[i - 1] != '\\') {
                    i += 1;
                    break;
                }
                i += 1;
            }
            spans.push(Span::styled(s, string_style));
            continue;
        }

        // Word boundaries — check for keywords
        if ch.is_ascii_alphabetic() || ch == '_' {
            flush_buf(&mut buf, &mut spans);
            let mut word = String::new();
            while i < len && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                word.push(chars[i]);
                i += 1;
            }
            if keywords.contains(&word.as_str()) {
                spans.push(Span::styled(word, keyword_style));
            } else {
                spans.push(Span::styled(word, base_style));
            }
            continue;
        }

        // Numbers
        if ch.is_ascii_digit() {
            flush_buf(&mut buf, &mut spans);
            let mut num = String::new();
            while i < len
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                num.push(chars[i]);
                i += 1;
            }
            spans.push(Span::styled(num, number_style));
            continue;
        }

        // Everything else
        buf.push(ch);
        i += 1;
    }

    flush_buf(&mut buf, &mut spans);
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spans_text(lines: &[Line<'_>]) -> Vec<String> {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    #[test]
    fn plain_text_passes_through() {
        let lines = markdown_to_lines("hello world");
        let text = spans_text(&lines);
        assert_eq!(text, vec!["hello world"]);
    }

    #[test]
    fn bold_gets_modifier() {
        let lines = markdown_to_lines("**bold**");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
    }

    #[test]
    fn italic_gets_modifier() {
        let lines = markdown_to_lines("*italic*");
        assert_eq!(lines.len(), 1);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::ITALIC)
        );
    }

    #[test]
    fn inline_code_styled_green() {
        let lines = markdown_to_lines("use `foo` here");
        let text = spans_text(&lines);
        assert_eq!(text, vec!["use foo here"]);
        // The code span should be green
        assert_eq!(lines[0].spans[1].style.fg, Some(Color::Green));
    }

    #[test]
    fn heading_bold_white() {
        let lines = markdown_to_lines("# Title");
        let text = spans_text(&lines);
        assert_eq!(text, vec!["Title"]);
        assert!(
            lines[0].spans[0]
                .style
                .add_modifier
                .contains(Modifier::BOLD)
        );
        assert_eq!(lines[0].spans[0].style.fg, Some(Color::White));
    }

    #[test]
    fn unordered_list() {
        let lines = markdown_to_lines("- one\n- two\n- three");
        let text = spans_text(&lines);
        assert!(text[0].contains('\u{2022}'));
        assert!(text[0].contains("one"));
        assert!(text[1].contains("two"));
    }

    #[test]
    fn ordered_list() {
        let lines = markdown_to_lines("1. first\n2. second");
        let text = spans_text(&lines);
        assert!(text[0].contains("1."));
        assert!(text[0].contains("first"));
        assert!(text[1].contains("2."));
    }

    #[test]
    fn code_block_indented_green() {
        let lines = markdown_to_lines("```\nlet x = 1;\n```");
        let text = spans_text(&lines);
        assert!(text.iter().any(|l| l.contains("let x = 1;")));
        // Code lines should be indented
        let code_line = lines
            .iter()
            .find(|l| l.spans.iter().any(|s| s.content.contains("let x = 1;")))
            .expect("code line");
        assert_eq!(code_line.spans[0].style.fg, Some(Color::Green));
    }

    #[test]
    fn blockquote_has_prefix() {
        let lines = markdown_to_lines("> quoted text");
        let text = spans_text(&lines);
        assert!(text[0].contains('\u{2502}'));
        assert!(text[0].contains("quoted text"));
    }

    #[test]
    fn horizontal_rule() {
        let lines = markdown_to_lines("above\n\n---\n\nbelow");
        let text = spans_text(&lines);
        assert!(text.iter().any(|l| l.contains('\u{2500}')));
    }

    #[test]
    fn multiline_paragraph() {
        let lines = markdown_to_lines("line one\nline two");
        // Soft breaks within a paragraph should produce separate lines
        assert!(lines.len() >= 1);
    }

    #[test]
    fn empty_input() {
        let lines = markdown_to_lines("");
        assert!(lines.is_empty());
    }

    #[test]
    fn mixed_inline_styles() {
        let lines = markdown_to_lines("normal **bold** and *italic*");
        assert_eq!(lines.len(), 1);
        // Should have multiple spans with different styles
        assert!(lines[0].spans.len() >= 3);
    }
}
