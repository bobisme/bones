//! Comprehensive error types for bones-core.
//!
//! Every error explains what went wrong, why, and how to fix it. Errors are
//! organized by category and carry stable machine-readable codes for
//! programmatic handling via `--json`.
//!
//! # Error Code Ranges
//!
//! | Range       | Category          |
//! |-------------|-------------------|
//! | E1xxx       | Configuration     |
//! | E2xxx       | Domain model      |
//! | E3xxx       | Data integrity    |
//! | E4xxx       | Event operations  |
//! | E5xxx       | I/O and system    |
//! | E6xxx       | Search/index      |
//! | E9xxx       | Internal          |

use serde::Serialize;
use std::fmt;
use std::path::PathBuf;
use std::time::Duration;

// ---------------------------------------------------------------------------
// Machine-readable error codes (backward-compatible)
// ---------------------------------------------------------------------------

/// Machine-readable error codes for agent-friendly decision making.
///
/// These are kept for backward compatibility with existing code that
/// references `ErrorCode` directly (e.g., `lock.rs`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum ErrorCode {
    NotInitialized,
    ConfigParseError,
    ConfigInvalidValue,
    ModelNotFound,
    ItemNotFound,
    InvalidStateTransition,
    CycleDetected,
    AmbiguousId,
    InvalidEnumValue,
    InvalidItemId,
    DuplicateItem,
    ShardManifestMismatch,
    EventHashCollision,
    CorruptProjection,
    EventParseFailed,
    EventUnknownType,
    EventInvalidTimestamp,
    EventOversizedPayload,
    EventFileWriteFailed,
    ShardNotFound,
    LockContention,
    LockAlreadyHeld,
    FtsIndexMissing,
    SemanticModelLoadFailed,
    PermissionDenied,
    DiskFull,
    NotABonesProject,
    DbMissing,
    DbSchemaVersion,
    DbQueryFailed,
    DbRebuildFailed,
    InternalUnexpected,
}

impl ErrorCode {
    /// Stable code identifier (`E####`) for machine parsing.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotInitialized => "E1001",
            Self::ConfigParseError => "E1002",
            Self::ConfigInvalidValue => "E1003",
            Self::ModelNotFound => "E1004",
            Self::ItemNotFound => "E2001",
            Self::InvalidStateTransition => "E2002",
            Self::CycleDetected => "E2003",
            Self::AmbiguousId => "E2004",
            Self::InvalidEnumValue => "E2005",
            Self::InvalidItemId => "E2006",
            Self::DuplicateItem => "E2007",
            Self::ShardManifestMismatch => "E3001",
            Self::EventHashCollision => "E3002",
            Self::CorruptProjection => "E3003",
            Self::EventParseFailed => "E4001",
            Self::EventUnknownType => "E4002",
            Self::EventInvalidTimestamp => "E4003",
            Self::EventOversizedPayload => "E4004",
            Self::EventFileWriteFailed => "E5001",
            Self::LockContention => "E5002",
            Self::LockAlreadyHeld => "E5003",
            Self::PermissionDenied => "E5004",
            Self::DiskFull => "E5005",
            Self::NotABonesProject => "E5006",
            Self::ShardNotFound => "E5007",
            Self::DbMissing => "E5008",
            Self::DbSchemaVersion => "E5009",
            Self::DbQueryFailed => "E5010",
            Self::DbRebuildFailed => "E5011",
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
            Self::ConfigInvalidValue => "Invalid config value",
            Self::ModelNotFound => "Semantic model not found",
            Self::ItemNotFound => "Item not found",
            Self::InvalidStateTransition => "Invalid state transition",
            Self::CycleDetected => "Cycle would be created",
            Self::AmbiguousId => "Ambiguous item ID",
            Self::InvalidEnumValue => "Invalid kind/urgency/size value",
            Self::InvalidItemId => "Invalid item ID format",
            Self::DuplicateItem => "Duplicate item",
            Self::ShardManifestMismatch => "Shard manifest mismatch",
            Self::EventHashCollision => "Event hash collision",
            Self::CorruptProjection => "Corrupt SQLite projection",
            Self::EventParseFailed => "Event parse failed",
            Self::EventUnknownType => "Unknown event type",
            Self::EventInvalidTimestamp => "Invalid event timestamp",
            Self::EventOversizedPayload => "Event payload too large",
            Self::EventFileWriteFailed => "Event file write failed",
            Self::LockContention => "Lock contention",
            Self::LockAlreadyHeld => "Lock already held",
            Self::PermissionDenied => "Permission denied",
            Self::DiskFull => "Disk full",
            Self::NotABonesProject => "Not a bones project",
            Self::ShardNotFound => "Shard file not found",
            Self::DbMissing => "Projection database missing",
            Self::DbSchemaVersion => "Schema version mismatch",
            Self::DbQueryFailed => "Database query failed",
            Self::DbRebuildFailed => "Database rebuild failed",
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
            Self::ConfigInvalidValue => {
                Some("Check .bones/config.toml for the invalid key and correct it.")
            }
            Self::ModelNotFound => Some("Install or configure the semantic model before search."),
            Self::ItemNotFound => {
                Some("Check the item ID and try again. Use `bn list` to find valid IDs.")
            }
            Self::InvalidStateTransition => {
                Some("Follow valid transitions: open -> doing -> done -> archived.")
            }
            Self::CycleDetected => {
                Some("Remove/adjust dependency links to keep the graph acyclic.")
            }
            Self::AmbiguousId => Some("Use a longer ID prefix to disambiguate."),
            Self::InvalidEnumValue => Some("Use one of the documented kind/urgency/size values."),
            Self::InvalidItemId => {
                Some("Item IDs must be alphanumeric. Use `bn list` to find valid IDs.")
            }
            Self::DuplicateItem => Some("An item with this ID already exists."),
            Self::ShardManifestMismatch => Some("Run `bn rebuild` to repair the shard manifest."),
            Self::EventHashCollision => {
                Some("Regenerate the event with a different payload/metadata.")
            }
            Self::CorruptProjection => Some("Run `bn rebuild` to repair the SQLite projection."),
            Self::EventParseFailed => {
                Some("Check the event file for malformed lines. Run `bn verify` for details.")
            }
            Self::EventUnknownType => {
                Some("This event type is not recognized. You may need a newer version of bn.")
            }
            Self::EventInvalidTimestamp => {
                Some("The timestamp is malformed. Check the event file for corruption.")
            }
            Self::EventOversizedPayload => {
                Some("Reduce the event payload size or split into smaller events.")
            }
            Self::EventFileWriteFailed => Some("Check disk space and write permissions."),
            Self::LockContention => Some("Retry after the other `bn` process releases its lock."),
            Self::LockAlreadyHeld => {
                Some("Another process holds the lock. Wait or check for stale lock files.")
            }
            Self::PermissionDenied => {
                Some("Check file permissions and ownership on the .bones directory.")
            }
            Self::DiskFull => Some("Free disk space and retry."),
            Self::NotABonesProject => {
                Some("Run `bn init` in the project root, or cd to a bones project.")
            }
            Self::ShardNotFound => {
                Some("The shard file may have been deleted. Run `bn verify` to check integrity.")
            }
            Self::DbMissing => Some("Run `bn rebuild` to recreate the projection database."),
            Self::DbSchemaVersion => {
                Some("Run `bn rebuild` to migrate to the current schema version.")
            }
            Self::DbQueryFailed => Some(
                "Run `bn rebuild` to repair the database. If the error persists, report a bug.",
            ),
            Self::DbRebuildFailed => Some(
                "Check disk space and permissions. Try deleting .bones/db.sqlite and rebuilding.",
            ),
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

// ---------------------------------------------------------------------------
// Top-level BonesError
// ---------------------------------------------------------------------------

/// Top-level error type for all bones-core operations.
///
/// Each variant delegates to a category-specific error enum that carries
/// contextual details. Use [`error_code()`](BonesError::error_code) for
/// machine-readable codes and [`suggestion()`](BonesError::suggestion)
/// for actionable remediation hints.
#[derive(Debug, thiserror::Error)]
pub enum BonesError {
    /// Event parsing, writing, or validation failures.
    #[error(transparent)]
    Event(#[from] EventError),

    /// SQLite projection failures (schema, query, rebuild).
    #[error(transparent)]
    Projection(#[from] ProjectionError),

    /// Configuration loading or validation failures.
    #[error(transparent)]
    Config(#[from] ConfigError),

    /// Filesystem and I/O failures.
    #[error(transparent)]
    Io(#[from] IoError),

    /// Domain model violations (invalid state transition, circular containment).
    #[error(transparent)]
    Model(#[from] ModelError),

    /// Concurrency failures (lock timeout, locked DB).
    #[error(transparent)]
    Lock(#[from] LockError),
}

impl BonesError {
    /// Machine-readable error code for `--json` output (e.g., `"E2001"`).
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Event(e) => e.error_code(),
            Self::Projection(e) => e.error_code(),
            Self::Config(e) => e.error_code(),
            Self::Io(e) => e.error_code(),
            Self::Model(e) => e.error_code(),
            Self::Lock(e) => e.error_code(),
        }
    }

    /// Human-readable suggestion for how to fix the error.
    #[must_use]
    pub fn suggestion(&self) -> String {
        match self {
            Self::Event(e) => e.suggestion(),
            Self::Projection(e) => e.suggestion(),
            Self::Config(e) => e.suggestion(),
            Self::Io(e) => e.suggestion(),
            Self::Model(e) => e.suggestion(),
            Self::Lock(e) => e.suggestion(),
        }
    }

    /// Structured error payload for JSON serialization.
    #[must_use]
    pub fn to_json_error(&self) -> JsonError {
        JsonError {
            error_code: self.error_code().to_string(),
            message: self.to_string(),
            suggestion: self.suggestion(),
        }
    }
}

/// JSON-serializable error payload for `--json` mode.
#[derive(Debug, Clone, Serialize)]
pub struct JsonError {
    /// Machine-readable error code (e.g., `"E2001"`).
    pub error_code: String,
    /// Human-readable error message.
    pub message: String,
    /// Actionable suggestion for fixing the error.
    pub suggestion: String,
}

// ---------------------------------------------------------------------------
// EventError
// ---------------------------------------------------------------------------

/// Errors related to event parsing, writing, and validation.
#[derive(Debug, thiserror::Error)]
pub enum EventError {
    /// A line in the event file could not be parsed.
    #[error(
        "Error: Failed to parse event at line {line_num}\nCause: {reason}\nFix: Check the event file for malformed lines. Run `bn verify` for details."
    )]
    ParseFailed {
        /// 1-based line number within the shard file.
        line_num: usize,
        /// Description of the parse failure.
        reason: String,
    },

    /// The event type string is not recognized.
    #[error(
        "Error: Unknown event type '{event_type}'\nCause: This event type is not part of the bones schema\nFix: You may need a newer version of bn. Supported types: item.create, item.update, item.state, item.tag, item.untag, item.link, item.unlink, item.move, item.assign, item.unassign, item.comment"
    )]
    UnknownType {
        /// The unrecognized event type string.
        event_type: String,
    },

    /// A timestamp in an event line is malformed.
    #[error(
        "Error: Invalid timestamp '{raw}'\nCause: Timestamp does not match expected microsecond epoch format\nFix: Check the event file for corruption. Valid timestamps are positive integers (microseconds since Unix epoch)."
    )]
    InvalidTimestamp {
        /// The raw timestamp string that failed to parse.
        raw: String,
    },

    /// The referenced shard file does not exist on disk.
    #[error(
        "Error: Shard file not found at {path}\nCause: The shard file may have been deleted or moved\nFix: Run `bn verify` to check integrity. Run `bn rebuild` if the projection is stale."
    )]
    ShardNotFound {
        /// Path where the shard was expected.
        path: PathBuf,
    },

    /// A sealed shard's content does not match its manifest.
    #[error(
        "Error: Shard manifest mismatch for {shard}\nCause: Expected hash {expected_hash}, got {actual_hash}\nFix: Run `bn rebuild` to repair. If the shard was modified externally, the data may be corrupted."
    )]
    ManifestMismatch {
        /// Path to the shard file.
        shard: PathBuf,
        /// Hash recorded in the manifest.
        expected_hash: String,
        /// Hash computed from the current file.
        actual_hash: String,
    },

    /// An event payload exceeds the maximum allowed size.
    #[error(
        "Error: Event payload is {size} bytes (max: {max} bytes)\nCause: The event data exceeds the size limit\nFix: Reduce the payload size or split into smaller events."
    )]
    OversizedPayload {
        /// Actual payload size in bytes.
        size: usize,
        /// Maximum allowed size in bytes.
        max: usize,
    },

    /// An event line contains an invalid hash.
    #[error(
        "Error: Event hash collision detected\nCause: Two events produced the same hash, which should be statistically impossible\nFix: Regenerate the event with different metadata. If this recurs, report a bug."
    )]
    HashCollision,

    /// Failed to write an event to the shard file.
    #[error(
        "Error: Failed to write event to shard\nCause: {reason}\nFix: Check disk space and file permissions on the .bones/events directory."
    )]
    WriteFailed {
        /// Description of the write failure.
        reason: String,
    },

    /// JSON serialization of event data failed.
    #[error(
        "Error: Failed to serialize event data\nCause: {reason}\nFix: Check that event data contains only valid JSON-serializable values."
    )]
    SerializeFailed {
        /// Description of the serialization failure.
        reason: String,
    },
}

impl EventError {
    /// Machine-readable error code.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::ParseFailed { .. } => ErrorCode::EventParseFailed.code(),
            Self::UnknownType { .. } => ErrorCode::EventUnknownType.code(),
            Self::InvalidTimestamp { .. } => ErrorCode::EventInvalidTimestamp.code(),
            Self::ShardNotFound { .. } => ErrorCode::ShardNotFound.code(),
            Self::ManifestMismatch { .. } => ErrorCode::ShardManifestMismatch.code(),
            Self::OversizedPayload { .. } => ErrorCode::EventOversizedPayload.code(),
            Self::HashCollision => ErrorCode::EventHashCollision.code(),
            Self::WriteFailed { .. } => ErrorCode::EventFileWriteFailed.code(),
            Self::SerializeFailed { .. } => ErrorCode::EventFileWriteFailed.code(),
        }
    }

    /// Human-readable suggestion.
    #[must_use]
    pub fn suggestion(&self) -> String {
        match self {
            Self::ParseFailed { .. } => {
                "Check the event file for malformed lines. Run `bn verify` for details.".into()
            }
            Self::UnknownType { .. } => {
                "You may need a newer version of bn to handle this event type.".into()
            }
            Self::InvalidTimestamp { .. } => {
                "Check the event file for corruption. Run `bn verify`.".into()
            }
            Self::ShardNotFound { .. } => {
                "Run `bn verify` to check integrity. Run `bn rebuild` if needed.".into()
            }
            Self::ManifestMismatch { .. } => {
                "Run `bn rebuild` to repair. The shard may have been modified externally.".into()
            }
            Self::OversizedPayload { .. } => {
                "Reduce the payload size or split into smaller events.".into()
            }
            Self::HashCollision => {
                "Regenerate the event with different metadata. Report a bug if this recurs.".into()
            }
            Self::WriteFailed { .. } => {
                "Check disk space and file permissions on the .bones/events directory.".into()
            }
            Self::SerializeFailed { .. } => {
                "Check that event data contains only valid JSON-serializable values.".into()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ProjectionError
// ---------------------------------------------------------------------------

/// Errors related to the SQLite projection layer.
#[derive(Debug, thiserror::Error)]
pub enum ProjectionError {
    /// The projection database file does not exist.
    #[error(
        "Error: Projection database not found at {path}\nCause: The database file is missing or was deleted\nFix: Run `bn rebuild` to recreate the projection database."
    )]
    DbMissing {
        /// Expected path to the database file.
        path: PathBuf,
    },

    /// The database schema version does not match the expected version.
    #[error(
        "Error: Schema version mismatch (expected v{expected}, found v{found})\nCause: The database was created by a different version of bn\nFix: Run `bn rebuild` to migrate to the current schema version."
    )]
    SchemaVersion {
        /// Expected schema version.
        expected: u32,
        /// Actual schema version found.
        found: u32,
    },

    /// A SQL query failed.
    #[error(
        "Error: Database query failed\nCause: {reason}\nFix: Run `bn rebuild` to repair the database. If the error persists, report a bug."
    )]
    QueryFailed {
        /// The SQL that failed (may be truncated for large queries).
        sql: String,
        /// Description of the failure.
        reason: String,
    },

    /// Rebuilding the projection from events failed.
    #[error(
        "Error: Projection rebuild failed\nCause: {reason}\nFix: Delete .bones/db.sqlite and retry `bn rebuild`. Check disk space and permissions."
    )]
    RebuildFailed {
        /// Description of the failure.
        reason: String,
    },

    /// The projection database appears corrupt.
    #[error(
        "Error: Corrupt projection database\nCause: {reason}\nFix: Delete .bones/db.sqlite and run `bn rebuild` to recreate from events."
    )]
    Corrupt {
        /// Description of the corruption.
        reason: String,
    },

    /// The full-text search index is missing.
    #[error(
        "Error: FTS index is missing from the projection database\nCause: The database may have been created without FTS support\nFix: Run `bn rebuild` to create the FTS index."
    )]
    FtsIndexMissing,
}

impl ProjectionError {
    /// Machine-readable error code.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::DbMissing { .. } => ErrorCode::DbMissing.code(),
            Self::SchemaVersion { .. } => ErrorCode::DbSchemaVersion.code(),
            Self::QueryFailed { .. } => ErrorCode::DbQueryFailed.code(),
            Self::RebuildFailed { .. } => ErrorCode::DbRebuildFailed.code(),
            Self::Corrupt { .. } => ErrorCode::CorruptProjection.code(),
            Self::FtsIndexMissing => ErrorCode::FtsIndexMissing.code(),
        }
    }

    /// Human-readable suggestion.
    #[must_use]
    pub fn suggestion(&self) -> String {
        match self {
            Self::DbMissing { .. } => {
                "Run `bn rebuild` to recreate the projection database.".into()
            }
            Self::SchemaVersion { .. } => {
                "Run `bn rebuild` to migrate to the current schema version.".into()
            }
            Self::QueryFailed { .. } => {
                "Run `bn rebuild` to repair. If the error persists, report a bug.".into()
            }
            Self::RebuildFailed { .. } => {
                "Delete .bones/db.sqlite and retry `bn rebuild`. Check disk space.".into()
            }
            Self::Corrupt { .. } => {
                "Delete .bones/db.sqlite and run `bn rebuild` to recreate from events.".into()
            }
            Self::FtsIndexMissing => "Run `bn rebuild` to create the FTS index.".into(),
        }
    }
}

// ---------------------------------------------------------------------------
// ConfigError
// ---------------------------------------------------------------------------

/// Errors related to configuration loading and validation.
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// The config file does not exist.
    #[error(
        "Error: Config file not found at {path}\nCause: The config file is missing\nFix: Run `bn init` to create a default configuration, or create .bones/config.toml manually."
    )]
    NotFound {
        /// Expected path to the config file.
        path: PathBuf,
    },

    /// A config value is invalid.
    #[error(
        "Error: Invalid config value for '{key}': '{value}'\nCause: {reason}\nFix: Edit .bones/config.toml and correct the value for '{key}'."
    )]
    InvalidValue {
        /// The config key with the invalid value.
        key: String,
        /// The invalid value.
        value: String,
        /// Why the value is invalid.
        reason: String,
    },

    /// The config file could not be parsed.
    #[error(
        "Error: Failed to parse config file at {path}\nCause: {reason}\nFix: Fix the syntax in .bones/config.toml. Check for missing quotes, brackets, or invalid TOML."
    )]
    ParseFailed {
        /// Path to the config file.
        path: PathBuf,
        /// Parse error description.
        reason: String,
    },
}

impl ConfigError {
    /// Machine-readable error code.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::NotFound { .. } => ErrorCode::NotInitialized.code(),
            Self::InvalidValue { .. } => ErrorCode::ConfigInvalidValue.code(),
            Self::ParseFailed { .. } => ErrorCode::ConfigParseError.code(),
        }
    }

    /// Human-readable suggestion.
    #[must_use]
    pub fn suggestion(&self) -> String {
        match self {
            Self::NotFound { .. } => {
                "Run `bn init` to create a default config, or create .bones/config.toml manually."
                    .into()
            }
            Self::InvalidValue { key, .. } => {
                format!("Edit .bones/config.toml and correct the value for '{key}'.")
            }
            Self::ParseFailed { .. } => {
                "Fix the TOML syntax in .bones/config.toml and retry.".into()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// IoError
// ---------------------------------------------------------------------------

/// Errors related to filesystem and I/O operations.
#[derive(Debug, thiserror::Error)]
pub enum IoError {
    /// Permission denied accessing a path.
    #[error(
        "Error: Permission denied at {path}\nCause: The current user lacks read/write access\nFix: Check file permissions and ownership. Run `ls -la {path}` to inspect."
    )]
    PermissionDenied {
        /// The path that could not be accessed.
        path: PathBuf,
    },

    /// The disk is full.
    #[error(
        "Error: Disk full — cannot write to {path}\nCause: No disk space remaining on the target filesystem\nFix: Free disk space and retry. Check usage with `df -h`."
    )]
    DiskFull {
        /// The path where the write failed.
        path: PathBuf,
    },

    /// The directory is not a bones project.
    #[error(
        "Error: Not a bones project at {path}\nCause: No .bones directory found in this path or any parent\nFix: Run `bn init` to create a new bones project, or cd to an existing one."
    )]
    NotABonesProject {
        /// The path that was checked.
        path: PathBuf,
    },

    /// Generic I/O error with context.
    #[error(
        "Error: I/O error at {path}\nCause: {reason}\nFix: Check that the path exists and is accessible. Verify disk space and permissions."
    )]
    Generic {
        /// The path involved in the error.
        path: PathBuf,
        /// Description of the I/O error.
        reason: String,
    },
}

impl IoError {
    /// Machine-readable error code.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::PermissionDenied { .. } => ErrorCode::PermissionDenied.code(),
            Self::DiskFull { .. } => ErrorCode::DiskFull.code(),
            Self::NotABonesProject { .. } => ErrorCode::NotABonesProject.code(),
            Self::Generic { .. } => ErrorCode::EventFileWriteFailed.code(),
        }
    }

    /// Human-readable suggestion.
    #[must_use]
    pub fn suggestion(&self) -> String {
        match self {
            Self::PermissionDenied { path } => {
                format!(
                    "Check file permissions and ownership. Run `ls -la {}` to inspect.",
                    path.display()
                )
            }
            Self::DiskFull { .. } => "Free disk space and retry. Check usage with `df -h`.".into(),
            Self::NotABonesProject { .. } => {
                "Run `bn init` to create a new bones project, or cd to an existing one.".into()
            }
            Self::Generic { .. } => {
                "Check that the path exists and is accessible. Verify disk space and permissions."
                    .into()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// ModelError
// ---------------------------------------------------------------------------

/// Errors related to domain model violations.
#[derive(Debug, thiserror::Error)]
pub enum ModelError {
    /// An invalid state transition was attempted.
    #[error(
        "Error: Cannot transition item '{item_id}' from '{from}' to '{to}'\nCause: This state transition is not allowed by lifecycle rules\nFix: Valid transitions: open->doing, open->done, doing->done, doing->open, done->archived, done->open, archived->open"
    )]
    InvalidTransition {
        /// The item being transitioned.
        item_id: String,
        /// Current state.
        from: String,
        /// Attempted target state.
        to: String,
    },

    /// The referenced item does not exist.
    #[error(
        "Error: Item '{item_id}' not found\nCause: No item with this ID exists in the project\nFix: Check the ID and try again. Use `bn list` to see all items. Use a longer prefix if the ID is ambiguous."
    )]
    ItemNotFound {
        /// The ID that was not found.
        item_id: String,
    },

    /// Moving the item would create a circular containment chain.
    #[error("Error: Moving this item would create a cycle: {}\nCause: Circular containment is not allowed in the hierarchy\nFix: Choose a different parent or restructure the hierarchy. Remove/adjust links to break the cycle.", cycle.join(" -> "))]
    CircularContainment {
        /// The IDs forming the cycle.
        cycle: Vec<String>,
    },

    /// The item ID format is invalid.
    #[error(
        "Error: Invalid item ID '{raw}'\nCause: Item IDs must be valid terseid identifiers\nFix: Use `bn list` to find valid item IDs. IDs are short alphanumeric strings."
    )]
    InvalidItemId {
        /// The raw string that failed validation.
        raw: String,
    },

    /// The ID prefix matches multiple items.
    #[error("Error: Ambiguous item ID '{prefix}' matches {count} items\nCause: The prefix is too short to uniquely identify an item\nFix: Use a longer prefix. Matching items: {}", matches.join(", "))]
    AmbiguousId {
        /// The ambiguous prefix.
        prefix: String,
        /// Number of matching items.
        count: usize,
        /// The matching item IDs (up to a reasonable limit).
        matches: Vec<String>,
    },

    /// An enum value (kind, state, urgency, size) is invalid.
    #[error(
        "Error: Invalid {field} value '{value}'\nCause: '{value}' is not a recognized {field}\nFix: Valid {field} values: {valid_values}"
    )]
    InvalidEnumValue {
        /// Which field (e.g., "kind", "state", "urgency", "size").
        field: String,
        /// The invalid value.
        value: String,
        /// Comma-separated list of valid values.
        valid_values: String,
    },

    /// A duplicate item was detected.
    #[error(
        "Error: Duplicate item '{item_id}'\nCause: An item with this ID already exists\nFix: Use a different ID or update the existing item."
    )]
    DuplicateItem {
        /// The duplicate item ID.
        item_id: String,
    },

    /// A dependency cycle was detected.
    #[error("Error: Adding this dependency would create a cycle: {}\nCause: Circular dependencies are not allowed\nFix: Remove/adjust dependency links to keep the graph acyclic.", cycle.join(" -> "))]
    CycleDetected {
        /// The IDs forming the cycle.
        cycle: Vec<String>,
    },
}

impl ModelError {
    /// Machine-readable error code.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::InvalidTransition { .. } => ErrorCode::InvalidStateTransition.code(),
            Self::ItemNotFound { .. } => ErrorCode::ItemNotFound.code(),
            Self::CircularContainment { .. } => ErrorCode::CycleDetected.code(),
            Self::InvalidItemId { .. } => ErrorCode::InvalidItemId.code(),
            Self::AmbiguousId { .. } => ErrorCode::AmbiguousId.code(),
            Self::InvalidEnumValue { .. } => ErrorCode::InvalidEnumValue.code(),
            Self::DuplicateItem { .. } => ErrorCode::DuplicateItem.code(),
            Self::CycleDetected { .. } => ErrorCode::CycleDetected.code(),
        }
    }

    /// Human-readable suggestion.
    #[must_use]
    pub fn suggestion(&self) -> String {
        match self {
            Self::InvalidTransition { .. } => {
                "Valid transitions: open->doing, open->done, doing->done, doing->open, done->archived, done->open, archived->open".into()
            }
            Self::ItemNotFound { .. } => {
                "Check the ID and try again. Use `bn list` to see all items.".into()
            }
            Self::CircularContainment { .. } => {
                "Choose a different parent or restructure the hierarchy.".into()
            }
            Self::InvalidItemId { .. } => {
                "Use `bn list` to find valid item IDs. IDs are short alphanumeric strings.".into()
            }
            Self::AmbiguousId { prefix, .. } => {
                format!("Use a longer prefix than '{prefix}' to uniquely identify the item.")
            }
            Self::InvalidEnumValue { field, valid_values, .. } => {
                format!("Use one of the valid {field} values: {valid_values}")
            }
            Self::DuplicateItem { .. } => {
                "Use a different ID or update the existing item.".into()
            }
            Self::CycleDetected { .. } => {
                "Remove/adjust dependency links to keep the graph acyclic.".into()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// LockError
// ---------------------------------------------------------------------------

/// Errors related to concurrency and locking.
#[derive(Debug, thiserror::Error)]
pub enum LockError {
    /// A lock acquisition timed out.
    #[error(
        "Error: Lock timed out after {waited:?} at {path}\nCause: Another bn process is holding the lock\nFix: Wait for the other process to finish, then retry. Check for stale lock files at {path}."
    )]
    Timeout {
        /// The lock file path.
        path: PathBuf,
        /// How long the acquisition was attempted.
        waited: Duration,
    },

    /// The lock is already held by another process.
    #[error("Error: Lock already held at {path}{}\nCause: Another process is using the repository\nFix: Wait for the other process to finish. If no process is running, remove the lock file.", holder.as_ref().map(|h| format!(" by {h}")).unwrap_or_default())]
    AlreadyLocked {
        /// The lock file path.
        path: PathBuf,
        /// Optional holder identity.
        holder: Option<String>,
    },
}

impl LockError {
    /// Machine-readable error code.
    #[must_use]
    pub fn error_code(&self) -> &'static str {
        match self {
            Self::Timeout { .. } => ErrorCode::LockContention.code(),
            Self::AlreadyLocked { .. } => ErrorCode::LockAlreadyHeld.code(),
        }
    }

    /// Human-readable suggestion.
    #[must_use]
    pub fn suggestion(&self) -> String {
        match self {
            Self::Timeout { path, .. } => {
                format!(
                    "Wait for the other process to finish, then retry. Check for stale lock files at {}.",
                    path.display()
                )
            }
            Self::AlreadyLocked { .. } => {
                "Wait for the other process to finish. If no process is running, remove the lock file.".into()
            }
        }
    }
}

// ---------------------------------------------------------------------------
// From implementations for common error types
// ---------------------------------------------------------------------------

impl From<std::io::Error> for BonesError {
    fn from(err: std::io::Error) -> Self {
        let kind = err.kind();
        match kind {
            std::io::ErrorKind::PermissionDenied => Self::Io(IoError::PermissionDenied {
                path: PathBuf::from("<unknown>"),
            }),
            _ => Self::Io(IoError::Generic {
                path: PathBuf::from("<unknown>"),
                reason: err.to_string(),
            }),
        }
    }
}

impl From<rusqlite::Error> for BonesError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Projection(ProjectionError::QueryFailed {
            sql: String::new(),
            reason: err.to_string(),
        })
    }
}

impl From<serde_json::Error> for BonesError {
    fn from(err: serde_json::Error) -> Self {
        Self::Event(EventError::SerializeFailed {
            reason: err.to_string(),
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    // --- ErrorCode backward-compat tests ---

    #[test]
    fn all_codes_are_unique() {
        let all = [
            ErrorCode::NotInitialized,
            ErrorCode::ConfigParseError,
            ErrorCode::ConfigInvalidValue,
            ErrorCode::ModelNotFound,
            ErrorCode::ItemNotFound,
            ErrorCode::InvalidStateTransition,
            ErrorCode::CycleDetected,
            ErrorCode::AmbiguousId,
            ErrorCode::InvalidEnumValue,
            ErrorCode::InvalidItemId,
            ErrorCode::DuplicateItem,
            ErrorCode::ShardManifestMismatch,
            ErrorCode::EventHashCollision,
            ErrorCode::CorruptProjection,
            ErrorCode::EventParseFailed,
            ErrorCode::EventUnknownType,
            ErrorCode::EventInvalidTimestamp,
            ErrorCode::EventOversizedPayload,
            ErrorCode::EventFileWriteFailed,
            ErrorCode::ShardNotFound,
            ErrorCode::LockContention,
            ErrorCode::LockAlreadyHeld,
            ErrorCode::PermissionDenied,
            ErrorCode::DiskFull,
            ErrorCode::NotABonesProject,
            ErrorCode::DbMissing,
            ErrorCode::DbSchemaVersion,
            ErrorCode::DbQueryFailed,
            ErrorCode::DbRebuildFailed,
            ErrorCode::FtsIndexMissing,
            ErrorCode::SemanticModelLoadFailed,
            ErrorCode::InternalUnexpected,
        ];

        let mut seen = HashSet::new();
        for code in all {
            assert!(seen.insert(code.code()), "duplicate code {}", code.code());
        }

        // Acceptance criterion: 30+ distinct error conditions
        assert!(
            all.len() >= 30,
            "Expected 30+ error codes, got {}",
            all.len()
        );
    }

    #[test]
    fn code_format_is_machine_friendly() {
        let code = ErrorCode::InvalidStateTransition.code();
        assert_eq!(code.len(), 5);
        assert!(code.starts_with('E'));
        assert!(code.chars().skip(1).all(|c| c.is_ascii_digit()));
    }

    #[test]
    fn all_codes_have_messages() {
        let all = [
            ErrorCode::NotInitialized,
            ErrorCode::ConfigParseError,
            ErrorCode::ConfigInvalidValue,
            ErrorCode::ModelNotFound,
            ErrorCode::ItemNotFound,
            ErrorCode::InvalidStateTransition,
            ErrorCode::CycleDetected,
            ErrorCode::AmbiguousId,
            ErrorCode::InvalidEnumValue,
            ErrorCode::InvalidItemId,
            ErrorCode::DuplicateItem,
            ErrorCode::ShardManifestMismatch,
            ErrorCode::EventHashCollision,
            ErrorCode::CorruptProjection,
            ErrorCode::EventParseFailed,
            ErrorCode::EventUnknownType,
            ErrorCode::EventInvalidTimestamp,
            ErrorCode::EventOversizedPayload,
            ErrorCode::EventFileWriteFailed,
            ErrorCode::ShardNotFound,
            ErrorCode::LockContention,
            ErrorCode::LockAlreadyHeld,
            ErrorCode::PermissionDenied,
            ErrorCode::DiskFull,
            ErrorCode::NotABonesProject,
            ErrorCode::DbMissing,
            ErrorCode::DbSchemaVersion,
            ErrorCode::DbQueryFailed,
            ErrorCode::DbRebuildFailed,
            ErrorCode::FtsIndexMissing,
            ErrorCode::SemanticModelLoadFailed,
            ErrorCode::InternalUnexpected,
        ];

        for code in all {
            assert!(!code.message().is_empty(), "{:?} has empty message", code);
        }
    }

    #[test]
    fn all_codes_have_hints() {
        let all = [
            ErrorCode::NotInitialized,
            ErrorCode::ConfigParseError,
            ErrorCode::ConfigInvalidValue,
            ErrorCode::ModelNotFound,
            ErrorCode::ItemNotFound,
            ErrorCode::InvalidStateTransition,
            ErrorCode::CycleDetected,
            ErrorCode::AmbiguousId,
            ErrorCode::InvalidEnumValue,
            ErrorCode::InvalidItemId,
            ErrorCode::DuplicateItem,
            ErrorCode::ShardManifestMismatch,
            ErrorCode::EventHashCollision,
            ErrorCode::CorruptProjection,
            ErrorCode::EventParseFailed,
            ErrorCode::EventUnknownType,
            ErrorCode::EventInvalidTimestamp,
            ErrorCode::EventOversizedPayload,
            ErrorCode::EventFileWriteFailed,
            ErrorCode::ShardNotFound,
            ErrorCode::LockContention,
            ErrorCode::LockAlreadyHeld,
            ErrorCode::PermissionDenied,
            ErrorCode::DiskFull,
            ErrorCode::NotABonesProject,
            ErrorCode::DbMissing,
            ErrorCode::DbSchemaVersion,
            ErrorCode::DbQueryFailed,
            ErrorCode::DbRebuildFailed,
            ErrorCode::FtsIndexMissing,
            ErrorCode::SemanticModelLoadFailed,
            ErrorCode::InternalUnexpected,
        ];

        for code in all {
            assert!(code.hint().is_some(), "{:?} has no hint", code);
        }
    }

    // --- BonesError hierarchy tests ---

    #[test]
    fn bones_error_from_event_error() {
        let err = BonesError::Event(EventError::ParseFailed {
            line_num: 42,
            reason: "unexpected token".into(),
        });
        assert_eq!(err.error_code(), "E4001");
        assert!(err.to_string().contains("line 42"));
        assert!(err.to_string().contains("unexpected token"));
        assert!(!err.suggestion().is_empty());
    }

    #[test]
    fn bones_error_from_projection_error() {
        let err = BonesError::Projection(ProjectionError::SchemaVersion {
            expected: 3,
            found: 1,
        });
        assert_eq!(err.error_code(), "E5009");
        assert!(err.to_string().contains("v3"));
        assert!(err.to_string().contains("v1"));
    }

    #[test]
    fn bones_error_from_config_error() {
        let err = BonesError::Config(ConfigError::InvalidValue {
            key: "shard_size".into(),
            value: "-1".into(),
            reason: "must be positive".into(),
        });
        assert_eq!(err.error_code(), "E1003");
        assert!(err.to_string().contains("shard_size"));
    }

    #[test]
    fn bones_error_from_io_error() {
        let err = BonesError::Io(IoError::NotABonesProject {
            path: PathBuf::from("/tmp/foo"),
        });
        assert_eq!(err.error_code(), "E5006");
        assert!(err.to_string().contains("/tmp/foo"));
        assert!(err.suggestion().contains("bn init"));
    }

    #[test]
    fn bones_error_from_model_error() {
        let err = BonesError::Model(ModelError::InvalidTransition {
            item_id: "abc123".into(),
            from: "done".into(),
            to: "doing".into(),
        });
        assert_eq!(err.error_code(), "E2002");
        assert!(err.to_string().contains("abc123"));
        assert!(err.to_string().contains("done"));
        assert!(err.to_string().contains("doing"));
    }

    #[test]
    fn bones_error_from_lock_error() {
        let err = BonesError::Lock(LockError::Timeout {
            path: PathBuf::from("/repo/.bones/lock"),
            waited: Duration::from_secs(5),
        });
        assert_eq!(err.error_code(), "E5002");
        assert!(err.to_string().contains("timed out"));
    }

    #[test]
    fn model_error_item_not_found() {
        let err = ModelError::ItemNotFound {
            item_id: "xyz789".into(),
        };
        assert_eq!(err.error_code(), ErrorCode::ItemNotFound.code());
        assert!(err.to_string().contains("xyz789"));
        assert!(err.suggestion().contains("bn list"));
    }

    #[test]
    fn model_error_ambiguous_id() {
        let err = ModelError::AmbiguousId {
            prefix: "ab".into(),
            count: 3,
            matches: vec!["abc".into(), "abd".into(), "abe".into()],
        };
        assert_eq!(err.error_code(), ErrorCode::AmbiguousId.code());
        assert!(err.to_string().contains("3 items"));
        assert!(err.to_string().contains("abc"));
    }

    #[test]
    fn model_error_invalid_enum_value() {
        let err = ModelError::InvalidEnumValue {
            field: "kind".into(),
            value: "epic".into(),
            valid_values: "task, goal, bug".into(),
        };
        assert_eq!(err.error_code(), ErrorCode::InvalidEnumValue.code());
        assert!(err.to_string().contains("epic"));
        assert!(err.to_string().contains("task, goal, bug"));
    }

    #[test]
    fn model_error_cycle_detected() {
        let err = ModelError::CycleDetected {
            cycle: vec!["a".into(), "b".into(), "c".into(), "a".into()],
        };
        assert_eq!(err.error_code(), ErrorCode::CycleDetected.code());
        assert!(err.to_string().contains("a -> b -> c -> a"));
    }

    #[test]
    fn event_error_unknown_type() {
        let err = EventError::UnknownType {
            event_type: "item.frobnicate".into(),
        };
        assert_eq!(err.error_code(), ErrorCode::EventUnknownType.code());
        assert!(err.to_string().contains("item.frobnicate"));
    }

    #[test]
    fn event_error_manifest_mismatch() {
        let err = EventError::ManifestMismatch {
            shard: PathBuf::from("2026-01.events"),
            expected_hash: "blake3:aaa".into(),
            actual_hash: "blake3:bbb".into(),
        };
        assert_eq!(err.error_code(), ErrorCode::ShardManifestMismatch.code());
        assert!(err.to_string().contains("blake3:aaa"));
        assert!(err.to_string().contains("blake3:bbb"));
    }

    #[test]
    fn event_error_oversized_payload() {
        let err = EventError::OversizedPayload {
            size: 2_000_000,
            max: 1_000_000,
        };
        assert_eq!(err.error_code(), ErrorCode::EventOversizedPayload.code());
        assert!(err.to_string().contains("2000000"));
        assert!(err.to_string().contains("1000000"));
    }

    #[test]
    fn projection_error_db_missing() {
        let err = ProjectionError::DbMissing {
            path: PathBuf::from(".bones/db.sqlite"),
        };
        assert_eq!(err.error_code(), ErrorCode::DbMissing.code());
        assert!(err.to_string().contains("db.sqlite"));
    }

    #[test]
    fn projection_error_fts_missing() {
        let err = ProjectionError::FtsIndexMissing;
        assert_eq!(err.error_code(), ErrorCode::FtsIndexMissing.code());
        assert!(err.suggestion().contains("bn rebuild"));
    }

    #[test]
    fn config_error_not_found() {
        let err = ConfigError::NotFound {
            path: PathBuf::from(".bones/config.toml"),
        };
        assert_eq!(err.error_code(), ErrorCode::NotInitialized.code());
        assert!(err.suggestion().contains("bn init"));
    }

    #[test]
    fn config_error_parse_failed() {
        let err = ConfigError::ParseFailed {
            path: PathBuf::from(".bones/config.toml"),
            reason: "expected '=' at line 5".into(),
        };
        assert_eq!(err.error_code(), ErrorCode::ConfigParseError.code());
        assert!(err.to_string().contains("line 5"));
    }

    #[test]
    fn io_error_permission_denied() {
        let err = IoError::PermissionDenied {
            path: PathBuf::from("/etc/secret"),
        };
        assert_eq!(err.error_code(), ErrorCode::PermissionDenied.code());
        assert!(err.to_string().contains("/etc/secret"));
    }

    #[test]
    fn io_error_disk_full() {
        let err = IoError::DiskFull {
            path: PathBuf::from("/mnt/data"),
        };
        assert_eq!(err.error_code(), ErrorCode::DiskFull.code());
        assert!(err.suggestion().contains("df -h"));
    }

    #[test]
    fn lock_error_already_locked() {
        let err = LockError::AlreadyLocked {
            path: PathBuf::from(".bones/lock"),
            holder: Some("pid:1234".into()),
        };
        assert_eq!(err.error_code(), ErrorCode::LockAlreadyHeld.code());
        assert!(err.to_string().contains("pid:1234"));
    }

    #[test]
    fn bones_error_to_json_error() {
        let err = BonesError::Model(ModelError::ItemNotFound {
            item_id: "test123".into(),
        });
        let json_err = err.to_json_error();
        assert_eq!(json_err.error_code, "E2001");
        assert!(json_err.message.contains("test123"));
        assert!(!json_err.suggestion.is_empty());

        // Verify it serializes cleanly
        let serialized = serde_json::to_string(&json_err).unwrap();
        assert!(serialized.contains("E2001"));
        assert!(serialized.contains("test123"));
    }

    #[test]
    fn bones_error_from_std_io_error_permission() {
        let io_err = std::io::Error::new(std::io::ErrorKind::PermissionDenied, "forbidden");
        let err: BonesError = io_err.into();
        assert_eq!(err.error_code(), ErrorCode::PermissionDenied.code());
    }

    #[test]
    fn bones_error_from_std_io_error_generic() {
        let io_err = std::io::Error::new(std::io::ErrorKind::Other, "disk on fire");
        let err: BonesError = io_err.into();
        assert!(err.to_string().contains("disk on fire"));
    }

    #[test]
    fn bones_error_from_serde_json_error() {
        let json_err =
            serde_json::from_str::<serde_json::Value>("{{bad}}").expect_err("should fail");
        let err: BonesError = json_err.into();
        assert!(matches!(
            err,
            BonesError::Event(EventError::SerializeFailed { .. })
        ));
    }

    #[test]
    fn every_error_variant_has_suggestion() {
        // Comprehensive check: create one of each variant and verify suggestion is non-empty
        let errors: Vec<BonesError> = vec![
            EventError::ParseFailed {
                line_num: 1,
                reason: "x".into(),
            }
            .into(),
            EventError::UnknownType {
                event_type: "x".into(),
            }
            .into(),
            EventError::InvalidTimestamp { raw: "x".into() }.into(),
            EventError::ShardNotFound {
                path: PathBuf::from("x"),
            }
            .into(),
            EventError::ManifestMismatch {
                shard: PathBuf::from("x"),
                expected_hash: "a".into(),
                actual_hash: "b".into(),
            }
            .into(),
            EventError::OversizedPayload { size: 1, max: 0 }.into(),
            EventError::HashCollision.into(),
            EventError::WriteFailed { reason: "x".into() }.into(),
            EventError::SerializeFailed { reason: "x".into() }.into(),
            ProjectionError::DbMissing {
                path: PathBuf::from("x"),
            }
            .into(),
            ProjectionError::SchemaVersion {
                expected: 1,
                found: 0,
            }
            .into(),
            ProjectionError::QueryFailed {
                sql: "x".into(),
                reason: "x".into(),
            }
            .into(),
            ProjectionError::RebuildFailed { reason: "x".into() }.into(),
            ProjectionError::Corrupt { reason: "x".into() }.into(),
            ProjectionError::FtsIndexMissing.into(),
            ConfigError::NotFound {
                path: PathBuf::from("x"),
            }
            .into(),
            ConfigError::InvalidValue {
                key: "k".into(),
                value: "v".into(),
                reason: "r".into(),
            }
            .into(),
            ConfigError::ParseFailed {
                path: PathBuf::from("x"),
                reason: "r".into(),
            }
            .into(),
            IoError::PermissionDenied {
                path: PathBuf::from("x"),
            }
            .into(),
            IoError::DiskFull {
                path: PathBuf::from("x"),
            }
            .into(),
            IoError::NotABonesProject {
                path: PathBuf::from("x"),
            }
            .into(),
            IoError::Generic {
                path: PathBuf::from("x"),
                reason: "r".into(),
            }
            .into(),
            ModelError::InvalidTransition {
                item_id: "x".into(),
                from: "a".into(),
                to: "b".into(),
            }
            .into(),
            ModelError::ItemNotFound {
                item_id: "x".into(),
            }
            .into(),
            ModelError::CircularContainment {
                cycle: vec!["a".into(), "b".into()],
            }
            .into(),
            ModelError::InvalidItemId { raw: "x".into() }.into(),
            ModelError::AmbiguousId {
                prefix: "x".into(),
                count: 2,
                matches: vec!["xa".into(), "xb".into()],
            }
            .into(),
            ModelError::InvalidEnumValue {
                field: "f".into(),
                value: "v".into(),
                valid_values: "a, b".into(),
            }
            .into(),
            ModelError::DuplicateItem {
                item_id: "x".into(),
            }
            .into(),
            ModelError::CycleDetected {
                cycle: vec!["a".into(), "b".into()],
            }
            .into(),
            LockError::Timeout {
                path: PathBuf::from("x"),
                waited: Duration::from_secs(1),
            }
            .into(),
            LockError::AlreadyLocked {
                path: PathBuf::from("x"),
                holder: None,
            }
            .into(),
        ];

        for (i, err) in errors.iter().enumerate() {
            assert!(
                !err.suggestion().is_empty(),
                "Error variant {i} has empty suggestion: {err}"
            );
            assert!(
                !err.error_code().is_empty(),
                "Error variant {i} has empty error_code: {err}"
            );
            assert!(
                !err.to_string().is_empty(),
                "Error variant {i} has empty display: {err}"
            );
        }

        // Acceptance criterion: 30+ distinct error conditions
        assert!(
            errors.len() >= 30,
            "Expected 30+ error variants, got {}",
            errors.len()
        );
    }

    #[test]
    fn display_format_has_error_cause_fix() {
        // Verify the Error/Cause/Fix pattern for representative variants
        let err = EventError::ParseFailed {
            line_num: 42,
            reason: "bad json".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("Error:"), "Missing 'Error:' in: {msg}");
        assert!(
            msg.contains("Cause:") || msg.contains("bad json"),
            "Missing cause in: {msg}"
        );
        assert!(msg.contains("Fix:"), "Missing 'Fix:' in: {msg}");

        let err = ModelError::InvalidTransition {
            item_id: "abc".into(),
            from: "done".into(),
            to: "doing".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("Error:"), "Missing 'Error:' in: {msg}");
        assert!(msg.contains("Fix:"), "Missing 'Fix:' in: {msg}");
    }

    #[test]
    fn json_error_serialization_stable() {
        let err = BonesError::Model(ModelError::ItemNotFound {
            item_id: "abc".into(),
        });
        let json_err = err.to_json_error();
        let value: serde_json::Value = serde_json::to_value(&json_err).unwrap();

        // Verify required fields exist
        assert!(value.get("error_code").is_some());
        assert!(value.get("message").is_some());
        assert!(value.get("suggestion").is_some());

        // Verify types
        assert!(value["error_code"].is_string());
        assert!(value["message"].is_string());
        assert!(value["suggestion"].is_string());
    }
}
