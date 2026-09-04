//! Canonical form (`spec/HACP-2.0-draft.md` §5.1).
//!
//! Every digest in HACP/2.0 — contract revision digests, artifact digests, evidence
//! digests — is taken over the canonical form defined here. Two independent
//! implementations must derive byte-identical canonical bytes from the same logical
//! value, or their digests cannot be compared; that requirement is why this module
//! exists before any wire type does, and why each rule below is pinned by a test
//! rather than by the serializer's mood.
//!
//! The rules, normative in §5.1:
//!
//! * UTF-8, no BOM, no whitespace between tokens.
//! * Object members sorted by key, byte-wise over the UTF-8 encoding, ascending.
//! * Numbers are integers only. Non-integers are forbidden in any digested object.
//! * Strings escape only `"` `\` and control characters below 0x20; controls use
//!   `\uXXXX` with lowercase hex; everything else is literal UTF-8.
//! * Timestamps are RFC 3339, UTC (`Z`), seconds precision: `YYYY-MM-DDTHH:MM:SSZ`.
//! * Digest: SHA-256 over the canonical UTF-8 bytes, 64 lowercase hex characters.

use serde_json::Value;
use sha2::{Digest, Sha256};

/// A canonical-form violation. Carries the JSON path where it was found, because a
/// digest input that cannot be canonicalized is a wire bug, and a wire bug that names
/// its location is half-fixed.
#[derive(Debug, thiserror::Error, PartialEq)]
pub enum CanonError {
    /// A non-integer number appeared at `path`. Quantities that are naturally
    /// fractional travel as strings with explicit units (§5.1).
    #[error("non-integer number {value} at {path}: canonical form carries integers only")]
    NonIntegerNumber { value: String, path: String },
    /// A timestamp field is not in canonical shape.
    #[error("timestamp {found:?} at {path} is not canonical: expected YYYY-MM-DDTHH:MM:SSZ")]
    BadTimestamp { found: String, path: String },
}

/// Render a JSON value to its canonical form.
pub fn canonical_json(value: &Value) -> Result<String, CanonError> {
    let mut out = String::new();
    write_value(value, "$", &mut out)?;
    Ok(out)
}

/// The protocol digest of a JSON value: SHA-256 over its canonical form, as 64
/// lowercase hex characters.
pub fn digest_of(value: &Value) -> Result<String, CanonError> {
    let canonical = canonical_json(value)?;
    Ok(digest_canonical(&canonical))
}

/// SHA-256 over bytes that are already canonical, as 64 lowercase hex characters.
/// Exposed separately because artifact digests are taken over file *content*, which
/// is canonical by being bytes, not by being JSON.
pub fn digest_canonical(canonical: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    hex_lower(&hasher.finalize())
}

/// The current instant as a canonical timestamp.
pub fn canonical_now() -> String {
    chrono::Utc::now().format("%Y-%m-%dT%H:%M:%SZ").to_string()
}

/// Validate the canonical timestamp shape: exactly `YYYY-MM-DDTHH:MM:SSZ`, a real
/// UTC date-time. Anything finer (fractions) or looser (offsets) is rejected —
/// producers must not emit it and digests must not see it.
pub fn validate_timestamp(s: &str) -> Result<(), CanonError> {
    let err = || CanonError::BadTimestamp {
        found: s.to_string(),
        path: "<timestamp argument>".to_string(),
    };
    let bytes = s.as_bytes();
    if bytes.len() != 20
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || bytes[10] != b'T'
        || bytes[13] != b':'
        || bytes[16] != b':'
        || bytes[19] != b'Z'
    {
        return Err(err());
    }
    if !bytes[..4]
        .iter()
        .chain(bytes[5..7].iter())
        .chain(bytes[8..10].iter())
        .chain(bytes[11..13].iter())
        .chain(bytes[14..16].iter())
        .chain(bytes[17..19].iter())
        .all(|b| b.is_ascii_digit())
    {
        return Err(err());
    }
    // Shape is right; the date itself must exist (no February 31st, no hour 25).
    // The trailing Z is a literal in the format, not parsed as an offset.
    chrono::NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%SZ")
        .map(|_| ())
        .map_err(|_| err())
}

fn write_value(value: &Value, path: &str, out: &mut String) -> Result<(), CanonError> {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(true) => out.push_str("true"),
        Value::Bool(false) => out.push_str("false"),
        Value::Number(n) => write_number(n, path, out)?,
        Value::String(s) => write_string(s, out),
        Value::Array(items) => {
            out.push('[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_value(item, &format!("{path}[{i}]"), out)?;
            }
            out.push(']');
        }
        Value::Object(map) => {
            // Order is the value's order sorted by key bytes, never insertion order:
            // two producers building the same object through different code paths
            // must agree byte-for-byte.
            let mut pairs: Vec<(&String, &Value)> = map.iter().collect();
            pairs.sort_by(|a, b| a.0.as_bytes().cmp(b.0.as_bytes()));
            out.push('{');
            for (i, (key, item)) in pairs.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_string(key, out);
                out.push(':');
                write_value(item, &format!("{path}.{key}"), out)?;
            }
            out.push('}');
        }
    }
    Ok(())
}

fn write_number(n: &serde_json::Number, path: &str, out: &mut String) -> Result<(), CanonError> {
    // Integers only (§5.1). `as_u64` first so positives render unsigned; `as_i64`
    // catches negatives; anything else — floats, exponent notation, out-of-range —
    // is a canonical-form violation, not a rounding decision.
    if let Some(u) = n.as_u64() {
        out.push_str(&u.to_string());
        Ok(())
    } else if let Some(i) = n.as_i64() {
        out.push_str(&i.to_string());
        Ok(())
    } else {
        Err(CanonError::NonIntegerNumber {
            value: n.to_string(),
            path: path.to_string(),
        })
    }
}

fn write_string(s: &str, out: &mut String) {
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            // Every control below 0x20 takes the long escape, including the ones
            // JSON gives short forms to: one rule, no judgment calls.
            '\u{00}'..='\u{1F}' => out.push_str(&format!("\\u{:04x}", c as u32)),
            _ => out.push(c),
        }
    }
    out.push('"');
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(HEX[(b >> 4) as usize] as char);
        s.push(HEX[(b & 0x0F) as usize] as char);
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // Digest vectors computed independently of this implementation (python3
    // hashlib) and pinned here: if canonical bytes drift by one byte, the digest
    // moves and this test says so.
    #[test]
    fn digest_vectors_pinned() {
        assert_eq!(
            digest_of(&json!({})).unwrap(),
            "44136fa355b3678a1146ad16f7e8649e94fb4fc21fe77e8310c060f61caaff8a"
        );
        assert_eq!(
            digest_of(&json!({"b": 2, "a": 1})).unwrap(),
            "43258cff783fe7036d8a43033f830adfc60ec037382473548ac742b888292777"
        );
        assert_eq!(
            digest_of(&json!([1, 2, 3])).unwrap(),
            "a615eeaee21de5179de080de8c3052c8da901138406ba71c38c032845f7d54f4"
        );
    }

    #[test]
    fn no_whitespace_anywhere() {
        assert_eq!(
            canonical_json(&json!({"a": [1, {"b": null, "c": true, "d": false}]})).unwrap(),
            r#"{"a":[1,{"b":null,"c":true,"d":false}]}"#
        );
    }

    #[test]
    fn object_keys_sort_by_utf8_bytes_regardless_of_insertion() {
        assert_eq!(
            canonical_json(&json!({"b": 1, "a": 2, "A": 3})).unwrap(),
            r#"{"A":3,"a":2,"b":1}"#
        );
        // Byte-wise, not code-point-wise by locale: "z" (0x7A) sorts before
        // "é" (0xC3 0xA9), and both sort before "中".
        assert_eq!(
            canonical_json(&json!({"é": 1, "z": 2, "中": 3})).unwrap(),
            r#"{"z":2,"é":1,"中":3}"#
        );
    }

    #[test]
    fn integers_render_as_plain_decimal() {
        assert_eq!(canonical_json(&json!(0)).unwrap(), "0");
        assert_eq!(canonical_json(&json!(-7)).unwrap(), "-7");
        assert_eq!(canonical_json(&json!(1_000_000)).unwrap(), "1000000");
    }

    #[test]
    fn non_integers_are_rejected_not_rounded() {
        for bad in [json!(1.5), json!(-0.5), json!(1e2), json!(1.0)] {
            let err = canonical_json(&bad).unwrap_err();
            assert!(matches!(err, CanonError::NonIntegerNumber { .. }), "got: {err}");
        }
        // The error names where, so a wire bug is findable.
        let err = canonical_json(&json!({"outer": {"inner": 0.25}})).unwrap_err();
        assert_eq!(err.to_string().contains("$.outer.inner"), true, "got: {err}");
    }

    #[test]
    fn strings_escape_the_minimum_and_lowercase_the_controls() {
        let s = "a\"b\\c\nd\te";
        assert_eq!(
            canonical_json(&json!(s)).unwrap(),
            r#""a\"b\\c\u000ad\u0009e""#
        );
        // Non-ASCII is literal UTF-8, never \u-escaped.
        assert_eq!(canonical_json(&json!("café")).unwrap(), "\"café\"");
    }

    #[test]
    fn canonical_timestamps_pass_and_everything_else_fails() {
        validate_timestamp("2026-09-04T11:29:11Z").unwrap();
        validate_timestamp(&canonical_now()).unwrap();
        for bad in [
            "2026-09-04T11:29:11.012514Z", // fractional seconds
            "2026-09-04T11:29:11+00:00",   // offset, not Z
            "2026-09-04 11:29:11Z",        // space, not T
            "2026-13-04T11:29:11Z",        // month 13 does not exist
            "2026-09-04T25:29:11Z",        // hour 25 does not exist
            "2026-09-04T11:29:11",         // missing Z
        ] {
            assert!(validate_timestamp(bad).is_err(), "accepted: {bad}");
        }
    }

    #[test]
    fn array_order_is_preserved_never_sorted() {
        assert_eq!(
            canonical_json(&json!([3, 1, 2])).unwrap(),
            "[3,1,2]"
        );
    }

    #[test]
    fn canonical_form_is_idempotent() {
        let value = json!({"z": [1, {"k": "v"}], "a": {"b": -2}});
        let once = canonical_json(&value).unwrap();
        let reparsed: Value = serde_json::from_str(&once).unwrap();
        assert_eq!(canonical_json(&reparsed).unwrap(), once);
    }
}
