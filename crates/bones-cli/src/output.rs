//! Shared output layer for text/JSON parity across all CLI commands.
//!
//! Every command handler receives an [`OutputMode`] and formats its output
//! accordingly: either human-readable text or stable JSON.

use serde::Serialize;
use std::io::{self, Write};

/// Output format selected by the `--json` flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputMode {
    /// Human-readable text (default).
    Human,
    /// Machine-readable JSON.
    Json,
}

impl OutputMode {
    /// Returns `true` if JSON output was requested.
    pub fn is_json(self) -> bool {
        matches!(self, Self::Json)
    }
}

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
        OutputMode::Human => {
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
        OutputMode::Human => {
            writeln!(out, "error: {}", error.message)?;
            if let Some(ref suggestion) = error.suggestion {
                writeln!(out, "  suggestion: {suggestion}")?;
            }
        }
    }
    Ok(())
}

/// Render a success message to stdout.
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
        OutputMode::Human => {
            writeln!(out, "✓ {message}")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_mode_is_json() {
        assert!(OutputMode::Json.is_json());
        assert!(!OutputMode::Human.is_json());
    }

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
    fn render_success_json() {
        let result = render_success(OutputMode::Json, "it worked");
        assert!(result.is_ok());
    }

    #[test]
    fn render_success_human() {
        let result = render_success(OutputMode::Human, "it worked");
        assert!(result.is_ok());
    }
}
