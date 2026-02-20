use std::path::Path;

use anyhow::Result;
use bones_core::verify::{ShardCheckStatus, verify_repository};
use serde::Serialize;

use crate::output::{CliError, OutputMode, render, render_error};

#[derive(Debug, Serialize)]
struct VerifyShardRow {
    shard: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

#[derive(Debug, Serialize)]
struct VerifyOutput {
    ok: bool,
    active_shard_parse_ok: bool,
    shards: Vec<VerifyShardRow>,
}

/// Run repository verification against `.bones/events` shards.
///
/// # Errors
///
/// Returns an error when verification checks fail.
pub fn run_verify(project_root: &Path, regenerate_missing: bool, output: OutputMode) -> Result<()> {
    let bones_dir = project_root.join(".bones");
    let report = match verify_repository(&bones_dir, regenerate_missing) {
        Ok(report) => report,
        Err(e) => {
            render_error(
                output,
                &CliError::with_details(
                    format!("verify failed: {e}"),
                    "run `bn diagnose` for deeper checks, then inspect .bones/events shards",
                    "verify_failed",
                ),
            )?;
            return Err(e.into());
        }
    };

    let shards: Vec<VerifyShardRow> = report
        .shards
        .iter()
        .map(|shard| {
            let (status, reason) = match &shard.status {
                ShardCheckStatus::Verified => ("verified".to_string(), None),
                ShardCheckStatus::Regenerated => ("regenerated".to_string(), None),
                ShardCheckStatus::Failed(reason) => ("failed".to_string(), Some(reason.clone())),
            };
            VerifyShardRow {
                shard: shard.shard_name.clone(),
                status,
                reason,
            }
        })
        .collect();

    let out = VerifyOutput {
        ok: report.is_ok(),
        active_shard_parse_ok: report.active_shard_parse_ok,
        shards,
    };

    render(output, &out, |out, w| {
        for shard in &out.shards {
            match (&*shard.status, &shard.reason) {
                ("verified", _) => {
                    writeln!(w, "OK   {}", shard.shard)?;
                }
                ("regenerated", _) => {
                    writeln!(w, "FIX  {} (regenerated missing manifest)", shard.shard)?;
                }
                ("failed", Some(reason)) => {
                    writeln!(w, "FAIL {} ({reason})", shard.shard)?;
                }
                ("failed", None) => {
                    writeln!(w, "FAIL {}", shard.shard)?;
                }
                _ => {}
            }
        }

        if out.active_shard_parse_ok {
            writeln!(w, "OK   active shard parse sanity")?;
        } else {
            writeln!(w, "FAIL active shard parse sanity")?;
        }

        if out.ok {
            writeln!(w, "verify: success")?;
        } else {
            writeln!(w, "verify: failed")?;
        }

        Ok(())
    })?;

    if out.ok {
        Ok(())
    } else {
        anyhow::bail!("verify: failed")
    }
}
