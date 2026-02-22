use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

const MANAGED_HEADER: &str = "# bones: derived/local-only files";
const MANAGED_ENTRIES: &[&str] = &[
    "bones.db",
    "bones.db-shm",
    "bones.db-wal",
    "feedback.jsonl",
    "agent_profiles/",
    "cache/",
    "itc/",
    "lock",
];

pub fn ensure_bones_gitignore(bones_dir: &Path) -> Result<()> {
    let path = bones_dir.join(".gitignore");

    let existing = if path.exists() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
    } else {
        String::new()
    };

    let missing: Vec<&str> = MANAGED_ENTRIES
        .iter()
        .copied()
        .filter(|entry| !existing.lines().any(|line| line.trim() == *entry))
        .collect();

    if missing.is_empty() {
        return Ok(());
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("failed to open {} for append", path.display()))?;

    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    if !existing.is_empty() {
        writeln!(file)?;
    }
    if !existing.lines().any(|line| line.trim() == MANAGED_HEADER) {
        writeln!(file, "{MANAGED_HEADER}")?;
    }
    for entry in &missing {
        writeln!(file, "{entry}")?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn creates_gitignore_with_managed_entries() {
        let dir = TempDir::new().expect("tmp");
        ensure_bones_gitignore(dir.path()).expect("ensure gitignore");

        let content = std::fs::read_to_string(dir.path().join(".gitignore")).expect("read");
        assert!(content.contains("bones.db"));
        assert!(content.contains("bones.db-wal"));
        assert!(content.contains("cache/"));
        assert!(content.contains("itc/"));
        assert!(content.contains("lock"));
    }

    #[test]
    fn ensure_is_idempotent() {
        let dir = TempDir::new().expect("tmp");
        ensure_bones_gitignore(dir.path()).expect("first");
        ensure_bones_gitignore(dir.path()).expect("second");

        let content = std::fs::read_to_string(dir.path().join(".gitignore")).expect("read");
        for entry in MANAGED_ENTRIES {
            let count = content.lines().filter(|line| line.trim() == *entry).count();
            assert_eq!(count, 1, "duplicate entry {entry}:\n{content}");
        }
    }
}
