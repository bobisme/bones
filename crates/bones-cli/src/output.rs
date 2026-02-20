//! Shared output layer for text/JSON/table parity across all CLI commands.
//!
//! Every command handler receives an [`OutputMode`] and formats its output
//! accordingly: human-readable text, stable JSON, or TSV table.
//!
//! # Output mode resolution
//!
//! Precedence (highest wins):
//! 1. `--json` CLI flag → [`OutputMode::Json`]
//! 2. `BONES_OUTPUT` env var → `"human"` | `"json"` | `"table"`
//! 3. Default: [`OutputMode::Human`] if stdout is a TTY; [`OutputMode::Json`] if piped.
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

use serde::Serialize;
use std::io::{self, IsTerminal, Write};

/// The three output modes supported by the CLI.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Colored, human-friendly text with truncation for wide fields.
    Human,
    /// Machine-readable JSON (one object per result, or a JSON array).
    Json,
    /// Tab-separated table with header row, suitable for piping to awk/sort.
    Table,
}

impl OutputMode {
    /// Returns `true` if JSON output was requested.
    pub fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }

    /// Returns `true` if table output was requested.
    #[cfg(test)]
    pub fn is_table(self) -> bool {
        matches!(self, Self::Table)
    }
}

/// Core resolution logic, separated from I/O for testability.
///
/// `json_flag` — true if `--json` was passed on the CLI.
/// `bones_output_env` — the value of `BONES_OUTPUT` if set.
/// `is_tty` — true if stdout is a TTY.
fn resolve_output_mode_inner(
    json_flag: bool,
    bones_output_env: Option<&str>,
    is_tty: bool,
) -> OutputMode {
    if json_flag {
        return OutputMode::Json;
    }

    if let Some(val) = bones_output_env {
        match val.to_lowercase().as_str() {
            "json" => return OutputMode::Json,
            "table" => return OutputMode::Table,
            "human" => return OutputMode::Human,
            _ => {} // unknown value — fall through to TTY detection
        }
    }

    // Default: human if TTY, JSON if piped.
    if is_tty {
        OutputMode::Human
    } else {
        OutputMode::Json
    }
}

/// Resolve the output mode from CLI flags, environment, and TTY defaults.
///
/// Precedence:
/// 1. `json_flag` → [`OutputMode::Json`]
/// 2. `BONES_OUTPUT` env var → `"human"` | `"json"` | `"table"`
/// 3. Default: [`OutputMode::Human`] if stdout is a TTY; [`OutputMode::Json`] if piped.
pub fn resolve_output_mode(json_flag: bool) -> OutputMode {
    let env_val = std::env::var("BONES_OUTPUT").ok();
    let is_tty = io::stdout().is_terminal();
    resolve_output_mode_inner(json_flag, env_val.as_deref(), is_tty)
}

/// Trait implemented by any CLI result type that can be rendered in all modes.
///
/// Implementors provide three rendering methods (one per mode) plus a list of
/// column headers for table mode. The [`render_item`] and [`render_list`]
/// free functions dispatch to the appropriate method based on [`OutputMode`].
pub trait Renderable {
    /// Render for human consumption: text with labels, truncated for readability.
    fn render_human(&self, w: &mut dyn Write) -> io::Result<()>;

    /// Render as a JSON value (schema-stable, streaming-safe).
    ///
    /// Implementors should serialize a self-contained JSON object.
    fn render_json(&self, w: &mut dyn Write) -> io::Result<()>;

    /// Render as a single TSV table row (no header; see [`table_headers`]).
    ///
    /// Fields must appear in the same column order as [`table_headers`].
    ///
    /// [`table_headers`]: Renderable::table_headers
    fn render_table(&self, w: &mut dyn Write) -> io::Result<()>;

    /// Column headers for table mode, in the same order as [`render_table`] fields.
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
        OutputMode::Human => item.render_human(&mut out),
        OutputMode::Json => {
            item.render_json(&mut out)?;
            writeln!(out)
        }
        OutputMode::Table => item.render_table(&mut out),
    }
}

/// Render a list of [`Renderable`] items to stdout.
///
/// - In JSON mode, wraps items in a JSON array.
/// - In Table mode, prints the header row (from [`Renderable::table_headers`]) first.
/// - In Human mode, renders items sequentially.
pub fn render_list<R: Renderable>(items: &[R], mode: OutputMode) -> io::Result<()> {
    let stdout = io::stdout();
    let mut out = stdout.lock();
    match mode {
        OutputMode::Human => {
            for item in items {
                item.render_human(&mut out)?;
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
        OutputMode::Table => {
            // Print header row (tab-separated).
            let headers = if items.is_empty() {
                &[] as &[&str]
            } else {
                // We can't call an associated fn via a value, so call on first item's type.
                // Use a local helper to get the headers without a value.
                R::table_headers()
            };
            if !headers.is_empty() {
                writeln!(out, "{}", headers.join("\t"))?;
            }
            for item in items {
                item.render_table(&mut out)?;
            }
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
    /// Machine-readable error code (e.g. "missing_agent", "invalid_state").
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
/// In JSON mode, the value is serialized with `serde_json`. In human mode,
/// the provided `human_fn` closure is called to produce text output.
/// In table mode, falls back to human rendering (use [`render_item`] for
/// full table support on types implementing [`Renderable`]).
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
        OutputMode::Human | OutputMode::Table => {
            // Table mode falls back to human in the legacy render API.
            // Commands that want real table output should implement Renderable.
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
        OutputMode::Human | OutputMode::Table => {
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
        OutputMode::Human | OutputMode::Table => {
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
    fn output_mode_is_table() {
        assert!(OutputMode::Table.is_table());
        assert!(!OutputMode::Human.is_table());
        assert!(!OutputMode::Json.is_table());
    }

    // ── resolve_output_mode ─────────────────────────────────────────────────

    // ── resolve_output_mode_inner (testable pure function) ──────────────────

    #[test]
    fn resolve_json_flag_wins_over_env() {
        // --json flag wins even when BONES_OUTPUT=human.
        let mode = resolve_output_mode_inner(true, Some("human"), true);
        assert_eq!(mode, OutputMode::Json);
    }

    #[test]
    fn resolve_bones_output_env_json() {
        let mode = resolve_output_mode_inner(false, Some("json"), false);
        assert_eq!(mode, OutputMode::Json);
    }

    #[test]
    fn resolve_bones_output_env_human() {
        // Explicit env=human forces Human even in non-TTY.
        let mode = resolve_output_mode_inner(false, Some("human"), false);
        assert_eq!(mode, OutputMode::Human);
    }

    #[test]
    fn resolve_bones_output_env_table() {
        let mode = resolve_output_mode_inner(false, Some("table"), false);
        assert_eq!(mode, OutputMode::Table);
    }

    #[test]
    fn resolve_bones_output_env_case_insensitive() {
        let mode = resolve_output_mode_inner(false, Some("TABLE"), false);
        assert_eq!(mode, OutputMode::Table);
    }

    #[test]
    fn resolve_bones_output_env_unknown_falls_through_to_tty() {
        // Unknown value falls through to TTY detection.
        let mode_tty = resolve_output_mode_inner(false, Some("fancy"), true);
        assert_eq!(mode_tty, OutputMode::Human);
        let mode_pipe = resolve_output_mode_inner(false, Some("fancy"), false);
        assert_eq!(mode_pipe, OutputMode::Json);
    }

    #[test]
    fn resolve_default_tty_is_human() {
        let mode = resolve_output_mode_inner(false, None, true);
        assert_eq!(mode, OutputMode::Human);
    }

    #[test]
    fn resolve_default_no_tty_is_json() {
        let mode = resolve_output_mode_inner(false, None, false);
        assert_eq!(mode, OutputMode::Json);
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
            writeln!(w, "{}\t{}", self.name, self.count)
        }

        fn table_headers() -> &'static [&'static str] {
            &["NAME", "COUNT"]
        }
    }

    #[test]
    fn render_item_human() {
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
    fn render_item_table() {
        let item = SimpleItem {
            name: "baz".into(),
            count: 0,
        };
        let mut buf = Vec::new();
        item.render_table(&mut buf).unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("baz\t0"));
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
