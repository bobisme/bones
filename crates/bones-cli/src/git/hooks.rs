//! Git hook generation, installation, and staged event verification.

use std::path::Path;
use std::process::Command;

use anyhow::{Context as _, Result};
use bones_core::event::parser::parse_line;
#[cfg(test)]
use bones_core::event::writer::write_event;
#[cfg(test)]
use bones_core::event::{Event, EventData, EventType, data::CreateData};
#[cfg(test)]
use bones_core::model::item::*;
#[cfg(test)]
use bones_core::model::item_id::ItemId;
use std::{fs, io::Read};

const HOOK_MARKER: &str = "# bones-git-hook: managed";
const POST_MERGE_HOOK: &str = "post-merge";
const PRE_COMMIT_HOOK: &str = "pre-commit";

/// Generate the contents of `post-merge` hook.
pub fn generate_post_merge_hook() -> String {
    format!(
        "{marker}\n\
#!/bin/sh\
\
if command -v bn >/dev/null 2>&1; then\n\
  bn admin rebuild --incremental\n\
else\n\
  echo \"Warning: bn is not installed; skipping projection refresh hook\"\n\
fi\n",
        marker = HOOK_MARKER
    )
}

/// Generate the contents of `pre-commit` hook.
pub fn generate_pre_commit_hook() -> String {
    format!(
        "{marker}\n\
#!/bin/sh\
\
if command -v bn >/dev/null 2>&1; then\n\
  bn admin verify --staged\n\
  rc=$?\n\
  if [ \"$rc\" -ne 0 ]; then\n\
    echo \"Error: staged .events files failed format validation\"\n\
    exit $rc\n\
  fi\n\
else\n\
  echo \"Warning: bn is not installed; skipping staged .events validation\"\n\
fi\n",
        marker = HOOK_MARKER
    )
}

/// Install optional hook scripts into `.git/hooks`.
///
/// Existing hooks are preserved by appending these scripts (unless they are already
/// installed), so this function is safe to run multiple times.
pub fn install_hooks(project_root: &Path) -> Result<()> {
    let git_dir = project_root.join(".git");
    if !git_dir.exists() {
        anyhow::bail!("No .git directory found. Run this in a git repository.");
    }

    let hooks_dir = git_dir.join("hooks");
    fs::create_dir_all(&hooks_dir)
        .with_context(|| format!("Failed to create hook directory: {}", hooks_dir.display()))?;

    let mapping = [
        (POST_MERGE_HOOK, generate_post_merge_hook()),
        (PRE_COMMIT_HOOK, generate_pre_commit_hook()),
    ];

    for (hook_name, hook_contents) in mapping {
        let hook_path = hooks_dir.join(hook_name);
        install_single_hook(&hook_path, &hook_contents)
            .with_context(|| format!("Failed to install {hook_name}"))?;
    }

    println!("✓ Installed optional bn git hooks.");
    println!("  - .git/hooks/{POST_MERGE_HOOK}");
    println!("  - .git/hooks/{PRE_COMMIT_HOOK}");

    Ok(())
}

fn install_single_hook(path: &Path, hook_contents: &str) -> Result<()> {
    if path.exists() {
        let mut existing = String::new();
        fs::File::open(path)
            .with_context(|| format!("Failed to read existing hook: {}", path.display()))?
            .read_to_string(&mut existing)
            .with_context(|| format!("Failed to read existing hook: {}", path.display()))?;

        if existing.contains(HOOK_MARKER) {
            return Ok(());
        }

        let mut combined = existing;
        if !combined.ends_with('\n') {
            combined.push('\n');
        }
        if !combined.ends_with("\n\n") {
            combined.push('\n');
        }
        combined.push_str(hook_contents);

        fs::write(path, combined)
            .with_context(|| format!("Failed to write appended hook: {}", path.display()))?;
    } else {
        fs::write(path, hook_contents)
            .with_context(|| format!("Failed to write hook: {}", path.display()))?;
    }

    make_executable(path)
        .with_context(|| format!("Failed to make hook executable: {}", path.display()))?;

    Ok(())
}

fn make_executable(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perm = fs::metadata(path)?.permissions();
        perm.set_mode(0o755);
        fs::set_permissions(path, perm)?;
    }
    Ok(())
}

/// Verify staged `.events` files in git index are valid TSJSON.
pub fn verify_staged_events() -> Result<()> {
    let staged_events = staged_events_from_index()?;
    if staged_events.is_empty() {
        return Ok(());
    }

    let mut errors: Vec<String> = Vec::new();
    for file in staged_events {
        let file_content = staged_file_content(&file)
            .with_context(|| format!("Failed to read staged file {file}"))?;
        errors.extend(validate_staged_event_file(&file, &file_content));
    }

    if !errors.is_empty() {
        return Err(anyhow::anyhow!(
            "Validation failed for staged .events files:\n{}",
            errors.join("\n")
        ));
    }

    Ok(())
}

fn staged_events_from_index() -> Result<Vec<String>> {
    let output = Command::new("git")
        .args([
            "diff",
            "--cached",
            "--name-only",
            "--diff-filter=ACMR",
            "--",
            "*.events",
        ])
        .output()
        .context("Failed to run git diff --cached")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git diff failed: {stderr}");
    }

    let listed = String::from_utf8(output.stdout).context("Invalid UTF-8 output from git diff")?;
    Ok(listed
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(ToString::to_string)
        .collect())
}

fn staged_file_content(file: &str) -> Result<String> {
    let output = Command::new("git")
        .args(["show", &format!(":{file}")])
        .output()
        .context("Failed to run git show")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        anyhow::bail!("git show :{file} failed: {stderr}");
    }

    String::from_utf8(output.stdout).context("Invalid UTF-8 from git show")
}

/// Validate one staged file and return user-friendly errors.
pub(crate) fn validate_staged_event_file(path: &str, content: &str) -> Vec<String> {
    let mut errors = Vec::new();
    for (line_no, line) in content.lines().enumerate() {
        if let Err(err) = parse_line(line) {
            errors.push(format!("{path}:{}: {err}", line_no + 1));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::collections::BTreeMap;

    #[test]
    fn generate_post_merge_hook_includes_fallback() {
        let hook = generate_post_merge_hook();
        assert!(hook.contains("bn admin rebuild --incremental"));
        assert!(hook.contains("bn is not installed; skipping projection refresh hook"));
        assert!(hook.starts_with(HOOK_MARKER));
    }

    #[test]
    fn generate_pre_commit_hook_runs_staged_verify() {
        let hook = generate_pre_commit_hook();
        assert!(hook.contains("bn admin verify --staged"));
        assert!(hook.contains("staged .events files failed format validation"));
        assert!(hook.starts_with(HOOK_MARKER));
    }

    fn valid_create_event_line() -> String {
        let mut event = Event {
            wall_ts_us: 1720000000,
            agent: "alice".to_string(),
            itc: "itc:AQ".to_string(),
            parents: vec![],
            event_type: EventType::Create,
            item_id: ItemId::new_unchecked("bn-a7x"),
            data: EventData::Create(CreateData {
                title: "My item".to_string(),
                kind: Kind::Task,
                size: None,
                urgency: bones_core::model::item::Urgency::Default,
                labels: vec![],
                parent: None,
                causation: None,
                description: None,
                extra: BTreeMap::new(),
            }),
            event_hash: "placeholder".to_string(),
        };
        write_event(&mut event).expect("write_event")
    }

    #[test]
    fn validate_staged_event_file_accepts_valid_lines() {
        let line = valid_create_event_line();
        let content = format!("# bones event log v1\n{line}");
        let errors = validate_staged_event_file(".bones/events/2026-01.events", &content);
        assert!(
            errors.is_empty(),
            "expected no validation errors: {errors:?}"
        );
    }

    #[test]
    fn validate_staged_event_file_reports_invalid_tsjson() {
        let content = "# bones event log v1\nnot-tsjson-line\n";
        let errors = validate_staged_event_file("shard.events", content);
        assert_eq!(errors.len(), 1);
        assert!(errors[0].contains("shard.events"));
        assert!(errors[0].contains(":2:"));
    }
}
