//! Verification utilities for shard manifests and redaction completeness.

pub mod redact;

use std::fs;
use std::path::{Path, PathBuf};

use crate::event::parser;
use crate::shard::{ShardError, ShardManager, ShardManifest};

/// Verification error.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    /// Shard-level I/O or lock error.
    #[error("shard error: {0}")]
    Shard(#[from] ShardError),

    /// Generic I/O error.
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Per-shard verification result.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ShardCheck {
    /// Shard name (`YYYY-MM.events`).
    pub shard_name: String,
    /// Outcome.
    pub status: ShardCheckStatus,
}

/// Status for one shard verification.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShardCheckStatus {
    /// Manifest exists and matches shard contents.
    Verified,
    /// Manifest was missing and regenerated.
    Regenerated,
    /// Manifest mismatch or missing (without regeneration).
    Failed(String),
}

/// Aggregate verification report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VerifyReport {
    /// Results for sealed shards.
    pub shards: Vec<ShardCheck>,
    /// Active shard parse sanity check status.
    pub active_shard_parse_ok: bool,
}

impl VerifyReport {
    /// Return `true` when all checks passed.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.active_shard_parse_ok
            && self
                .shards
                .iter()
                .all(|s| !matches!(s.status, ShardCheckStatus::Failed(_)))
    }
}

/// Verify sealed shard manifests and parse-sanity-check the active shard.
///
/// Sealed shard policy:
/// - Every sealed shard (`all except latest`) must have a manifest.
/// - If missing and `regenerate_missing` is true, regenerate from shard file.
/// - Existing manifests must match computed `event_count`, `byte_len`, and `file_hash`.
///
/// Active shard policy:
/// - The active shard is not required to have a manifest.
/// - Active shard content is parsed for TSJSON sanity.
///
/// # Errors
///
/// Returns [`VerifyError`] on filesystem access failures.
pub fn verify_repository(
    bones_dir: &Path,
    regenerate_missing: bool,
) -> Result<VerifyReport, VerifyError> {
    let mgr = ShardManager::new(bones_dir);
    let shards = mgr.list_shards()?;

    if shards.is_empty() {
        return Ok(VerifyReport {
            shards: Vec::new(),
            active_shard_parse_ok: true,
        });
    }

    let active = shards.last().copied();
    let mut checks = Vec::new();

    for (year, month) in shards.iter().copied() {
        if Some((year, month)) == active {
            continue;
        }

        let shard_name = ShardManager::shard_filename(year, month);
        let computed = compute_manifest(&mgr, year, month)?;

        match mgr.read_manifest(year, month)? {
            Some(existing) => {
                if existing == computed {
                    checks.push(ShardCheck {
                        shard_name,
                        status: ShardCheckStatus::Verified,
                    });
                } else {
                    checks.push(ShardCheck {
                        shard_name,
                        status: ShardCheckStatus::Failed("manifest mismatch".to_string()),
                    });
                }
            }
            None if regenerate_missing => {
                let _ = mgr.write_manifest(year, month)?;
                checks.push(ShardCheck {
                    shard_name,
                    status: ShardCheckStatus::Regenerated,
                });
            }
            None => {
                checks.push(ShardCheck {
                    shard_name,
                    status: ShardCheckStatus::Failed("missing manifest".to_string()),
                });
            }
        }
    }

    let active_shard_parse_ok = if let Some((year, month)) = active {
        let content = mgr.read_shard(year, month)?;
        parser::parse_lines(&content).is_ok()
    } else {
        true
    };

    Ok(VerifyReport {
        shards: checks,
        active_shard_parse_ok,
    })
}

fn compute_manifest(
    mgr: &ShardManager,
    year: i32,
    month: u32,
) -> Result<ShardManifest, VerifyError> {
    let path: PathBuf = mgr.shard_path(year, month);
    let content = fs::read(&path)?;
    let content_str = String::from_utf8_lossy(&content);
    let event_count = content_str
        .lines()
        .filter(|line| !line.trim().is_empty() && !line.starts_with('#'))
        .count() as u64;

    Ok(ShardManifest {
        shard_name: ShardManager::shard_filename(year, month),
        event_count,
        byte_len: content.len() as u64,
        file_hash: format!("blake3:{}", blake3::hash(&content).to_hex()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn verify_regenerates_missing_manifest_for_sealed_shard() {
        let tmp = TempDir::new().expect("tmp");
        let bones = tmp.path().join(".bones");
        let mgr = ShardManager::new(&bones);
        mgr.ensure_dirs().expect("dirs");

        mgr.create_shard(2025, 1).expect("old shard");
        mgr.append_raw(2025, 1, "e1\n").expect("append");

        // Create a newer active shard so 2025-01 is sealed and missing manifest.
        mgr.create_shard(2030, 1).expect("new shard");

        let report = verify_repository(&bones, true).expect("verify");

        assert!(report.active_shard_parse_ok);
        assert!(
            report
                .shards
                .iter()
                .any(|s| matches!(s.status, ShardCheckStatus::Regenerated))
        );
        assert!(mgr.manifest_path(2025, 1).exists());
    }
}
