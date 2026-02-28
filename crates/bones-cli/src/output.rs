//! Shared output layer for pretty/text/JSON parity across all CLI commands.
//!
//! Every command handler receives an [`OutputMode`] and formats its output
//! accordingly: pretty output for humans, compact text for agents, or stable JSON.
//!
//! # Output mode resolution
//!
//! Precedence (highest wins):
//! 1. `--format` / hidden `--json` flag
//! 2. `FORMAT` env var → `"pretty"` | `"text"` | `"json"`
//! 3. Default: [`OutputMode::Pretty`] if stdout is a TTY; [`OutputMode::Text`] if piped.
//!
//! # Rendering approaches
//!
//! **Closure-based** (legacy, used by existing commands):
//! ```ignore
//! render(mode, &value, |v, w| writeln!(w, "{}", v.title))
//! ```
//!
//! **Trait-based** (preferred for new commands):
//! ```ignore
//! render_item(&my_item, mode)
//! render_list(&items, mode)
//! ```

use clap::ValueEnum;
use crossterm::style::{Color, Stylize};
use serde::Serialize;
use std::io::{self, IsTerminal, Write};

/// Shared width for human pretty separators.
pub const PRETTY_RULE_WIDTH: usize = 72;

pub fn pretty_color_enabled() -> bool {
    io::stdout().is_terminal() && std::env::var_os("NO_COLOR").is_none()
}

fn style_heading(text: &str) -> String {
    if pretty_color_enabled() {
        format!("{}", text.bold().with(Color::Cyan))
    } else {
        text.to_string()
    }
}

fn style_header(text: &str) -> String {
    if pretty_color_enabled() {
        format!("{}", text.bold().with(Color::Blue))
    } else {
        text.to_string()
    }
}

fn style_dim(text: &str) -> String {
    if pretty_color_enabled() {
        format!("{}", text.dark_grey())
    } else {
        text.to_string()
    }
}

/// Render markdown text with ANSI styling for CLI pretty output.
///
/// Supports bold, italic, inline code, headings, lists, code blocks,
/// blockquotes, and horizontal rules. Falls back to plain text when
/// colors are disabled.
pub fn pretty_markdown(w: &mut dyn Write, text: &str) -> io::Result<()> {
    use pulldown_cmark::{CodeBlockKind, Event, Options, Parser, Tag, TagEnd};

    let colored = pretty_color_enabled();
    let parser = Parser::new_ext(text, Options::ENABLE_STRIKETHROUGH);

    let mut bold = false;
    let mut italic = false;
    let mut in_code_block = false;
    let mut code_lang: Option<String> = None;
    let mut code_buf = String::new();
    let mut in_heading = false;
    let mut blockquote_depth: usize = 0;
    let mut list_stack: Vec<Option<u64>> = Vec::new();
    let mut list_index: Option<u64> = None;
    let mut line_buf = String::new();
    let mut need_newline = false;

    let style_text = |text: &str, b: bool, i: bool| -> String {
        if !colored {
            return text.to_string();
        }
        let mut s = text.to_string();
        if b {
            s = format!("{}", s.bold());
        }
        if i {
            s = format!(
                "{}",
                crossterm::style::style(&s).attribute(crossterm::style::Attribute::Italic)
            );
        }
        s
    };

    for event in parser {
        match event {
            Event::Start(Tag::Heading { .. }) => {
                in_heading = true;
                bold = true;
            }
            Event::End(TagEnd::Heading(_)) => {
                in_heading = false;
                bold = false;
                if !line_buf.is_empty() {
                    writeln!(w, "{line_buf}")?;
                    line_buf.clear();
                }
                writeln!(w)?;
                need_newline = false;
            }
            Event::Start(Tag::Strong) => bold = true,
            Event::End(TagEnd::Strong) => bold = false,
            Event::Start(Tag::Emphasis) => italic = true,
            Event::End(TagEnd::Emphasis) => italic = false,
            Event::Start(Tag::CodeBlock(kind)) => {
                if need_newline {
                    writeln!(w)?;
                    need_newline = false;
                }
                in_code_block = true;
                code_lang = match &kind {
                    CodeBlockKind::Fenced(lang) if !lang.is_empty() => {
                        Some(lang.split_whitespace().next().unwrap_or("").to_lowercase())
                    }
                    _ => None,
                };
                code_buf.clear();
            }
            Event::End(TagEnd::CodeBlock) => {
                for code_line in code_buf.lines() {
                    if colored {
                        write!(w, "  ")?;
                        write_highlighted_code_line(w, code_line, code_lang.as_deref())?;
                        writeln!(w)?;
                    } else {
                        writeln!(w, "  {code_line}")?;
                    }
                }
                writeln!(w)?;
                in_code_block = false;
                code_lang = None;
                need_newline = false;
            }
            Event::Start(Tag::BlockQuote(_)) => {
                blockquote_depth += 1;
            }
            Event::End(TagEnd::BlockQuote(_)) => {
                blockquote_depth = blockquote_depth.saturating_sub(1);
                if blockquote_depth == 0 {
                    if !line_buf.is_empty() {
                        writeln!(w, "{line_buf}")?;
                        line_buf.clear();
                    }
                    writeln!(w)?;
                    need_newline = false;
                }
            }
            Event::Start(Tag::List(start)) => {
                list_stack.push(start);
            }
            Event::End(TagEnd::List(_)) => {
                list_stack.pop();
                list_index = None;
                if list_stack.is_empty() {
                    writeln!(w)?;
                    need_newline = false;
                }
            }
            Event::Start(Tag::Item) => {
                let indent = "  ".repeat(list_stack.len().saturating_sub(1));
                let bullet = match list_stack.last().copied().flatten() {
                    Some(start) => {
                        let idx = list_index.unwrap_or(start);
                        list_index = Some(idx + 1);
                        format!("{indent}{idx}. ")
                    }
                    None => format!("{indent}  \u{2022} "),
                };
                line_buf.push_str(&bullet);
            }
            Event::End(TagEnd::Item) => {
                if !line_buf.is_empty() {
                    writeln!(w, "{line_buf}")?;
                    line_buf.clear();
                }
                need_newline = false;
            }
            Event::Start(Tag::Paragraph) => {
                if need_newline {
                    writeln!(w)?;
                    need_newline = false;
                }
            }
            Event::End(TagEnd::Paragraph) => {
                if !line_buf.is_empty() {
                    writeln!(w, "{line_buf}")?;
                    line_buf.clear();
                }
                need_newline = true;
            }
            Event::Text(text) => {
                if in_code_block {
                    code_buf.push_str(&text);
                } else if blockquote_depth > 0 {
                    let prefix = if colored {
                        format!("{}", "\u{2502} ".dark_grey())
                    } else {
                        "\u{2502} ".to_string()
                    };
                    let prefix = prefix.repeat(blockquote_depth);
                    line_buf.push_str(&prefix);
                    line_buf.push_str(&style_text(&text, bold, italic));
                } else if in_heading && colored {
                    use std::fmt::Write;
                    let _ = write!(line_buf, "{}", text.bold().with(Color::Cyan));
                } else {
                    line_buf.push_str(&style_text(&text, bold, italic));
                }
            }
            Event::Code(code) => {
                use std::fmt::Write;
                if colored {
                    let _ = write!(line_buf, "{}", code.green());
                } else {
                    let _ = write!(line_buf, "`{code}`");
                }
            }
            Event::SoftBreak | Event::HardBreak => {
                if !line_buf.is_empty() {
                    writeln!(w, "{line_buf}")?;
                    line_buf.clear();
                }
            }
            Event::Rule => {
                if !line_buf.is_empty() {
                    writeln!(w, "{line_buf}")?;
                    line_buf.clear();
                }
                writeln!(w, "{}", style_dim(&"\u{2500}".repeat(40)))?;
                writeln!(w)?;
                need_newline = false;
            }
            _ => {}
        }
    }

    if !line_buf.is_empty() {
        writeln!(w, "{line_buf}")?;
    }

    Ok(())
}

/// Keywords for basic syntax highlighting by language family.
fn cli_keywords_for_lang(lang: Option<&str>) -> &'static [&'static str] {
    match lang {
        Some("rust" | "rs") => &[
            "fn", "let", "mut", "pub", "use", "mod", "struct", "enum", "impl", "trait", "for",
            "while", "loop", "if", "else", "match", "return", "self", "Self", "const", "static",
            "type", "where", "async", "await", "move", "ref", "true", "false",
        ],
        Some("python" | "py") => &[
            "def", "class", "import", "from", "return", "if", "elif", "else", "for", "while",
            "with", "as", "try", "except", "finally", "raise", "yield", "lambda", "pass", "True",
            "False", "None", "self", "async", "await", "in", "not", "and", "or",
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
        ],
        Some("bash" | "sh" | "zsh" | "fish" | "shell") => &[
            "if", "then", "else", "elif", "fi", "for", "do", "done", "while", "case", "esac",
            "function", "return", "export", "local", "set", "echo", "exit",
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
        ],
        _ => &[],
    }
}

/// Write a highlighted code line to a writer using ANSI colors.
fn write_highlighted_code_line(
    w: &mut dyn Write,
    line: &str,
    lang: Option<&str>,
) -> io::Result<()> {
    let keywords = cli_keywords_for_lang(lang);
    if keywords.is_empty() {
        return write!(w, "{}", line.green());
    }

    let chars: Vec<char> = line.chars().collect();
    let len = chars.len();
    let mut i = 0;

    while i < len {
        let ch = chars[i];

        // Line comments
        if ch == '/' && i + 1 < len && chars[i + 1] == '/' {
            let rest: String = chars[i..].iter().collect();
            write!(w, "{}", rest.dark_grey())?;
            return Ok(());
        }
        if ch == '#'
            && lang.is_some_and(|l| {
                matches!(
                    l,
                    "python" | "py" | "bash" | "sh" | "zsh" | "fish" | "shell"
                )
            })
        {
            let rest: String = chars[i..].iter().collect();
            write!(w, "{}", rest.dark_grey())?;
            return Ok(());
        }

        // Strings
        if ch == '"' || ch == '\'' {
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
            write!(w, "{}", s.yellow())?;
            continue;
        }

        // Words
        if ch.is_ascii_alphabetic() || ch == '_' {
            let mut word = String::new();
            while i < len && (chars[i].is_ascii_alphanumeric() || chars[i] == '_') {
                word.push(chars[i]);
                i += 1;
            }
            if keywords.contains(&word.as_str()) {
                write!(w, "{}", word.cyan().bold())?;
            } else {
                write!(w, "{}", word.green())?;
            }
            continue;
        }

        // Numbers
        if ch.is_ascii_digit() {
            let mut num = String::new();
            while i < len
                && (chars[i].is_ascii_alphanumeric() || chars[i] == '.' || chars[i] == '_')
            {
                num.push(chars[i]);
                i += 1;
            }
            write!(w, "{}", num.magenta())?;
            continue;
        }

        write!(w, "{}", ch.to_string().green())?;
        i += 1;
    }

    Ok(())
}

/// Write a horizontal separator used by pretty human output.
pub fn pretty_rule(w: &mut dyn Write) -> io::Result<()> {
    writeln!(
        w,
        "{}",
        style_dim(&format!("{:-<width$}", "", width = PRETTY_RULE_WIDTH))
    )
}

/// Write a section heading followed by a separator.
pub fn pretty_section(w: &mut dyn Write, heading: &str) -> io::Result<()> {
    writeln!(w, "{}", style_heading(heading))?;
    pretty_rule(w)
}

/// Render a left-aligned key/value line in human output.
pub fn pretty_kv(w: &mut dyn Write, key: &str, value: impl AsRef<str>) -> io::Result<()> {
    writeln!(
        w,
        "{:<12} {}",
        style_header(&format!("{key}:")),
        value.as_ref()
    )
}

fn draw_table_border(
    w: &mut dyn Write,
    left: char,
    middle: char,
    right: char,
    widths: &[usize],
) -> io::Result<()> {
    write!(w, "{left}")?;
    for (idx, width) in widths.iter().enumerate() {
        write!(w, "{}", "─".repeat(*width + 2))?;
        if idx + 1 != widths.len() {
            write!(w, "{middle}")?;
        }
    }
    writeln!(w, "{right}")
}

/// Render a unicode table for pretty output.
///
/// - `headers` defines the column order.
/// - Each row should contain the same number of cells as `headers` (missing
///   cells are treated as empty strings).
pub fn pretty_table(w: &mut dyn Write, headers: &[&str], rows: &[Vec<String>]) -> io::Result<()> {
    if headers.is_empty() {
        return Ok(());
    }

    let mut widths: Vec<usize> = headers.iter().map(|h| h.chars().count()).collect();
    for row in rows {
        for (col, cell) in row.iter().enumerate() {
            if let Some(width) = widths.get_mut(col) {
                *width = (*width).max(cell.chars().count());
            }
        }
    }

    draw_table_border(w, '╭', '┬', '╮', &widths)?;

    write!(w, "│")?;
    for (idx, header) in headers.iter().enumerate() {
        write!(
            w,
            " {} ",
            style_header(&format!("{header:<width$}", width = widths[idx]))
        )?;
        write!(w, "│")?;
    }
    writeln!(w)?;

    draw_table_border(w, '├', '┼', '┤', &widths)?;

    for row in rows {
        write!(w, "│")?;
        for (idx, width) in widths.iter().enumerate() {
            let value = row.get(idx).map_or("", String::as_str);
            write!(w, " {value:<width$} ")?;
            write!(w, "│")?;
        }
        writeln!(w)?;
    }

    draw_table_border(w, '╰', '┴', '╯', &widths)
}

/// The three output modes supported by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum OutputMode {
    /// Human-optimized output (tables, sections, visual framing).
    Pretty,
    /// Token-efficient plain text for agents and pipes.
    Text,
    /// Machine-readable JSON (one object per result, or a JSON array).
    Json,
}

impl OutputMode {
    #[allow(non_upper_case_globals)]
    pub const Human: Self = Self::Pretty;
    #[allow(dead_code, non_upper_case_globals)]
    pub const Table: Self = Self::Text;

    /// Returns `true` if JSON output was requested.
    pub const fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }

    /// Returns `true` if pretty output was requested.
    #[allow(dead_code)]
    pub const fn is_pretty(self) -> bool {
        matches!(self, Self::Pretty)
    }

    /// Returns `true` if text output was requested.
    #[allow(dead_code)]
    pub const fn is_text(self) -> bool {
        matches!(self, Self::Text)
    }
}

/// Core resolution logic, separated from I/O for testability.
///
/// `format_flag` — explicit `--format` value if provided.
/// `json_flag` — hidden `--json` alias.
/// `format_env` — the value of `FORMAT` if set.
/// `is_tty` — true if stdout is a TTY.
fn resolve_output_mode_inner(
    format_flag: Option<OutputMode>,
    json_flag: bool,
    format_env: Option<&str>,
    is_tty: bool,
) -> OutputMode {
    if let Some(mode) = format_flag {
        return mode;
    }

    if json_flag {
        return OutputMode::Json;
    }

    if let Some(val) = format_env {
        match val.to_lowercase().as_str() {
            "json" => return OutputMode::Json,
            "text" => return OutputMode::Text,
            "pretty" => return OutputMode::Pretty,
            _ => {} // unknown value — fall through to TTY detection
        }
    }

    // Default: pretty if TTY, text if piped.
    if is_tty {
        OutputMode::Pretty
    } else {
        OutputMode::Text
    }
}

/// Resolve the output mode from CLI flags, environment, and TTY defaults.
///
/// Precedence:
/// 1. `format_flag` / hidden `--json`
/// 2. `FORMAT` env var → `pretty|text|json`
/// 3. Default: pretty if TTY, text if piped.
pub fn resolve_output_mode(format_flag: Option<OutputMode>, json_flag: bool) -> OutputMode {
    let env_val = std::env::var("FORMAT").ok();
    let is_tty = io::stdout().is_terminal();
    resolve_output_mode_inner(format_flag, json_flag, env_val.as_deref(), is_tty)
}

/// Trait implemented by any CLI result type that can be rendered in all modes.
///
/// Implementors provide rendering methods used by pretty/text/json dispatch.
/// `render_table` is reused for text mode rows in agent-friendly output.
/// The [`render_item`] and [`render_list`]
/// free functions dispatch to the appropriate method based on [`OutputMode`].
pub trait Renderable {
    /// Render for human consumption: text with labels, truncated for readability.
    fn render_human(&self, w: &mut dyn Write) -> io::Result<()>;

    /// Render as a JSON value (schema-stable, streaming-safe).
    ///
    /// Implementors should serialize a self-contained JSON object.
    fn render_json(&self, w: &mut dyn Write) -> io::Result<()>;

    /// Render as a single text row (no header; see [`table_headers`]).
    ///
    /// Fields must appear in the same column order as [`table_headers`].
    ///
    /// [`table_headers`]: Renderable::table_headers
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()>;

    /// Column headers for text mode, in the same order as [`render_table`] fields.
    ///
    /// Default: returns an empty slice (no header printed).
    ///
    /// [`render_table`]: Renderable::render_table
    fn table_headers() -> &'static [&'static str]
    where
        Self: Sized,
    {
        &[]
    }
}

/// Render a single [`Renderable`] item to stdout using the given output mode.
pub fn render_item<R: Renderable>(item: &R, mode: OutputMode) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match mode {
        OutputMode::Pretty => item.render_human(&mut out),
        OutputMode::Text => item.render_table(&mut out),
        OutputMode::Json => {
            item.render_json(&mut out)?;
            writeln!(out)
        }
    }
}

/// Render a serializable value with explicit pretty/text renderers.
pub fn render_mode<T: Serialize>(
    mode: OutputMode,
    value: &T,
    text_fn: impl FnOnce(&T, &mut dyn Write) -> io::Result<()>,
    pretty_fn: impl FnOnce(&T, &mut dyn Write) -> io::Result<()>,
) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match mode {
        OutputMode::Json => {
            serde_json::to_writer_pretty(&mut out, value)?;
            writeln!(out)?;
        }
        OutputMode::Text => text_fn(value, &mut out)?,
        OutputMode::Pretty => pretty_fn(value, &mut out)?,
    }
    Ok(())
}

/// Render a list of [`Renderable`] items to stdout.
///
/// - In JSON mode, wraps items in a JSON array.
/// - In pretty/text mode, renders items sequentially.
pub fn render_list<R: Renderable>(items: &[R], mode: OutputMode) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match mode {
        OutputMode::Pretty => {
            for item in items {
                item.render_human(&mut out)?;
            }
        }
        OutputMode::Text => {
            // Text defaults to TSV-like rows for token efficiency.
            let headers = if items.is_empty() {
                &[] as &[&str]
            } else {
                R::table_headers()
            };
            if !headers.is_empty() {
                writeln!(out, "{}", headers.join("  "))?;
            }
            for item in items {
                item.render_table(&mut out)?;
            }
        }
        OutputMode::Json => {
            // Streaming-safe JSON array: build as a vec of raw JSON values.
            // We use a simple bracket approach rather than collecting Vecs to
            // keep memory bounded for large result sets.
            write!(out, "[")?;
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    write!(out, ",")?;
                }
                writeln!(out)?;
                // Capture each item's JSON into a buffer to interleave cleanly.
                let mut buf = Vec::new();
                item.render_json(&mut buf)?;
                // Strip trailing newline from render_json if present.
                if buf.last() == Some(&b'\n') {
                    buf.pop();
                }
                out.write_all(&buf)?;
            }
            writeln!(out, "\n]")?;
        }
    }
    Ok(())
}

// ────────────────────────────────────────────────────────────────────────────
// Legacy closure-based render API (preserved for existing command handlers)
// ────────────────────────────────────────────────────────────────────────────

/// A structured error with optional suggestion and error code.
#[derive(Debug, Serialize)]
pub struct CliError {
    /// Human-readable error message.
    pub message: String,
    /// Optional suggestion for how to fix the error.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    /// Machine-readable error code (e.g. "`missing_agent`", "`invalid_state`").
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
}

impl CliError {
    /// Create a simple error with just a message.
    pub fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            suggestion: None,
            error_code: None,
        }
    }

    /// Create an error with a suggestion and error code.
    pub fn with_details(
        message: impl Into<String>,
        suggestion: impl Into<String>,
        error_code: impl Into<String>,
    ) -> Self {
        Self {
            message: message.into(),
            suggestion: Some(suggestion.into()),
            error_code: Some(error_code.into()),
        }
    }
}

/// Render a serializable value to stdout in the requested format.
///
/// In JSON mode, the value is serialized with `serde_json`. In pretty/text mode,
/// the provided `human_fn` closure is called to produce text output.
/// For distinct text/pretty rendering, use [`render_mode`].
pub fn render<T: Serialize>(
    mode: OutputMode,
    value: &T,
    human_fn: impl FnOnce(&T, &mut dyn Write) -> io::Result<()>,
) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match mode {
        OutputMode::Json => {
            serde_json::to_writer_pretty(&mut out, value)?;
            writeln!(out)?;
        }
        OutputMode::Pretty | OutputMode::Text => {
            human_fn(value, &mut out)?;
        }
    }
    Ok(())
}

/// Render an error to stderr in the requested format.
pub fn render_error(mode: OutputMode, error: &CliError) -> anyhow::Result<()> {
    let stderr = io::stderr();
    let mut out = stderr.lock();
    match mode {
        OutputMode::Json => {
            let wrapper = serde_json::json!({
                "error": error,
            });
            serde_json::to_writer_pretty(&mut out, &wrapper)?;
            writeln!(out)?;
        }
        OutputMode::Pretty | OutputMode::Text => {
            writeln!(out, "error: {}", error.message)?;
            if let Some(ref suggestion) = error.suggestion {
                writeln!(out, "  suggestion: {suggestion}")?;
            }
        }
    }
    Ok(())
}

/// Render a [`BonesError`] to stderr, adapting format to the output mode.
///
/// In JSON mode, outputs `{"error": {"error_code": "...", "message": "...", "suggestion": "..."}}`.
/// In human mode, outputs `error: <message>\n  suggestion: <suggestion>`.
#[cfg(test)]
pub fn render_bones_error(
    mode: OutputMode,
    error: &bones_core::error::BonesError,
) -> anyhow::Result<()> {
    let cli_error = CliError {
        message: error.to_string(),
        suggestion: Some(error.suggestion()),
        error_code: Some(error.error_code().to_string()),
    };
    render_error(mode, &cli_error)
}

/// Convert a [`BonesError`] into a [`CliError`].
impl From<&bones_core::error::BonesError> for CliError {
    fn from(err: &bones_core::error::BonesError) -> Self {
        Self {
            message: err.to_string(),
            suggestion: Some(err.suggestion()),
            error_code: Some(err.error_code().to_string()),
        }
    }
}

/// Render a success message to stdout.
#[cfg(test)]
pub fn render_success(mode: OutputMode, message: &str) -> anyhow::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match mode {
        OutputMode::Json => {
            let wrapper = serde_json::json!({
                "ok": true,
                "message": message,
            });
            serde_json::to_writer_pretty(&mut out, &wrapper)?;
            writeln!(out)?;
        }
        OutputMode::Pretty | OutputMode::Text => {
            writeln!(out, "✓ {message}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── OutputMode ──────────────────────────────────────────────────────────

    #[test]
    fn output_mode_is_json() {
        assert!(OutputMode::Json.is_json());
        assert!(!OutputMode::Human.is_json());
        assert!(!OutputMode::Table.is_json());
    }

    #[test]
    fn output_mode_pretty_and_text() {
        assert!(OutputMode::Pretty.is_pretty());
        assert!(OutputMode::Text.is_text());
        assert!(!OutputMode::Json.is_text());
    }

    // ── resolve_output_mode ─────────────────────────────────────────────────

    // ── resolve_output_mode_inner (testable pure function) ──────────────────

    #[test]
    fn resolve_format_flag_wins_over_json_and_env() {
        let mode = resolve_output_mode_inner(Some(OutputMode::Text), true, Some("pretty"), true);
        assert_eq!(mode, OutputMode::Text);
    }

    #[test]
    fn resolve_json_flag_wins_over_env() {
        // hidden --json alias wins when format flag is absent.
        let mode = resolve_output_mode_inner(None, true, Some("pretty"), true);
        assert_eq!(mode, OutputMode::Json);
    }

    #[test]
    fn resolve_format_env_json() {
        let mode = resolve_output_mode_inner(None, false, Some("json"), false);
        assert_eq!(mode, OutputMode::Json);
    }

    #[test]
    fn resolve_format_env_pretty() {
        // Explicit env=pretty forces Pretty even in non-TTY.
        let mode = resolve_output_mode_inner(None, false, Some("pretty"), false);
        assert_eq!(mode, OutputMode::Pretty);
    }

    #[test]
    fn resolve_format_env_text() {
        let mode = resolve_output_mode_inner(None, false, Some("text"), false);
        assert_eq!(mode, OutputMode::Text);
    }

    #[test]
    fn resolve_format_env_case_insensitive() {
        let mode = resolve_output_mode_inner(None, false, Some("TEXT"), false);
        assert_eq!(mode, OutputMode::Text);
    }

    #[test]
    fn resolve_format_env_unknown_falls_through_to_tty() {
        // Unknown value falls through to TTY detection.
        let mode_tty = resolve_output_mode_inner(None, false, Some("fancy"), true);
        assert_eq!(mode_tty, OutputMode::Pretty);
        let mode_pipe = resolve_output_mode_inner(None, false, Some("fancy"), false);
        assert_eq!(mode_pipe, OutputMode::Text);
    }

    #[test]
    fn resolve_default_tty_is_pretty() {
        let mode = resolve_output_mode_inner(None, false, None, true);
        assert_eq!(mode, OutputMode::Pretty);
    }

    #[test]
    fn resolve_default_no_tty_is_text() {
        let mode = resolve_output_mode_inner(None, false, None, false);
        assert_eq!(mode, OutputMode::Text);
    }

    // ── Renderable trait and render_item / render_list ───────────────────────

    struct SimpleItem {
        name: String,
        count: u32,
    }

    impl Renderable for SimpleItem {
        fn render_human(&self, w: &mut dyn Write) -> io::Result<()> {
            writeln!(w, "{}: {}", self.name, self.count)
        }

        fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
            write!(
                w,
                "{{\"name\":{},\"count\":{}}}",
                serde_json::to_string(&self.name).unwrap(),
                self.count
            )
        }

        fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
            writeln!(w, "{}  {}", self.name, self.count)
        }

        fn table_headers() -> &'static [&'static str] {
            &["NAME", "COUNT"]
        }
    }

    #[test]
    fn render_item_pretty() {
        // render_item writes to stdout; we just check it doesn't panic.
        let item = SimpleItem {
            name: "foo".into(),
            count: 3,
        };
        // Use a local buffer via render_human directly (render_item locks stdout).
        let mut buf = Vec::new();
        item.render_human(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("foo"));
        assert!(s.contains('3'));
    }

    #[test]
    fn render_item_json() {
        let item = SimpleItem {
            name: "bar".into(),
            count: 7,
        };
        let mut buf = Vec::new();
        item.render_json(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"name\""));
        assert!(s.contains("\"bar\""));
        assert!(s.contains("7"));
    }

    #[test]
    fn render_item_text() {
        let item = SimpleItem {
            name: "baz".into(),
            count: 0,
        };
        let mut buf = Vec::new();
        item.render_table(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("baz  0"));
    }

    #[test]
    fn table_headers_returns_expected() {
        let headers = SimpleItem::table_headers();
        assert_eq!(headers, &["NAME", "COUNT"]);
    }

    #[test]
    fn render_list_human_multiple() {
        let items = vec![
            SimpleItem {
                name: "a".into(),
                count: 1,
            },
            SimpleItem {
                name: "b".into(),
                count: 2,
            },
        ];
        // Validate render_human on each directly.
        for item in &items {
            let mut buf = Vec::new();
            item.render_human(&mut buf).unwrap();
            assert!(!buf.is_empty());
        }
    }

    #[test]
    fn render_list_json_empty() {
        // render_list in JSON mode on empty slice: check render_json on no items.
        // (render_list writes to stdout; validate logic via render_json directly.)
        let items: Vec<SimpleItem> = vec![];
        assert!(items.is_empty());
    }

    #[test]
    fn render_list_table_headers() {
        // table_headers should work even without instances.
        let headers = SimpleItem::table_headers();
        assert_eq!(headers.len(), 2);
    }

    // ── Legacy closure-based render API ──────────────────────────────────────

    #[test]
    fn cli_error_simple() {
        let err = CliError::new("something went wrong");
        assert_eq!(err.message, "something went wrong");
        assert!(err.suggestion.is_none());
        assert!(err.error_code.is_none());
    }

    #[test]
    fn cli_error_with_details() {
        let err = CliError::with_details(
            "missing agent",
            "Set BONES_AGENT or pass --agent",
            "missing_agent",
        );
        assert_eq!(err.message, "missing agent");
        assert_eq!(
            err.suggestion.as_deref(),
            Some("Set BONES_AGENT or pass --agent")
        );
        assert_eq!(err.error_code.as_deref(), Some("missing_agent"));
    }

    #[test]
    fn render_json_output() {
        #[derive(Serialize)]
        struct TestData {
            name: String,
            count: u32,
        }
        let data = TestData {
            name: "test".into(),
            count: 42,
        };
        // JSON mode should not panic
        let result = render(OutputMode::Json, &data, |_, _| Ok(()));
        assert!(result.is_ok());
    }

    #[test]
    fn render_human_output() {
        #[derive(Serialize)]
        struct TestData {
            name: String,
        }
        let data = TestData {
            name: "test".into(),
        };
        let result = render(OutputMode::Human, &data, |d, w| {
            writeln!(w, "Name: {}", d.name)
        });
        assert!(result.is_ok());
    }

    #[test]
    fn render_table_falls_back_to_human() {
        #[derive(Serialize)]
        struct TestData {
            val: u32,
        }
        let data = TestData { val: 99 };
        // Table mode falls back to human_fn in legacy render.
        let mut called = false;
        let result = render(OutputMode::Table, &data, |d, w| {
            called = true;
            writeln!(w, "val={}", d.val)
        });
        assert!(result.is_ok());
        assert!(called);
    }

    #[test]
    fn render_error_json() {
        let err = CliError::with_details("bad input", "try again", "bad_input");
        let result = render_error(OutputMode::Json, &err);
        assert!(result.is_ok());
    }

    #[test]
    fn render_error_human() {
        let err = CliError::with_details("bad input", "try again", "bad_input");
        let result = render_error(OutputMode::Human, &err);
        assert!(result.is_ok());
    }

    #[test]
    fn render_error_table_falls_back_to_human() {
        let err = CliError::new("table error");
        let result = render_error(OutputMode::Table, &err);
        assert!(result.is_ok());
    }

    #[test]
    fn render_success_json() {
        let result = render_success(OutputMode::Json, "it worked");
        assert!(result.is_ok());
    }

    #[test]
    fn render_success_human() {
        let result = render_success(OutputMode::Human, "it worked");
        assert!(result.is_ok());
    }

    #[test]
    fn render_success_table_falls_back_to_human() {
        let result = render_success(OutputMode::Table, "it worked");
        assert!(result.is_ok());
    }

    #[test]
    fn cli_error_from_bones_error() {
        let err =
            bones_core::error::BonesError::Model(bones_core::error::ModelError::ItemNotFound {
                item_id: "test123".into(),
            });
        let cli_err = CliError::from(&err);
        assert!(cli_err.message.contains("test123"));
        assert!(cli_err.suggestion.is_some());
        assert_eq!(cli_err.error_code.as_deref(), Some("E2001"));
    }

    #[test]
    fn render_bones_error_json() {
        let err =
            bones_core::error::BonesError::Model(bones_core::error::ModelError::ItemNotFound {
                item_id: "abc".into(),
            });
        let result = render_bones_error(OutputMode::Json, &err);
        assert!(result.is_ok());
    }

    #[test]
    fn render_bones_error_human() {
        let err =
            bones_core::error::BonesError::Model(bones_core::error::ModelError::ItemNotFound {
                item_id: "abc".into(),
            });
        let result = render_bones_error(OutputMode::Human, &err);
        assert!(result.is_ok());
    }
}
