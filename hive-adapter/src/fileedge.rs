//! The file edge (`spec/HACP.md` §13.2) — the only part of the protocol a worker sees.
//!
//! ```text
//!   BRIEF.md                     the worker's prompt (written by the coordinator)
//!   INBOX/<seq:06>-<kind>.json   adapter-written, one file per message, ordered by name
//!   OUTBOX/<id>-<kind>.json      worker-written: {"kind": "...", "body": {...}}
//!   REPORT.json                  worker-written (SHOULD)
//! ```
//!
//! The rule that governs this module is §2's content-blindness: the adapter validates
//! the **shape** of what the worker wrote and never its **meaning**. A body is moved
//! from a file into an envelope byte for byte. If a worker writes a body the arbiter
//! will hate, the arbiter — a named agent — is who says so, and that attribution is the
//! reason the protocol is auditable.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use hacp::envelope::urn;
use hacp::Envelope;
use serde_json::Value;

use crate::transport::MAX_ENVELOPE_BYTES;

/// A worker-written outbox file, after shape validation.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxItem {
    pub kind: String,
    pub body: Value,
    /// Optional recipient. §13.2 has the worker write `{kind, body}` and the adapter
    /// fill `to`, but §6 also requires a worker to be able to address a peer directly,
    /// and only the worker knows which peer it means. So `to` is accepted when present
    /// and defaulted to the coordinator when absent. Checking that it is a well-formed
    /// actor URN is shape validation; the coordinator still enforces who may say what.
    pub to: Option<String>,
    pub in_reply_to: Option<String>,
}

/// Why an outbox file was refused. Every variant is a statement about shape.
///
/// Hand-written `Display` rather than a derive: this crate's dependency set is frozen
/// and deliberately small, and one error enum does not justify widening it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ShapeError {
    NotJson(String),
    NotObject,
    Missing(&'static str),
    WrongType { field: &'static str, expected: &'static str },
    Empty(&'static str),
    AdapterOwned(&'static str),
    BadActor(String),
    TooLarge { bytes: usize, max: usize },
}

impl std::fmt::Display for ShapeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ShapeError::NotJson(e) => write!(f, "not JSON: {e}"),
            ShapeError::NotObject => f.write_str(
                "the top level must be a JSON object of the form {\"kind\": ..., \"body\": {...}}",
            ),
            ShapeError::Missing(field) => write!(f, "missing required field {field:?}"),
            ShapeError::WrongType { field, expected } => {
                write!(f, "field {field:?} must be {expected}")
            }
            ShapeError::Empty(field) => write!(f, "field {field:?} must not be empty"),
            ShapeError::AdapterOwned(field) => {
                write!(f, "field {field:?} is the adapter's to fill, not the worker's")
            }
            ShapeError::BadActor(urn) => write!(f, "{urn:?} is not a valid HACP actor URN"),
            ShapeError::TooLarge { bytes, max } => write!(
                f,
                "the envelope would be {bytes} bytes; the spec caps it at {max}. \
                 Reference artifacts by path and digest instead of embedding them"
            ),
        }
    }
}

impl std::error::Error for ShapeError {}

/// Fields the adapter owns. A worker writing one of these is either confused or trying
/// to speak as someone else; §13.3 makes impersonation the coordinator's job to reject,
/// but there is no reason to carry the attempt that far.
const ADAPTER_OWNED: &[&str] = &["protocol", "message_id", "run_id", "from", "timestamp"];

/// Validate one outbox file's shape. Never inspects the body's meaning.
pub fn parse_outbox(raw: &str) -> Result<OutboxItem, ShapeError> {
    let value: Value = serde_json::from_str(raw).map_err(|e| ShapeError::NotJson(e.to_string()))?;
    let Value::Object(map) = value else {
        return Err(ShapeError::NotObject);
    };

    for owned in ADAPTER_OWNED {
        if map.contains_key(*owned) {
            return Err(ShapeError::AdapterOwned(owned));
        }
    }

    let kind = match map.get("kind") {
        None => return Err(ShapeError::Missing("kind")),
        Some(Value::String(s)) if s.is_empty() => return Err(ShapeError::Empty("kind")),
        Some(Value::String(s)) => s.clone(),
        Some(_) => return Err(ShapeError::WrongType { field: "kind", expected: "a string" }),
    };

    // §5 says an unregistered kind is delivered, not rejected, so membership in the
    // registry is deliberately not checked here.
    let body = match map.get("body") {
        None => return Err(ShapeError::Missing("body")),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err(ShapeError::WrongType { field: "body", expected: "a JSON object" }),
    };

    let to = match map.get("to") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if urn::is_neutral(s) => Some(s.clone()),
        Some(Value::String(s)) => return Err(ShapeError::BadActor(s.clone())),
        Some(_) => return Err(ShapeError::WrongType { field: "to", expected: "a string URN" }),
    };

    let in_reply_to = match map.get("in_reply_to") {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) if s.is_empty() => return Err(ShapeError::Empty("in_reply_to")),
        Some(Value::String(s)) => Some(s.clone()),
        Some(_) => return Err(ShapeError::WrongType { field: "in_reply_to", expected: "a string" }),
    };

    Ok(OutboxItem { kind, body, to, in_reply_to })
}

/// Wrap a validated outbox item in an envelope, filling exactly the metadata §13.2
/// gives the adapter: `protocol`, `message_id`, `run_id`, `from`, `to`, `timestamp`.
///
/// `message_id` is passed in rather than generated here so a retry after a network
/// failure reuses the id of the attempt that may already have landed; the coordinator
/// then recognizes it as a duplicate instead of storing the same work twice.
pub fn envelope_for(
    item: &OutboxItem,
    run_id: &str,
    from: &str,
    default_to: &str,
    message_id: String,
) -> Result<Envelope, ShapeError> {
    let to = item.to.clone().unwrap_or_else(|| default_to.to_string());
    let mut envelope = Envelope::new(run_id, from, to, item.kind.clone(), item.body.clone());
    envelope.message_id = message_id;
    if let Some(reply) = &item.in_reply_to {
        envelope.in_reply_to = Some(reply.clone());
    }
    let bytes = serde_json::to_vec(&envelope).map(|v| v.len()).unwrap_or(0);
    if bytes > MAX_ENVELOPE_BYTES {
        return Err(ShapeError::TooLarge { bytes, max: MAX_ENVELOPE_BYTES });
    }
    Ok(envelope)
}

/// The INBOX filename for a message: zero-padded sequence, then kind, so that ordinary
/// name order is the run's total order (§13.2).
///
/// The kind is sanitized because it arrives from the bus and reaches a path here. A
/// kind of `../../.ssh/authorized_keys` must become a filename, not an escape.
pub fn inbox_filename(seq: u64, kind: &str) -> String {
    format!("{seq:06}-{}.json", sanitize(kind))
}

fn sanitize(kind: &str) -> String {
    let mut cleaned = String::with_capacity(kind.len());
    for c in kind.chars() {
        let c = if c.is_ascii_alphanumeric() || c == '.' || c == '-' || c == '_' { c } else { '_' };
        // Dots are legal in a kind (`contract.amendment.accepted`) but a run of them is
        // `..`, which is a path component with meaning even after separators are gone.
        if c == '.' && cleaned.ends_with('.') {
            continue;
        }
        cleaned.push(c);
    }
    let cleaned = cleaned.trim_matches('.');
    if cleaned.is_empty() {
        "unknown".to_string()
    } else {
        cleaned.chars().take(80).collect()
    }
}

/// Write one delivered envelope into `INBOX/`.
///
/// Written to a temporary name and renamed, because a worker polling its INBOX must
/// never be able to read half a JSON document. The filename is derived from the
/// sequence number, so re-writing a message after an adapter restart overwrites rather
/// than duplicates.
pub async fn write_inbox(inbox: &Path, seq: u64, envelope: &Envelope) -> anyhow::Result<PathBuf> {
    tokio::fs::create_dir_all(inbox).await?;
    let final_path = inbox.join(inbox_filename(seq, envelope.kind.as_str()));
    let tmp_path = inbox.join(format!(".{}.tmp", inbox_filename(seq, envelope.kind.as_str())));
    let json = serde_json::to_vec_pretty(envelope)?;
    tokio::fs::write(&tmp_path, &json).await?;
    tokio::fs::rename(&tmp_path, &final_path).await?;
    Ok(final_path)
}

/// The outbox files awaiting send, in name order.
///
/// Name order is used so a worker that numbers its files gets them sent in that order.
/// Files whose name starts with `.` are skipped: that is the temporary-file convention
/// a worker writing atomically would use, and reading one would be reading a partial
/// write.
pub async fn list_outbox(outbox: &Path) -> anyhow::Result<Vec<PathBuf>> {
    let mut found = BTreeMap::new();
    let mut entries = match tokio::fs::read_dir(outbox).await {
        Ok(e) => e,
        // No OUTBOX yet simply means the worker has not written one.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    while let Some(entry) = entries.next_entry().await? {
        let path = entry.path();
        if !entry.file_type().await.map(|t| t.is_file()).unwrap_or(false) {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with('.') || !name.ends_with(".json") {
            continue;
        }
        found.insert(name, path);
    }
    Ok(found.into_values().collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn accepts_the_documented_worker_shape() {
        let item = parse_outbox(r#"{"kind": "work.started", "body": {}}"#).expect("valid");
        assert_eq!(item.kind, "work.started");
        assert_eq!(item.body, json!({}));
        assert_eq!(item.to, None);
    }

    #[test]
    fn accepts_an_unregistered_kind() {
        // §5: forward compatibility is the whole reason `kind` is a string. An adapter
        // that gated on the registry would break exactly the messages it is meant to
        // carry forward.
        let item = parse_outbox(r#"{"kind": "some.future.kind", "body": {"x": 1}}"#).expect("valid");
        assert_eq!(item.kind, "some.future.kind");
    }

    #[test]
    fn accepts_a_peer_address_and_a_causal_link() {
        let item = parse_outbox(
            r#"{"kind": "peer.answer", "to": "urn:hacp:agent:b-3f0c",
                "in_reply_to": "m-9d2e", "body": {"text": "yes"}}"#,
        )
        .expect("valid");
        assert_eq!(item.to.as_deref(), Some("urn:hacp:agent:b-3f0c"));
        assert_eq!(item.in_reply_to.as_deref(), Some("m-9d2e"));
    }

    #[test]
    fn preserves_an_unknown_body_field_exactly() {
        let item = parse_outbox(r#"{"kind": "question", "body": {"about": "x", "future": [1, 2]}}"#)
            .expect("valid");
        assert_eq!(item.body, json!({"about": "x", "future": [1, 2]}));
    }

    #[test]
    fn reports_bad_shapes() {
        assert!(matches!(parse_outbox("not json at all"), Err(ShapeError::NotJson(_))));
        assert_eq!(parse_outbox("[1, 2]"), Err(ShapeError::NotObject));
        assert_eq!(parse_outbox(r#"{"body": {}}"#), Err(ShapeError::Missing("kind")));
        assert_eq!(parse_outbox(r#"{"kind": "hello"}"#), Err(ShapeError::Missing("body")));
        assert_eq!(parse_outbox(r#"{"kind": "", "body": {}}"#), Err(ShapeError::Empty("kind")));
        assert_eq!(
            parse_outbox(r#"{"kind": 7, "body": {}}"#),
            Err(ShapeError::WrongType { field: "kind", expected: "a string" })
        );
        assert_eq!(
            parse_outbox(r#"{"kind": "question", "body": "text"}"#),
            Err(ShapeError::WrongType { field: "body", expected: "a JSON object" })
        );
        assert_eq!(
            parse_outbox(r#"{"kind": "question", "body": {}, "to": "mailto:someone"}"#),
            Err(ShapeError::BadActor("mailto:someone".into()))
        );
    }

    #[test]
    fn refuses_to_let_a_worker_fill_adapter_owned_metadata() {
        // A worker that sets `from` is claiming to be someone; §13.3 has the
        // coordinator reject that, but there is no reason to relay the attempt.
        assert_eq!(
            parse_outbox(r#"{"kind": "hello", "body": {}, "from": "urn:hacp:arbiter:x"}"#),
            Err(ShapeError::AdapterOwned("from"))
        );
        assert_eq!(
            parse_outbox(r#"{"kind": "hello", "body": {}, "message_id": "m-forged"}"#),
            Err(ShapeError::AdapterOwned("message_id"))
        );
    }

    #[test]
    fn fills_envelope_metadata() {
        let item = parse_outbox(r#"{"kind": "work.started", "body": {"n": 1}}"#).unwrap();
        let env = envelope_for(
            &item,
            "run-8a41",
            "urn:hacp:agent:a-3f0c",
            "urn:hacp:coordinator:hive",
            "m-fixed".to_string(),
        )
        .expect("envelope builds");

        assert_eq!(env.protocol, hacp::PROTOCOL);
        assert_eq!(env.message_id, "m-fixed");
        assert_eq!(env.run_id, "run-8a41");
        assert_eq!(env.from, "urn:hacp:agent:a-3f0c");
        assert_eq!(env.to, "urn:hacp:coordinator:hive");
        assert_eq!(env.kind.as_str(), "work.started");
        assert_eq!(env.in_reply_to, None);
        assert_eq!(env.body, json!({"n": 1}));
        // The body is carried, not rewritten, and the result passes the spec's own
        // shape check.
        env.validate().expect("a filled envelope is valid");
    }

    #[test]
    fn a_peer_address_overrides_the_default_recipient() {
        let item = parse_outbox(
            r#"{"kind": "peer.question", "to": "urn:hacp:agent:b-3f0c", "body": {"about": "x", "text": "y"}}"#,
        )
        .unwrap();
        let env = envelope_for(&item, "run-1", "urn:hacp:agent:a-3f0c", "urn:hacp:coordinator:hive", "m-1".into())
            .unwrap();
        assert_eq!(env.to, "urn:hacp:agent:b-3f0c");
    }

    #[test]
    fn rejects_an_oversized_envelope() {
        let big = "x".repeat(MAX_ENVELOPE_BYTES + 16);
        let item = OutboxItem {
            kind: "artifact.published".into(),
            body: json!({"blob": big}),
            to: None,
            in_reply_to: None,
        };
        assert!(matches!(
            envelope_for(&item, "run-1", "urn:hacp:agent:a-1", "urn:hacp:coordinator:hive", "m-1".into()),
            Err(ShapeError::TooLarge { .. })
        ));
    }

    #[test]
    fn inbox_filenames_sort_in_sequence_order_and_cannot_escape_the_directory() {
        assert_eq!(inbox_filename(7, "run.started"), "000007-run.started.json");
        assert!(inbox_filename(9, "run.started") < inbox_filename(10, "run.started"));
        let hostile = inbox_filename(1, "../../.ssh/authorized_keys");
        assert!(!hostile.contains('/'), "sanitized name still contains a separator: {hostile}");
        assert!(!hostile.contains(".."), "sanitized name still contains a traversal: {hostile}");
        assert_eq!(inbox_filename(1, ".."), "000001-unknown.json");
    }
}
