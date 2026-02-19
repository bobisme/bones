//! Git merge driver for `.events` shard files.
//!
//! This module implements `bn merge-driver`, a custom git merge driver that
//! handles concurrent modifications to TSJSON event shard files.
//!
//! # How Git Invokes the Driver
//!
//! After adding to `.gitattributes`:
//!
//! ```text
//! *.events merge=bones-events
//! ```
//!
//! And `.git/config`:
//!
//! ```text
//! [merge "bones-events"]
//!     name = Bones event merge driver
//!     driver = bn merge-driver %O %A %B
//! ```
//!
//! Git calls the driver with three paths:
//!
//! - `%O` — base (common ancestor version)
//! - `%A` — ours (local branch version; **also the output path**)
//! - `%B` — theirs (remote branch version)
//!
//! The driver merges ours and theirs, writes the result back to `%A`, and
//! exits with code `0` to tell git the merge succeeded.
//!
//! # Merge Algorithm
//!
//! 1. Read and parse TSJSON events from base, ours, and theirs.
//! 2. Call [`bones_core::sync::merge::merge_event_sets`] to union ours +
//!    theirs, deduplicating by event hash.
//! 3. Sort merged events by `(wall_ts_us, agent, event_hash)` for
//!    deterministic output.
//! 4. Write the shard header followed by all merged events to the ours path.
//! 5. Return `Ok(())` — the caller exits with code 0.

use std::fs;
use std::io::Write as _;
use std::path::Path;

use anyhow::{Context as _, Result};
use bones_core::event::parser::parse_lines;
use bones_core::event::writer::{shard_header, write_line};
use bones_core::sync::merge::merge_event_sets;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Public API
// ---------------------------------------------------------------------------

/// Run the git merge driver for a TSJSON `.events` shard file.
///
/// Reads events from `base`, `ours`, and `theirs`, merges ours and theirs
/// using union semantics (dedup by hash, sort by timestamp/agent/hash), and
/// writes the merged result to `ours` (overwriting it).
///
/// The `base` file is read and parsed but not included in the merge output —
/// all base events that were not superseded will already appear on at least
/// one of the two sides. Reading base enables future optimisations (e.g.
/// detecting genuine conflicts) without changing the current behaviour.
///
/// # Errors
///
/// Returns an error if any file cannot be read, cannot be parsed, or if
/// writing the merged output fails.
pub fn merge_driver_main(base: &Path, ours: &Path, theirs: &Path) -> Result<()> {
    info!(
        base = %base.display(),
        ours = %ours.display(),
        theirs = %theirs.display(),
        "bones git merge driver invoked"
    );

    // --- Parse base (for logging / future use) ---
    let base_content = fs::read_to_string(base)
        .with_context(|| format!("failed to read base file: {}", base.display()))?;
    let base_events = parse_lines(&base_content)
        .map_err(|(line, err)| anyhow::anyhow!("parse error in base at line {line}: {err}"))?;
    info!(count = base_events.len(), "parsed base events");

    // --- Parse ours ---
    let ours_content = fs::read_to_string(ours)
        .with_context(|| format!("failed to read ours file: {}", ours.display()))?;
    let ours_events = parse_lines(&ours_content)
        .map_err(|(line, err)| anyhow::anyhow!("parse error in ours at line {line}: {err}"))?;
    info!(count = ours_events.len(), "parsed ours events");

    // --- Parse theirs ---
    let theirs_content = fs::read_to_string(theirs)
        .with_context(|| format!("failed to read theirs file: {}", theirs.display()))?;
    let theirs_events = parse_lines(&theirs_content)
        .map_err(|(line, err)| anyhow::anyhow!("parse error in theirs at line {line}: {err}"))?;
    info!(count = theirs_events.len(), "parsed theirs events");

    // --- Merge ours + theirs ---
    let merge_result = merge_event_sets(&ours_events, &theirs_events);
    let merged_count = merge_result.events.len();
    let dedup_count = ours_events.len() + theirs_events.len() - merged_count;

    info!(
        merged = merged_count,
        deduped = dedup_count,
        "merge complete"
    );

    if dedup_count > 0 {
        warn!(
            count = dedup_count,
            "deduplicated events (same event appeared on both sides)"
        );
    }

    // --- Write merged output to ours path ---
    let mut output = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(ours)
        .with_context(|| format!("failed to open ours file for writing: {}", ours.display()))?;

    // Write shard header
    output
        .write_all(shard_header().as_bytes())
        .context("failed to write shard header")?;

    // Write each merged event as a TSJSON line
    for event in &merge_result.events {
        let line = write_line(event)
            .with_context(|| format!("failed to serialize event: {}", event.event_hash))?;
        output
            .write_all(line.as_bytes())
            .context("failed to write event line")?;
    }

    output.flush().context("failed to flush output")?;

    info!(
        output = %ours.display(),
        events = merged_count,
        "merge driver wrote output successfully"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use bones_core::event::parser::parse_lines;
    use bones_core::event::writer::{shard_header, write_event};
    use bones_core::event::{Event, EventData, EventType, data::*};
    use bones_core::model::item::*;
    use bones_core::model::item_id::ItemId;
    use std::collections::BTreeMap;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Counter for unique temp directories
    static COUNTER: AtomicU64 = AtomicU64::new(0);

    fn temp_dir(label: &str) -> std::path::PathBuf {
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("bones-merge-driver-{label}-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("failed to create temp dir");
        dir
    }

    /// Build a valid Event with its hash computed.
    fn make_create_event(ts: i64, agent: &str) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: agent.to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-a7x"),
            data: EventData::Create(CreateData {
                title: "Test item".to_string(),
                kind: Kind::Task,
                size: None,
                urgency: Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: "placeholder".to_string(),
        };
        // Compute and set the real hash
        let line = write_event(&mut event).expect("write_event");
        let _ = line; // hash is now set on event
        event
    }

    /// Build a valid comment Event with its hash computed.
    fn make_comment_event(ts: i64, agent: &str, body: &str) -> Event {
        let mut event = Event {
            wall_ts_us: ts,
            agent: agent.to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Comment,
            item_id: ItemId::new_unchecked("bn-a7x"),
            data: EventData::Comment(CommentData {
                body: body.to_string(),
                extra: BTreeMap::new(),
            }),
            event_hash: "placeholder".to_string(),
        };
        let line = write_event(&mut event).expect("write_event");
        let _ = line;
        event
    }

    /// Write a shard file with the given events.
    fn write_shard(path: &Path, events: &[Event]) {
        let mut content = shard_header();
        for event in events {
            let line = write_event(&mut event.clone()).expect("write_event");
            content.push_str(&line);
        }
        fs::write(path, content).expect("write shard");
    }

    // -----------------------------------------------------------------------
    // Basic operation
    // -----------------------------------------------------------------------

    #[test]
    fn merge_disjoint_shards() {
        let dir = temp_dir("disjoint");
        let base = dir.join("base.events");
        let ours = dir.join("ours.events");
        let theirs = dir.join("theirs.events");

        // Shared base: one create event
        let create = make_create_event(1000, "alice");

        // Ours adds a comment
        let our_comment = make_comment_event(2000, "alice", "Our comment");
        // Theirs adds a different comment
        let their_comment = make_comment_event(3000, "bob", "Their comment");

        write_shard(&base, &[create.clone()]);
        write_shard(&ours, &[create.clone(), our_comment.clone()]);
        write_shard(&theirs, &[create, their_comment]);

        merge_driver_main(&base, &ours, &theirs).expect("merge should succeed");

        // Read the merged output
        let merged_content = fs::read_to_string(&ours).expect("read merged output");
        let merged_events = parse_lines(&merged_content).expect("parse merged output");

        // Should have: create + our_comment + their_comment = 3 events
        assert_eq!(merged_events.len(), 3, "expected 3 merged events");
    }

    #[test]
    fn merge_deduplicates_shared_events() {
        let dir = temp_dir("dedup");
        let base = dir.join("base.events");
        let ours = dir.join("ours.events");
        let theirs = dir.join("theirs.events");

        let create = make_create_event(1000, "alice");
        let comment = make_comment_event(2000, "alice", "Shared comment");

        // Both sides have the same events (no divergence)
        write_shard(&base, &[create.clone()]);
        write_shard(&ours, &[create.clone(), comment.clone()]);
        write_shard(&theirs, &[create, comment]);

        merge_driver_main(&base, &ours, &theirs).expect("merge should succeed");

        let merged_content = fs::read_to_string(&ours).expect("read merged output");
        let merged_events = parse_lines(&merged_content).expect("parse merged output");

        assert_eq!(merged_events.len(), 2, "duplicate events should be deduped");
    }

    #[test]
    fn merge_preserves_sort_order_by_timestamp() {
        let dir = temp_dir("sort-order");
        let base = dir.join("base.events");
        let ours = dir.join("ours.events");
        let theirs = dir.join("theirs.events");

        let e1 = make_comment_event(1000, "alice", "First");
        let e2 = make_comment_event(2000, "alice", "Second");
        let e3 = make_comment_event(3000, "bob", "Third");

        write_shard(&base, &[]);
        // Ours has e3, e1 (out of order)
        write_shard(&ours, &[e3.clone(), e1.clone()]);
        // Theirs has e2
        write_shard(&theirs, &[e2.clone()]);

        merge_driver_main(&base, &ours, &theirs).expect("merge should succeed");

        let merged_content = fs::read_to_string(&ours).expect("read merged output");
        let merged_events = parse_lines(&merged_content).expect("parse merged output");

        assert_eq!(merged_events.len(), 3);
        assert_eq!(merged_events[0].wall_ts_us, 1000, "first event has ts=1000");
        assert_eq!(
            merged_events[1].wall_ts_us, 2000,
            "second event has ts=2000"
        );
        assert_eq!(merged_events[2].wall_ts_us, 3000, "third event has ts=3000");
    }

    #[test]
    fn merge_empty_ours_with_nonempty_theirs() {
        let dir = temp_dir("empty-ours");
        let base = dir.join("base.events");
        let ours = dir.join("ours.events");
        let theirs = dir.join("theirs.events");

        let comment = make_comment_event(1000, "bob", "Only on theirs");

        write_shard(&base, &[]);
        write_shard(&ours, &[]);
        write_shard(&theirs, &[comment]);

        merge_driver_main(&base, &ours, &theirs).expect("merge should succeed");

        let merged_content = fs::read_to_string(&ours).expect("read merged output");
        let merged_events = parse_lines(&merged_content).expect("parse merged output");

        assert_eq!(
            merged_events.len(),
            1,
            "theirs event should appear in merged output"
        );
    }

    #[test]
    fn merge_nonempty_ours_with_empty_theirs() {
        let dir = temp_dir("empty-theirs");
        let base = dir.join("base.events");
        let ours = dir.join("ours.events");
        let theirs = dir.join("theirs.events");

        let comment = make_comment_event(1000, "alice", "Only on ours");

        write_shard(&base, &[]);
        write_shard(&ours, &[comment]);
        write_shard(&theirs, &[]);

        merge_driver_main(&base, &ours, &theirs).expect("merge should succeed");

        let merged_content = fs::read_to_string(&ours).expect("read merged output");
        let merged_events = parse_lines(&merged_content).expect("parse merged output");

        assert_eq!(merged_events.len(), 1, "ours event should be preserved");
    }

    #[test]
    fn merge_all_empty_produces_valid_shard() {
        let dir = temp_dir("all-empty");
        let base = dir.join("base.events");
        let ours = dir.join("ours.events");
        let theirs = dir.join("theirs.events");

        write_shard(&base, &[]);
        write_shard(&ours, &[]);
        write_shard(&theirs, &[]);

        merge_driver_main(&base, &ours, &theirs).expect("merge should succeed");

        let merged_content = fs::read_to_string(&ours).expect("read merged output");
        // Should have header but no events
        assert!(
            merged_content.contains("# bones event log v1"),
            "missing header"
        );
        let merged_events = parse_lines(&merged_content).expect("parse merged output");
        assert!(merged_events.is_empty(), "no events expected");
    }

    #[test]
    fn merged_output_has_shard_header() {
        let dir = temp_dir("header-check");
        let base = dir.join("base.events");
        let ours = dir.join("ours.events");
        let theirs = dir.join("theirs.events");

        let comment = make_comment_event(1000, "alice", "Hello");

        write_shard(&base, &[]);
        write_shard(&ours, &[comment]);
        write_shard(&theirs, &[]);

        merge_driver_main(&base, &ours, &theirs).expect("merge should succeed");

        let merged_content = fs::read_to_string(&ours).expect("read merged output");
        assert!(
            merged_content.starts_with("# bones event log v1\n"),
            "merged output must start with shard header"
        );
        assert!(
            merged_content.contains("# fields:"),
            "merged output must include field comment"
        );
    }

    #[test]
    fn merge_is_idempotent() {
        let dir = temp_dir("idempotent");
        let base = dir.join("base.events");
        let ours = dir.join("ours.events");
        let theirs = dir.join("theirs.events");

        let create = make_create_event(1000, "alice");
        let comment = make_comment_event(2000, "bob", "Comment");

        write_shard(&base, &[]);
        write_shard(&ours, &[create.clone(), comment.clone()]);
        write_shard(&theirs, &[create, comment]);

        // First merge
        merge_driver_main(&base, &ours, &theirs).expect("first merge should succeed");
        let after_first = fs::read_to_string(&ours).expect("read after first merge");

        // Second merge (re-use result as ours)
        write_shard(&theirs, &[]); // theirs is now empty
        merge_driver_main(&base, &ours, &theirs).expect("second merge should succeed");
        let after_second = fs::read_to_string(&ours).expect("read after second merge");

        let events_first = parse_lines(&after_first).expect("parse first");
        let events_second = parse_lines(&after_second).expect("parse second");

        let hashes_first: Vec<&str> = events_first.iter().map(|e| e.event_hash.as_str()).collect();
        let hashes_second: Vec<&str> = events_second
            .iter()
            .map(|e| e.event_hash.as_str())
            .collect();
        assert_eq!(hashes_first, hashes_second, "merge should be idempotent");
    }

    #[test]
    fn missing_base_returns_error() {
        let dir = temp_dir("missing-base");
        let base = dir.join("nonexistent.events");
        let ours = dir.join("ours.events");
        let theirs = dir.join("theirs.events");

        write_shard(&ours, &[]);
        write_shard(&theirs, &[]);

        let result = merge_driver_main(&base, &ours, &theirs);
        assert!(result.is_err(), "missing base file should return an error");
    }

    #[test]
    fn missing_theirs_returns_error() {
        let dir = temp_dir("missing-theirs");
        let base = dir.join("base.events");
        let ours = dir.join("ours.events");
        let theirs = dir.join("nonexistent.events");

        write_shard(&base, &[]);
        write_shard(&ours, &[]);

        let result = merge_driver_main(&base, &ours, &theirs);
        assert!(
            result.is_err(),
            "missing theirs file should return an error"
        );
    }
}
