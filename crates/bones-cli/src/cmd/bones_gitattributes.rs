use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::Path;

const EVENTS_ENTRY: &str = "events/*.events merge=union";
const MANIFEST_ENTRY: &str = "events/*.manifest -merge";
const LEGACY_ROOT_ENTRY: &str = ".bones/events merge=union";
/// Old pattern that matched only a file literally named `events`, not files
/// inside the `events/` directory.
const LEGACY_BARE_ENTRY: &str = "events merge=union";
/// Over-broad pattern: it (wrongly) applied the union merge driver to the
/// `.manifest` snapshot files alongside the `.events` logs, corrupting their
/// integrity records on merge. Superseded by [`EVENTS_ENTRY`] + [`MANIFEST_ENTRY`].
const LEGACY_GLOB_ENTRY: &str = "events/** merge=union";

/// The canonical bones-managed block. Event logs are append-only and replay
/// order-independent, so `union` is safe; manifests are single coherent
/// snapshots that must never be union-merged.
const MANAGED_BLOCK: &str = "\
# bones: merge policy for event logs
# Event logs are append-only and replay order-independent: union concatenates
# both sides' new lines (duplicates dedupe by event hash on replay).
events/*.events merge=union

# Manifests are single coherent snapshots (count + byte_len + file_hash).
# Never union them — that corrupts the integrity record. Let merges surface a
# conflict; regenerate with `bn verify` / rebuild after merging a sealed shard.
events/*.manifest -merge
";

pub fn ensure_bones_gitattributes(bones_dir: &Path) -> Result<()> {
    let path = bones_dir.join(".gitattributes");
    let existing = if path.exists() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
    } else {
        String::new()
    };

    let has_events = existing.lines().any(|line| line.trim() == EVENTS_ENTRY);
    let has_manifest = existing.lines().any(|line| line.trim() == MANIFEST_ENTRY);
    let has_legacy = existing.lines().any(|line| {
        let t = line.trim();
        t == LEGACY_BARE_ENTRY || t == LEGACY_GLOB_ENTRY
    });

    // Already in the desired end state with nothing to migrate.
    if has_events && has_manifest && !has_legacy {
        return Ok(());
    }

    // Lines bones manages: the canonical block plus the superseded legacy
    // entries. Strip them all so the block can be re-emitted cleanly without
    // duplicating entries or comments, while preserving any user-added lines.
    let managed: HashSet<&str> = MANAGED_BLOCK
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .chain([LEGACY_BARE_ENTRY, LEGACY_GLOB_ENTRY])
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
    out.push_str(MANAGED_BLOCK);

    std::fs::write(&path, &out).with_context(|| format!("failed to write {}", path.display()))?;

    Ok(())
}

pub fn remove_legacy_root_gitattributes_entry(project_root: &Path) -> Result<()> {
    let path = project_root.join(".gitattributes");
    if !path.exists() {
        return Ok(());
    }

    let existing = std::fs::read_to_string(&path)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let lines: Vec<&str> = existing.lines().collect();
    let filtered: Vec<&str> = lines
        .iter()
        .copied()
        .filter(|line| line.trim() != LEGACY_ROOT_ENTRY)
        .collect();

    if filtered.len() == lines.len() {
        return Ok(());
    }

    if filtered.is_empty() {
        std::fs::remove_file(&path)
            .with_context(|| format!("failed to remove {}", path.display()))?;
    } else {
        std::fs::write(&path, format!("{}\n", filtered.join("\n")))
            .with_context(|| format!("failed to update {}", path.display()))?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn creates_bones_gitattributes_with_scoped_entries() {
        let dir = TempDir::new().expect("tmp");
        ensure_bones_gitattributes(dir.path()).expect("ensure gitattributes");

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).expect("read");
        assert!(
            content.contains(EVENTS_ENTRY),
            "events entry missing:\n{content}"
        );
        assert!(
            content.contains(MANIFEST_ENTRY),
            "manifest entry missing:\n{content}"
        );
        // The over-broad glob must never be emitted.
        assert!(
            !content.lines().any(|l| l.trim() == LEGACY_GLOB_ENTRY),
            "over-broad glob emitted:\n{content}"
        );
    }

    #[test]
    fn ensure_bones_gitattributes_is_idempotent() {
        let dir = TempDir::new().expect("tmp");
        ensure_bones_gitattributes(dir.path()).expect("first");
        ensure_bones_gitattributes(dir.path()).expect("second");
        ensure_bones_gitattributes(dir.path()).expect("third");

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).expect("read");
        for entry in [EVENTS_ENTRY, MANIFEST_ENTRY] {
            let count = content.lines().filter(|line| line.trim() == entry).count();
            assert_eq!(count, 1, "duplicate `{entry}`:\n{content}");
        }
    }

    #[test]
    fn migrates_legacy_glob_entry_to_scoped() {
        let dir = TempDir::new().expect("tmp");
        std::fs::write(
            dir.path().join(".gitattributes"),
            "# bones: merge policy for event logs\nevents/** merge=union\n",
        )
        .expect("seed");

        ensure_bones_gitattributes(dir.path()).expect("migrate");

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).expect("read");
        assert!(
            !content.lines().any(|l| l.trim() == LEGACY_GLOB_ENTRY),
            "over-broad glob still present:\n{content}"
        );
        assert!(
            content.contains(EVENTS_ENTRY),
            "events entry missing:\n{content}"
        );
        assert!(
            content.contains(MANIFEST_ENTRY),
            "manifest entry missing:\n{content}"
        );
    }

    #[test]
    fn preserves_user_added_entries() {
        let dir = TempDir::new().expect("tmp");
        std::fs::write(
            dir.path().join(".gitattributes"),
            "*.png binary\nevents/** merge=union\n",
        )
        .expect("seed");

        ensure_bones_gitattributes(dir.path()).expect("migrate");

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).expect("read");
        assert!(
            content.contains("*.png binary"),
            "user line lost:\n{content}"
        );
        assert!(content.contains(EVENTS_ENTRY));
        assert!(content.contains(MANIFEST_ENTRY));
    }

    #[test]
    fn removes_legacy_root_entry_when_present() {
        let dir = TempDir::new().expect("tmp");
        let root = dir.path();
        std::fs::write(
            root.join(".gitattributes"),
            ".bones/events merge=union\n*.png binary\n",
        )
        .expect("seed");

        remove_legacy_root_gitattributes_entry(root).expect("cleanup");

        let content = std::fs::read_to_string(root.join(".gitattributes")).expect("read");
        assert!(!content.contains(LEGACY_ROOT_ENTRY));
        assert!(content.contains("*.png binary"));
    }

    #[test]
    fn removes_root_file_if_legacy_was_only_entry() {
        let dir = TempDir::new().expect("tmp");
        let root = dir.path();
        std::fs::write(root.join(".gitattributes"), ".bones/events merge=union\n").expect("seed");

        remove_legacy_root_gitattributes_entry(root).expect("cleanup");

        assert!(!root.join(".gitattributes").exists());
    }

    #[test]
    fn migrates_legacy_bare_entry_to_scoped() {
        let dir = TempDir::new().expect("tmp");
        std::fs::write(
            dir.path().join(".gitattributes"),
            "# bones: merge policy for event logs\nevents merge=union\n",
        )
        .expect("seed");

        ensure_bones_gitattributes(dir.path()).expect("migrate");

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).expect("read");
        assert!(
            content.contains(EVENTS_ENTRY),
            "new pattern missing:\n{content}"
        );
        assert!(
            content.contains(MANIFEST_ENTRY),
            "manifest entry missing:\n{content}"
        );
        assert!(
            !content.lines().any(|l| l.trim() == LEGACY_BARE_ENTRY),
            "old bare pattern still present:\n{content}"
        );
    }
}
