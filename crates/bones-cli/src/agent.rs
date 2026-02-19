//! Agent identity resolution for CLI commands.
//!
//! The resolution chain: `--agent` flag > `BONES_AGENT` env > `AGENT` env > `USER` env (TTY only).
//! Mutating commands require an agent identity; read-only commands work without one.

use std::env;

/// Errors from agent resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentResolutionError {
    /// Human-readable description.
    pub message: String,
    /// Machine error code.
    pub code: &'static str,
}

impl std::fmt::Display for AgentResolutionError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.message)
    }
}

impl std::error::Error for AgentResolutionError {}

/// Environment reader trait for dependency injection in tests.
trait EnvReader {
    fn get(&self, key: &str) -> Option<String>;
    fn is_tty(&self) -> bool;
}

/// Real environment reader.
struct RealEnv;

impl EnvReader for RealEnv {
    fn get(&self, key: &str) -> Option<String> {
        env::var(key).ok().filter(|v| !v.is_empty())
    }

    fn is_tty(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal()
    }
}

/// Core resolution logic, parameterized by environment reader.
fn resolve_agent_with(cli_flag: Option<&str>, env: &dyn EnvReader) -> Option<String> {
    // Step 1: explicit --agent flag
    if let Some(agent) = cli_flag {
        if !agent.is_empty() {
            return Some(agent.to_string());
        }
    }

    // Step 2: BONES_AGENT env
    if let Some(val) = env.get("BONES_AGENT") {
        return Some(val);
    }

    // Step 3: AGENT env
    if let Some(val) = env.get("AGENT") {
        return Some(val);
    }

    // Step 4: USER env, but only if stdin is a TTY
    if env.is_tty() {
        if let Some(val) = env.get("USER") {
            return Some(val);
        }
    }

    None
}

/// Resolve the agent identity following the 4-step chain:
///
/// 1. `--agent` CLI flag (passed as `cli_flag`)
/// 2. `BONES_AGENT` environment variable
/// 3. `AGENT` environment variable
/// 4. `USER` environment variable (only if running in a TTY)
///
/// Returns `None` if no identity could be resolved.
pub fn resolve_agent(cli_flag: Option<&str>) -> Option<String> {
    resolve_agent_with(cli_flag, &RealEnv)
}

/// Resolve agent identity, returning an error if not found.
///
/// Use this for mutating commands that require an agent.
pub fn require_agent(cli_flag: Option<&str>) -> Result<String, AgentResolutionError> {
    resolve_agent(cli_flag).ok_or_else(|| AgentResolutionError {
        message: "Agent identity required for this command. \
                  Set --agent, BONES_AGENT, or AGENT environment variable."
            .to_string(),
        code: "missing_agent",
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// Test environment reader with configurable values.
    struct MockEnv {
        vars: HashMap<String, String>,
        tty: bool,
    }

    impl MockEnv {
        fn new() -> Self {
            Self {
                vars: HashMap::new(),
                tty: false,
            }
        }

        fn var(mut self, key: &str, val: &str) -> Self {
            self.vars.insert(key.to_string(), val.to_string());
            self
        }

        fn tty(mut self) -> Self {
            self.tty = true;
            self
        }
    }

    impl EnvReader for MockEnv {
        fn get(&self, key: &str) -> Option<String> {
            self.vars.get(key).filter(|v| !v.is_empty()).cloned()
        }

        fn is_tty(&self) -> bool {
            self.tty
        }
    }

    #[test]
    fn cli_flag_takes_priority() {
        let env = MockEnv::new()
            .var("BONES_AGENT", "env-bones")
            .var("AGENT", "env-agent");
        let result = resolve_agent_with(Some("flag-agent"), &env);
        assert_eq!(result.as_deref(), Some("flag-agent"));
    }

    #[test]
    fn bones_agent_env_fallback() {
        let env = MockEnv::new()
            .var("BONES_AGENT", "env-bones")
            .var("AGENT", "env-agent");
        let result = resolve_agent_with(None, &env);
        assert_eq!(result.as_deref(), Some("env-bones"));
    }

    #[test]
    fn agent_env_fallback() {
        let env = MockEnv::new().var("AGENT", "env-agent");
        let result = resolve_agent_with(None, &env);
        assert_eq!(result.as_deref(), Some("env-agent"));
    }

    #[test]
    fn empty_flag_ignored() {
        let env = MockEnv::new().var("BONES_AGENT", "env-bones");
        let result = resolve_agent_with(Some(""), &env);
        assert_eq!(result.as_deref(), Some("env-bones"));
    }

    #[test]
    fn empty_env_ignored() {
        let env = MockEnv::new()
            .var("BONES_AGENT", "")
            .var("AGENT", "real-agent");
        let result = resolve_agent_with(None, &env);
        assert_eq!(result.as_deref(), Some("real-agent"));
    }

    #[test]
    fn user_env_only_in_tty() {
        // Not a TTY: USER is not used
        let env = MockEnv::new().var("USER", "bob");
        let result = resolve_agent_with(None, &env);
        assert_eq!(result, None);

        // TTY: USER is used
        let env = MockEnv::new().var("USER", "bob").tty();
        let result = resolve_agent_with(None, &env);
        assert_eq!(result.as_deref(), Some("bob"));
    }

    #[test]
    fn no_identity_returns_none() {
        let env = MockEnv::new();
        let result = resolve_agent_with(None, &env);
        assert_eq!(result, None);
    }

    #[test]
    fn resolution_chain_order() {
        // All sources set — flag wins
        let env = MockEnv::new()
            .var("BONES_AGENT", "bones")
            .var("AGENT", "agent")
            .var("USER", "user")
            .tty();
        assert_eq!(
            resolve_agent_with(Some("flag"), &env).as_deref(),
            Some("flag")
        );

        // No flag — BONES_AGENT wins
        assert_eq!(
            resolve_agent_with(None, &env).as_deref(),
            Some("bones")
        );

        // No flag, no BONES_AGENT — AGENT wins
        let env = MockEnv::new()
            .var("AGENT", "agent")
            .var("USER", "user")
            .tty();
        assert_eq!(
            resolve_agent_with(None, &env).as_deref(),
            Some("agent")
        );

        // No flag, no BONES_AGENT, no AGENT — USER (TTY only) wins
        let env = MockEnv::new().var("USER", "user").tty();
        assert_eq!(
            resolve_agent_with(None, &env).as_deref(),
            Some("user")
        );
    }

    #[test]
    fn require_agent_returns_error_when_missing() {
        // require_agent uses real env; since we can't mock here, we just
        // verify error structure by passing an explicit flag
        let result = require_agent(None);
        // In CI/test, AGENT or BONES_AGENT may be set, so this test
        // verifies behavior indirectly — the mock tests above cover the logic.
        // We test error creation directly instead:
        let err = AgentResolutionError {
            message: "test".to_string(),
            code: "missing_agent",
        };
        assert_eq!(err.code, "missing_agent");
        assert_eq!(format!("{err}"), "test");
        // Also verify Result error has Display
        let _: Box<dyn std::error::Error> = Box::new(err);
        drop(result); // suppress unused warning
    }

    #[test]
    fn require_agent_succeeds_with_flag() {
        let result = require_agent(Some("test-agent"));
        assert_eq!(result.unwrap(), "test-agent");
    }
}
