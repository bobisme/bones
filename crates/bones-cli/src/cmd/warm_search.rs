use std::io::Write;
use std::path::Path;
use std::time::Instant;

use anyhow::{Context, Result};
use bones_core::db::query;
use bones_search::semantic::{SemanticModel, sync_projection_embeddings};
use serde::Serialize;

use crate::output::{CliError, OutputMode, pretty_kv, pretty_section, render_error, render_mode};

#[derive(Debug, Serialize)]
struct WarmSearchReport {
    embedded: usize,
    removed: usize,
    embeddings_before: usize,
    embeddings_after: usize,
    projection_offset: i64,
    semantic_offset: i64,
    cursor_in_sync: bool,
    elapsed_ms: u128,
}

pub fn run_warm_search(project_root: &Path, output: OutputMode) -> Result<()> {
    let db_path = project_root.join(".bones/bones.db");
    let conn = if let Some(c) = query::try_open_projection(&db_path)? { c } else {
        render_error(
            output,
            &CliError::with_details(
                "projection database not found",
                "run `bn admin rebuild` to initialize the projection",
                "projection_missing",
            ),
        )?;
        anyhow::bail!("projection not found");
    };

    let model = match SemanticModel::load() {
        Ok(model) => model,
        Err(err) => {
            render_error(
                output,
                &CliError::with_details(
                    "semantic model unavailable",
                    "run `bn search --lexical ...` for lexical-only search, or configure semantic model assets and retry",
                    "semantic_unavailable",
                ),
            )?;
            anyhow::bail!("semantic model unavailable: {err}");
        }
    };

    bones_search::semantic::ensure_semantic_index_schema(&conn)
        .context("initialize semantic index schema")?;
    let embeddings_before = embedding_count(&conn)?;

    let start = Instant::now();
    let stats = sync_projection_embeddings(&conn, &model)
        .context("synchronize semantic embeddings for warm-search")?;
    let elapsed_ms = start.elapsed().as_millis();

    let embeddings_after = embedding_count(&conn)?;
    let (projection_offset, projection_hash) =
        query::get_projection_cursor(&conn).context("read projection cursor after warm-search")?;
    let (semantic_offset, semantic_hash) = semantic_cursor(&conn)?;

    let report = WarmSearchReport {
        embedded: stats.embedded,
        removed: stats.removed,
        embeddings_before,
        embeddings_after,
        projection_offset,
        semantic_offset,
        cursor_in_sync: projection_offset == semantic_offset && projection_hash == semantic_hash,
        elapsed_ms,
    };

    render_mode(output, &report, render_warm_text, render_warm_pretty)
}

fn embedding_count(conn: &rusqlite::Connection) -> Result<usize> {
    let count: i64 = conn
        .query_row("SELECT COUNT(*) FROM item_embeddings", [], |row| row.get(0))
        .context("count semantic embeddings")?;
    Ok(usize::try_from(count).unwrap_or(0))
}

fn semantic_cursor(conn: &rusqlite::Connection) -> Result<(i64, Option<String>)> {
    conn.query_row(
        "SELECT last_event_offset, last_event_hash FROM semantic_meta WHERE id = 1",
        [],
        |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
    )
    .context("read semantic cursor")
}

fn render_warm_text(report: &WarmSearchReport, w: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        w,
        "warm-search embedded={} removed={} embeddings_before={} embeddings_after={} cursor_in_sync={} elapsed_ms={}",
        report.embedded,
        report.removed,
        report.embeddings_before,
        report.embeddings_after,
        report.cursor_in_sync,
        report.elapsed_ms
    )
}

fn render_warm_pretty(report: &WarmSearchReport, w: &mut dyn Write) -> std::io::Result<()> {
    pretty_section(w, "Search Warmup")?;
    pretty_kv(w, "Embedded", report.embedded.to_string())?;
    pretty_kv(w, "Removed", report.removed.to_string())?;
    pretty_kv(w, "Embeddings", report.embeddings_after.to_string())?;
    pretty_kv(
        w,
        "Cursor",
        if report.cursor_in_sync {
            "in sync".to_string()
        } else {
            "behind projection".to_string()
        },
    )?;
    pretty_kv(w, "Elapsed ms", report.elapsed_ms.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_renderer_includes_core_fields() {
        let report = WarmSearchReport {
            embedded: 5,
            removed: 1,
            embeddings_before: 10,
            embeddings_after: 14,
            projection_offset: 123,
            semantic_offset: 123,
            cursor_in_sync: true,
            elapsed_ms: 456,
        };

        let mut buf = Vec::new();
        render_warm_text(&report, &mut buf).expect("render text");
        let out = String::from_utf8(buf).expect("utf8");
        assert!(out.contains("warm-search"));
        assert!(out.contains("embedded=5"));
        assert!(out.contains("cursor_in_sync=true"));
    }
}
