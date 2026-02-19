//! Canonical JSON serialization.
//!
//! Produces compact JSON with object keys sorted lexicographically at every
//! nesting level. This is required for deterministic event hashing — the
//! same logical payload must always produce the same byte sequence.
//!
//! Rules:
//! - Compact: no whitespace between tokens.
//! - Object keys sorted lexicographically (recursive at every depth).
//! - Arrays preserve element order.
//! - Numbers, strings, booleans, and null serialized normally.

use serde_json::Value;

/// Produce a canonical JSON string from a [`serde_json::Value`].
///
/// Keys at every object level are sorted lexicographically. Output is compact
/// (no extraneous whitespace).
///
/// # Examples
///
/// ```
/// use serde_json::json;
/// use bones_core::event::canonical::canonicalize_json;
///
/// let val = json!({"z": 1, "a": {"c": 3, "b": 2}});
/// assert_eq!(canonicalize_json(&val), r#"{"a":{"b":2,"c":3},"z":1}"#);
/// ```
#[must_use]
pub fn canonicalize_json(value: &Value) -> String {
    let mut buf = String::new();
    write_canonical(value, &mut buf);
    buf
}

/// Produce canonical JSON from a JSON string.
///
/// Parses the input, sorts keys, and re-serializes. Returns an error if the
/// input is not valid JSON.
///
/// # Errors
///
/// Returns `serde_json::Error` if the input string is not valid JSON.
pub fn canonicalize_json_str(json: &str) -> Result<String, serde_json::Error> {
    let value: Value = serde_json::from_str(json)?;
    Ok(canonicalize_json(&value))
}

fn write_canonical(value: &Value, buf: &mut String) {
    match value {
        Value::Null => buf.push_str("null"),
        Value::Bool(b) => {
            if *b {
                buf.push_str("true");
            } else {
                buf.push_str("false");
            }
        }
        Value::Number(n) => {
            buf.push_str(&n.to_string());
        }
        Value::String(s) => {
            // Use serde_json's string escaping for correctness
            buf.push_str(&serde_json::to_string(s).expect("string serialization cannot fail"));
        }
        Value::Array(arr) => {
            buf.push('[');
            for (i, item) in arr.iter().enumerate() {
                if i > 0 {
                    buf.push(',');
                }
                write_canonical(item, buf);
            }
            buf.push(']');
        }
        Value::Object(map) => {
            // Collect keys and sort lexicographically
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();

            buf.push('{');
            for (i, key) in keys.iter().enumerate() {
                if i > 0 {
                    buf.push(',');
                }
                // Key
                buf.push_str(
                    &serde_json::to_string(key).expect("string serialization cannot fail"),
                );
                buf.push(':');
                // Value (recursive)
                if let Some(val) = map.get(*key) {
                    write_canonical(val, buf);
                }
            }
            buf.push('}');
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn null_value() {
        assert_eq!(canonicalize_json(&json!(null)), "null");
    }

    #[test]
    fn boolean_values() {
        assert_eq!(canonicalize_json(&json!(true)), "true");
        assert_eq!(canonicalize_json(&json!(false)), "false");
    }

    #[test]
    fn integer_value() {
        assert_eq!(canonicalize_json(&json!(42)), "42");
    }

    #[test]
    fn float_value() {
        assert_eq!(canonicalize_json(&json!(3.14)), "3.14");
    }

    #[test]
    fn string_value() {
        assert_eq!(canonicalize_json(&json!("hello")), "\"hello\"");
    }

    #[test]
    fn string_with_escapes() {
        assert_eq!(
            canonicalize_json(&json!("he said \"hi\"")),
            "\"he said \\\"hi\\\"\""
        );
    }

    #[test]
    fn empty_array() {
        assert_eq!(canonicalize_json(&json!([])), "[]");
    }

    #[test]
    fn array_preserves_order() {
        assert_eq!(canonicalize_json(&json!([3, 1, 2])), "[3,1,2]");
    }

    #[test]
    fn empty_object() {
        assert_eq!(canonicalize_json(&json!({})), "{}");
    }

    #[test]
    fn object_keys_sorted() {
        let val = json!({"z": 1, "a": 2, "m": 3});
        assert_eq!(canonicalize_json(&val), r#"{"a":2,"m":3,"z":1}"#);
    }

    #[test]
    fn nested_object_keys_sorted() {
        let val = json!({"z": 1, "a": {"c": 3, "b": 2}});
        assert_eq!(
            canonicalize_json(&val),
            r#"{"a":{"b":2,"c":3},"z":1}"#
        );
    }

    #[test]
    fn deeply_nested_sorting() {
        let val = json!({
            "b": {
                "d": {
                    "f": 1,
                    "e": 2
                },
                "c": 3
            },
            "a": 4
        });
        assert_eq!(
            canonicalize_json(&val),
            r#"{"a":4,"b":{"c":3,"d":{"e":2,"f":1}}}"#
        );
    }

    #[test]
    fn array_of_objects_sorted() {
        let val = json!([{"b": 1, "a": 2}, {"d": 3, "c": 4}]);
        assert_eq!(
            canonicalize_json(&val),
            r#"[{"a":2,"b":1},{"c":4,"d":3}]"#
        );
    }

    #[test]
    fn mixed_types() {
        let val = json!({"num": 42, "str": "hello", "arr": [1, "two"], "nil": null, "bool": true});
        assert_eq!(
            canonicalize_json(&val),
            r#"{"arr":[1,"two"],"bool":true,"nil":null,"num":42,"str":"hello"}"#
        );
    }

    #[test]
    fn no_whitespace() {
        let val = json!({"key": "value"});
        let result = canonicalize_json(&val);
        // Should have no spaces, newlines, or tabs
        assert!(!result.contains(' '));
        assert!(!result.contains('\n'));
        assert!(!result.contains('\t'));
    }

    #[test]
    fn create_event_payload_canonical() {
        let val = json!({
            "title": "Fix auth retry",
            "kind": "task",
            "size": "m",
            "labels": ["backend"]
        });
        assert_eq!(
            canonicalize_json(&val),
            r#"{"kind":"task","labels":["backend"],"size":"m","title":"Fix auth retry"}"#
        );
    }

    #[test]
    fn canonicalize_json_str_valid() {
        let input = r#"{"z":1,"a":2}"#;
        let result = canonicalize_json_str(input).expect("valid JSON");
        assert_eq!(result, r#"{"a":2,"z":1}"#);
    }

    #[test]
    fn canonicalize_json_str_invalid() {
        let result = canonicalize_json_str("not json");
        assert!(result.is_err());
    }

    #[test]
    fn idempotent() {
        let val = json!({"b": 1, "a": {"d": 2, "c": 3}});
        let first = canonicalize_json(&val);
        let reparsed: serde_json::Value = serde_json::from_str(&first).expect("parse");
        let second = canonicalize_json(&reparsed);
        assert_eq!(first, second);
    }

    #[test]
    fn unicode_string() {
        let val = json!({"emoji": "🎉", "cjk": "日本語"});
        let result = canonicalize_json(&val);
        assert!(result.contains("🎉"));
        assert!(result.contains("日本語"));
    }
}
