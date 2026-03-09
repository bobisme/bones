use crate::output::CliError;
use bones_core::model::item::{Kind, Size, State};

pub const MAX_TITLE_LEN: usize = 200;
pub const MAX_LABEL_LEN: usize = 50;
pub const MAX_AGENT_LEN: usize = 64;

#[derive(Debug, Clone)]
pub struct ValidationError {
    pub field: &'static str,
    pub value: String,
    pub reason: String,
    pub suggestion: String,
    pub code: &'static str,
}

impl ValidationError {
    pub fn new(
        field: &'static str,
        value: impl Into<String>,
        reason: impl Into<String>,
        suggestion: impl Into<String>,
        code: &'static str,
    ) -> Self {
        Self {
            field,
            value: value.into(),
            reason: reason.into(),
            suggestion: suggestion.into(),
            code,
        }
    }

    pub fn to_cli_error(&self) -> CliError {
        CliError::with_details(
            format!("invalid {} '{}': {}", self.field, self.value, self.reason),
            self.suggestion.clone(),
            self.code,
        )
    }
}

pub fn validate_title(s: &str) -> Result<(), ValidationError> {
    if s.trim() != s {
        return Err(ValidationError::new(
            "title",
            s,
            "must not start or end with whitespace",
            "trim leading/trailing whitespace from --title",
            "invalid_title",
        ));
    }
    if s.is_empty() {
        return Err(ValidationError::new(
            "title",
            s,
            "must not be empty",
            "provide a non-empty --title",
            "invalid_title",
        ));
    }
    if s.chars().count() > MAX_TITLE_LEN {
        return Err(ValidationError::new(
            "title",
            s,
            format!("must be <= {MAX_TITLE_LEN} characters"),
            "shorten the title",
            "invalid_title",
        ));
    }
    if s.chars().any(char::is_control) {
        return Err(ValidationError::new(
            "title",
            s,
            "must not contain control characters",
            "remove control characters from the title",
            "invalid_title",
        ));
    }
    Ok(())
}

pub fn validate_item_id(s: &str) -> Result<(), ValidationError> {
    let value = s.trim();
    if value.is_empty() {
        return Err(ValidationError::new(
            "item_id",
            s,
            "must not be empty",
            "use an ID like bn-abc123 or a partial like abc123",
            "invalid_item_id",
        ));
    }

    // Accept any terseid-prefixed ID (e.g. bn-xxx, bd-xxx from beads migration).
    if let Some((prefix, rest)) = value.split_once('-') {
        let valid_prefix = !prefix.is_empty() && prefix.chars().all(|c| c.is_ascii_lowercase());
        if valid_prefix && is_valid_item_id_segments(rest) {
            return Ok(());
        }
    } else if value.chars().all(|c| c.is_ascii_alphanumeric()) {
        // Allow partial IDs for commands that support resolution.
        return Ok(());
    }

    Err(ValidationError::new(
        "item_id",
        s,
        "must match bn-[a-z0-9]+(.[0-9]+)* or be an alphanumeric partial ID",
        "use IDs like bn-abc123, bn-abc123.1, or partial abc123",
        "invalid_item_id",
    ))
}

fn is_valid_item_id_segments(rest: &str) -> bool {
    let mut parts = rest.split('.');
    let Some(head) = parts.next() else {
        return false;
    };
    if head.is_empty()
        || !head
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit())
    {
        return false;
    }

    for seg in parts {
        if seg.is_empty() || !seg.chars().all(|c| c.is_ascii_digit()) {
            return false;
        }
    }

    true
}

pub fn validate_label(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::new(
            "label",
            s,
            "must not be empty",
            "provide a non-empty label",
            "invalid_label",
        ));
    }
    if s.chars().count() > MAX_LABEL_LEN {
        return Err(ValidationError::new(
            "label",
            s,
            format!("must be <= {MAX_LABEL_LEN} characters"),
            "shorten the label",
            "invalid_label",
        ));
    }

    let mut chars = s.chars();
    let first = chars.next().unwrap();
    if !first.is_ascii_alphanumeric() {
        return Err(ValidationError::new(
            "label",
            s,
            "must start with an ASCII letter or number",
            "start the label with [a-zA-Z0-9]",
            "invalid_label",
        ));
    }

    if !chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(ValidationError::new(
            "label",
            s,
            "may only contain ASCII letters, numbers, '-' or '_'",
            "remove spaces or punctuation from the label",
            "invalid_label",
        ));
    }

    Ok(())
}

pub fn validate_agent(s: &str) -> Result<(), ValidationError> {
    if s.is_empty() {
        return Err(ValidationError::new(
            "agent",
            s,
            "must not be empty",
            "set --agent or BONES_AGENT/AGENT",
            "invalid_agent",
        ));
    }
    if s.chars().count() > MAX_AGENT_LEN {
        return Err(ValidationError::new(
            "agent",
            s,
            format!("must be <= {MAX_AGENT_LEN} characters"),
            "use a shorter agent identifier",
            "invalid_agent",
        ));
    }
    if s.chars().any(char::is_whitespace) {
        return Err(ValidationError::new(
            "agent",
            s,
            "must not contain whitespace",
            "remove spaces and tabs from the agent identifier",
            "invalid_agent",
        ));
    }
    Ok(())
}

pub fn validate_size(s: &str) -> Result<Size, ValidationError> {
    s.parse().map_err(|_| {
        ValidationError::new(
            "size",
            s,
            "expected one of xs, s, m, l, xl",
            "use --size s, --size m, etc.",
            "invalid_size",
        )
    })
}

pub fn validate_state(s: &str) -> Result<State, ValidationError> {
    s.parse().map_err(|_| {
        ValidationError::new(
            "state",
            s,
            "expected one of open, doing, done, archived",
            "use --state open|doing|done|archived",
            "invalid_state",
        )
    })
}

pub fn validate_kind(s: &str) -> Result<Kind, ValidationError> {
    s.parse().map_err(|_| {
        ValidationError::new(
            "kind",
            s,
            "expected one of task, goal, bug",
            "use --kind task|goal|bug",
            "invalid_kind",
        )
    })
}

pub fn detect_secret_kind(s: &str) -> Option<&'static str> {
    if s.contains("-----BEGIN ") && s.contains(" PRIVATE KEY-----") {
        return Some("private_key_block");
    }

    if has_prefixed_run(s, "ghp_", 30) || has_prefixed_run(s, "github_pat_", 20) {
        return Some("github_token");
    }

    if has_aws_access_key_pattern(s) {
        return Some("aws_access_key_id");
    }

    None
}

pub fn validate_no_secrets(field: &'static str, s: &str) -> Result<(), ValidationError> {
    if let Some(kind) = detect_secret_kind(s) {
        return Err(ValidationError::new(
            field,
            "<redacted>",
            format!("contains a high-confidence secret pattern ({kind})"),
            "remove credentials from text or pass --allow-secret to override intentionally",
            "secret_detected",
        ));
    }
    Ok(())
}

fn has_prefixed_run(s: &str, prefix: &str, min_len: usize) -> bool {
    let mut start = 0;
    while let Some(idx) = s[start..].find(prefix) {
        let abs = start + idx + prefix.len();
        let run = s[abs..]
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
            .count();
        if run >= min_len {
            return true;
        }
        start = abs;
    }
    false
}

fn has_aws_access_key_pattern(s: &str) -> bool {
    let bytes = s.as_bytes();
    for i in 0..bytes.len().saturating_sub(19) {
        if &bytes[i..i + 4] != b"AKIA" {
            continue;
        }
        let tail = &bytes[i + 4..i + 20];
        if tail
            .iter()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit())
        {
            return true;
        }
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn valid_ids() {
        assert!(validate_item_id("bn-abc123").is_ok());
        assert!(validate_item_id("bn-abc123.1").is_ok());
        assert!(validate_item_id("abc123").is_ok());
        // Migrated beads IDs use bd- prefix
        assert!(validate_item_id("bd-abc123").is_ok());
        assert!(validate_item_id("bd-abc123.1").is_ok());
    }

    #[test]
    fn invalid_ids() {
        assert!(validate_item_id("bn-ABC").is_err());
        assert!(validate_item_id("bn-abc.").is_err());
        assert!(validate_item_id("bn-abc.x").is_err());
        // Numeric prefix is not valid
        assert!(validate_item_id("123-abc").is_err());
    }

    #[test]
    fn label_rules() {
        assert!(validate_label("backend_api").is_ok());
        assert!(validate_label("-bad").is_err());
        assert!(validate_label("bad label").is_err());
    }

    #[test]
    fn detects_private_key_like_material() {
        let s = "-----BEGIN OPENSSH PRIVATE KEY-----\nAAA";
        assert_eq!(detect_secret_kind(s), Some("private_key_block"));
        assert!(validate_no_secrets("title", s).is_err());
    }

    #[test]
    fn detects_github_and_aws_tokens() {
        let gh = "token=ghp_abcdefghijklmnopqrstuvwxyz012345";
        let aws = "AKIA1234567890ABCD12";
        assert_eq!(detect_secret_kind(gh), Some("github_token"));
        assert_eq!(detect_secret_kind(aws), Some("aws_access_key_id"));
    }

    #[test]
    fn ignores_regular_text() {
        let s = "normal project update text with no credentials";
        assert_eq!(detect_secret_kind(s), None);
        assert!(validate_no_secrets("description", s).is_ok());
    }
}
