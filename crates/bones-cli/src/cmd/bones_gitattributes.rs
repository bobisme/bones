use anyhow::{Context, Result};
use std::io::Write;
use std::path::Path;

const MANAGED_HEADER: &str = "# bones: merge policy for event logs";
const BONES_ENTRY: &str = "events/** merge=union";
const LEGACY_ROOT_ENTRY: &str = ".bones/events merge=union";
/// Old pattern that matched only a file literally named `events`, not files
/// inside the `events/` directory.
const LEGACY_BONES_ENTRY: &str = "events merge=union";

pub fn ensure_bones_gitattributes(bones_dir: &Path) -> Result<()> {
    let path = bones_dir.join(".gitattributes");
    let existing = if path.exists() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("failed to read {}", path.display()))?
    } else {
        String::new()
    };

    // Migrate: replace the old buggy pattern with the correct one.
    if existing
        .lines()
        .any(|line| line.trim() == LEGACY_BONES_ENTRY)
    {
        let updated = existing.replace(LEGACY_BONES_ENTRY, BONES_ENTRY);
        std::fs::write(&path, updated)
            .with_context(|| format!("failed to update {}", path.display()))?;
        return Ok(());
    }

    if existing.lines().any(|line| line.trim() == BONES_ENTRY) {
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
    writeln!(file, "{BONES_ENTRY}")?;

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
    fn creates_bones_gitattributes_with_union_entry() {
        let dir = TempDir::new().expect("tmp");
        ensure_bones_gitattributes(dir.path()).expect("ensure gitattributes");

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).expect("read");
        assert!(content.contains(BONES_ENTRY));
    }

    #[test]
    fn ensure_bones_gitattributes_is_idempotent() {
        let dir = TempDir::new().expect("tmp");
        ensure_bones_gitattributes(dir.path()).expect("first");
        ensure_bones_gitattributes(dir.path()).expect("second");

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).expect("read");
        let count = content
            .lines()
            .filter(|line| line.trim() == BONES_ENTRY)
            .count();
        assert_eq!(count, 1, "duplicate entry found:\n{content}");
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
    fn migrates_legacy_bones_entry_to_glob() {
        let dir = TempDir::new().expect("tmp");
        std::fs::write(
            dir.path().join(".gitattributes"),
            "# bones: merge policy for event logs\nevents merge=union\n",
        )
        .expect("seed");

        ensure_bones_gitattributes(dir.path()).expect("migrate");

        let content = std::fs::read_to_string(dir.path().join(".gitattributes")).expect("read");
        assert!(
            content.contains("events/** merge=union"),
            "new pattern missing:\n{content}"
        );
        assert!(
            !content.contains("\nevents merge=union"),
            "old pattern still present:\n{content}"
        );
    }
}
