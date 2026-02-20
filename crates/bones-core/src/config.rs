use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::env;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectConfig {
    #[serde(default)]
    pub goals: GoalConfig,
    #[serde(default)]
    pub search: SearchConfig,
    #[serde(default)]
    pub triage: TriageConfig,
    #[serde(default)]
    pub done: DoneConfig,
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            goals: GoalConfig::default(),
            search: SearchConfig::default(),
            triage: TriageConfig::default(),
            done: DoneConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GoalConfig {
    #[serde(default = "default_true")]
    pub auto_complete: bool,
}

impl Default for GoalConfig {
    fn default() -> Self {
        Self {
            auto_complete: default_true(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SearchConfig {
    #[serde(default = "default_true")]
    pub semantic: bool,
    #[serde(default = "default_search_model")]
    pub model: String,
    #[serde(default = "default_duplicate_threshold")]
    pub duplicate_threshold: f64,
    #[serde(default = "default_related_threshold")]
    pub related_threshold: f64,
    #[serde(default = "default_true")]
    pub warn_on_create: bool,
}

impl Default for SearchConfig {
    fn default() -> Self {
        Self {
            semantic: default_true(),
            model: default_search_model(),
            duplicate_threshold: default_duplicate_threshold(),
            related_threshold: default_related_threshold(),
            warn_on_create: default_true(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TriageConfig {
    #[serde(default = "default_true")]
    pub feedback_learning: bool,
}

impl Default for TriageConfig {
    fn default() -> Self {
        Self {
            feedback_learning: default_true(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DoneConfig {
    #[serde(default)]
    pub require_reason: bool,
}

impl Default for DoneConfig {
    fn default() -> Self {
        Self {
            require_reason: false,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoConfig {
    pub name: String,
    pub path: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    #[serde(default)]
    pub output: Option<String>,
    #[serde(default)]
    pub repos: Vec<RepoConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EffectiveConfig {
    pub project: ProjectConfig,
    pub user: UserConfig,
    pub resolved_output: String,
}

pub fn load_project_config(project_root: &Path) -> Result<ProjectConfig> {
    let path = project_root.join(".bones/config.toml");
    if !path.exists() {
        return Ok(ProjectConfig::default());
    }

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    toml::from_str::<ProjectConfig>(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))
}

pub fn load_user_config() -> Result<UserConfig> {
    let Some(config_dir) = dirs::config_dir() else {
        return Ok(UserConfig::default());
    };

    let path = config_dir.join("bones/config.toml");
    if !path.exists() {
        return Ok(UserConfig::default());
    }

    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("Failed to read {}", path.display()))?;

    toml::from_str::<UserConfig>(&content)
        .with_context(|| format!("Failed to parse {}", path.display()))
}

pub fn discover_repos(config: &UserConfig) -> Vec<(String, PathBuf, bool)> {
    config
        .repos
        .iter()
        .map(|repo_config| {
            let path = &repo_config.path;
            let bones_dir = path.join(".bones");

            let available = path.exists() && bones_dir.exists();

            if !available {
                if !path.exists() {
                    eprintln!(
                        "Warning: Repository '{}' configured at {} does not exist",
                        repo_config.name,
                        path.display()
                    );
                } else {
                    eprintln!(
                        "Warning: Repository '{}' at {} does not contain .bones/ directory",
                        repo_config.name,
                        path.display()
                    );
                }
            }

            (repo_config.name.clone(), path.clone(), available)
        })
        .collect()
}

pub fn resolve_config(project_root: &Path, cli_json: bool) -> Result<EffectiveConfig> {
    let project = load_project_config(project_root)?;
    let user = load_user_config()?;

    let env_format = env::var("FORMAT").ok();
    let resolved_output = resolve_output(cli_json, user.output.clone(), env_format)?;

    Ok(EffectiveConfig {
        project,
        user,
        resolved_output,
    })
}

fn resolve_output(
    cli_json: bool,
    user_output: Option<String>,
    env_format: Option<String>,
) -> Result<String> {
    fn normalize_output_mode(raw: &str) -> Option<&'static str> {
        match raw.trim().to_ascii_lowercase().as_str() {
            // canonical values
            "pretty" => Some("pretty"),
            "text" => Some("text"),
            "json" => Some("json"),
            // legacy compatibility
            "human" => Some("pretty"),
            "table" => Some("text"),
            _ => None,
        }
    }

    if cli_json {
        return Ok("json".to_string());
    }

    if let Some(mode) = env_format.as_deref().and_then(normalize_output_mode) {
        return Ok(mode.to_string());
    }

    if let Some(mode) = user_output.as_deref().and_then(normalize_output_mode) {
        return Ok(mode.to_string());
    }

    if std::io::stdout().is_terminal() {
        Ok("pretty".to_string())
    } else {
        Ok("text".to_string())
    }
}

const fn default_true() -> bool {
    true
}

fn default_search_model() -> String {
    "minilm-l6-v2-int8".to_string()
}

const fn default_duplicate_threshold() -> f64 {
    0.85
}

const fn default_related_threshold() -> f64 {
    0.65
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn make_temp_dir(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!("bones-config-test-{label}-{id}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp dir must be created");
        dir
    }

    #[test]
    fn missing_project_config_uses_defaults() {
        let root = make_temp_dir("project-default");
        let cfg = load_project_config(&root).expect("load should succeed");
        assert!(cfg.goals.auto_complete);
        assert!(cfg.search.semantic);
        assert_eq!(cfg.search.model, "minilm-l6-v2-int8");
        assert!(cfg.triage.feedback_learning);
        assert!(!cfg.done.require_reason);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn cli_json_overrides_env_and_config() {
        let output = resolve_output(true, Some("pretty".to_string()), Some("text".to_string()))
            .expect("resolve should succeed");
        assert_eq!(output, "json");
    }

    #[test]
    fn legacy_aliases_are_normalized() {
        let pretty = resolve_output(false, Some("table".to_string()), Some("human".to_string()))
            .expect("resolve should succeed");
        assert_eq!(pretty, "pretty");

        let text = resolve_output(false, Some("human".to_string()), Some("table".to_string()))
            .expect("resolve should succeed");
        assert_eq!(text, "text");
    }

    #[test]
    fn user_config_parses_repos_list() {
        let temp_dir = make_temp_dir("user-config-repos");
        let config_dir = temp_dir.join("config/bones");
        std::fs::create_dir_all(&config_dir).expect("create config dir");

        let config_content = r#"
output = "json"

[[repos]]
name = "backend"
path = "/home/alice/src/backend"

[[repos]]
name = "frontend"
path = "/home/alice/src/frontend"
"#;

        let config_file = config_dir.join("config.toml");
        std::fs::write(&config_file, config_content).expect("write config");

        let content = std::fs::read_to_string(&config_file).expect("read back");
        let cfg: UserConfig = toml::from_str(&content).expect("parse");

        assert_eq!(cfg.output, Some("json".to_string()));
        assert_eq!(cfg.repos.len(), 2);
        assert_eq!(cfg.repos[0].name, "backend");
        assert_eq!(cfg.repos[0].path, PathBuf::from("/home/alice/src/backend"));
        assert_eq!(cfg.repos[1].name, "frontend");
        assert_eq!(cfg.repos[1].path, PathBuf::from("/home/alice/src/frontend"));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn discover_repos_validates_bones_directory() {
        let temp_dir = make_temp_dir("discover-valid");

        // Create first repo with .bones/
        let repo1_path = temp_dir.join("repo1");
        std::fs::create_dir_all(repo1_path.join(".bones")).expect("create repo1/.bones");

        // Create second repo with .bones/
        let repo2_path = temp_dir.join("repo2");
        std::fs::create_dir_all(repo2_path.join(".bones")).expect("create repo2/.bones");

        let config = UserConfig {
            output: None,
            repos: vec![
                RepoConfig {
                    name: "repo1".to_string(),
                    path: repo1_path.clone(),
                },
                RepoConfig {
                    name: "repo2".to_string(),
                    path: repo2_path.clone(),
                },
            ],
        };

        let discovered = discover_repos(&config);

        assert_eq!(discovered.len(), 2);
        assert_eq!(discovered[0], ("repo1".to_string(), repo1_path, true));
        assert_eq!(discovered[1], ("repo2".to_string(), repo2_path, true));

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn discover_repos_handles_missing_directories() {
        let temp_dir = make_temp_dir("discover-missing");
        let nonexistent = temp_dir.join("nonexistent");

        let config = UserConfig {
            output: None,
            repos: vec![RepoConfig {
                name: "missing".to_string(),
                path: nonexistent.clone(),
            }],
        };

        let discovered = discover_repos(&config);

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].0, "missing");
        assert_eq!(discovered[0].1, nonexistent);
        assert!(!discovered[0].2); // available = false

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn discover_repos_handles_missing_bones_directory() {
        let temp_dir = make_temp_dir("discover-no-bones");
        let repo_path = temp_dir.join("repo");
        std::fs::create_dir(&repo_path).expect("create repo dir");
        // Note: not creating .bones/ subdirectory

        let config = UserConfig {
            output: None,
            repos: vec![RepoConfig {
                name: "incomplete".to_string(),
                path: repo_path.clone(),
            }],
        };

        let discovered = discover_repos(&config);

        assert_eq!(discovered.len(), 1);
        assert_eq!(discovered[0].0, "incomplete");
        assert_eq!(discovered[0].1, repo_path);
        assert!(!discovered[0].2); // available = false

        let _ = std::fs::remove_dir_all(&temp_dir);
    }

    #[test]
    fn discover_repos_empty_config() {
        let config = UserConfig {
            output: None,
            repos: vec![],
        };

        let discovered = discover_repos(&config);
        assert_eq!(discovered.len(), 0);
    }
}
