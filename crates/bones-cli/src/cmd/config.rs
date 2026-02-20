use anyhow::{Context, Result, anyhow, bail};
use bones_core::config::{EffectiveConfig, resolve_config};
use clap::{Args, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use toml::Value;

use crate::output::OutputMode;

#[derive(Args, Debug)]
pub struct ConfigArgs {
    #[command(subcommand)]
    command: ConfigCommand,
}

#[derive(Subcommand, Debug)]
enum ConfigCommand {
    /// Show resolved or raw configuration
    Show(ShowArgs),
    /// Set a configuration key in project or user scope
    Set(SetArgs),
    /// Unset a configuration key in project or user scope
    Unset(UnsetArgs),
}

#[derive(Args, Debug)]
struct ShowArgs {
    /// Show raw project config only
    #[arg(long, conflicts_with = "user")]
    project: bool,

    /// Show raw user config only
    #[arg(long)]
    user: bool,
}

#[derive(Args, Debug)]
struct SetArgs {
    /// Scope to mutate
    #[arg(long, default_value = "project")]
    scope: ConfigScope,

    /// Dot path key (e.g. search.semantic, user.output)
    key: String,

    /// New value
    value: String,
}

#[derive(Args, Debug)]
struct UnsetArgs {
    /// Scope to mutate
    #[arg(long, default_value = "project")]
    scope: ConfigScope,

    /// Dot path key (e.g. search.semantic, user.output)
    key: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, ValueEnum)]
enum ConfigScope {
    Project,
    User,
}

pub fn run_config(args: &ConfigArgs, project_root: &Path, output: OutputMode) -> Result<()> {
    match &args.command {
        ConfigCommand::Show(show) => run_show(show, project_root, output),
        ConfigCommand::Set(set) => run_set(set, project_root, output),
        ConfigCommand::Unset(unset) => run_unset(unset, project_root, output),
    }
}

fn run_show(args: &ShowArgs, project_root: &Path, output: OutputMode) -> Result<()> {
    if args.project {
        let value = load_toml_table(&project_config_path(project_root))?;
        print_toml_or_json(&value, output);
        return Ok(());
    }

    if args.user {
        let value = load_toml_table(&user_config_path()?)?;
        print_toml_or_json(&value, output);
        return Ok(());
    }

    let effective = resolve_config(project_root, output.is_json())?;
    print_effective(&effective, output)?;
    Ok(())
}

fn run_set(args: &SetArgs, project_root: &Path, output: OutputMode) -> Result<()> {
    let path = match args.scope {
        ConfigScope::Project => project_config_path(project_root),
        ConfigScope::User => user_config_path()?,
    };

    let mut value = load_toml_table(&path)?;
    apply_set(&mut value, args.scope, &args.key, &args.value)?;
    write_toml_table(&path, &value)?;
    render_mutation(output, "set", scope_label(args.scope), &args.key)?;
    Ok(())
}

fn run_unset(args: &UnsetArgs, project_root: &Path, output: OutputMode) -> Result<()> {
    let path = match args.scope {
        ConfigScope::Project => project_config_path(project_root),
        ConfigScope::User => user_config_path()?,
    };

    let mut value = load_toml_table(&path)?;
    apply_unset(&mut value, args.scope, &args.key)?;
    write_toml_table(&path, &value)?;
    render_mutation(output, "unset", scope_label(args.scope), &args.key)?;
    Ok(())
}

fn apply_set(root: &mut Value, scope: ConfigScope, key: &str, raw: &str) -> Result<()> {
    let parsed = parse_value(scope, key, raw)?;
    let (section, leaf) = split_known_key(scope, key)?;

    let table = root
        .as_table_mut()
        .ok_or_else(|| anyhow!("Config root must be a TOML table"))?;

    let section_entry = table
        .entry(section.to_string())
        .or_insert_with(|| Value::Table(toml::map::Map::new()));

    let section_table = section_entry
        .as_table_mut()
        .ok_or_else(|| anyhow!("Section {section} must be a TOML table"))?;

    section_table.insert(leaf.to_string(), parsed);
    Ok(())
}

fn apply_unset(root: &mut Value, scope: ConfigScope, key: &str) -> Result<()> {
    let (section, leaf) = split_known_key(scope, key)?;
    let table = root
        .as_table_mut()
        .ok_or_else(|| anyhow!("Config root must be a TOML table"))?;

    if let Some(section_entry) = table.get_mut(section)
        && let Some(section_table) = section_entry.as_table_mut()
    {
        section_table.remove(leaf);
        if section_table.is_empty() {
            table.remove(section);
        }
    }

    Ok(())
}

fn split_known_key(scope: ConfigScope, key: &str) -> Result<(&str, &str)> {
    let (section, leaf) = key
        .split_once('.')
        .ok_or_else(|| anyhow!("Key must use section.key format"))?;

    let valid = match scope {
        ConfigScope::Project => matches!(
            (section, leaf),
            ("goals", "auto_complete")
                | ("search", "semantic")
                | ("search", "model")
                | ("search", "duplicate_threshold")
                | ("search", "related_threshold")
                | ("search", "warn_on_create")
                | ("triage", "feedback_learning")
                | ("done", "require_reason")
        ),
        ConfigScope::User => matches!((section, leaf), ("user", "output")),
    };

    if valid {
        Ok((section, leaf))
    } else {
        bail!("Unsupported key `{key}` for {} scope", scope_label(scope));
    }
}

fn parse_value(scope: ConfigScope, key: &str, raw: &str) -> Result<Value> {
    let (section, leaf) = split_known_key(scope, key)?;

    match (section, leaf) {
        ("search", "model") | ("user", "output") => Ok(Value::String(raw.to_string())),
        ("search", "duplicate_threshold") | ("search", "related_threshold") => {
            let number: f64 = raw
                .parse()
                .with_context(|| format!("{key} expects a number"))?;
            let toml_num = toml::Value::try_from(number)
                .map_err(|_| anyhow!("{key} could not be represented as TOML number"))?;
            Ok(toml_num)
        }
        _ => {
            let value: bool = raw
                .parse()
                .with_context(|| format!("{key} expects true or false"))?;
            Ok(Value::Boolean(value))
        }
    }
}

fn load_toml_table(path: &Path) -> Result<Value> {
    if !path.exists() {
        return Ok(Value::Table(toml::map::Map::new()));
    }

    let content = std::fs::read_to_string(path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    let value: Value =
        toml::from_str(&content).with_context(|| format!("Failed to parse {}", path.display()))?;

    if !value.is_table() {
        bail!("{} must contain a top-level TOML table", path.display());
    }

    Ok(value)
}

fn write_toml_table(path: &Path, value: &Value) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("Failed to create {}", parent.display()))?;
    }

    let serialized = toml::to_string_pretty(value)?;
    std::fs::write(path, serialized).with_context(|| format!("Failed to write {}", path.display()))
}

fn print_toml_or_json(value: &Value, output: OutputMode) {
    match output {
        OutputMode::Json => match serde_json::to_string_pretty(value) {
            Ok(json) => println!("{json}"),
            Err(_) => println!("{{}}"),
        },
        OutputMode::Text | OutputMode::Pretty => {
            println!("{}", toml::to_string_pretty(value).unwrap_or_default());
        }
    }
}

fn print_effective(value: &EffectiveConfig, output: OutputMode) -> Result<()> {
    match output {
        OutputMode::Json => {
            println!("{}", serde_json::to_string_pretty(value)?);
        }
        OutputMode::Text => {
            println!("resolved_output={}", value.resolved_output);
            println!("goals.auto_complete={}", value.project.goals.auto_complete);
            println!("search.semantic={}", value.project.search.semantic);
            println!("search.model={}", value.project.search.model);
            println!(
                "search.duplicate_threshold={}",
                value.project.search.duplicate_threshold
            );
            println!(
                "search.related_threshold={}",
                value.project.search.related_threshold
            );
            println!(
                "search.warn_on_create={}",
                value.project.search.warn_on_create
            );
            println!(
                "triage.feedback_learning={}",
                value.project.triage.feedback_learning
            );
            println!("done.require_reason={}", value.project.done.require_reason);
            if let Some(out) = &value.user.output {
                println!("user.output={out}");
            }
        }
        OutputMode::Pretty => {
            println!("resolved_output = \"{}\"", value.resolved_output);
            println!();
            println!("[goals]");
            println!("auto_complete = {}", value.project.goals.auto_complete);
            println!();
            println!("[search]");
            println!("semantic = {}", value.project.search.semantic);
            println!("model = \"{}\"", value.project.search.model);
            println!(
                "duplicate_threshold = {}",
                value.project.search.duplicate_threshold
            );
            println!(
                "related_threshold = {}",
                value.project.search.related_threshold
            );
            println!("warn_on_create = {}", value.project.search.warn_on_create);
            println!();
            println!("[triage]");
            println!(
                "feedback_learning = {}",
                value.project.triage.feedback_learning
            );
            println!();
            println!("[done]");
            println!("require_reason = {}", value.project.done.require_reason);
            println!();
            println!("[user]");
            if let Some(out) = &value.user.output {
                println!("output = \"{out}\"");
            }
        }
    }

    Ok(())
}

fn render_mutation(output: OutputMode, action: &str, scope: &str, key: &str) -> Result<()> {
    match output {
        OutputMode::Json => {
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "ok": true,
                    "action": action,
                    "scope": scope,
                    "key": key,
                }))?
            );
        }
        OutputMode::Text => {
            println!("ok=true action={action} scope={scope} key={key}");
        }
        OutputMode::Pretty => {
            println!("{} {} in {} config", action_to_title(action), key, scope);
        }
    }
    Ok(())
}

fn action_to_title(action: &str) -> &'static str {
    match action {
        "set" => "Set",
        "unset" => "Unset",
        _ => "Updated",
    }
}

fn project_config_path(project_root: &Path) -> PathBuf {
    project_root.join(".bones/config.toml")
}

fn user_config_path() -> Result<PathBuf> {
    let config_dir =
        dirs::config_dir().ok_or_else(|| anyhow!("Unable to resolve user config directory"))?;
    Ok(config_dir.join("bones/config.toml"))
}

const fn scope_label(scope: ConfigScope) -> &'static str {
    match scope {
        ConfigScope::Project => "project",
        ConfigScope::User => "user",
    }
}
