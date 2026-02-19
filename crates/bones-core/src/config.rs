use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::env;
use std::path::Path;

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

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct UserConfig {
    #[serde(default)]
    pub output: Option<String>,
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

pub fn resolve_config(project_root: &Path, cli_json: bool) -> Result<EffectiveConfig> {
    let project = load_project_config(project_root)?;
    let user = load_user_config()?;

    let env_output = env::var("BONES_OUTPUT").ok();
    let env_format = env::var("BONES_FORMAT").ok();
    let resolved_output = resolve_output(cli_json, user.output.clone(), env_output, env_format)?;

    Ok(EffectiveConfig {
        project,
        user,
        resolved_output,
    })
}

fn resolve_output(
    cli_json: bool,
    user_output: Option<String>,
    env_output: Option<String>,
    env_format: Option<String>,
) -> Result<String> {
    if let (Some(output), Some(format)) = (&env_output, &env_format)
        && output != format
    {
        bail!(
            "Conflicting env vars: BONES_OUTPUT={output} and BONES_FORMAT={format}. Set only one or make them match."
        );
    }

    if cli_json {
        Ok("json".to_string())
    } else if let Some(output) = env_output.or(env_format) {
        Ok(output)
    } else if let Some(output) = user_output {
        Ok(output)
    } else {
        Ok("human".to_string())
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
        let output = resolve_output(
            true,
            Some("human".to_string()),
            Some("table".to_string()),
            None,
        )
        .expect("resolve should succeed");
        assert_eq!(output, "json");
    }

    #[test]
    fn conflicting_output_env_vars_error() {
        let err = resolve_output(
            false,
            None,
            Some("json".to_string()),
            Some("table".to_string()),
        )
        .expect_err("must error on conflict");
        let msg = err.to_string();
        assert!(msg.contains("Conflicting env vars"));
    }
}
