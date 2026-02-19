//! `bn sync` — pull/rebuild/push workflow with git configuration management.

use anyhow::{Context as _, Result};
use clap::Args;
use serde::Serialize;
use std::io::Write as _;
use std::path::Path;
use std::process::{Command, Stdio};

/// Result of a `bn sync` run.
#[derive(Debug, Default, Serialize)]
pub struct SyncReport {
    /// Whether `git pull` succeeded.
    pub pulled: bool,
    /// Number of event lines merged (from git pull output; heuristic).
    pub events_merged: usize,
    /// Whether `bn rebuild --incremental` succeeded.
    pub rebuilt: bool,
    /// Whether `git push` succeeded.
    pub pushed: bool,
    /// Any non-fatal warnings collected during the run.
    pub errors: Vec<String>,
}

#[derive(Args, Debug)]
pub struct SyncArgs {
    /// Only update .gitattributes / .gitignore — skip pull/rebuild/push.
    #[arg(long)]
    pub config_only: bool,

    /// Skip `git push` after rebuilding.
    #[arg(long)]
    pub no_push: bool,

    /// Output in JSON (machine-readable) format.
    #[arg(long)]
    pub json: bool,
}

// ─── public API ─────────────────────────────────────────────────────────────

/// Orchestrate `git pull` → `bn rebuild --incremental` → `git push`.
///
/// Each step is attempted in order. If `git pull` fails the workflow still
/// continues so callers can see the full picture.
pub fn sync_workflow(repo_dir: &Path, no_push: bool) -> Result<SyncReport> {
    let mut report = SyncReport::default();

    // Step 1: git pull
    match run_git_pull(repo_dir) {
        Ok(events_merged) => {
            report.pulled = true;
            report.events_merged = events_merged;
        }
        Err(e) => {
            report.errors.push(format!("git pull: {e}"));
        }
    }

    // Step 2: bn rebuild --incremental
    match run_rebuild(repo_dir) {
        Ok(()) => {
            report.rebuilt = true;
        }
        Err(e) => {
            report.errors.push(format!("bn rebuild: {e}"));
        }
    }

    // Step 3: git push (skipped with --no-push)
    if !no_push {
        match run_git_push(repo_dir) {
            Ok(()) => {
                report.pushed = true;
            }
            Err(e) => {
                report.errors.push(format!("git push: {e}"));
            }
        }
    }

    Ok(report)
}

/// Ensure `.gitattributes` contains the bones-events merge driver entry.
///
/// If the file exists the function appends the line only when it is not already
/// present.  If the file does not exist it is created.
pub fn ensure_gitattributes(repo_dir: &Path) -> Result<()> {
    const ENTRY: &str = "*.events merge=bones-events";

    let path = repo_dir.join(".gitattributes");

    if path.exists() {
        let contents = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?;
        if contents.lines().any(|l| l.trim() == ENTRY) {
            return Ok(()); // already present
        }
        // Append, ensuring a trailing newline before our entry
        let mut file = std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .with_context(|| format!("Failed to open {} for appending", path.display()))?;
        if !contents.ends_with('\n') {
            writeln!(file)?;
        }
        writeln!(file, "{ENTRY}")?;
    } else {
        std::fs::write(&path, format!("{ENTRY}\n"))
            .with_context(|| format!("Failed to create {}", path.display()))?;
    }

    Ok(())
}

/// Ensure the project-root `.gitignore` contains entries for derived bones files.
///
/// Entries managed: `bones.db`, `.bones/feedback.jsonl`, `.bones/cache/`.
pub fn ensure_gitignore(repo_dir: &Path) -> Result<()> {
    const MANAGED_HEADER: &str = "# bones: generated-file ignores";
    const MANAGED_ENTRIES: &[&str] = &["bones.db", ".bones/feedback.jsonl", ".bones/cache/"];

    let path = repo_dir.join(".gitignore");

    let existing = if path.exists() {
        std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read {}", path.display()))?
    } else {
        String::new()
    };

    // Collect entries that are missing
    let missing: Vec<&str> = MANAGED_ENTRIES
        .iter()
        .copied()
        .filter(|entry| !existing.lines().any(|l| l.trim() == *entry))
        .collect();

    if missing.is_empty() {
        return Ok(()); // everything already present
    }

    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .with_context(|| format!("Failed to open {} for appending", path.display()))?;

    // Add a blank line + header before our block (if file is non-empty)
    if !existing.is_empty() && !existing.ends_with('\n') {
        writeln!(file)?;
    }
    if !existing.is_empty() {
        writeln!(file)?;
    }
    writeln!(file, "{MANAGED_HEADER}")?;
    for entry in &missing {
        writeln!(file, "{entry}")?;
    }

    Ok(())
}

/// Entry point wired from `main.rs`.
pub fn run_sync(args: &SyncArgs, project_root: &Path) -> Result<()> {
    // Always ensure git configuration is up-to-date
    ensure_gitattributes(project_root)
        .context("Failed to update .gitattributes")?;
    ensure_gitignore(project_root)
        .context("Failed to update .gitignore")?;

    if args.config_only {
        if args.json {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "status": "ok",
                    "message": "git configuration updated"
                }))?
            );
        } else {
            println!("✓ Updated .gitattributes (merge driver entry)");
            println!("✓ Updated .gitignore (derived file entries)");
        }
        return Ok(());
    }

    let report = sync_workflow(project_root, args.no_push)?;

    if args.json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        print_report(&report);
    }

    if !report.errors.is_empty() {
        // Propagate as non-zero exit — use anyhow for structured exit
        anyhow::bail!(
            "Sync completed with errors:\n{}",
            report.errors.join("\n  ")
        );
    }

    Ok(())
}

// ─── private helpers ─────────────────────────────────────────────────────────

fn run_git_pull(repo_dir: &Path) -> Result<usize> {
    let output = Command::new("git")
        .args(["pull", "--rebase"])
        .current_dir(repo_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to spawn git pull")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{}", stderr.trim());
    }

    // Count `.events` related lines in the output as a heuristic for
    // "events merged".  This is intentionally approximate.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let events_merged = stdout.lines().filter(|l| l.contains(".events")).count();

    Ok(events_merged)
}

fn run_rebuild(repo_dir: &Path) -> Result<()> {
    // Prefer the installed `bn` binary; fall back to `cargo run` for dev
    // environments where the binary is not on PATH.
    let status = Command::new("bn")
        .args(["rebuild", "--incremental"])
        .current_dir(repo_dir)
        .status();

    match status {
        Ok(s) if s.success() => return Ok(()),
        Ok(s) => {
            anyhow::bail!(
                "bn rebuild exited with code {}",
                s.code().unwrap_or(-1)
            );
        }
        Err(_) => {
            // `bn` not on PATH — treat as non-fatal and warn caller
            anyhow::bail!("`bn` binary not found; skipping projection rebuild");
        }
    }
}

fn run_git_push(repo_dir: &Path) -> Result<()> {
    let output = Command::new("git")
        .args(["push"])
        .current_dir(repo_dir)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("Failed to spawn git push")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("{}", stderr.trim());
    }

    Ok(())
}

fn print_report(report: &SyncReport) {
    let check = |ok: bool| if ok { "✓" } else { "✗" };

    println!("bn sync");
    println!("  {} git pull  ({} event file(s) merged)", check(report.pulled), report.events_merged);
    println!("  {} bn rebuild --incremental", check(report.rebuilt));
    println!("  {} git push", check(report.pushed));

    if !report.errors.is_empty() {
        println!();
        println!("Errors:");
        for e in &report.errors {
            println!("  • {e}");
        }
    }
}

// ─── tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp(label: &str) -> std::path::PathBuf {
        static CTR: AtomicU64 = AtomicU64::new(0);
        let id = CTR.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("bones-sync-test-{label}-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("failed to create temp dir");
        dir
    }

    // ── ensure_gitattributes ──────────────────────────────────────────────────

    #[test]
    fn gitattributes_created_when_absent() {
        let root = tmp("ga-create");
        ensure_gitattributes(&root).expect("should succeed");

        let content = fs::read_to_string(root.join(".gitattributes")).unwrap();
        assert!(
            content.contains("*.events merge=bones-events"),
            "entry missing: {content}"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gitattributes_appended_when_entry_missing() {
        let root = tmp("ga-append");
        let path = root.join(".gitattributes");
        fs::write(&path, "*.png binary\n").unwrap();

        ensure_gitattributes(&root).expect("should succeed");

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("*.png binary"), "existing entry removed");
        assert!(
            content.contains("*.events merge=bones-events"),
            "new entry missing"
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gitattributes_idempotent() {
        let root = tmp("ga-idempotent");
        ensure_gitattributes(&root).expect("first call");
        ensure_gitattributes(&root).expect("second call");

        let content = fs::read_to_string(root.join(".gitattributes")).unwrap();
        let count = content
            .lines()
            .filter(|l| l.trim() == "*.events merge=bones-events")
            .count();
        assert_eq!(count, 1, "entry duplicated: {content}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gitattributes_no_duplicate_when_already_present() {
        let root = tmp("ga-no-dup");
        let path = root.join(".gitattributes");
        fs::write(&path, "*.events merge=bones-events\n").unwrap();

        ensure_gitattributes(&root).expect("should succeed");

        let content = fs::read_to_string(&path).unwrap();
        let count = content
            .lines()
            .filter(|l| l.trim() == "*.events merge=bones-events")
            .count();
        assert_eq!(count, 1, "entry duplicated on pre-existing file");
        let _ = fs::remove_dir_all(&root);
    }

    // ── ensure_gitignore ──────────────────────────────────────────────────────

    #[test]
    fn gitignore_created_when_absent() {
        let root = tmp("gi-create");
        ensure_gitignore(&root).expect("should succeed");

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        assert!(content.contains("bones.db"), "bones.db missing");
        assert!(
            content.contains(".bones/feedback.jsonl"),
            "feedback.jsonl missing"
        );
        assert!(content.contains(".bones/cache/"), "cache/ missing");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gitignore_appended_when_entries_missing() {
        let root = tmp("gi-append");
        let path = root.join(".gitignore");
        fs::write(&path, "target/\n").unwrap();

        ensure_gitignore(&root).expect("should succeed");

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains("target/"), "existing entry removed");
        assert!(content.contains("bones.db"), "bones.db missing");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gitignore_idempotent() {
        let root = tmp("gi-idempotent");
        ensure_gitignore(&root).expect("first call");
        ensure_gitignore(&root).expect("second call");

        let content = fs::read_to_string(root.join(".gitignore")).unwrap();
        let count = content
            .lines()
            .filter(|l| l.trim() == "bones.db")
            .count();
        assert_eq!(count, 1, "bones.db duplicated: {content}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gitignore_no_duplicate_when_already_present() {
        let root = tmp("gi-no-dup");
        let path = root.join(".gitignore");
        fs::write(
            &path,
            "bones.db\n.bones/feedback.jsonl\n.bones/cache/\n",
        )
        .unwrap();

        ensure_gitignore(&root).expect("should succeed");

        let content = fs::read_to_string(&path).unwrap();
        let count = content
            .lines()
            .filter(|l| l.trim() == "bones.db")
            .count();
        assert_eq!(count, 1, "bones.db duplicated: {content}");
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gitignore_partial_update_adds_only_missing() {
        let root = tmp("gi-partial");
        let path = root.join(".gitignore");
        // Only bones.db is present; the other two should be added
        fs::write(&path, "bones.db\n").unwrap();

        ensure_gitignore(&root).expect("should succeed");

        let content = fs::read_to_string(&path).unwrap();
        assert!(content.contains(".bones/feedback.jsonl"));
        assert!(content.contains(".bones/cache/"));
        let count = content
            .lines()
            .filter(|l| l.trim() == "bones.db")
            .count();
        assert_eq!(count, 1, "bones.db duplicated during partial update");
        let _ = fs::remove_dir_all(&root);
    }

    // ── SyncReport serialization ──────────────────────────────────────────────

    #[test]
    fn sync_report_serializes_to_json() {
        let report = SyncReport {
            pulled: true,
            events_merged: 3,
            rebuilt: true,
            pushed: false,
            errors: vec!["git push: no remote".to_string()],
        };
        let json = serde_json::to_string(&report).expect("serialize");
        assert!(json.contains("\"pulled\":true"));
        assert!(json.contains("\"events_merged\":3"));
        assert!(json.contains("\"pushed\":false"));
        assert!(json.contains("git push: no remote"));
    }

    #[test]
    fn sync_report_default_is_all_false() {
        let r = SyncReport::default();
        assert!(!r.pulled);
        assert!(!r.rebuilt);
        assert!(!r.pushed);
        assert_eq!(r.events_merged, 0);
        assert!(r.errors.is_empty());
    }
}
