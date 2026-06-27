#![deny(unsafe_code)]
//! bones-core library.

pub mod cache;
pub mod capabilities;
pub mod clock;
pub mod compact;
pub mod config;
pub mod crdt;
pub mod dag;
pub mod db;
pub mod error;
pub mod event;
pub mod graph;
pub mod lock;
pub mod model;
pub mod recovery;
pub mod shard;
pub mod sync;
pub mod timing;
pub mod undo;
pub mod verify;

use tracing::{info, instrument};

/// # Conventions
///
/// - **Errors**: Use `anyhow::Result` for return types where appropriate.
/// - **Logging**: Use `tracing` macros (`info!`, `warn!`, `error!`, `debug!`, `trace!`).

#[instrument]
pub fn init() {
    info!("bones-core initialized");
    // Ensure .bones/.gitattributes exists and has the union merge hint for events.
    let bones_dir = std::path::Path::new(".bones");
    if !bones_dir.is_dir() {
        return;
    }

    let gitattributes_path = bones_dir.join(".gitattributes");
    let events_entry = "events/*.events merge=union";
    let manifest_entry = "events/*.manifest -merge";
    // Patterns superseded by the scoped entries above. `events merge=union`
    // matched only a file literally named `events`; `events/** merge=union`
    // was over-broad and (wrongly) union-merged the `.manifest` snapshots.
    let legacy_bare = "events merge=union";
    let legacy_glob = "events/** merge=union";
    let managed_block = "\
# bones: merge policy for event logs
# Event logs are append-only and replay order-independent: union concatenates
# both sides' new lines (duplicates dedupe by event hash on replay).
events/*.events merge=union

# Manifests are single coherent snapshots (count + byte_len + file_hash).
# Never union them — that corrupts the integrity record. Let merges surface a
# conflict; regenerate with `bn verify` / rebuild after merging a sealed shard.
events/*.manifest -merge
";

    let existing = if gitattributes_path.exists() {
        std::fs::read_to_string(&gitattributes_path).unwrap_or_default()
    } else {
        String::new()
    };

    let has_events = existing.lines().any(|line| line.trim() == events_entry);
    let has_manifest = existing.lines().any(|line| line.trim() == manifest_entry);
    let has_legacy = existing.lines().any(|line| {
        let t = line.trim();
        t == legacy_bare || t == legacy_glob
    });

    if has_events && has_manifest && !has_legacy {
        return;
    }

    // Strip bones-managed and superseded legacy lines, then re-emit the block,
    // preserving any user-added entries.
    let managed: std::collections::HashSet<&str> = managed_block
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .chain([legacy_bare, legacy_glob])
        .collect();

    let mut kept: Vec<&str> = existing
        .lines()
        .filter(|line| !managed.contains(line.trim()))
        .collect();
    while kept.last().is_some_and(|line| line.trim().is_empty()) {
        kept.pop();
    }

    let mut out = String::new();
    if !kept.is_empty() {
        out.push_str(&kept.join("\n"));
        out.push_str("\n\n");
    }
    out.push_str(managed_block);

    let _ = std::fs::write(gitattributes_path, out);
}

#[cfg(test)]
mod tests {
    #[test]
    fn it_works() {
        assert!(true);
    }
}
