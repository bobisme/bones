use anyhow::{Context, Result, anyhow, bail};
use bones_core::config::{EffectiveConfig, resolve_config};
use clap::{Args, Subcommand, ValueEnum};
use std::path::{Path, PathBuf};
use toml::Value;

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

    /// Emit JSON output
    #[arg(long)]
    json: bool,
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

pub fn run_config(args: &ConfigArgs, project_root: &Path, cli_json: bool) -> Result<()> {
    match &args.command {
        ConfigCommand::Show(show) => run_show(show, project_root, cli_json),
        ConfigCommand::Set(set) => run_set(set, project_root),
        ConfigCommand::Unset(unset) => run_unset(unset, project_root),
    }
}

fn run_show(args: &ShowArgs, project_root: &Path, cli_json: bool) -> Result<()> {
    if args.project {
        let value = load_toml_table(&project_config_path(project_root))?;
        print_toml_or_json(&value, args.json || cli_json);
        return Ok(());
    }

    if args.user {
        let value = load_toml_table(&user_config_path()?)?;
        print_toml_or_json(&value, args.json || cli_json);
        return Ok(());
    }

    let effective = resolve_config(project_root, cli_json)?;
    print_effective(&effective, args.json || cli_json)?;
    Ok(())
}

fn run_set(args: &SetArgs, project_root: &Path) -> Result<()> {
    let path = match args.scope {
        ConfigScope::Project => project_config_path(project_root),
        ConfigScope::User => user_config_path()?,
    };

    let mut value = load_toml_table(&path)?;
    apply_set(&mut value, args.scope, &args.key, &args.value)?;
    write_toml_table(&path, &value)?;
    println!("Set {} in {} config", args.key, scope_label(args.scope));
    Ok(())
}

fn run_unset(args: &UnsetArgs, project_root: &Path) -> Result<()> {
    let path = match args.scope {
        ConfigScope::Project => project_config_path(project_root),
        ConfigScope::User => user_config_path()?,
    };

    let mut value = load_toml_table(&path)?;
    apply_unset(&mut value, args.scope, &args.key)?;
    write_toml_table(&path, &value)?;
    println!("Unset {} in {} config", args.key, scope_label(args.scope));
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

fn print_toml_or_json(value: &Value, as_json: bool) {
    if as_json {
        match serde_json::to_string_pretty(value) {
            Ok(json) => println!("{json}"),
            Err(_) => println!("{{}}"),
        }
    } else {
        println!("{}", toml::to_string_pretty(value).unwrap_or_default());
    }
}

fn print_effective(value: &EffectiveConfig, as_json: bool) -> Result<()> {
    if as_json {
        println!("{}", serde_json::to_string_pretty(value)?);
    } else {
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
        if let Some(output) = &value.user.output {
            println!("output = \"{output}\"");
        }
    }

    Ok(())
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
