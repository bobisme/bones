use anyhow::{Context as _, Result, anyhow, bail};
use bones_core::event::writer::{shard_header, write_line};
use bones_core::event::{CURRENT_VERSION, ParsedLine, detect_version, migrate_event, parse_line};
use bones_core::shard::ShardManager;
use clap::Args;
use std::fs;
use std::path::{Path, PathBuf};

use crate::output::{OutputMode, pretty_kv, pretty_section};

#[derive(Args, Debug)]
pub struct MigrateFormatArgs {
    /// Overwrite existing .events.bak files.
    #[arg(long)]
    pub force_backup: bool,
}

pub fn run_migrate_format(
    args: &MigrateFormatArgs,
    output: OutputMode,
    project_root: &Path,
) -> Result<()> {
    let shard_manager = ShardManager::new(project_root.join(".bones"));
    let shards = shard_manager
        .list_shards()
        .context("failed to list event shards")?;

    if shards.is_empty() {
        match output {
            OutputMode::Json => {
                println!(
                    "{}",
                    serde_json::json!({"rewritten": 0, "version": CURRENT_VERSION})
                );
            }
            OutputMode::Text => {
                println!("migrate_format rewritten=0 version={CURRENT_VERSION}");
            }
            OutputMode::Pretty => {
                println!("No event shards found.");
            }
        }
        return Ok(());
    }

    let mut rewritten = 0usize;

    for (year, month) in shards {
        let shard_path = shard_manager.shard_path(year, month);
        let backup_path = backup_path_for(&shard_path)?;

        if backup_path.exists() && !args.force_backup {
            bail!(
                "backup already exists for {} at {} (use --force-backup to overwrite)",
                shard_path.display(),
                backup_path.display()
            );
        }

        fs::copy(&shard_path, &backup_path).with_context(|| {
            format!(
                "failed to create shard backup {} -> {}",
                shard_path.display(),
                backup_path.display()
            )
        })?;

        let content = fs::read_to_string(&shard_path)
            .with_context(|| format!("failed to read shard {}", shard_path.display()))?;

        let shard_version = detect_shard_version(&content, &shard_path)?;

        let mut out = shard_header();
        for (line_no, line) in content.lines().enumerate() {
            match parse_line(line).with_context(|| {
                format!(
                    "parse error in {} at line {}",
                    shard_path.display(),
                    line_no + 1
                )
            })? {
                ParsedLine::Event(event) => {
                    let migrated = migrate_event(*event, shard_version).with_context(|| {
                        format!(
                            "failed to migrate event in {} at line {}",
                            shard_path.display(),
                            line_no + 1
                        )
                    })?;
                    let serialized = write_line(&migrated).with_context(|| {
                        format!(
                            "failed to serialize migrated event in {} at line {}",
                            shard_path.display(),
                            line_no + 1
                        )
                    })?;
                    out.push_str(&serialized);
                    out.push('\n');
                }
                ParsedLine::Comment(_) | ParsedLine::Blank => {}
            }
        }

        fs::write(&shard_path, out)
            .with_context(|| format!("failed to rewrite shard {}", shard_path.display()))?;
        shard_manager
            .write_manifest(year, month)
            .with_context(|| format!("failed to rewrite manifest for {}", shard_path.display()))?;

        rewritten += 1;
    }

    match output {
        OutputMode::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "rewritten": rewritten,
                    "version": CURRENT_VERSION,
                }))?
            );
        }
        OutputMode::Text => {
            println!(
                "migrate_format rewritten={} version={}",
                rewritten, CURRENT_VERSION
            );
        }
        OutputMode::Pretty => {
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            pretty_section(&mut w, "Format Migration")?;
            pretty_kv(&mut w, "Rewritten shards", rewritten.to_string())?;
            pretty_kv(&mut w, "Target version", CURRENT_VERSION.to_string())?;
        }
    }

    Ok(())
}

fn detect_shard_version(content: &str, shard_path: &Path) -> Result<u32> {
    let Some(first_line) = content.lines().find(|line| !line.trim().is_empty()) else {
        return Ok(CURRENT_VERSION);
    };

    if first_line.trim_start().starts_with("# bones event log v") {
        detect_version(first_line)
            .map_err(|msg| anyhow!("{}: {}", shard_path.display(), msg))
            .context("failed to detect shard version")
    } else {
        Ok(CURRENT_VERSION)
    }
}

fn backup_path_for(shard_path: &Path) -> Result<PathBuf> {
    let Some(name) = shard_path.file_name().and_then(|n| n.to_str()) else {
        bail!("invalid shard filename: {}", shard_path.display());
    };
    Ok(shard_path.with_file_name(format!("{name}.bak")))
}
