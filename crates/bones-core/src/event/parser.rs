//! Zero-copy TSJSON line parser.
//!
//! Parses TSJSON (tab-separated fields with JSON payload) event lines into
//! [`Event`] structs or partially-parsed [`PartialEvent`] records. Designed
//! for high-throughput scanning of event shard files.
//!
//! # TSJSON Format (v1, 8-field)
//!
//! ```text
//! wall_ts_us \t agent \t itc \t parents \t type \t item_id \t data \t event_hash
//! ```
//!
//! - Comment lines start with `#` and are returned as [`ParsedLine::Comment`].
//! - Blank/whitespace-only lines are returned as [`ParsedLine::Blank`].
//! - Data lines are split on exactly 7 tab characters (yielding 8 fields).
//!
//! # Zero-copy
//!
//! [`PartialEvent`] borrows `&str` slices from the input line wherever
//! possible. Full parse ([`parse_line`]) copies into owned [`Event`] only
//! after validation succeeds.

use std::fmt;

use tracing::warn;

use crate::event::Event;
use crate::event::canonical::canonicalize_json;
use crate::event::data::EventData;
use crate::event::types::EventType;
use crate::model::item_id::ItemId;

// ---------------------------------------------------------------------------
// Shard header constants
// ---------------------------------------------------------------------------

/// The shard header line written at the top of every `.events` file.
pub const SHARD_HEADER: &str = "# bones event log v1";

/// The field comment line that follows the shard header.
pub const FIELD_COMMENT: &str = "# fields: wall_ts_us \\t agent \\t itc \\t parents \\t type \\t item_id \\t data \\t event_hash";

/// The current event log format version understood by this build of bones.
pub const CURRENT_VERSION: u32 = 1;

/// The header prefix for detecting format version.
const HEADER_PREFIX: &str = "# bones event log v";

// ---------------------------------------------------------------------------
// Error types
// ---------------------------------------------------------------------------

/// Errors that can occur while parsing a TSJSON line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Line has the wrong number of tab-separated fields.
    FieldCount {
        /// Number of fields found.
        found: usize,
        /// Expected number of fields.
        expected: usize,
    },
    /// The `wall_ts_us` field is not a valid i64.
    InvalidTimestamp(String),
    /// The `agent` field is empty or contains whitespace.
    InvalidAgent(String),
    /// The `itc` field is empty.
    EmptyItc,
    /// A parent hash has an invalid format (not `blake3:<hex>`).
    InvalidParentHash(String),
    /// The event type string is not a known `item.<verb>`.
    InvalidEventType(String),
    /// The item ID is not a valid bones ID.
    InvalidItemId(String),
    /// The data field is not valid JSON.
    InvalidDataJson(String),
    /// The data JSON does not match the expected schema for the event type.
    DataSchemaMismatch {
        /// The event type.
        event_type: String,
        /// Details of the mismatch.
        details: String,
    },
    /// The `event_hash` field has an invalid format.
    InvalidEventHash(String),
    /// The computed hash does not match `event_hash`.
    HashMismatch {
        /// Expected (from the line).
        expected: String,
        /// Computed from fields 1–7.
        computed: String,
    },
    /// The shard was written by a newer version of bones.
    ///
    /// The inner string is a human-readable upgrade message.
    VersionMismatch(String),
}

impl fmt::Display for ParseError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::FieldCount { found, expected } => {
                write!(f, "expected {expected} tab-separated fields, found {found}")
            }
            Self::InvalidTimestamp(raw) => {
                write!(f, "invalid wall_ts_us (not i64): '{raw}'")
            }
            Self::InvalidAgent(raw) => {
                write!(f, "invalid agent field: '{raw}'")
            }
            Self::EmptyItc => write!(f, "itc field is empty"),
            Self::InvalidParentHash(raw) => {
                write!(f, "invalid parent hash: '{raw}'")
            }
            Self::InvalidEventType(raw) => {
                write!(f, "unknown event type: '{raw}'")
            }
            Self::InvalidItemId(raw) => {
                write!(f, "invalid item ID: '{raw}'")
            }
            Self::InvalidDataJson(details) => {
                write!(f, "invalid data JSON: {details}")
            }
            Self::DataSchemaMismatch {
                event_type,
                details,
            } => {
                write!(f, "data schema mismatch for {event_type}: {details}")
            }
            Self::InvalidEventHash(raw) => {
                write!(f, "invalid event_hash format: '{raw}'")
            }
            Self::HashMismatch { expected, computed } => {
                write!(
                    f,
                    "event_hash mismatch: line has '{expected}', computed '{computed}'"
                )
            }
            Self::VersionMismatch(msg) => write!(f, "event log version mismatch: {msg}"),
        }
    }
}

impl std::error::Error for ParseError {}

// ---------------------------------------------------------------------------
// Version detection
// ---------------------------------------------------------------------------

/// Detect the event log format version from the first line of a shard file.
///
/// The expected header format is `# bones event log v<N>` where `N` is a
/// positive integer.
///
/// # Returns
///
/// - `Ok(version)` if the header is present and the version is ≤
///   [`CURRENT_VERSION`].
/// - `Err(message)` with an actionable upgrade instruction if the version
///   is newer than this build of bones, the header is malformed, or the
///   version number cannot be parsed.
///
/// # Forward compatibility
///
/// A version number greater than [`CURRENT_VERSION`] means this file was
/// written by a newer version of bones and may contain format changes that
/// this version cannot handle. The error message instructs the user to
/// upgrade.
///
/// # Backward compatibility
///
/// All prior format versions are guaranteed to be readable by this version.
/// Version-specific parsing is dispatched via the returned version number.
pub fn detect_version(first_line: &str) -> Result<u32, String> {
    let line = first_line.trim();
    if !line.starts_with(HEADER_PREFIX) {
        return Err(format!(
            "Invalid event log header: expected '{}N', got '{}'.\n\
             This file may not be a bones event log, or it may be from \
             a version of bones that predates format versioning.",
            HEADER_PREFIX, line
        ));
    }
    let version_str = &line[HEADER_PREFIX.len()..];
    let version: u32 = version_str.parse().map_err(|_| {
        format!(
            "Invalid version number '{}' in event log header.\n\
             Expected a positive integer after '{}'.",
            version_str, HEADER_PREFIX
        )
    })?;
    if version > CURRENT_VERSION {
        return Err(format!(
            "Event log version {} is newer than this version of bones \
             (supports up to v{}).\n\
             Please upgrade bones: cargo install bones-cli\n\
             Or download the latest release from: \
             https://github.com/bobisme/bones/releases",
            version, CURRENT_VERSION
        ));
    }
    Ok(version)
}

// ---------------------------------------------------------------------------
// Parsed output types
// ---------------------------------------------------------------------------

/// The result of parsing a single line from a TSJSON shard file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedLine {
    /// A comment line (starts with `#`). The text includes the `#` prefix.
    Comment(String),
    /// A blank or whitespace-only line.
    Blank,
    /// A successfully parsed event (boxed to reduce enum size).
    Event(Box<Event>),
}

/// A partially-parsed event that borrows from the input line.
///
/// Extracts the fixed header fields (`wall_ts_us` through `item_id`) without
/// parsing the JSON data payload or verifying the event hash. Useful for
/// filtering and scanning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PartialEvent<'a> {
    /// Wall-clock timestamp in microseconds.
    pub wall_ts_us: i64,
    /// Agent identifier.
    pub agent: &'a str,
    /// ITC clock stamp.
    pub itc: &'a str,
    /// Raw parents field (comma-separated hashes or empty).
    pub parents_raw: &'a str,
    /// Event type.
    pub event_type: EventType,
    /// Item ID (raw string, not yet validated as `ItemId`).
    pub item_id_raw: &'a str,
    /// Raw data JSON (unparsed).
    pub data_raw: &'a str,
    /// Raw event hash.
    pub event_hash_raw: &'a str,
}

/// The result of partially parsing a single line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PartialParsedLine<'a> {
    /// A comment line.
    Comment(&'a str),
    /// A blank or whitespace-only line.
    Blank,
    /// A partially-parsed event.
    Event(PartialEvent<'a>),
}

// ---------------------------------------------------------------------------
// Validation helpers
// ---------------------------------------------------------------------------

/// Validate that a string looks like a `blake3:<hex>` hash.
fn is_valid_blake3_hash(s: &str) -> bool {
    s.strip_prefix("blake3:")
        .is_some_and(|hex| !hex.is_empty() && hex.chars().all(|c| c.is_ascii_hexdigit()))
}

/// Compute the BLAKE3 event hash from the first 7 fields joined by tabs.
///
/// Hash input: `{f1}\t{f2}\t{f3}\t{f4}\t{f5}\t{f6}\t{f7}\n`
fn compute_event_hash(fields: &[&str; 7]) -> String {
    let mut input = String::new();
    for (i, field) in fields.iter().enumerate() {
        if i > 0 {
            input.push('\t');
        }
        input.push_str(field);
    }
    input.push('\n');
    let hash = blake3::hash(input.as_bytes());
    format!("blake3:{}", hash.to_hex())
}

/// Split a line on tab characters. Returns an iterator of field slices.
fn split_fields(line: &str) -> impl Iterator<Item = &str> {
    line.split('\t')
}

// ---------------------------------------------------------------------------
// Partial parse (zero-copy)
// ---------------------------------------------------------------------------

/// Parse a TSJSON line into a [`PartialParsedLine`] without deserializing
/// the JSON payload or verifying the event hash.
///
/// This is the fast path for filtering and scanning. It validates:
/// - Field count (exactly 8)
/// - `wall_ts_us` is a valid i64
/// - `event_type` is a known variant
///
/// It does **not** validate: agent format, ITC format, parent hashes,
/// item ID format, JSON validity, or event hash.
///
/// # Errors
///
/// Returns [`ParseError`] if the line cannot be parsed.
pub fn parse_line_partial(line: &str) -> Result<PartialParsedLine<'_>, ParseError> {
    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');

    // Comment line
    if trimmed.starts_with('#') {
        return Ok(PartialParsedLine::Comment(trimmed));
    }

    // Blank line
    if trimmed.trim().is_empty() {
        return Ok(PartialParsedLine::Blank);
    }

    // Split on tabs
    let fields: Vec<&str> = split_fields(trimmed).collect();
    if fields.len() != 8 {
        return Err(ParseError::FieldCount {
            found: fields.len(),
            expected: 8,
        });
    }

    // Parse wall_ts_us
    let wall_ts_us: i64 = fields[0]
        .parse()
        .map_err(|_| ParseError::InvalidTimestamp(fields[0].to_string()))?;

    // Parse event type
    let event_type: EventType = fields[4]
        .parse()
        .map_err(|_| ParseError::InvalidEventType(fields[4].to_string()))?;

    Ok(PartialParsedLine::Event(PartialEvent {
        wall_ts_us,
        agent: fields[1],
        itc: fields[2],
        parents_raw: fields[3],
        event_type,
        item_id_raw: fields[5],
        data_raw: fields[6],
        event_hash_raw: fields[7],
    }))
}

// ---------------------------------------------------------------------------
// Full parse
// ---------------------------------------------------------------------------

/// Fully parse and validate a TSJSON line into a [`ParsedLine`].
///
/// Performs all validations including:
/// - Field count (exactly 8 tab-separated fields)
/// - `wall_ts_us` is a valid i64
/// - `agent` is non-empty and contains no whitespace
/// - `itc` is non-empty
/// - `parents` are valid `blake3:<hex>` hashes (or empty)
/// - `event_type` is a known `item.<verb>`
/// - `item_id` is a valid bones ID
/// - `data` is valid JSON matching the event type schema
/// - `event_hash` is `blake3:<hex>` and matches the recomputed hash
///
/// # Errors
///
/// Returns [`ParseError`] with a specific variant for each validation failure.
pub fn parse_line(line: &str) -> Result<ParsedLine, ParseError> {
    let trimmed = line.trim_end_matches('\n').trim_end_matches('\r');

    // Comment line
    if trimmed.starts_with('#') {
        return Ok(ParsedLine::Comment(trimmed.to_string()));
    }

    // Blank line
    if trimmed.trim().is_empty() {
        return Ok(ParsedLine::Blank);
    }

    // Split on tabs
    let fields: Vec<&str> = split_fields(trimmed).collect();
    if fields.len() != 8 {
        return Err(ParseError::FieldCount {
            found: fields.len(),
            expected: 8,
        });
    }

    // --- Field 1: wall_ts_us ---
    let wall_ts_us: i64 = fields[0]
        .parse()
        .map_err(|_| ParseError::InvalidTimestamp(fields[0].to_string()))?;

    // --- Field 2: agent ---
    let agent = fields[1];
    if agent.is_empty() || agent.chars().any(|c| c == '\t' || c == '\n' || c == '\r') {
        return Err(ParseError::InvalidAgent(agent.to_string()));
    }

    // --- Field 3: itc ---
    let itc = fields[2];
    if itc.is_empty() {
        return Err(ParseError::EmptyItc);
    }

    // --- Field 4: parents ---
    let parents_raw = fields[3];
    let parents: Vec<String> = if parents_raw.is_empty() {
        Vec::new()
    } else {
        let parts: Vec<&str> = parents_raw.split(',').collect();
        for p in &parts {
            if !is_valid_blake3_hash(p) {
                return Err(ParseError::InvalidParentHash((*p).to_string()));
            }
        }
        parts.iter().map(|s| (*s).to_string()).collect()
    };

    // --- Field 5: event type ---
    let event_type: EventType = fields[4]
        .parse()
        .map_err(|_| ParseError::InvalidEventType(fields[4].to_string()))?;

    // --- Field 6: item_id ---
    let item_id =
        ItemId::parse(fields[5]).map_err(|_| ParseError::InvalidItemId(fields[5].to_string()))?;

    // --- Field 7: data (JSON) ---
    let data_json = fields[6];
    // Validate JSON syntax first
    let _: serde_json::Value =
        serde_json::from_str(data_json).map_err(|e| ParseError::InvalidDataJson(e.to_string()))?;
    // Deserialize into typed payload
    let data = EventData::deserialize_for(event_type, data_json).map_err(|e| {
        ParseError::DataSchemaMismatch {
            event_type: event_type.to_string(),
            details: e.to_string(),
        }
    })?;

    // --- Field 8: event_hash ---
    let event_hash = fields[7];
    if !is_valid_blake3_hash(event_hash) {
        return Err(ParseError::InvalidEventHash(event_hash.to_string()));
    }

    // Verify hash matches recomputed value.
    // The canonical data JSON is used for hashing (keys sorted).
    // Safety: we already validated `data_json` above so this cannot fail.
    let canonical_data = serde_json::from_str::<serde_json::Value>(data_json)
        .map(|v| canonicalize_json(&v))
        .map_err(|e| ParseError::InvalidDataJson(e.to_string()))?;
    let hash_fields: [&str; 7] = [
        fields[0],
        fields[1],
        fields[2],
        fields[3],
        fields[4],
        fields[5],
        &canonical_data,
    ];
    let computed = compute_event_hash(&hash_fields);
    if computed != event_hash {
        return Err(ParseError::HashMismatch {
            expected: event_hash.to_string(),
            computed,
        });
    }

    Ok(ParsedLine::Event(Box::new(Event {
        wall_ts_us,
        agent: agent.to_string(),
        itc: itc.to_string(),
        parents,
        event_type,
        item_id,
        data,
        event_hash: event_hash.to_string(),
    })))
}

/// Parse multiple TSJSON lines, skipping comments and blanks.
///
/// Returns a `Vec` of successfully parsed events. Stops at the first error,
/// **except** for unknown event types which are skipped with a [`tracing`]
/// warning (forward-compatibility policy: new event types may be added
/// without a format version bump).
///
/// If the first non-blank content line looks like a shard header
/// (`# bones event log v<N>`), the version is checked via [`detect_version`]
/// and an error is returned immediately if the file was written by a newer
/// version of bones.
///
/// # Errors
///
/// Returns `(line_number, ParseError)` on the first malformed data line
/// (excluding unknown event types, which are warned and skipped).
/// Line numbers are 1-indexed.
pub fn parse_lines(input: &str) -> Result<Vec<Event>, (usize, ParseError)> {
    let mut events = Vec::new();
    let mut version_checked = false;

    for (i, line) in input.lines().enumerate() {
        let line_no = i + 1;

        // Version check: the first comment line that matches the header
        // pattern triggers version validation.
        if !version_checked && line.trim_start().starts_with(HEADER_PREFIX) {
            version_checked = true;
            if let Err(msg) = detect_version(line) {
                return Err((line_no, ParseError::VersionMismatch(msg)));
            }
            continue; // header line itself is not an event
        }

        match parse_line(line) {
            Ok(ParsedLine::Event(event)) => events.push(*event),
            Ok(ParsedLine::Comment(_) | ParsedLine::Blank) => {}
            // Forward-compatible: unknown event types are skipped with a
            // warning.  This allows newer event types to be added without
            // breaking older readers (no format version bump needed).
            Err(ParseError::InvalidEventType(raw)) => {
                warn!(
                    line = line_no,
                    event_type = %raw,
                    "skipping line with unknown event type (forward-compatibility)"
                );
            }
            Err(e) => return Err((line_no, e)),
        }
    }
    Ok(events)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::canonical::canonicalize_json;
    use crate::event::data::{CreateData, MoveData};
    use crate::model::item::{Kind, Size, State, Urgency};
    use std::collections::BTreeMap;

    // -----------------------------------------------------------------------
    // Test helpers
    // -----------------------------------------------------------------------

    /// Build a valid TSJSON line with a correct event hash.
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

    fn sample_move_json() -> String {
        canonicalize_json(&serde_json::json!({
            "state": "doing"
        }))
    }

    fn sample_comment_json() -> String {
        canonicalize_json(&serde_json::json!({
            "body": "Root cause found"
        }))
    }

    // -----------------------------------------------------------------------
    // Comment and blank lines
    // -----------------------------------------------------------------------

    #[test]
    fn parse_comment_line() {
        let result = parse_line("# bones event log v1").expect("should parse");
        assert_eq!(result, ParsedLine::Comment("# bones event log v1".into()));
    }

    #[test]
    fn parse_comment_with_whitespace_prefix() {
        // Lines starting with # are comments even if the rest looks odd
        let result = parse_line("# fields: wall_ts_us \\t agent").expect("should parse");
        assert!(matches!(result, ParsedLine::Comment(_)));
    }

    #[test]
    fn parse_blank_line() {
        assert_eq!(parse_line("").expect("should parse"), ParsedLine::Blank);
        assert_eq!(parse_line("  ").expect("should parse"), ParsedLine::Blank);
        assert_eq!(parse_line("\t").expect("should parse"), ParsedLine::Blank);
    }

    #[test]
    fn parse_newline_only() {
        assert_eq!(parse_line("\n").expect("should parse"), ParsedLine::Blank);
        assert_eq!(parse_line("\r\n").expect("should parse"), ParsedLine::Blank);
    }

    // -----------------------------------------------------------------------
    // Partial parse
    // -----------------------------------------------------------------------

    #[test]
    fn partial_parse_comment() {
        let result = parse_line_partial("# comment").expect("should parse");
        assert_eq!(result, PartialParsedLine::Comment("# comment"));
    }

    #[test]
    fn partial_parse_blank() {
        let result = parse_line_partial("").expect("should parse");
        assert_eq!(result, PartialParsedLine::Blank);
    }

    #[test]
    fn partial_parse_valid_line() {
        let line = make_line(
            1_000_000,
            "agent-1",
            "itc:AQ",
            "",
            "item.create",
            "bn-a7x",
            &sample_create_json(),
        );
        let result = parse_line_partial(&line).expect("should parse");
        match result {
            PartialParsedLine::Event(pe) => {
                assert_eq!(pe.wall_ts_us, 1_000_000);
                assert_eq!(pe.agent, "agent-1");
                assert_eq!(pe.itc, "itc:AQ");
                assert_eq!(pe.parents_raw, "");
                assert_eq!(pe.event_type, EventType::Create);
                assert_eq!(pe.item_id_raw, "bn-a7x");
            }
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn partial_parse_does_not_validate_json() {
        // Partial parse should succeed even with invalid JSON in data field
        let line = "1000\tagent\titc:A\t\titem.create\tbn-a7x\tNOT_JSON\tblake3:aaa";
        let result = parse_line_partial(line).expect("should parse");
        assert!(matches!(result, PartialParsedLine::Event(_)));
    }

    #[test]
    fn partial_parse_wrong_field_count() {
        let err = parse_line_partial("a\tb\tc").expect_err("should fail");
        assert!(matches!(
            err,
            ParseError::FieldCount {
                found: 3,
                expected: 8
            }
        ));
    }

    #[test]
    fn partial_parse_bad_timestamp() {
        let line = "not_a_number\tagent\titc\t\titem.create\tbn-a7x\t{}\tblake3:abc";
        let err = parse_line_partial(line).expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidTimestamp(_)));
    }

    #[test]
    fn partial_parse_bad_event_type() {
        let line = "1000\tagent\titc\t\titem.unknown\tbn-a7x\t{}\tblake3:abc";
        let err = parse_line_partial(line).expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidEventType(_)));
    }

    // -----------------------------------------------------------------------
    // Full parse — valid lines
    // -----------------------------------------------------------------------

    #[test]
    fn parse_valid_create_event() {
        let line = make_line(
            1_708_012_200_123_456,
            "claude-abc",
            "itc:AQ",
            "",
            "item.create",
            "bn-a7x",
            &sample_create_json(),
        );
        let result = parse_line(&line).expect("should parse");
        match result {
            ParsedLine::Event(event) => {
                assert_eq!(event.wall_ts_us, 1_708_012_200_123_456);
                assert_eq!(event.agent, "claude-abc");
                assert_eq!(event.itc, "itc:AQ");
                assert!(event.parents.is_empty());
                assert_eq!(event.event_type, EventType::Create);
                assert_eq!(event.item_id.as_str(), "bn-a7x");
                match &event.data {
                    EventData::Create(d) => {
                        assert_eq!(d.title, "Fix auth retry");
                        assert_eq!(d.kind, Kind::Task);
                        assert_eq!(d.size, Some(Size::M));
                    }
                    other => panic!("expected Create data, got {other:?}"),
                }
            }
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn parse_valid_move_event_with_parent() {
        let parent_hash = "blake3:a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6a1b2c3d4e5f6abcd";
        let line = make_line(
            1_708_012_201_000_000,
            "claude-abc",
            "itc:AQ.1",
            parent_hash,
            "item.move",
            "bn-a7x",
            &sample_move_json(),
        );
        let result = parse_line(&line).expect("should parse");
        match result {
            ParsedLine::Event(event) => {
                assert_eq!(event.parents, vec![parent_hash]);
                assert_eq!(event.event_type, EventType::Move);
                match &event.data {
                    EventData::Move(d) => assert_eq!(d.state, State::Doing),
                    other => panic!("expected Move data, got {other:?}"),
                }
            }
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn parse_valid_event_with_multiple_parents() {
        let p1 = "blake3:aaaa";
        let p2 = "blake3:bbbb";
        let parents = format!("{p1},{p2}");
        let line = make_line(
            1_000,
            "agent",
            "itc:X",
            &parents,
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let result = parse_line(&line).expect("should parse");
        match result {
            ParsedLine::Event(event) => {
                assert_eq!(event.parents, vec![p1, p2]);
            }
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn parse_negative_timestamp() {
        let line = make_line(
            -1_000_000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let result = parse_line(&line).expect("should parse");
        match result {
            ParsedLine::Event(event) => assert_eq!(event.wall_ts_us, -1_000_000),
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn parse_line_with_trailing_newline() {
        let line = make_line(
            1_000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let with_newline = format!("{line}\n");
        let result = parse_line(&with_newline).expect("should parse");
        assert!(matches!(result, ParsedLine::Event(_)));
    }

    #[test]
    fn parse_line_with_crlf() {
        let line = make_line(
            1_000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let with_crlf = format!("{line}\r\n");
        let result = parse_line(&with_crlf).expect("should parse");
        assert!(matches!(result, ParsedLine::Event(_)));
    }

    // -----------------------------------------------------------------------
    // Full parse — field validation errors
    // -----------------------------------------------------------------------

    #[test]
    fn parse_wrong_field_count_too_few() {
        let err = parse_line("only\ttwo\tfields").expect_err("should fail");
        assert!(matches!(
            err,
            ParseError::FieldCount {
                found: 3,
                expected: 8
            }
        ));
    }

    #[test]
    fn parse_wrong_field_count_too_many() {
        let err = parse_line("1\t2\t3\t4\t5\t6\t7\t8\t9").expect_err("should fail");
        assert!(matches!(
            err,
            ParseError::FieldCount {
                found: 9,
                expected: 8
            }
        ));
    }

    #[test]
    fn parse_invalid_timestamp_not_number() {
        let line = "abc\tagent\titc:A\t\titem.create\tbn-a7x\t{}\tblake3:aaa";
        let err = parse_line(line).expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidTimestamp(_)));
    }

    #[test]
    fn parse_invalid_timestamp_float() {
        let line = "1.5\tagent\titc:A\t\titem.create\tbn-a7x\t{}\tblake3:aaa";
        let err = parse_line(line).expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidTimestamp(_)));
    }

    #[test]
    fn parse_empty_agent() {
        let line = "1000\t\titc:A\t\titem.create\tbn-a7x\t{}\tblake3:aaa";
        let err = parse_line(line).expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidAgent(_)));
    }

    #[test]
    fn parse_empty_itc() {
        let line = "1000\tagent\t\t\titem.create\tbn-a7x\t{}\tblake3:aaa";
        let err = parse_line(line).expect_err("should fail");
        assert!(matches!(err, ParseError::EmptyItc));
    }

    #[test]
    fn parse_invalid_parent_hash_no_prefix() {
        let line = "1000\tagent\titc:A\tabc123\titem.create\tbn-a7x\t{}\tblake3:aaa";
        let err = parse_line(line).expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidParentHash(_)));
    }

    #[test]
    fn parse_invalid_parent_hash_non_hex() {
        let line = "1000\tagent\titc:A\tblake3:xyz!\titem.create\tbn-a7x\t{}\tblake3:aaa";
        let err = parse_line(line).expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidParentHash(_)));
    }

    #[test]
    fn parse_invalid_event_type() {
        let line = "1000\tagent\titc:A\t\titem.unknown\tbn-a7x\t{}\tblake3:aaa";
        let err = parse_line(line).expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidEventType(_)));
    }

    #[test]
    fn parse_invalid_item_id() {
        let line = "1000\tagent\titc:A\t\titem.create\tnot-valid-id\t{}\tblake3:aaa";
        let err = parse_line(line).expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidItemId(_)));
    }

    #[test]
    fn parse_invalid_json() {
        let line = "1000\tagent\titc:A\t\titem.create\tbn-a7x\t{not json}\tblake3:aaa";
        let err = parse_line(line).expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidDataJson(_)));
    }

    #[test]
    fn parse_json_schema_mismatch() {
        // Valid JSON but doesn't match CreateData schema (missing title)
        let line = make_line(
            1000,
            "agent",
            "itc:A",
            "",
            "item.create",
            "bn-a7x",
            r#"{"kind":"task"}"#,
        );
        let err = parse_line(&line).expect_err("should fail");
        assert!(matches!(err, ParseError::DataSchemaMismatch { .. }));
    }

    #[test]
    fn parse_invalid_event_hash_format() {
        // Build a line manually with bad hash format
        let line = "1000\tagent\titc:A\t\titem.comment\tbn-a7x\t{\"body\":\"hi\"}\tsha256:abc";
        let err = parse_line(line).expect_err("should fail");
        assert!(matches!(err, ParseError::InvalidEventHash(_)));
    }

    #[test]
    fn parse_hash_mismatch() {
        // Valid format but wrong hash value
        let line = format!(
            "1000\tagent\titc:A\t\titem.comment\tbn-a7x\t{}\tblake3:{}",
            &sample_comment_json(),
            "0".repeat(64)
        );
        let err = parse_line(&line).expect_err("should fail");
        assert!(matches!(err, ParseError::HashMismatch { .. }));
    }

    // -----------------------------------------------------------------------
    // Round-trip with writer (conceptual — write then parse)
    // -----------------------------------------------------------------------

    #[test]
    fn roundtrip_create_event() {
        let data = CreateData {
            title: "Fix auth retry".into(),
            kind: Kind::Task,
            size: Some(Size::M),
            urgency: Urgency::Default,
            labels: vec!["backend".into()],
            parent: None,
            causation: None,
            description: None,
            extra: BTreeMap::new(),
        };

        let data_json = canonicalize_json(&serde_json::to_value(&data).expect("serialize"));

        let line = make_line(
            1_708_012_200_123_456,
            "claude-abc",
            "itc:AQ",
            "",
            "item.create",
            "bn-a7x",
            &data_json,
        );

        let parsed = parse_line(&line).expect("should parse");
        match parsed {
            ParsedLine::Event(event) => {
                assert_eq!(event.wall_ts_us, 1_708_012_200_123_456);
                assert_eq!(event.agent, "claude-abc");
                assert_eq!(event.event_type, EventType::Create);
                assert_eq!(event.data, EventData::Create(data));
            }
            other => panic!("expected Event, got {other:?}"),
        }
    }

    #[test]
    fn roundtrip_move_event() {
        let data = MoveData {
            state: State::Doing,
            reason: None,
            extra: BTreeMap::new(),
        };

        let data_json = canonicalize_json(&serde_json::to_value(&data).expect("serialize"));

        let parent_hash = "blake3:a1b2c3d4e5f6";
        let line = make_line(
            1_708_012_201_000_000,
            "agent-x",
            "itc:AQ.1",
            parent_hash,
            "item.move",
            "bn-a7x",
            &data_json,
        );

        let parsed = parse_line(&line).expect("should parse");
        match parsed {
            ParsedLine::Event(event) => {
                assert_eq!(event.parents, vec![parent_hash]);
                assert_eq!(event.event_type, EventType::Move);
                assert_eq!(event.data, EventData::Move(data));
            }
            other => panic!("expected Event, got {other:?}"),
        }
    }

    // -----------------------------------------------------------------------
    // parse_lines
    // -----------------------------------------------------------------------

    #[test]
    fn parse_lines_mixed_content() {
        let line1 = make_line(
            1_000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let line2 = make_line(
            2_000,
            "agent",
            "itc:AQ.1",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );

        let input = format!("# bones event log v1\n# fields: ...\n\n{line1}\n{line2}\n");

        let events = parse_lines(&input).expect("should parse");
        assert_eq!(events.len(), 2);
        assert_eq!(events[0].wall_ts_us, 1_000);
        assert_eq!(events[1].wall_ts_us, 2_000);
    }

    #[test]
    fn parse_lines_error_reports_line_number() {
        let good = make_line(
            1_000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let input = format!("# header\n{good}\nbad_line\n");
        let err = parse_lines(&input).expect_err("should fail");
        assert_eq!(err.0, 3); // 1-indexed line number
    }

    #[test]
    fn parse_lines_empty_input() {
        let events = parse_lines("").expect("should parse");
        assert!(events.is_empty());
    }

    // -----------------------------------------------------------------------
    // detect_version
    // -----------------------------------------------------------------------

    #[test]
    fn detect_version_valid_v1() {
        let version = detect_version("# bones event log v1").expect("should parse");
        assert_eq!(version, 1);
    }

    #[test]
    fn detect_version_with_leading_whitespace() {
        // trim() handles leading/trailing whitespace
        let version = detect_version("  # bones event log v1  ").expect("should parse");
        assert_eq!(version, 1);
    }

    #[test]
    fn detect_version_future_version_errors() {
        let err = detect_version("# bones event log v99").expect_err("should fail");
        assert!(err.contains("99"), "should mention version in error: {err}");
        assert!(
            err.to_lowercase().contains("upgrade")
                || err.to_lowercase().contains("install")
                || err.to_lowercase().contains("newer"),
            "should give upgrade advice: {err}"
        );
    }

    #[test]
    fn detect_version_invalid_header() {
        let err = detect_version("not a valid header").expect_err("should fail");
        assert!(err.contains("Invalid") || err.contains("invalid"), "{err}");
    }

    #[test]
    fn detect_version_non_numeric_version() {
        let err = detect_version("# bones event log vX").expect_err("should fail");
        assert!(!err.is_empty());
    }

    #[test]
    fn detect_version_empty_version() {
        let err = detect_version("# bones event log v").expect_err("should fail");
        assert!(!err.is_empty());
    }

    // -----------------------------------------------------------------------
    // parse_lines — version detection
    // -----------------------------------------------------------------------

    #[test]
    fn parse_lines_version_header_v1_accepted() {
        let line = make_line(
            1_000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let input = format!("# bones event log v1\n{line}\n");
        let events = parse_lines(&input).expect("v1 should be accepted");
        assert_eq!(events.len(), 1);
    }

    #[test]
    fn parse_lines_future_version_rejected() {
        let line = make_line(
            1_000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let input = format!("# bones event log v999\n{line}\n");
        let (line_no, err) = parse_lines(&input).expect_err("future version should fail");
        assert_eq!(line_no, 1);
        assert!(
            matches!(err, ParseError::VersionMismatch(_)),
            "expected VersionMismatch, got {err:?}"
        );
        let msg = err.to_string();
        assert!(msg.contains("999"), "error should mention version: {msg}");
    }

    // -----------------------------------------------------------------------
    // parse_lines — forward compatibility (unknown event types)
    // -----------------------------------------------------------------------

    #[test]
    fn parse_lines_skips_unknown_event_type() {
        // An event line with an unknown type should be warned and skipped,
        // not cause an error.
        let good_line = make_line(
            1_000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        // Construct a line with an unknown event type (simulating a future
        // event type).  We build it manually since make_line only handles
        // known types.
        let unknown_data = r#"{"body":"future"}"#;
        let canonical_unknown = serde_json::from_str::<serde_json::Value>(unknown_data)
            .map(|v| canonicalize_json(&v))
            .unwrap();
        let hash_input =
            format!("2000\tagent\titc:AQ.1\t\titem.future_type\tbn-a7x\t{canonical_unknown}\n");
        let hash = blake3::hash(hash_input.as_bytes());
        let unknown_line = format!(
            "2000\tagent\titc:AQ.1\t\titem.future_type\tbn-a7x\t{canonical_unknown}\tblake3:{}",
            hash.to_hex()
        );

        let input = format!("# bones event log v1\n{good_line}\n{unknown_line}\n");
        // Should succeed, skipping the unknown event type
        let events = parse_lines(&input).expect("unknown event type should be skipped");
        assert_eq!(events.len(), 1, "only the known event should be returned");
        assert_eq!(events[0].wall_ts_us, 1_000);
    }

    #[test]
    fn parse_lines_unknown_type_does_not_stop_parsing() {
        // Multiple unknown event types should all be skipped, and known
        // events following them should still be parsed.
        let known1 = make_line(
            1_000,
            "agent",
            "itc:AQ",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );
        let known2 = make_line(
            3_000,
            "agent",
            "itc:AQ.2",
            "",
            "item.comment",
            "bn-a7x",
            &sample_comment_json(),
        );

        // Build two unknown-type lines manually
        let unknown_data = r#"{"x":1}"#;
        let canonical_u =
            canonicalize_json(&serde_json::from_str::<serde_json::Value>(unknown_data).unwrap());
        let mk_unknown = |ts: i64, et: &str| -> String {
            let hash_input = format!("{ts}\tagent\titc:X\t\t{et}\tbn-a7x\t{canonical_u}\n");
            let hash = blake3::hash(hash_input.as_bytes());
            format!(
                "{ts}\tagent\titc:X\t\t{et}\tbn-a7x\t{canonical_u}\tblake3:{}",
                hash.to_hex()
            )
        };
        let unknown1 = mk_unknown(2_000, "item.new_future_type");
        let unknown2 = mk_unknown(2_500, "item.another_future_type");

        let input = format!("# bones event log v1\n{known1}\n{unknown1}\n{unknown2}\n{known2}\n");
        let events = parse_lines(&input).expect("should succeed skipping unknowns");
        assert_eq!(events.len(), 2, "only known events returned");
        assert_eq!(events[0].wall_ts_us, 1_000);
        assert_eq!(events[1].wall_ts_us, 3_000);
    }

    // -----------------------------------------------------------------------
    // Constants
    // -----------------------------------------------------------------------

    #[test]
    fn shard_header_constant() {
        assert_eq!(SHARD_HEADER, "# bones event log v1");
    }

    #[test]
    fn current_version_constant() {
        assert_eq!(CURRENT_VERSION, 1);
        // The SHARD_HEADER must embed the current version number.
        assert!(
            SHARD_HEADER.ends_with(&CURRENT_VERSION.to_string()),
            "SHARD_HEADER '{SHARD_HEADER}' must end with CURRENT_VERSION {CURRENT_VERSION}"
        );
    }

    #[test]
    fn field_comment_constant() {
        assert!(FIELD_COMMENT.starts_with("# fields:"));
        assert!(FIELD_COMMENT.contains("wall_ts_us"));
        assert!(FIELD_COMMENT.contains("event_hash"));
    }

    // -----------------------------------------------------------------------
    // is_valid_blake3_hash
    // -----------------------------------------------------------------------

    #[test]
    fn valid_blake3_hashes() {
        assert!(is_valid_blake3_hash("blake3:abcdef0123456789"));
        assert!(is_valid_blake3_hash("blake3:a"));
        assert!(is_valid_blake3_hash(&format!("blake3:{}", "0".repeat(64))));
    }

    #[test]
    fn invalid_blake3_hashes() {
        assert!(!is_valid_blake3_hash("blake3:")); // empty hex
        assert!(!is_valid_blake3_hash("sha256:abc")); // wrong prefix
        assert!(!is_valid_blake3_hash("abc123")); // no prefix
        assert!(!is_valid_blake3_hash("blake3:xyz!")); // non-hex chars
        assert!(!is_valid_blake3_hash("")); // empty
    }

    // -----------------------------------------------------------------------
    // compute_event_hash
    // -----------------------------------------------------------------------

    #[test]
    fn compute_hash_deterministic() {
        let fields: [&str; 7] = ["1000", "agent", "itc:A", "", "item.create", "bn-a7x", "{}"];
        let h1 = compute_event_hash(&fields);
        let h2 = compute_event_hash(&fields);
        assert_eq!(h1, h2);
        assert!(h1.starts_with("blake3:"));
    }

    #[test]
    fn compute_hash_changes_with_different_fields() {
        let fields1: [&str; 7] = ["1000", "agent", "itc:A", "", "item.create", "bn-a7x", "{}"];
        let fields2: [&str; 7] = ["2000", "agent", "itc:A", "", "item.create", "bn-a7x", "{}"];
        assert_ne!(compute_event_hash(&fields1), compute_event_hash(&fields2));
    }

    // -----------------------------------------------------------------------
    // Error Display
    // -----------------------------------------------------------------------

    #[test]
    fn error_display_field_count() {
        let err = ParseError::FieldCount {
            found: 3,
            expected: 8,
        };
        let msg = err.to_string();
        assert!(msg.contains("8"));
        assert!(msg.contains("3"));
    }

    #[test]
    fn error_display_hash_mismatch() {
        let err = ParseError::HashMismatch {
            expected: "blake3:aaa".into(),
            computed: "blake3:bbb".into(),
        };
        let msg = err.to_string();
        assert!(msg.contains("aaa"));
        assert!(msg.contains("bbb"));
    }

    // -----------------------------------------------------------------------
    // All 11 event types parse successfully
    // -----------------------------------------------------------------------

    #[test]
    fn parse_all_event_types() {
        let test_cases = vec![
            ("item.create", r#"{"title":"T","kind":"task"}"#),
            ("item.update", r#"{"field":"title","value":"New"}"#),
            ("item.move", r#"{"state":"doing"}"#),
            ("item.assign", r#"{"agent":"alice","action":"assign"}"#),
            ("item.comment", r#"{"body":"Hello"}"#),
            ("item.link", r#"{"target":"bn-b8y","link_type":"blocks"}"#),
            ("item.unlink", r#"{"target":"bn-b8y"}"#),
            ("item.delete", r#"{}"#),
            ("item.compact", r#"{"summary":"TL;DR"}"#),
            ("item.snapshot", r#"{"state":{"id":"bn-a7x"}}"#),
            (
                "item.redact",
                r#"{"target_hash":"blake3:abc","reason":"oops"}"#,
            ),
        ];

        for (event_type, data_json) in test_cases {
            let line = make_line(1000, "agent", "itc:AQ", "", event_type, "bn-a7x", data_json);
            let result = parse_line(&line);
            assert!(
                result.is_ok(),
                "failed to parse {event_type}: {:?}",
                result.err()
            );
            match result.expect("just checked") {
                ParsedLine::Event(event) => {
                    assert_eq!(event.event_type.as_str(), event_type);
                }
                other => panic!("expected Event for {event_type}, got {other:?}"),
            }
        }
    }

    // -----------------------------------------------------------------------
    // No panics on adversarial input
    // -----------------------------------------------------------------------

    #[test]
    fn no_panic_on_garbage() {
        let long_string = "a".repeat(10_000);
        let inputs = vec![
            "",
            "\t",
            "\t\t\t\t\t\t\t",
            "\t\t\t\t\t\t\t\t",
            "🎉🎉🎉",
            "\0\0\0",
            &long_string,
            "1\t2\t3\t4\t5\t6\t7\t8",
            "-1\t\t\t\t\t\t\t",
        ];

        for input in inputs {
            // Should not panic, errors are fine
            let _ = parse_line(input);
            let _ = parse_line_partial(input);
        }
    }
}
