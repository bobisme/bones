//! `bn doctor` — find and fix repository integrity issues.
//!
//! A superset of `bn verify` that also checks shard headers, orphaned events,
//! parse errors, projection drift, and stale `current.events` symlinks.
//! With `--fix`, automatically repairs what is safe to fix.

use std::fs;
use std::io::Write;
use std::path::Path;

use anyhow::Result;
use bones_core::event::parser::{ParsedLine, parse_line};
use bones_core::event::writer::SHARD_HEADER;
use bones_core::shard::{ShardManager, validate_shard_header};
use bones_core::verify::verify_repository;
use clap::Args;
use serde::Serialize;

use crate::output::{OutputMode, render};

#[derive(Args, Debug)]
pub struct DoctorArgs {
    /// Automatically repair safe-to-fix issues (regenerate manifests, remove
    /// stale `current.events`, rebuild drifted projection).
    #[arg(long)]
    pub fix: bool,
}

// ---------------------------------------------------------------------------
// Report types
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
pub struct DoctorReport {
    pub ok: bool,
    pub sections: Vec<DoctorSection>,
    pub fixes_applied: Vec<String>,
}

#[derive(Debug, Serialize)]
pub struct DoctorSection {
    pub name: String,
    pub status: SectionStatus,
    pub details: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "UPPERCASE")]
pub enum SectionStatus {
    Ok,
    Warn,
    Fail,
}

impl std::fmt::Display for SectionStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Ok => write!(f, "OK"),
            Self::Warn => write!(f, "WARN"),
            Self::Fail => write!(f, "FAIL"),
        }
    }
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn run_doctor(args: &DoctorArgs, output: OutputMode, project_root: &Path) -> Result<()> {
    let report = build_doctor_report(args, project_root)?;
    render(output, &report, |r, w| render_human(r, w))?;

    if report.ok {
        Ok(())
    } else {
        anyhow::bail!("doctor: issues found")
    }
}

// ---------------------------------------------------------------------------
// Report builder
// ---------------------------------------------------------------------------

fn build_doctor_report(args: &DoctorArgs, project_root: &Path) -> Result<DoctorReport> {
    let bones_dir = project_root.join(".bones");
    let events_dir = bones_dir.join("events");
    let mut sections = Vec::new();
    let mut fixes_applied = Vec::new();

    // 1. Shard integrity (manifests)
    sections.push(check_shard_integrity(
        &bones_dir,
        args.fix,
        &mut fixes_applied,
    )?);

    // 2. Shard headers
    sections.push(check_shard_headers(&bones_dir)?);

    // 3. Parse errors
    sections.push(check_parse_errors(&bones_dir)?);

    // 4. Orphaned events (items referenced without create)
    sections.push(check_orphaned_events(&bones_dir)?);

    // 5. Projection drift
    sections.push(check_projection_drift(
        &bones_dir,
        args.fix,
        &mut fixes_applied,
    )?);

    // 6. Stale current.events symlink
    sections.push(check_stale_symlink(
        &events_dir,
        args.fix,
        &mut fixes_applied,
    )?);

    let ok = sections.iter().all(|s| s.status != SectionStatus::Fail);

    Ok(DoctorReport {
        ok,
        sections,
        fixes_applied,
    })
}

// ---------------------------------------------------------------------------
// Individual checks
// ---------------------------------------------------------------------------

fn check_shard_integrity(
    bones_dir: &Path,
    fix: bool,
    fixes: &mut Vec<String>,
) -> Result<DoctorSection> {
    let mut details = Vec::new();
    let status;

    match verify_repository(bones_dir, fix) {
        Ok(report) => {
            for shard in &report.shards {
                let shard_status = match &shard.status {
                    bones_core::verify::ShardCheckStatus::Verified => "verified",
                    bones_core::verify::ShardCheckStatus::Regenerated => {
                        fixes.push(format!("regenerated manifest for {}", shard.shard_name));
                        "regenerated"
                    }
                    bones_core::verify::ShardCheckStatus::Failed(reason) => {
                        details.push(format!("{}: {reason}", shard.shard_name));
                        "failed"
                    }
                };
                details.push(format!("{}: {shard_status}", shard.shard_name));
            }

            if report.is_ok() {
                status = SectionStatus::Ok;
            } else {
                status = SectionStatus::Fail;
            }
        }
        Err(e) => {
            details.push(format!("verify error: {e}"));
            status = SectionStatus::Fail;
        }
    }

    Ok(DoctorSection {
        name: "shard_integrity".into(),
        status,
        details,
    })
}

fn check_shard_headers(bones_dir: &Path) -> Result<DoctorSection> {
    let shard_mgr = ShardManager::new(bones_dir);
    let shards = shard_mgr
        .list_shards()
        .map_err(|e| anyhow::anyhow!("list shards: {e}"))?;

    let mut details = Vec::new();
    let mut has_failure = false;

    for (year, month) in &shards {
        let path = shard_mgr.shard_path(*year, *month);
        let name = ShardManager::shard_filename(*year, *month);
        match validate_shard_header(&path) {
            Ok(()) => details.push(format!("{name}: {SHARD_HEADER}")),
            Err(e) => {
                details.push(format!("{name}: CORRUPT — {e}"));
                has_failure = true;
            }
        }
    }

    Ok(DoctorSection {
        name: "shard_headers".into(),
        status: if has_failure {
            SectionStatus::Fail
        } else {
            SectionStatus::Ok
        },
        details,
    })
}

fn check_parse_errors(bones_dir: &Path) -> Result<DoctorSection> {
    let shard_mgr = ShardManager::new(bones_dir);
    let shards = shard_mgr
        .list_shards()
        .map_err(|e| anyhow::anyhow!("list shards: {e}"))?;

    let mut total_errors = 0usize;
    let mut details = Vec::new();

    for (year, month) in &shards {
        let path = shard_mgr.shard_path(*year, *month);
        let name = ShardManager::shard_filename(*year, *month);
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(e) => {
                details.push(format!("{name}: read error — {e}"));
                total_errors += 1;
                continue;
            }
        };

        let mut shard_errors = 0usize;
        for (i, line) in content.lines().enumerate() {
            match parse_line(line) {
                Ok(ParsedLine::Event(_) | ParsedLine::Comment(_) | ParsedLine::Blank) => {}
                Err(e) => {
                    shard_errors += 1;
                    if details.len() < 10 {
                        details.push(format!("{name}:{}: {e}", i + 1));
                    }
                }
            }
        }
        total_errors += shard_errors;
    }

    Ok(DoctorSection {
        name: "parse_errors".into(),
        status: if total_errors > 0 {
            SectionStatus::Warn
        } else {
            SectionStatus::Ok
        },
        details: if total_errors > 0 {
            let mut d = vec![format!("{total_errors} parse error(s) across all shards")];
            d.extend(details);
            d
        } else {
            vec!["no parse errors".into()]
        },
    })
}

fn check_orphaned_events(bones_dir: &Path) -> Result<DoctorSection> {
    let shard_mgr = ShardManager::new(bones_dir);
    let shards = shard_mgr
        .list_shards()
        .map_err(|e| anyhow::anyhow!("list shards: {e}"))?;

    let mut created_items = std::collections::HashSet::new();
    let mut non_create_items = std::collections::HashSet::new();

    for (year, month) in &shards {
        let path = shard_mgr.shard_path(*year, *month);
        let content = match fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };
        for line in content.lines() {
            if let Ok(ParsedLine::Event(event)) = parse_line(line) {
                let id = event.item_id.as_str().to_string();
                if event.event_type == bones_core::event::types::EventType::Create {
                    created_items.insert(id);
                } else {
                    non_create_items.insert(id);
                }
            }
        }
    }

    let orphans: Vec<_> = non_create_items
        .difference(&created_items)
        .take(20)
        .cloned()
        .collect();
    let orphan_count = non_create_items.difference(&created_items).count();

    Ok(DoctorSection {
        name: "orphaned_events".into(),
        status: if orphan_count > 0 {
            SectionStatus::Warn
        } else {
            SectionStatus::Ok
        },
        details: if orphan_count > 0 {
            let mut d = vec![format!(
                "{orphan_count} item(s) referenced without item.create"
            )];
            for id in orphans {
                d.push(format!("  {id}"));
            }
            d
        } else {
            vec!["no orphaned events".into()]
        },
    })
}

fn check_projection_drift(
    bones_dir: &Path,
    fix: bool,
    fixes: &mut Vec<String>,
) -> Result<DoctorSection> {
    let db_path = bones_dir.join("bones.db");
    let events_dir = bones_dir.join("events");

    if !db_path.exists() {
        if fix {
            // Rebuild projection from scratch
            bones_core::db::rebuild::rebuild(&events_dir, &db_path)?;
            fixes.push("rebuilt missing projection database".into());
            return Ok(DoctorSection {
                name: "projection_drift".into(),
                status: SectionStatus::Ok,
                details: vec!["projection rebuilt from events".into()],
            });
        }
        return Ok(DoctorSection {
            name: "projection_drift".into(),
            status: SectionStatus::Warn,
            details: vec!["projection database missing; run with --fix to rebuild".into()],
        });
    }

    let conn = match bones_core::db::query::try_open_projection_raw(&db_path) {
        Ok(Some(conn)) => conn,
        _ => {
            return Ok(DoctorSection {
                name: "projection_drift".into(),
                status: SectionStatus::Warn,
                details: vec!["projection database corrupt or unreadable".into()],
            });
        }
    };

    let (cursor_offset, cursor_hash) = bones_core::db::query::get_projection_cursor(&conn)?;

    let shard_mgr = ShardManager::new(bones_dir);
    let total_len = shard_mgr
        .total_content_len()
        .map_err(|e| anyhow::anyhow!("total_content_len: {e}"))?;
    let expected_offset = i64::try_from(total_len).unwrap_or(i64::MAX);

    let offset_ok = cursor_offset == expected_offset;
    let mut details = vec![format!(
        "cursor_offset={cursor_offset} expected={expected_offset} match={offset_ok}"
    )];

    if let Some(hash) = &cursor_hash {
        details.push(format!("cursor_hash={hash}"));
    }

    if offset_ok {
        return Ok(DoctorSection {
            name: "projection_drift".into(),
            status: SectionStatus::Ok,
            details,
        });
    }

    if fix {
        drop(conn);
        bones_core::db::rebuild::rebuild(&events_dir, &db_path)?;
        fixes.push("rebuilt drifted projection database".into());
        details.push("projection rebuilt".into());
        return Ok(DoctorSection {
            name: "projection_drift".into(),
            status: SectionStatus::Ok,
            details,
        });
    }

    details.push("run with --fix to rebuild projection".into());
    Ok(DoctorSection {
        name: "projection_drift".into(),
        status: SectionStatus::Fail,
        details,
    })
}

fn check_stale_symlink(
    events_dir: &Path,
    fix: bool,
    fixes: &mut Vec<String>,
) -> Result<DoctorSection> {
    let symlink_path = events_dir.join("current.events");
    let exists = symlink_path.is_symlink()
        || (symlink_path.exists()
            && symlink_path
                .file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n == "current.events"));

    if !exists {
        return Ok(DoctorSection {
            name: "stale_symlink".into(),
            status: SectionStatus::Ok,
            details: vec!["no stale current.events found".into()],
        });
    }

    if fix {
        if let Err(e) = fs::remove_file(&symlink_path) {
            return Ok(DoctorSection {
                name: "stale_symlink".into(),
                status: SectionStatus::Fail,
                details: vec![format!("failed to remove stale current.events: {e}")],
            });
        }
        fixes.push("removed stale current.events symlink".into());
        return Ok(DoctorSection {
            name: "stale_symlink".into(),
            status: SectionStatus::Ok,
            details: vec!["removed stale current.events".into()],
        });
    }

    Ok(DoctorSection {
        name: "stale_symlink".into(),
        status: SectionStatus::Warn,
        details: vec!["stale current.events exists; run with --fix to remove".into()],
    })
}

// ---------------------------------------------------------------------------
// Human rendering
// ---------------------------------------------------------------------------

fn render_human(report: &DoctorReport, w: &mut dyn Write) -> std::io::Result<()> {
    for section in &report.sections {
        write!(w, "{:<4} {}", section.status.to_string(), section.name)?;
        if section.details.len() == 1 {
            writeln!(w, " — {}", section.details[0])?;
        } else {
            writeln!(w)?;
            for detail in &section.details {
                writeln!(w, "     {detail}")?;
            }
        }
    }

    if !report.fixes_applied.is_empty() {
        writeln!(w)?;
        writeln!(w, "Fixes applied:")?;
        for fix in &report.fixes_applied {
            writeln!(w, "  - {fix}")?;
        }
    }

    writeln!(w)?;
    if report.ok {
        writeln!(w, "doctor: all checks passed")?;
    } else {
        writeln!(w, "doctor: issues found (re-run with --fix to repair)")?;
    }

    Ok(())
}
