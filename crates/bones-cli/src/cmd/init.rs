use crate::git;
use anyhow::{Context as _, Result};
use chrono::Local;
use clap::Args;
use std::path::Path;

#[derive(Args, Debug)]
pub struct InitArgs {
    /// Force re-initialization even if `.bones/` already exists.
    #[arg(long)]
    pub force: bool,

    /// Install optional git hooks (`post-merge`, `pre-commit`) during initialization.
    #[arg(long)]
    pub hooks: bool,
}

const CONFIG_TOML: &str = "[goals]\n\
    auto_complete = true\n\
    \n\
    [search]\n\
    semantic = true\n\
    model = \"minilm-l6-v2-int8\"\n\
    duplicate_threshold = 0.85\n\
    related_threshold = 0.65\n\
    warn_on_create = true\n\
    \n\
    [triage]\n\
    feedback_learning = true\n\
    \n\
    [archive]\n\
    auto_days = 30\n";

const GITIGNORE: &str = "bones.db\nfeedback.jsonl\nagent_profiles/\ncache/\n";

const SHARD_HEADER: &str =
    "# bones event log v1\n# fields: timestamp\tagent\ttype\titem_id\tdata\n";

/// Execute `bn init`. Creates the project skeleton:
///
/// ```text
/// .bones/
///   events/
///     YYYY-MM.events    (active shard with header comment)
///     current.events    (symlink -> YYYY-MM.events)
///   config.toml         (default project config template)
///   .gitignore          (bones.db, feedback.jsonl, agent_profiles/, cache/)
/// ```
///
/// # Errors
///
/// Returns an error if `.bones/` already exists and `--force` is not set,
/// or if any filesystem operation fails.
pub fn run_init(args: &InitArgs, project_root: &Path) -> Result<()> {
    let bones_dir = project_root.join(".bones");

    if bones_dir.exists() && !args.force {
        anyhow::bail!(".bones/ already exists. Use `bn init --force` to reinitialize.");
    }

    // Warn about standalone (non-git) mode
    let is_git_repo = project_root.join(".git").exists();
    if !is_git_repo {
        eprintln!("Note: No git repository detected. Running in standalone mode.");
        eprintln!("      Git integration features (merge driver, hooks, push/pull) will be");
        eprintln!("      unavailable until you run `git init` and then `bn init --force`.");
        eprintln!();

        if args.hooks {
            anyhow::bail!("--hooks requires a git repository; initialize git first.");
        }
    }

    // Create directory structure
    let events_dir = bones_dir.join("events");
    std::fs::create_dir_all(&events_dir).with_context(|| {
        format!(
            "Failed to create events directory: {}",
            events_dir.display()
        )
    })?;

    // Create initial shard named for current month (YYYY-MM.events)
    let shard_name = Local::now().format("%Y-%m.events").to_string();
    let shard_path = events_dir.join(&shard_name);
    std::fs::write(&shard_path, SHARD_HEADER)
        .with_context(|| format!("Failed to write shard file: {}", shard_path.display()))?;

    // Create current.events symlink pointing to the active shard
    let symlink_path = events_dir.join("current.events");
    if symlink_path.exists() || symlink_path.is_symlink() {
        std::fs::remove_file(&symlink_path)
            .with_context(|| "Failed to remove existing current.events symlink")?;
    }
    std::os::unix::fs::symlink(&shard_name, &symlink_path)
        .with_context(|| "Failed to create current.events symlink")?;

    // Write default config.toml
    let config_path = bones_dir.join("config.toml");
    std::fs::write(&config_path, CONFIG_TOML)
        .with_context(|| format!("Failed to write config: {}", config_path.display()))?;

    // Write .gitignore for derived/cache files
    let gitignore_path = bones_dir.join(".gitignore");
    std::fs::write(&gitignore_path, GITIGNORE)
        .with_context(|| format!("Failed to write .gitignore: {}", gitignore_path.display()))?;

    if args.hooks {
        git::hooks::install_hooks(project_root)?;
    }

    // Onboarding hints
    println!("✓ Initialized .bones/ project structure.");
    println!();
    println!("  Active shard: .bones/events/{shard_name}");
    println!("  Config:       .bones/config.toml");
    println!();
    println!("Next steps:");
    println!("  Set your agent identity (required for mutations):");
    println!("    export AGENT=your-name        # short form");
    println!("    export BONES_AGENT=your-name  # explicit override");
    println!();
    println!("  Create your first item:");
    println!("    bn create --title \"My first item\"");

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::{fs, path::PathBuf};

    fn make_temp_dir(label: &str) -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("bones-init-test-{label}-{id}"));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).expect("failed to create temp dir");
        dir
    }

    #[test]
    fn fresh_init_creates_structure() {
        let root = make_temp_dir("fresh");
        let args = InitArgs {
            force: false,
            hooks: false,
        };
        run_init(&args, &root).expect("init should succeed");

        assert!(root.join(".bones").is_dir());
        assert!(root.join(".bones/events").is_dir());
        assert!(root.join(".bones/config.toml").is_file());
        assert!(root.join(".bones/.gitignore").is_file());

        // Events dir must have at least the shard + symlink
        let count = fs::read_dir(root.join(".bones/events"))
            .expect("events dir readable")
            .filter_map(|e| e.ok())
            .count();
        assert!(
            count >= 2,
            "events dir should have shard + current.events symlink"
        );

        let symlink = root.join(".bones/events/current.events");
        assert!(symlink.is_symlink(), "current.events must be a symlink");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reinit_without_force_fails() {
        let root = make_temp_dir("no-force");
        let args = InitArgs {
            force: false,
            hooks: false,
        };
        run_init(&args, &root).expect("first init should succeed");

        let result = run_init(&args, &root);
        assert!(result.is_err(), "reinit without --force must fail");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reinit_with_force_succeeds() {
        let root = make_temp_dir("with-force");
        run_init(
            &InitArgs {
                force: false,
                hooks: false,
            },
            &root,
        )
        .expect("first init should succeed");
        run_init(
            &InitArgs {
                force: true,
                hooks: false,
            },
            &root,
        )
        .expect("reinit --force should succeed");

        assert!(root.join(".bones/config.toml").is_file());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn config_toml_has_required_sections() {
        let root = make_temp_dir("config");
        run_init(
            &InitArgs {
                force: false,
                hooks: false,
            },
            &root,
        )
        .expect("init should succeed");

        let content =
            fs::read_to_string(root.join(".bones/config.toml")).expect("config.toml readable");
        assert!(content.contains("[goals]"), "missing [goals]");
        assert!(
            content.contains("auto_complete = true"),
            "missing auto_complete"
        );
        assert!(content.contains("[search]"), "missing [search]");
        assert!(
            content.contains("duplicate_threshold"),
            "missing duplicate_threshold"
        );
        assert!(content.contains("[triage]"), "missing [triage]");
        assert!(
            content.contains("feedback_learning = true"),
            "missing feedback_learning"
        );
        assert!(content.contains("[archive]"), "missing [archive]");
        assert!(
            content.contains("auto_days = 30"),
            "missing archive auto_days"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn gitignore_covers_derived_files() {
        let root = make_temp_dir("gitignore");
        run_init(
            &InitArgs {
                force: false,
                hooks: false,
            },
            &root,
        )
        .expect("init should succeed");

        let content =
            fs::read_to_string(root.join(".bones/.gitignore")).expect(".gitignore readable");
        assert!(content.contains("bones.db"), "must ignore bones.db");
        assert!(
            content.contains("feedback.jsonl"),
            "must ignore feedback.jsonl"
        );
        assert!(
            content.contains("agent_profiles/"),
            "must ignore agent_profiles/"
        );
        assert!(content.contains("cache/"), "must ignore cache/");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn shard_has_correct_header() {
        let root = make_temp_dir("shard-header");
        run_init(
            &InitArgs {
                force: false,
                hooks: false,
            },
            &root,
        )
        .expect("init should succeed");

        // Read via the symlink so we confirm the symlink is wired correctly
        let symlink = root.join(".bones/events/current.events");
        let content = fs::read_to_string(&symlink).expect("current.events readable via symlink");
        assert!(
            content.contains("# bones event log v1"),
            "missing log version header"
        );
        assert!(
            content.contains("# fields: timestamp"),
            "missing fields header"
        );

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn shard_name_matches_current_month() {
        let root = make_temp_dir("shard-name");
        run_init(
            &InitArgs {
                force: false,
                hooks: false,
            },
            &root,
        )
        .expect("init should succeed");

        let expected_name = Local::now().format("%Y-%m.events").to_string();
        let shard_path = root.join(".bones/events").join(&expected_name);
        assert!(
            shard_path.is_file(),
            "shard file {expected_name} should exist"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
