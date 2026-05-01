use crate::cmd::bones_gitattributes::{
    ensure_bones_gitattributes, remove_legacy_root_gitattributes_entry,
};
use crate::cmd::bones_gitignore::ensure_bones_gitignore;
use crate::git;
use crate::output::{OutputMode, pretty_kv, pretty_section};
use anyhow::{Context as _, Result};
use chrono::Utc;
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

const SHARD_HEADER: &str =
    "# bones event log v1\n# fields: timestamp\tagent\ttype\titem_id\tdata\n";

/// Execute `bn init`. Creates the project skeleton:
///
/// ```text
/// .bones/
///   events/
///     YYYY-MM.events    (active shard with header comment)
///   config.toml         (default project config template)
///   .gitignore          (derived/runtime files: db/cache/itc/lock)
///   .gitattributes      (local merge policy for events)
/// ```
///
/// # Errors
///
/// Returns an error if `.bones/` already exists and `--force` is not set,
/// or if any filesystem operation fails.
pub fn run_init(args: &InitArgs, output: OutputMode, project_root: &Path) -> Result<()> {
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
    let shard_name = Utc::now().format("%Y-%m.events").to_string();
    let shard_path = events_dir.join(&shard_name);
    std::fs::write(&shard_path, SHARD_HEADER)
        .with_context(|| format!("Failed to write shard file: {}", shard_path.display()))?;

    // Write default config.toml
    let config_path = bones_dir.join("config.toml");
    std::fs::write(&config_path, CONFIG_TOML)
        .with_context(|| format!("Failed to write config: {}", config_path.display()))?;

    // Ensure .gitignore for derived/cache files
    ensure_bones_gitignore(&bones_dir).with_context(|| {
        format!(
            "Failed to ensure {}",
            bones_dir.join(".gitignore").display()
        )
    })?;

    ensure_bones_gitattributes(&bones_dir).with_context(|| {
        format!(
            "Failed to ensure {}",
            bones_dir.join(".gitattributes").display()
        )
    })?;

    remove_legacy_root_gitattributes_entry(project_root)
        .context("Failed to migrate legacy root .gitattributes entry")?;

    if args.hooks {
        git::hooks::install_hooks(project_root)?;
    }

    match output {
        OutputMode::Json => {
            let val = serde_json::json!({
                "status": "ok",
                "message": "initialized .bones project structure",
                "paths": {
                    "active_shard": format!(".bones/events/{shard_name}"),
                    "config": ".bones/config.toml",
                    "gitignore": ".bones/.gitignore",
                    "gitattributes": ".bones/.gitattributes"
                },
                "next_steps": [
                    "export AGENT=your-name",
                    "bn create --title \"My first item\""
                ]
            });
            println!("{}", serde_json::to_string_pretty(&val)?);
        }
        OutputMode::Text => {
            println!(
                "init status=ok active_shard=.bones/events/{shard_name} config=.bones/config.toml"
            );
            println!("hint export AGENT=your-name");
            println!("hint bn create --title \"My first item\"");
        }
        OutputMode::Pretty => {
            let stdout = std::io::stdout();
            let mut w = stdout.lock();
            pretty_section(&mut w, "Initialization Complete")?;
            pretty_kv(&mut w, "Status", "initialized .bones project structure")?;
            pretty_kv(
                &mut w,
                "Active shard",
                format!(".bones/events/{shard_name}"),
            )?;
            pretty_kv(&mut w, "Config", ".bones/config.toml")?;
            println!();
            pretty_section(&mut w, "Next Steps")?;
            println!("- export AGENT=your-name");
            println!("- export BONES_AGENT=your-name");
            println!("- bn create --title \"My first item\"");
        }
    }

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
        run_init(&args, OutputMode::Pretty, &root).expect("init should succeed");

        assert!(root.join(".bones").is_dir());
        assert!(root.join(".bones/events").is_dir());
        assert!(root.join(".bones/config.toml").is_file());
        assert!(root.join(".bones/.gitignore").is_file());
        assert!(root.join(".bones/.gitattributes").is_file());

        // Events dir must have the shard file
        let count = fs::read_dir(root.join(".bones/events"))
            .expect("events dir readable")
            .filter_map(|e| e.ok())
            .count();
        assert!(count >= 1, "events dir should have at least one shard file");

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn init_migrates_legacy_root_gitattributes_entry() {
        let root = make_temp_dir("legacy-root-gitattributes");
        fs::write(
            root.join(".gitattributes"),
            ".bones/events merge=union\n*.png binary\n",
        )
        .expect("seed root gitattributes");

        run_init(
            &InitArgs {
                force: false,
                hooks: false,
            },
            OutputMode::Pretty,
            &root,
        )
        .expect("init should succeed");

        let root_content = fs::read_to_string(root.join(".gitattributes"))
            .expect("root .gitattributes should remain with non-legacy entries");
        assert!(!root_content.contains(".bones/events merge=union"));
        assert!(root_content.contains("*.png binary"));

        let bones_content = fs::read_to_string(root.join(".bones/.gitattributes"))
            .expect(".bones/.gitattributes readable");
        assert!(bones_content.contains("events/** merge=union"));

        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn reinit_without_force_fails() {
        let root = make_temp_dir("no-force");
        let args = InitArgs {
            force: false,
            hooks: false,
        };
        run_init(&args, OutputMode::Pretty, &root).expect("first init should succeed");

        let result = run_init(&args, OutputMode::Pretty, &root);
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
            OutputMode::Pretty,
            &root,
        )
        .expect("first init should succeed");
        run_init(
            &InitArgs {
                force: true,
                hooks: false,
            },
            OutputMode::Pretty,
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
            OutputMode::Pretty,
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
            OutputMode::Pretty,
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
        assert!(content.contains("itc/"), "must ignore itc/");
        assert!(content.contains("lock"), "must ignore lock");
        assert!(content.contains("bones.db-wal"), "must ignore bones.db-wal");
        assert!(content.contains("bones.db-shm"), "must ignore bones.db-shm");

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
            OutputMode::Pretty,
            &root,
        )
        .expect("init should succeed");

        // Read the active shard directly
        let expected_name = Utc::now().format("%Y-%m.events").to_string();
        let shard_path = root.join(".bones/events").join(&expected_name);
        let content = fs::read_to_string(&shard_path).expect("shard readable");
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
            OutputMode::Pretty,
            &root,
        )
        .expect("init should succeed");

        let expected_name = Utc::now().format("%Y-%m.events").to_string();
        let shard_path = root.join(".bones/events").join(&expected_name);
        assert!(
            shard_path.is_file(),
            "shard file {expected_name} should exist"
        );

        let _ = fs::remove_dir_all(&root);
    }
}
