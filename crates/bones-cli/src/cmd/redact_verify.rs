//! `bn redact-verify` — verify redaction completeness for work items.
//!
//! Checks that all `item.redact` events have been fully applied:
//! redacted content must be absent from projection rows, FTS5 index,
//! and comment bodies.

use crate::output::{
    CliError, OutputMode, Renderable, pretty_kv, pretty_section, render_error, render_item,
    render_list,
};
use bones_core::db::query::try_open_projection;
use bones_core::verify::redact::{
    RedactionFailure, RedactionReport, ResidualLocation, verify_item_redaction, verify_redactions,
};
use clap::Args;
use serde_json::json;
use std::io::{self, Write};

#[derive(Args, Debug)]
pub struct RedactVerifyArgs {
    /// Optional item ID to verify. If omitted, verifies all redactions.
    pub item_id: Option<String>,
}

/// Renderable wrapper for a single RedactionFailure.
struct FailureRow<'a>(&'a RedactionFailure);

impl Renderable for FailureRow<'_> {
    fn render_human(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "FAIL {}  target={}", self.0.item_id, self.0.event_hash)?;
        for loc in &self.0.residual_locations {
            match loc {
                ResidualLocation::MissingRedactionRecord => {
                    writeln!(w, "     ↳ redaction record missing from event_redactions")?;
                }
                ResidualLocation::CommentNotRedacted { comment_id } => {
                    writeln!(
                        w,
                        "     ↳ comment #{comment_id} body not replaced with [redacted]"
                    )?;
                }
                ResidualLocation::Fts5Index { matched_term } => {
                    writeln!(
                        w,
                        "     ↳ FTS5 index still contains term \"{matched_term}\""
                    )?;
                }
            }
        }
        Ok(())
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        let val =
            serde_json::to_string(&self.0).map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        write!(w, "{val}")
    }

    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        let locs: Vec<String> = self
            .0
            .residual_locations
            .iter()
            .map(|l| match l {
                ResidualLocation::MissingRedactionRecord => "missing_record".to_string(),
                ResidualLocation::CommentNotRedacted { comment_id } => {
                    format!("comment:{comment_id}")
                }
                ResidualLocation::Fts5Index { matched_term } => {
                    format!("fts5:{matched_term}")
                }
            })
            .collect();
        writeln!(
            w,
            "{}\t{}\t{}",
            self.0.item_id,
            self.0.event_hash,
            locs.join(",")
        )
    }

    fn table_headers() -> &'static [&'static str]
    where
        Self: Sized,
    {
        &["ITEM_ID", "EVENT_HASH", "RESIDUAL_LOCATIONS"]
    }
}

/// Renderable wrapper for the full RedactionReport (summary).
struct ReportSummary<'a>(&'a RedactionReport);

impl Renderable for ReportSummary<'_> {
    fn render_human(&self, w: &mut dyn Write) -> io::Result<()> {
        let r = self.0;
        if r.redactions_checked == 0 {
            writeln!(w, "redact-verify: no redaction events found")?;
            return Ok(());
        }

        pretty_section(w, "Redaction Verification")?;
        pretty_kv(w, "Checked", r.redactions_checked.to_string())?;
        pretty_kv(w, "Passed", r.passed.to_string())?;
        pretty_kv(w, "Failed", r.failed.to_string())?;
        pretty_kv(
            w,
            "Status",
            if r.is_ok() {
                "all redactions verified"
            } else {
                "verification failed"
            },
        )?;
        Ok(())
    }

    fn render_json(&self, w: &mut dyn Write) -> io::Result<()> {
        let val = serde_json::to_string_pretty(self.0)
            .map_err(|e| io::Error::new(io::ErrorKind::Other, e))?;
        write!(w, "{val}")
    }

    fn render_table(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(
            w,
            "{}\t{}\t{}",
            self.0.redactions_checked, self.0.passed, self.0.failed
        )
    }

    fn table_headers() -> &'static [&'static str]
    where
        Self: Sized,
    {
        &["CHECKED", "PASSED", "FAILED"]
    }
}

pub fn run_redact_verify(
    args: &RedactVerifyArgs,
    output: OutputMode,
    project_root: &std::path::Path,
) -> anyhow::Result<()> {
    let bones_dir = project_root.join(".bones");
    let db_path = bones_dir.join("bones.db");
    let events_dir = bones_dir.join("events");

    // Open projection DB
    let conn = match try_open_projection(&db_path)? {
        Some(conn) => conn,
        None => {
            render_error(
                output,
                &CliError::with_details(
                    "projection database not found or corrupt",
                    "Run `bn admin rebuild` to initialize it.",
                    "missing_projection",
                ),
            )?;
            anyhow::bail!("projection database not found at {}", db_path.display());
        }
    };

    match &args.item_id {
        Some(item_id) => {
            // Verify a single item
            let failures = verify_item_redaction(item_id, &events_dir, &conn)?;

            if failures.is_empty() {
                match output {
                    OutputMode::Json => {
                        let val = json!({
                            "ok": true,
                            "item_id": item_id,
                            "message": "all redactions verified for this item",
                            "failures": [],
                        });
                        let stdout = io::stdout();
                        let mut out = stdout.lock();
                        serde_json::to_writer_pretty(&mut out, &val)?;
                        writeln!(out)?;
                    }
                    OutputMode::Text => {
                        println!("ok=true item_id={item_id} checked=1 failed=0");
                    }
                    OutputMode::Pretty => {
                        let stdout = io::stdout();
                        let mut out = stdout.lock();
                        pretty_section(&mut out, &format!("Redaction Check {item_id}"))?;
                        pretty_kv(&mut out, "Status", "all redactions verified")?;
                    }
                }
            } else {
                let rows: Vec<FailureRow> = failures.iter().map(FailureRow).collect();
                render_list(&rows, output)?;

                anyhow::bail!(
                    "redact-verify {}: {} failure(s) found",
                    item_id,
                    failures.len()
                );
            }
        }
        None => {
            // Verify all redactions
            let report = verify_redactions(&events_dir, &conn)?;

            // Print failures first
            if !report.failures.is_empty() {
                let rows: Vec<FailureRow> = report.failures.iter().map(FailureRow).collect();
                render_list(&rows, output)?;
            }

            // Print summary
            render_item(&ReportSummary(&report), output)?;

            if !report.is_ok() {
                anyhow::bail!("redact-verify: {} failure(s) found", report.failed);
            }
        }
    }

    Ok(())
}
