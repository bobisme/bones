use std::fmt;

/// Machine-readable error codes for agent-friendly decision making.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    NotInitialized,
    ConfigParseError,
    ModelNotFound,
    ItemNotFound,
    InvalidStateTransition,
    CycleDetected,
    AmbiguousId,
    InvalidEnumValue,
    ShardManifestMismatch,
    EventHashCollision,
    CorruptProjection,
    EventFileWriteFailed,
    LockContention,
    FtsIndexMissing,
    SemanticModelLoadFailed,
    InternalUnexpected,
}

impl ErrorCode {
    /// Stable code identifier (`E####`) for machine parsing.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotInitialized => "E1001",
            Self::ConfigParseError => "E1002",
            Self::ModelNotFound => "E1003",
            Self::ItemNotFound => "E2001",
            Self::InvalidStateTransition => "E2002",
            Self::CycleDetected => "E2003",
            Self::AmbiguousId => "E2004",
            Self::InvalidEnumValue => "E2005",
            Self::ShardManifestMismatch => "E3001",
            Self::EventHashCollision => "E3002",
            Self::CorruptProjection => "E3003",
            Self::EventFileWriteFailed => "E5001",
            Self::LockContention => "E5002",
            Self::FtsIndexMissing => "E6001",
            Self::SemanticModelLoadFailed => "E6002",
            Self::InternalUnexpected => "E9001",
        }
    }

    /// Short human-facing summary for logs and terminal output.
    #[must_use]
    pub const fn message(self) -> &'static str {
        match self {
            Self::NotInitialized => "Project not initialized",
            Self::ConfigParseError => "Config file parse error",
            Self::ModelNotFound => "Semantic model not found",
            Self::ItemNotFound => "Item not found",
            Self::InvalidStateTransition => "Invalid state transition",
            Self::CycleDetected => "Cycle would be created",
            Self::AmbiguousId => "Ambiguous item ID",
            Self::InvalidEnumValue => "Invalid kind/urgency/size value",
            Self::ShardManifestMismatch => "Shard manifest mismatch",
            Self::EventHashCollision => "Event hash collision",
            Self::CorruptProjection => "Corrupt SQLite projection",
            Self::EventFileWriteFailed => "Event file write failed",
            Self::LockContention => "Lock contention",
            Self::FtsIndexMissing => "FTS index missing",
            Self::SemanticModelLoadFailed => "Semantic model load failed",
            Self::InternalUnexpected => "Internal unexpected error",
        }
    }

    /// Optional remediation hint that can be surfaced to operators and agents.
    #[must_use]
    pub const fn hint(self) -> Option<&'static str> {
        match self {
            Self::NotInitialized => Some("Run `bn init` to initialize this repository."),
            Self::ConfigParseError => Some("Fix syntax in .bones/config.toml and retry."),
            Self::ModelNotFound => Some("Install or configure the semantic model before search."),
            Self::ItemNotFound => None,
            Self::InvalidStateTransition => {
                Some("Follow valid transitions: open -> doing -> done -> archived.")
            }
            Self::CycleDetected => Some("Remove/adjust dependency links to keep the graph acyclic."),
            Self::AmbiguousId => Some("Use a longer ID prefix to disambiguate."),
            Self::InvalidEnumValue => Some("Use one of the documented kind/urgency/size values."),
            Self::ShardManifestMismatch => Some("Run `bn rebuild` to repair the shard manifest."),
            Self::EventHashCollision => Some("Regenerate the event with a different payload/metadata."),
            Self::CorruptProjection => Some("Run `bn rebuild` to repair the SQLite projection."),
            Self::EventFileWriteFailed => Some("Check disk space and write permissions."),
            Self::LockContention => Some("Retry after the other `bn` process releases its lock."),
            Self::FtsIndexMissing => Some("Run `bn rebuild` to create the FTS index."),
            Self::SemanticModelLoadFailed => {
                Some("Verify model files and runtime dependencies are available.")
            }
            Self::InternalUnexpected => Some("Retry once. If persistent, report a bug with logs."),
        }
    }
}

impl fmt::Display for ErrorCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.code())
    }
}

#[cfg(test)]
mod tests {
    use super::ErrorCode;
    use std::collections::HashSet;

    #[test]
    fn all_codes_are_unique() {
        let all = [
            ErrorCode::NotInitialized,
            ErrorCode::ConfigParseError,
            ErrorCode::ModelNotFound,
            ErrorCode::ItemNotFound,
            ErrorCode::InvalidStateTransition,
            ErrorCode::CycleDetected,
            ErrorCode::AmbiguousId,
            ErrorCode::InvalidEnumValue,
            ErrorCode::ShardManifestMismatch,
            ErrorCode::EventHashCollision,
            ErrorCode::CorruptProjection,
            ErrorCode::EventFileWriteFailed,
            ErrorCode::LockContention,
            ErrorCode::FtsIndexMissing,
            ErrorCode::SemanticModelLoadFailed,
            ErrorCode::InternalUnexpected,
        ];

        let mut seen = HashSet::new();
        for code in all {
            assert!(seen.insert(code.code()), "duplicate code {}", code.code());
        }
    }

    #[test]
    fn code_format_is_machine_friendly() {
        let code = ErrorCode::InvalidStateTransition.code();
        assert_eq!(code.len(), 5);
        assert!(code.starts_with('E'));
        assert!(code.chars().skip(1).all(|c| c.is_ascii_digit()));
    }
}
