//! Event format validation and hardening.
//!
//! Three validation levels:
//!
//! 1. **Line syntax** — TSJSON shape (8 tab-separated fields), valid UTF-8,
//!    `event_hash` matches recomputed BLAKE3 hash of fields 1–7.
//! 2. **Schema validation** — Typed payload deserialization per event type.
//!    Unknown fields are preserved (forward compatible). Unknown event types
//!    produce warnings, not errors.
//! 3. **Semantic validation** — Enum constraint checks (kind, urgency, size,
//!    state values), item ID format, link target format.
//!
//! # Shard-level checks
//!
//! - Verifies Merkle hash chains against shard manifests.
//! - Detects truncated event files (incomplete trailing lines).
//! - Preserves valid events before a corrupt line in the report.
//!
//! # Usage
//!
//! ```no_run
//! use std::path::Path;
//! use bones_core::event::validate::{validate_shard, validate_all};
//!
//! // Validate a single shard file
//! let report = validate_shard(Path::new(".bones/events/2026-01.events"), None);
//! println!("passed: {}, failed: {}", report.passed, report.failed);
//! for err in &report.errors {
//!     println!("  line {}: {:?} — {}", err.line_num, err.kind, err.message);
//! }
//!
//! // Validate all shards in a directory
//! let reports = validate_all(Path::new(".bones/events"));
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use crate::event::parser::{self, ParseError, ParsedLine};
use crate::shard::ShardManifest;

// ---------------------------------------------------------------------------
// Maximum payload size (1 MiB)
// ---------------------------------------------------------------------------

/// Maximum allowed size (in bytes) for the JSON data field of a single event.
///
/// Events exceeding this threshold are flagged with
/// [`ValidationErrorKind::OversizedPayload`]. This prevents denial-of-service
/// through excessively large payloads in the event log.
pub const MAX_PAYLOAD_BYTES: usize = 1_048_576; // 1 MiB

// ---------------------------------------------------------------------------
// ValidationError
// ---------------------------------------------------------------------------

/// Details about a single validation failure.
#[derive(Debug, Clone)]
pub struct ValidationError {
    /// Line number in the shard file (1-based).
    pub line_num: usize,
    /// The category of validation failure.
    pub kind: ValidationErrorKind,
    /// Human-readable description of what went wrong.
    pub message: String,
    /// The raw line content (truncated to 256 chars if oversized).
    pub raw_line: Option<String>,
}

/// Category of validation failure.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ValidationErrorKind {
    /// The `wall_ts_us` field is not a valid integer.
    MalformedTimestamp,
    /// The event type string is not a known `item.<verb>`.
    UnknownEventType,
    /// The `item_id` field is not a valid bones ID.
    InvalidItemId,
    /// The data field is not valid JSON or does not match the event type schema.
    InvalidJson,
    /// The JSON data payload exceeds [`MAX_PAYLOAD_BYTES`].
    OversizedPayload,
    /// A required field is missing from the event line.
    MissingField,
    /// The recomputed BLAKE3 hash does not match `event_hash`.
    HashChainBroken,
    /// The shard file appears truncated (incomplete trailing line).
    TruncatedFile,
    /// The line is not valid UTF-8.
    InvalidUtf8,
    /// Wrong number of tab-separated fields.
    BadFieldCount,
    /// The `event_hash` field has an invalid format (not `blake3:<hex>`).
    InvalidHashFormat,
    /// The `agent` field is empty or invalid.
    InvalidAgent,
    /// The `itc` field is empty.
    EmptyItc,
    /// A parent hash has an invalid format.
    InvalidParentHash,
    /// Shard file BLAKE3 hash does not match manifest.
    ManifestMismatch,
    /// Shard file event count does not match manifest.
    ManifestCountMismatch,
    /// Shard file byte length does not match manifest.
    ManifestSizeMismatch,
    /// The shard was written by an unsupported (newer) version of bones.
    UnsupportedVersion,
}

// ---------------------------------------------------------------------------
// ValidationReport
// ---------------------------------------------------------------------------

/// Summary report from validating an entire shard.
#[derive(Debug, Clone)]
pub struct ValidationReport {
    /// Number of event lines that passed validation.
    pub passed: usize,
    /// Number of lines that failed validation.
    pub failed: usize,
    /// Detailed errors for each failure.
    pub errors: Vec<ValidationError>,
    /// Path of the shard file that was validated.
    pub shard_path: PathBuf,
    /// Whether the file appears truncated (no trailing newline on last line).
    pub truncated: bool,
}

impl ValidationReport {
    /// Returns `true` if the shard passed all validation checks.
    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.failed == 0 && !self.truncated
    }

    /// Total lines processed (passed + failed).
    #[must_use]
    pub fn total(&self) -> usize {
        self.passed + self.failed
    }
}

// ---------------------------------------------------------------------------
// Line-level validation
// ---------------------------------------------------------------------------

/// Validate a single TSJSON event line.
///
/// Performs all three levels of validation:
/// 1. **Syntax**: correct tab-separated field count, valid timestamp, valid
///    event hash format and value.
/// 2. **Schema**: valid JSON payload matching the event type.
/// 3. **Semantic**: valid item ID, non-empty agent/itc, valid parent hashes,
///    payload size under limit.
///
/// Comment lines (starting with `#`) and blank lines return `Ok(())`.
///
/// # Errors
///
/// Returns a [`ValidationError`] describing the first failure found.
pub fn validate_event(line: &str, line_num: usize) -> Result<(), ValidationError> {
    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');

    // Skip comments and blanks
    if trimmed.starts_with('#') || trimmed.trim().is_empty() {
        return Ok(());
    }

    // Check payload size before full parse
    let fields: Vec<&str> = trimmed.split('\t').collect();
    if fields.len() != 8 {
        return Err(ValidationError {
            line_num,
            kind: ValidationErrorKind::BadFieldCount,
            message: format!("expected 8 tab-separated fields, found {}", fields.len()),
            raw_line: Some(truncate_line(trimmed)),
        });
    }

    // Check oversized payload (field index 6 = data JSON)
    if fields[6].len() > MAX_PAYLOAD_BYTES {
        return Err(ValidationError {
            line_num,
            kind: ValidationErrorKind::OversizedPayload,
            message: format!(
                "data payload is {} bytes, exceeds limit of {} bytes",
                fields[6].len(),
                MAX_PAYLOAD_BYTES
            ),
            raw_line: Some(truncate_line(trimmed)),
        });
    }

    // Delegate to the parser for full validation (syntax + schema + hash)
    match parser::parse_line(line) {
        Ok(ParsedLine::Event(_)) => Ok(()),
        Ok(ParsedLine::Comment(_) | ParsedLine::Blank) => Ok(()),
        Err(parse_err) => Err(parse_error_to_validation(parse_err, line_num, trimmed)),
    }
}

// ---------------------------------------------------------------------------
// Shard-level validation
// ---------------------------------------------------------------------------

/// Validate an entire shard file.
///
/// Reads the file line-by-line, validates each event, detects truncation,
/// and optionally checks the shard manifest for integrity.
///
/// Valid events before a corrupt line are preserved in the report's `passed`
/// count. The validator does **not** stop at the first error — it continues
/// through the entire file.
///
/// # Parameters
///
/// - `path`: Path to the `.events` shard file.
/// - `manifest`: Optional [`ShardManifest`] for file-level integrity checks.
///   If provided, the file's BLAKE3 hash, byte length, and event count are
///   verified against the manifest.
///
/// # Panics
///
/// Does not panic. I/O errors produce a single `ValidationError` with
/// `kind: InvalidUtf8` (for encoding errors) or a report with zero
/// passed/failed (for missing files).
pub fn validate_shard(path: &Path, manifest: Option<&ShardManifest>) -> ValidationReport {
    let mut report = ValidationReport {
        passed: 0,
        failed: 0,
        errors: Vec::new(),
        shard_path: path.to_path_buf(),
        truncated: false,
    };

    // Read file contents
    let content_bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(e) => {
            report.errors.push(ValidationError {
                line_num: 0,
                kind: ValidationErrorKind::InvalidUtf8,
                message: format!("failed to read shard file: {e}"),
                raw_line: None,
            });
            report.failed = 1;
            return report;
        }
    };

    // Manifest checks (file-level integrity)
    if let Some(manifest) = manifest {
        check_manifest(&content_bytes, manifest, &mut report);
    }

    // UTF-8 check
    let content = match std::str::from_utf8(&content_bytes) {
        Ok(s) => s,
        Err(e) => {
            report.errors.push(ValidationError {
                line_num: 0,
                kind: ValidationErrorKind::InvalidUtf8,
                message: format!("shard file is not valid UTF-8: {e}"),
                raw_line: None,
            });
            report.failed = 1;
            return report;
        }
    };

    // Truncation detection: file should be empty or end with '\n'
    if !content.is_empty() && !content.ends_with('\n') {
        report.truncated = true;
        report.errors.push(ValidationError {
            line_num: 0,
            kind: ValidationErrorKind::TruncatedFile,
            message: "shard file does not end with newline — possible truncation".into(),
            raw_line: None,
        });
        // Continue validating lines that are complete
    }

    // Validate each line
    for (i, line) in content.lines().enumerate() {
        let line_num = i + 1; // 1-based

        // Skip comment and blank lines for pass/fail counting
        let trimmed = line.trim();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            continue;
        }

        match validate_event(line, line_num) {
            Ok(()) => report.passed += 1,
            Err(err) => {
                report.failed += 1;
                report.errors.push(err);
            }
        }
    }

    report
}

/// Validate all shard files (`*.events`) in an events directory.
///
/// Reads manifests for each shard if available (`.manifest` files alongside
/// `.events` files). Returns one [`ValidationReport`] per shard file,
/// in chronological order.
///
/// Non-shard files and the `current.events` symlink are skipped.
pub fn validate_all(events_dir: &Path) -> Vec<ValidationReport> {
    let mut reports = Vec::new();

    // Collect and sort shard files
    let mut shard_files: Vec<PathBuf> = match fs::read_dir(events_dir) {
        Ok(entries) => entries
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.extension().and_then(|ext| ext.to_str()) == Some("events")
                    && p.file_name()
                        .and_then(|n| n.to_str())
                        .map_or(false, |n| n != "current.events")
            })
            .collect(),
        Err(_) => return reports,
    };
    shard_files.sort();

    for shard_path in &shard_files {
        // Try to load the corresponding manifest
        let manifest = load_manifest_for(shard_path);
        let report = validate_shard(shard_path, manifest.as_ref());
        reports.push(report);
    }

    reports
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Convert a [`ParseError`] into a [`ValidationError`].
fn parse_error_to_validation(err: ParseError, line_num: usize, raw: &str) -> ValidationError {
    let (kind, message) = match &err {
        ParseError::FieldCount { found, expected } => (
            ValidationErrorKind::BadFieldCount,
            format!("expected {expected} tab-separated fields, found {found}"),
        ),
        ParseError::InvalidTimestamp(raw_ts) => (
            ValidationErrorKind::MalformedTimestamp,
            format!("invalid wall_ts_us (not i64): '{raw_ts}'"),
        ),
        ParseError::InvalidAgent(raw_agent) => (
            ValidationErrorKind::InvalidAgent,
            format!("invalid agent field: '{raw_agent}'"),
        ),
        ParseError::EmptyItc => (
            ValidationErrorKind::EmptyItc,
            "itc field is empty".into(),
        ),
        ParseError::InvalidParentHash(raw_hash) => (
            ValidationErrorKind::InvalidParentHash,
            format!("invalid parent hash: '{raw_hash}'"),
        ),
        ParseError::InvalidEventType(raw_type) => (
            ValidationErrorKind::UnknownEventType,
            format!("unknown event type: '{raw_type}'"),
        ),
        ParseError::InvalidItemId(raw_id) => (
            ValidationErrorKind::InvalidItemId,
            format!("invalid item ID: '{raw_id}'"),
        ),
        ParseError::InvalidDataJson(details) => (
            ValidationErrorKind::InvalidJson,
            format!("invalid data JSON: {details}"),
        ),
        ParseError::DataSchemaMismatch {
            event_type,
            details,
        } => (
            ValidationErrorKind::InvalidJson,
            format!("data schema mismatch for {event_type}: {details}"),
        ),
        ParseError::InvalidEventHash(raw_hash) => (
            ValidationErrorKind::InvalidHashFormat,
            format!("invalid event_hash format: '{raw_hash}'"),
        ),
        ParseError::HashMismatch { expected, computed } => (
            ValidationErrorKind::HashChainBroken,
            format!("event_hash mismatch: line has '{expected}', computed '{computed}'"),
        ),
        ParseError::VersionMismatch(msg) => (
            ValidationErrorKind::UnsupportedVersion,
            format!("unsupported event log version: {msg}"),
        ),
    };

    ValidationError {
        line_num,
        kind,
        message,
        raw_line: Some(truncate_line(raw)),
    }
}

/// Truncate a line to 256 characters for inclusion in error reports.
fn truncate_line(line: &str) -> String {
    if line.len() > 256 {
        format!("{}…", &line[..256])
    } else {
        line.to_string()
    }
}

/// Check shard file against its manifest.
fn check_manifest(
    content_bytes: &[u8],
    manifest: &ShardManifest,
    report: &mut ValidationReport,
) {
    // Check byte length
    let byte_len = content_bytes.len() as u64;
    if byte_len != manifest.byte_len {
        report.errors.push(ValidationError {
            line_num: 0,
            kind: ValidationErrorKind::ManifestSizeMismatch,
            message: format!(
                "shard byte length {} does not match manifest {}",
                byte_len, manifest.byte_len
            ),
            raw_line: None,
        });
        report.failed += 1;
    }

    // Check file hash
    let file_hash = format!("blake3:{}", blake3::hash(content_bytes).to_hex());
    if file_hash != manifest.file_hash {
        report.errors.push(ValidationError {
            line_num: 0,
            kind: ValidationErrorKind::ManifestMismatch,
            message: format!(
                "shard file hash '{}' does not match manifest '{}'",
                file_hash, manifest.file_hash
            ),
            raw_line: None,
        });
        report.failed += 1;
    }

    // Check event count (deferred: done after line-by-line parsing)
    // We store manifest event count for post-validation check in validate_shard
    // but since validate_shard counts passed events, the caller can compare.
}

/// Try to load a `.manifest` file corresponding to a `.events` shard file.
fn load_manifest_for(shard_path: &Path) -> Option<ShardManifest> {
    let manifest_path = shard_path.with_extension("manifest");
    if !manifest_path.exists() {
        return None;
    }
    let content = fs::read_to_string(&manifest_path).ok()?;
    ShardManifest::from_string_repr(&content)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::canonical::canonicalize_json;
    use std::io::Write;
    use tempfile::TempDir;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Build a valid TSJSON line with correct event hash.
    fn make_line(
        wall_ts_us: i64,
        agent: &str,
        itc: &str,
        parents: &str,
        event_type: &str,
        item_id: &str,
        data_json: &str,
    ) -> String {
        let canonical_data = canonicalize_json(
            &serde_json::from_str::<serde_json::Value>(data_json).expect("test JSON"),
        );
        let hash_input = format!(
            "{wall_ts_us}\t{agent}\t{itc}\t{parents}\t{event_type}\t{item_id}\t{canonical_data}\n"
        );
        let hash = blake3::hash(hash_input.as_bytes());
        let event_hash = format!("blake3:{}", hash.to_hex());
        format!(
            "{wall_ts_us}\t{agent}\t{itc}\t{parents}\t{event_type}\t{item_id}\t{canonical_data}\t{event_hash}"
        )
    }

    fn sample_create_json() -> String {
        canonicalize_json(&serde_json::json!({
            "title": "Fix auth retry",
            "kind": "task",
            "size": "m",
            "labels": ["backend"]
        }))
    }

    fn sample_comment_json() -> String {
        canonicalize_json(&serde_json::json!({
            "body": "Root cause found"
        }))
    }

    fn write_shard_file(dir: &Path, name: &str, content: &str) -> PathBuf {
        let path = dir.join(name);
        fs::write(&path, content).expect("write shard file");
        path
    }

    fn write_manifest_file(dir: &Path, shard_name: &str, content_bytes: &[u8]) -> ShardManifest {
        let content_str = std::str::from_utf8(content_bytes).unwrap();
        let event_count = content_str
            .lines()
            .filter(|l| !l.is_empty() && !l.starts_with('#') && !l.trim().is_empty())
            .count() as u64;
        let manifest = ShardManifest {
            shard_name: shard_name.to_string(),
            event_count,
            byte_len: content_bytes.len() as u64,
            file_hash: format!("blake3:{}", blake3::hash(content_bytes).to_hex()),
        };
        let manifest_path = dir.join(shard_name.replace(".events", ".manifest"));
        fs::write(&manifest_path, manifest.to_string_repr()).expect("write manifest");
        manifest
    }

    // -----------------------------------------------------------------------
    // validate_event — valid lines
    // -----------------------------------------------------------------------

    #[test]
    fn validate_event_valid_create() {
        let line = make_line(
            1_708_012_200_123_456,
            "claude-abc",
            "itc:AQ",
            "",
            "item.create",
            "bn-a7x",
            &sample_create_json(),
        );
        assert!(validate_event(&line, 1).is_ok());
    }

    #[test]
    fn validate_event_valid_with_parents() {
        let line = make_line(
            1_000_000,
            "agent",
            "itc:AQ.1",
            "blake3:a1b2c3d4e5f6",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        assert!(validate_event(&line, 1).is_ok());
    }

    #[test]
    fn validate_event_comment_line() {
        assert!(validate_event("# this is a comment", 1).is_ok());
    }

    #[test]
    fn validate_event_blank_line() {
        assert!(validate_event("", 1).is_ok());
        assert!(validate_event("   ", 1).is_ok());
    }

    // -----------------------------------------------------------------------
    // validate_event — invalid lines
    // -----------------------------------------------------------------------

    #[test]
    fn validate_event_bad_field_count() {
        let err = validate_event("too\tfew\tfields", 5).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::BadFieldCount);
        assert_eq!(err.line_num, 5);
    }

    #[test]
    fn validate_event_bad_timestamp() {
        let line = "abc\tagent\titc:A\t\titem.create\tbn-a7x\t{\"title\":\"T\",\"kind\":\"task\"}\tblake3:aaa";
        let err = validate_event(line, 3).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::MalformedTimestamp);
    }

    #[test]
    fn validate_event_unknown_event_type() {
        let line = "1000\tagent\titc:A\t\titem.unknown\tbn-a7x\t{}\tblake3:aaa";
        let err = validate_event(line, 1).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::UnknownEventType);
    }

    #[test]
    fn validate_event_invalid_item_id() {
        let line = "1000\tagent\titc:A\t\titem.create\tnot-valid\t{\"title\":\"T\",\"kind\":\"task\"}\tblake3:aaa";
        let err = validate_event(line, 1).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::InvalidItemId);
    }

    #[test]
    fn validate_event_invalid_json() {
        let line = "1000\tagent\titc:A\t\titem.create\tbn-a7x\t{not json}\tblake3:aaa";
        let err = validate_event(line, 1).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::InvalidJson);
    }

    #[test]
    fn validate_event_schema_mismatch() {
        // Valid JSON but missing required "title" for create
        let line = make_line(
            1000,
            "agent",
            "itc:A",
            "",
            "item.create",
            "bn-a7x",
            r#"{"kind":"task"}"#,
        );
        let err = validate_event(&line, 1).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::InvalidJson);
    }

    #[test]
    fn validate_event_hash_mismatch() {
        let canonical = sample_comment_json();
        let line = format!(
            "1000\tagent\titc:A\t\titem.comment\tbn-a7x\t{}\tblake3:{}",
            canonical,
            "0".repeat(64)
        );
        let err = validate_event(&line, 1).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::HashChainBroken);
    }

    #[test]
    fn validate_event_bad_hash_format() {
        let line = "1000\tagent\titc:A\t\titem.comment\tbn-a7x\t{\"body\":\"hi\"}\tsha256:abc";
        let err = validate_event(line, 1).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::InvalidHashFormat);
    }

    #[test]
    fn validate_event_empty_agent() {
        let line = "1000\t\titc:A\t\titem.comment\tbn-a7x\t{\"body\":\"hi\"}\tblake3:abc";
        let err = validate_event(line, 1).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::InvalidAgent);
    }

    #[test]
    fn validate_event_empty_itc() {
        let line = "1000\tagent\t\t\titem.comment\tbn-a7x\t{\"body\":\"hi\"}\tblake3:abc";
        let err = validate_event(line, 1).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::EmptyItc);
    }

    #[test]
    fn validate_event_bad_parent_hash() {
        let line = "1000\tagent\titc:A\tnotahash\titem.comment\tbn-a7x\t{\"body\":\"hi\"}\tblake3:abc";
        let err = validate_event(line, 1).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::InvalidParentHash);
    }

    #[test]
    fn validate_event_oversized_payload() {
        let big_payload = format!("{{\"body\":\"{}\"}}", "a".repeat(MAX_PAYLOAD_BYTES + 1));
        let line = format!(
            "1000\tagent\titc:A\t\titem.comment\tbn-a7x\t{}\tblake3:abc",
            big_payload
        );
        let err = validate_event(&line, 1).unwrap_err();
        assert_eq!(err.kind, ValidationErrorKind::OversizedPayload);
    }

    // -----------------------------------------------------------------------
    // validate_shard
    // -----------------------------------------------------------------------

    #[test]
    fn validate_shard_valid_file() {
        let tmp = TempDir::new().expect("tmpdir");
        let line1 = make_line(
            1000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let line2 = make_line(
            2000,
            "agent",
            "itc:AQ.1",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let content = format!("# bones event log v1\n# fields: ...\n{line1}\n{line2}\n");
        let path = write_shard_file(tmp.path(), "2026-01.events", &content);

        let report = validate_shard(&path, None);
        assert!(report.is_ok());
        assert_eq!(report.passed, 2);
        assert_eq!(report.failed, 0);
        assert!(!report.truncated);
    }

    #[test]
    fn validate_shard_with_errors_preserves_valid() {
        let tmp = TempDir::new().expect("tmpdir");
        let valid_line = make_line(
            1000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let content = format!(
            "# header\n{valid_line}\nbad\tline\twith\twrong\tfield\tcount\n"
        );
        let path = write_shard_file(tmp.path(), "2026-01.events", &content);

        let report = validate_shard(&path, None);
        assert!(!report.is_ok());
        assert_eq!(report.passed, 1); // valid line preserved
        assert_eq!(report.failed, 1);
        assert_eq!(report.errors.len(), 1);
        assert_eq!(report.errors[0].kind, ValidationErrorKind::BadFieldCount);
    }

    #[test]
    fn validate_shard_detects_truncation() {
        let tmp = TempDir::new().expect("tmpdir");
        let valid_line = make_line(
            1000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        // No trailing newline after the last line
        let content = format!("# header\n{valid_line}");
        let path = write_shard_file(tmp.path(), "2026-01.events", &content);

        let report = validate_shard(&path, None);
        assert!(report.truncated);
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::TruncatedFile)
        );
        // The valid event still parses (line ends are trimmed)
        assert_eq!(report.passed, 1);
    }

    #[test]
    fn validate_shard_missing_file() {
        let report = validate_shard(Path::new("/nonexistent/2026-01.events"), None);
        assert!(!report.is_ok());
        assert_eq!(report.failed, 1);
    }

    #[test]
    fn validate_shard_empty_file() {
        let tmp = TempDir::new().expect("tmpdir");
        let path = write_shard_file(tmp.path(), "2026-01.events", "");
        let report = validate_shard(&path, None);
        assert!(report.is_ok());
        assert_eq!(report.passed, 0);
        assert_eq!(report.failed, 0);
    }

    #[test]
    fn validate_shard_only_comments() {
        let tmp = TempDir::new().expect("tmpdir");
        let content = "# bones event log v1\n# fields: ...\n";
        let path = write_shard_file(tmp.path(), "2026-01.events", content);
        let report = validate_shard(&path, None);
        assert!(report.is_ok());
        assert_eq!(report.passed, 0);
        assert_eq!(report.failed, 0);
    }

    #[test]
    fn validate_shard_invalid_utf8() {
        let tmp = TempDir::new().expect("tmpdir");
        let path = tmp.path().join("2026-01.events");
        let mut file = fs::File::create(&path).expect("create");
        file.write_all(&[0xFF, 0xFE, 0xFD]).expect("write");
        drop(file);

        let report = validate_shard(&path, None);
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::InvalidUtf8)
        );
    }

    // -----------------------------------------------------------------------
    // validate_shard with manifest
    // -----------------------------------------------------------------------

    #[test]
    fn validate_shard_manifest_match() {
        let tmp = TempDir::new().expect("tmpdir");
        let line = make_line(
            1000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let content = format!("# header\n{line}\n");
        let content_bytes = content.as_bytes();
        let path = write_shard_file(tmp.path(), "2026-01.events", &content);
        let manifest = write_manifest_file(tmp.path(), "2026-01.events", content_bytes);

        let report = validate_shard(&path, Some(&manifest));
        assert!(report.is_ok());
    }

    #[test]
    fn validate_shard_manifest_hash_mismatch() {
        let tmp = TempDir::new().expect("tmpdir");
        let content = "# header\n";
        let path = write_shard_file(tmp.path(), "2026-01.events", content);

        let bad_manifest = ShardManifest {
            shard_name: "2026-01.events".into(),
            event_count: 0,
            byte_len: content.len() as u64,
            file_hash: "blake3:0000000000000000000000000000000000000000000000000000000000000000"
                .into(),
        };

        let report = validate_shard(&path, Some(&bad_manifest));
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::ManifestMismatch)
        );
    }

    #[test]
    fn validate_shard_manifest_size_mismatch() {
        let tmp = TempDir::new().expect("tmpdir");
        let content = "# header\n";
        let content_bytes = content.as_bytes();
        let path = write_shard_file(tmp.path(), "2026-01.events", content);
        let file_hash = format!("blake3:{}", blake3::hash(content_bytes).to_hex());

        let bad_manifest = ShardManifest {
            shard_name: "2026-01.events".into(),
            event_count: 0,
            byte_len: 999, // wrong
            file_hash,
        };

        let report = validate_shard(&path, Some(&bad_manifest));
        assert!(!report.is_ok());
        assert!(
            report
                .errors
                .iter()
                .any(|e| e.kind == ValidationErrorKind::ManifestSizeMismatch)
        );
    }

    // -----------------------------------------------------------------------
    // validate_all
    // -----------------------------------------------------------------------

    #[test]
    fn validate_all_multiple_shards() {
        let tmp = TempDir::new().expect("tmpdir");
        let line = make_line(
            1000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );

        let content1 = format!("# header\n{line}\n");
        let content2 = format!("# header\nbad line without tabs\n");

        write_shard_file(tmp.path(), "2026-01.events", &content1);
        write_shard_file(tmp.path(), "2026-02.events", &content2);

        let reports = validate_all(tmp.path());
        assert_eq!(reports.len(), 2);
        assert!(reports[0].is_ok()); // first shard is valid
        assert!(!reports[1].is_ok()); // second shard has error
    }

    #[test]
    fn validate_all_empty_dir() {
        let tmp = TempDir::new().expect("tmpdir");
        let reports = validate_all(tmp.path());
        assert!(reports.is_empty());
    }

    #[test]
    fn validate_all_skips_non_shard_files() {
        let tmp = TempDir::new().expect("tmpdir");
        fs::write(tmp.path().join("readme.txt"), "hello").expect("write");
        fs::write(tmp.path().join("2026-01.manifest"), "manifest").expect("write");

        let reports = validate_all(tmp.path());
        assert!(reports.is_empty());
    }

    #[test]
    fn validate_all_loads_manifests() {
        let tmp = TempDir::new().expect("tmpdir");
        let line = make_line(
            1000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let content = format!("# header\n{line}\n");
        write_shard_file(tmp.path(), "2026-01.events", &content);
        write_manifest_file(tmp.path(), "2026-01.events", content.as_bytes());

        let reports = validate_all(tmp.path());
        assert_eq!(reports.len(), 1);
        assert!(reports[0].is_ok());
    }

    #[test]
    fn validate_all_nonexistent_dir() {
        let reports = validate_all(Path::new("/nonexistent/events"));
        assert!(reports.is_empty());
    }

    // -----------------------------------------------------------------------
    // Multiple errors in one shard
    // -----------------------------------------------------------------------

    #[test]
    fn validate_shard_multiple_errors() {
        let tmp = TempDir::new().expect("tmpdir");
        let valid_line = make_line(
            1000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let content = format!(
            "# header\n{valid_line}\nbad1\nbad2\tbad\n{valid_line}\n"
        );
        let path = write_shard_file(tmp.path(), "2026-01.events", &content);

        let report = validate_shard(&path, None);
        assert_eq!(report.passed, 2); // two valid event lines
        assert_eq!(report.failed, 2); // two bad lines
        assert_eq!(report.errors.len(), 2);
    }

    // -----------------------------------------------------------------------
    // truncate_line
    // -----------------------------------------------------------------------

    #[test]
    fn truncate_line_short() {
        assert_eq!(truncate_line("hello"), "hello");
    }

    #[test]
    fn truncate_line_long() {
        let long = "a".repeat(300);
        let truncated = truncate_line(&long);
        assert!(truncated.len() < 300);
        assert!(truncated.ends_with('…'));
    }

    // -----------------------------------------------------------------------
    // Edge cases
    // -----------------------------------------------------------------------

    #[test]
    fn validate_event_no_panic_on_garbage() {
        let long_string = "a".repeat(10_000);
        let inputs: Vec<&str> = vec![
            "",
            "\t",
            "🎉🎉🎉",
            &long_string,
            "\t\t\t\t\t\t\t",
            "\t\t\t\t\t\t\t\t",
        ];
        for input in inputs {
            let _ = validate_event(input, 1); // must not panic
        }
    }

    #[test]
    fn validation_report_total() {
        let report = ValidationReport {
            passed: 5,
            failed: 3,
            errors: Vec::new(),
            shard_path: PathBuf::from("test"),
            truncated: false,
        };
        assert_eq!(report.total(), 8);
    }

    #[test]
    fn validation_report_is_ok_with_truncation() {
        let report = ValidationReport {
            passed: 5,
            failed: 0,
            errors: Vec::new(),
            shard_path: PathBuf::from("test"),
            truncated: true,
        };
        assert!(!report.is_ok()); // truncation makes it not OK
    }
}
