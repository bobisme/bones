//! How an `item.update` value writes its field (bn-18fs).
//!
//! The CRDT (`WorkItemState`) and the `SQLite` projection must read every
//! update value the same way, or a snapshot and the projection disagree.
//! Both use these functions.
//!
//! # Rule
//!
//! A malformed value is **no write**: the event does not claim the field,
//! so the field keeps its earlier value. This protects items from buggy or
//! foreign writers.
//!
//! - `title`: a string is written (`""` included). Anything else is no write.
//! - `kind`, `urgency`: a string that parses is written. Anything else is no
//!   write.
//! - `description`: a string is written, `""` meaning no description. JSON
//!   `null` clears it. Anything else is no write.
//! - `size`: a string that parses is written. `null` clears it. Anything
//!   else (an unknown size included) is no write.
//! - `parent`: an item ID (see [`is_item_ref`]) is written. `null` and `""`
//!   clear it. Anything else is no write.
//!
//! In each function, `None` means no write and `Some(None)` means clear.
//!
//! Set members (a label, an assignee, a link type) that are blank (see
//! [`is_blank_member`]) are no write too: the schema rejects them.
//!
//! A link whose target the projection cannot hold (see [`can_link`]) is no
//! write. Its removal has no effect, as no such link exists.
//!
//! # Forward compatibility
//!
//! A newer client can add a kind, urgency or size value (for example a kind
//! `"epic"`). This bn does not parse it, so an update to that value is no
//! write in both the projection and `WorkItemState`: the field keeps the
//! value of the newest write this bn can read. A snapshot that holds such a
//! value does not deserialize as a `SnapshotPayload`, so the projection
//! keeps it as JSON only.

use serde_json::Value;

use crate::model::item::{Kind, Size, Urgency};

/// Title an update writes, or `None` for no write.
#[must_use]
pub fn title(value: &Value) -> Option<&str> {
    value.as_str()
}

/// Kind an update writes, or `None` for no write.
#[must_use]
pub fn kind(value: &Value) -> Option<Kind> {
    value.as_str().and_then(|s| s.parse().ok())
}

/// Urgency an update writes, or `None` for no write.
#[must_use]
pub fn urgency(value: &Value) -> Option<Urgency> {
    value.as_str().and_then(|s| s.parse().ok())
}

/// Description an update writes: `Some(None)` clears, `None` is no write.
#[must_use]
pub fn description(value: &Value) -> Option<Option<&str>> {
    match value {
        Value::Null => Some(None),
        Value::String(s) => Some(Some(s.as_str()).filter(|s| !s.is_empty())),
        _ => None,
    }
}

/// Size an update writes: `Some(None)` clears, `None` is no write.
#[must_use]
pub fn size(value: &Value) -> Option<Option<Size>> {
    match value {
        Value::Null => Some(None),
        Value::String(s) => s.parse().ok().map(Some),
        _ => None,
    }
}

/// Parent an update writes: `Some(None)` clears, `None` is no write.
#[must_use]
pub fn parent(value: &Value) -> Option<Option<&str>> {
    match value {
        Value::Null => Some(None),
        Value::String(s) if s.is_empty() => Some(None),
        Value::String(s) if is_item_ref(s) => Some(Some(s.as_str())),
        _ => None,
    }
}

/// Parent of a create or snapshot: an item ID, or none.
///
/// A create writes every field, so a malformed parent there clears it.
#[must_use]
pub fn parent_or_none(parent: Option<&str>) -> Option<&str> {
    parent.filter(|p| is_item_ref(p))
}

/// `true` when the projection schema rejects `member` as a label, an
/// assignee or a link type: `length(trim(member)) > 0` fails.
///
/// This follows `SQLite` exactly. `trim` removes spaces only, and `length`
/// of a text value counts the characters before the first NUL. So a member
/// that starts with a NUL after its spaces is blank too.
#[must_use]
pub fn is_blank_member(member: &str) -> bool {
    let trimmed = member.trim_matches(' ');
    trimmed.is_empty() || trimmed.starts_with('\0')
}

/// `true` when the projection can hold a link from `item_id` to `target`:
/// the target is an item ID (see [`is_item_ref`]) and not the item itself.
///
/// The schema rejects other links (the `items` ID CHECK of the target's
/// placeholder and `item_id <> depends_on_item_id`). The projection claims
/// them but writes no row, and `WorkItemState` ignores them.
#[must_use]
pub fn can_link(item_id: &str, target: &str) -> bool {
    is_item_ref(target) && target != item_id
}

/// `true` when `id` can be an `items.item_id`: two or three lowercase ASCII
/// letters, then `-` (the projection schema's CHECK constraint).
#[must_use]
pub fn is_item_ref(id: &str) -> bool {
    let prefix = id.bytes().take_while(u8::is_ascii_lowercase).count();
    (prefix == 2 || prefix == 3) && id.as_bytes().get(prefix) == Some(&b'-')
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn malformed_values_are_no_write() {
        for bad in [json!(5), json!(true), json!({"x": 1}), json!(["a"])] {
            assert_eq!(title(&bad), None);
            assert_eq!(kind(&bad), None);
            assert_eq!(urgency(&bad), None);
            assert_eq!(description(&bad), None);
            assert_eq!(size(&bad), None);
            assert_eq!(parent(&bad), None);
        }
        assert_eq!(title(&Value::Null), None);
        assert_eq!(kind(&Value::Null), None);
        assert_eq!(kind(&json!("epic")), None);
        assert_eq!(urgency(&json!("")), None);
        assert_eq!(size(&json!("mega")), None);
        assert_eq!(parent(&json!("not an id")), None);
    }

    #[test]
    fn clears_and_writes() {
        assert_eq!(title(&json!("")), Some(""));
        assert_eq!(kind(&json!("bug")), Some(Kind::Bug));
        assert_eq!(description(&Value::Null), Some(None));
        assert_eq!(description(&json!("")), Some(None));
        assert_eq!(description(&json!("d")), Some(Some("d")));
        assert_eq!(size(&Value::Null), Some(None));
        assert_eq!(size(&json!("m")), Some(Some(Size::M)));
        assert_eq!(parent(&Value::Null), Some(None));
        assert_eq!(parent(&json!("")), Some(None));
        assert_eq!(parent(&json!("bn-x1")), Some(Some("bn-x1")));
        assert_eq!(parent_or_none(Some("x")), None);
    }

    #[test]
    fn blank_members_follow_the_schema_check() {
        assert!(is_blank_member(""));
        assert!(is_blank_member("   "));
        assert!(!is_blank_member("\t"));
        assert!(!is_blank_member(" a "));
        assert!(is_blank_member("\u{0}x"));
        assert!(is_blank_member(" \u{0}"));
        assert!(is_blank_member("  \u{0} x"));
        assert!(!is_blank_member("x\u{0}"));
        assert!(!is_blank_member("a"));
    }

    /// `is_blank_member` must agree with the schema's CHECK in `SQLite`
    /// itself, for every member a handler can write.
    #[test]
    fn blank_members_match_sqlite() {
        let conn = rusqlite::Connection::open_in_memory().expect("open db");
        conn.execute_batch("CREATE TABLE t (m TEXT NOT NULL CHECK (length(trim(m)) > 0))")
            .expect("create table");
        for member in [
            "",
            " ",
            "  ",
            "a",
            " a ",
            "\t",
            "\n",
            "\u{0}",
            "\u{0}x",
            " \u{0}",
            "\u{0} ",
            "x\u{0}",
            " x\u{0}",
            "  \u{0} x",
            "\u{a0}",
            "\u{3000}",
            "é",
        ] {
            let accepted = conn
                .execute("INSERT INTO t (m) VALUES (?1)", [member])
                .is_ok();
            assert_eq!(
                is_blank_member(member),
                !accepted,
                "member {member:?}: SQLite accepted = {accepted}"
            );
        }
    }

    #[test]
    fn links_follow_the_schema_checks() {
        assert!(can_link("bn-a1", "bn-b2"));
        assert!(!can_link("bn-a1", "bn-a1"));
        assert!(!can_link("bn-a1", "not an id"));
        assert!(!can_link("bn-a1", ""));
        assert!(!can_link("bn-a1", "BN-B2"));
    }

    #[test]
    fn item_refs_follow_the_schema_check() {
        assert!(is_item_ref("bn-a1"));
        assert!(is_item_ref("abc-"));
        assert!(!is_item_ref("b-1"));
        assert!(!is_item_ref("abcd-1"));
        assert!(!is_item_ref("BN-1"));
        assert!(!is_item_ref(""));
    }
}
