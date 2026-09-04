//! The bus edge: ingest and poll over HTTP (`spec/HACP.md` §13.1).
//!
//! Everything that touches the network lives behind [`Bus`], and every decision the
//! transport makes is factored into a pure function ([`parse_poll_page`],
//! [`classify_ingest`]) so the interesting cases — a duplicate, a role-bound rejection,
//! a page whose messages carry no sequence numbers — are testable without a server.
//!
//! The trait uses boxed futures rather than `async fn` in a trait so that a `Box<dyn
//! Bus>` remains possible; the adapter is written once against the trait and given a
//! real or a fake bus.

use std::future::Future;
use std::pin::Pin;

use anyhow::{anyhow, Context, Result};
use hacp::Envelope;
use serde_json::Value;

/// A boxed, `Send` future — the shape every [`Bus`] method returns.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// The envelope size ceiling of §5. Artifacts are referenced, never embedded.
pub const MAX_ENVELOPE_BYTES: usize = 1024 * 1024;

/// What the coordinator did with one ingested envelope (§13.1).
///
/// `Accepted` and `Duplicate` are kept apart because at-least-once edges make
/// redelivery ordinary traffic; collapsing them would hide a resend that the
/// coordinator recognized. Both mean "the bus has it", which is what lets the outbox
/// delete the file. `Rejected` means the coordinator refused it permanently — a
/// protocol-version mismatch or a role-binding failure — and a resend cannot help.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IngestOutcome {
    Accepted { seq: Option<u64> },
    Duplicate { seq: Option<u64> },
    Rejected { code: String, detail: String },
}

impl IngestOutcome {
    /// Whether the bus has durably taken the message, so the outbox file may go.
    pub fn is_settled(&self) -> bool {
        matches!(self, IngestOutcome::Accepted { .. } | IngestOutcome::Duplicate { .. })
    }
}

/// One envelope as delivered, with the coordinator's sequence number when it supplies
/// one.
#[derive(Debug, Clone)]
pub struct Delivered {
    /// The run's total-order position. `None` when the binding returns bare envelopes;
    /// the adapter then falls back to a local ordinal for INBOX filenames, which
    /// preserves order but is not the bus's number (see `fileedge`).
    pub seq: Option<u64>,
    pub envelope: Envelope,
}

/// One page of the poll response.
#[derive(Debug, Clone, Default)]
pub struct PollPage {
    /// The caller's next cursor, if the binding reports one.
    pub cursor: Option<u64>,
    /// The run's current state as a label. Carried for logging only: the adapter is
    /// content-blind and does not branch on the run's state.
    pub state: Option<String>,
    pub messages: Vec<Delivered>,
}

/// The two operations §13.1 requires of any binding.
pub trait Bus: Send + Sync {
    /// Offer one envelope to the coordinator.
    fn ingest<'a>(&'a self, envelope: &'a Envelope) -> BoxFuture<'a, Result<IngestOutcome>>;

    /// Messages addressed to this adapter's agent after `since`.
    fn poll(&self, since: u64) -> BoxFuture<'_, Result<PollPage>>;
}

/// The HTTP binding: `POST <base>/api/collab/runs/{run}/ingest` and
/// `GET <base>/api/collab/runs/{run}/messages?since=&agent=`.
pub struct HttpBus {
    client: reqwest::Client,
    base: String,
    run_id: String,
    /// The role id the coordinator knows this adapter by; it names the poll cursor's
    /// owner and is the identity the token is bound to (§13.3).
    role: String,
    /// Per-run, per-role bearer token. Read from the environment, never from argv,
    /// where every other process on the machine could read it out of the process table.
    token: String,
}

impl HttpBus {
    pub fn new(base: impl Into<String>, run_id: impl Into<String>, role: impl Into<String>, token: impl Into<String>) -> Result<Self> {
        let client = reqwest::Client::builder()
            .build()
            .context("building the HTTP client")?;
        let base = base.into();
        Ok(Self {
            base: base.trim_end_matches('/').to_string(),
            client,
            run_id: run_id.into(),
            role: role.into(),
            token: token.into(),
        })
    }

    fn ingest_url(&self) -> String {
        format!("{}/api/collab/runs/{}/ingest", self.base, self.run_id)
    }

    fn messages_url(&self, since: u64) -> String {
        format!(
            "{}/api/collab/runs/{}/messages?since={}&agent={}",
            self.base, self.run_id, since, self.role
        )
    }
}

impl Bus for HttpBus {
    fn ingest<'a>(&'a self, envelope: &'a Envelope) -> BoxFuture<'a, Result<IngestOutcome>> {
        Box::pin(async move {
            let response = self
                .client
                .post(self.ingest_url())
                .bearer_auth(&self.token)
                .json(envelope)
                .send()
                .await
                .context("POST ingest")?;
            let status = response.status().as_u16();
            // A body is advisory: some failures answer with a status and nothing else.
            let body: Option<Value> = response.json().await.ok();
            if is_transient_status(status) {
                // A server error is not the coordinator refusing the message; it is the
                // coordinator being unable to answer. Reported as an error so the
                // caller keeps the outbox file and retries, rather than filing a
                // worker's message under "refused" for an outage.
                return Err(anyhow!("ingest failed with HTTP {status}"));
            }
            Ok(classify_ingest(status, body.as_ref()))
        })
    }

    fn poll(&self, since: u64) -> BoxFuture<'_, Result<PollPage>> {
        Box::pin(async move {
            let response = self
                .client
                .get(self.messages_url(since))
                .bearer_auth(&self.token)
                .send()
                .await
                .context("GET messages")?;
            let status = response.status();
            let body = response.text().await.context("reading the poll response")?;
            if !status.is_success() {
                return Err(anyhow!("poll failed with HTTP {status}: {}", tail(&body, 200)));
            }
            let value: Value = serde_json::from_str(&body).context("poll response is not JSON")?;
            parse_poll_page(&value)
        })
    }
}

/// Keep the last `n` characters of a string, for error messages that must not become a
/// vector for dumping a whole response into a log.
fn tail(s: &str, n: usize) -> String {
    if s.len() <= n {
        return s.to_string();
    }
    s.chars().skip(s.chars().count().saturating_sub(n)).collect()
}

/// Decide what an ingest response means.
///
/// The status code is authoritative for the transport-level outcome; an explicit
/// `status` field in the body refines it, because a binding may answer 200 for a
/// duplicate. Anything not recognized as settled or permanently refused is an error, so
/// the caller retries rather than dropping a message.
pub fn classify_ingest(status: u16, body: Option<&Value>) -> IngestOutcome {
    let seq = body.and_then(|b| b.get("seq")).and_then(Value::as_u64);
    let labelled = body
        .and_then(|b| b.get("status").or_else(|| b.get("outcome")))
        .and_then(Value::as_str)
        .map(str::to_ascii_lowercase);

    if let Some(label) = labelled.as_deref() {
        match label {
            "accepted" => return IngestOutcome::Accepted { seq },
            "duplicate" => return IngestOutcome::Duplicate { seq },
            "rejected" => {
                return IngestOutcome::Rejected {
                    code: extract(body, "code").unwrap_or_else(|| "rejected".into()),
                    detail: extract(body, "detail").unwrap_or_default(),
                }
            }
            _ => {}
        }
    }

    match status {
        200..=299 => IngestOutcome::Accepted { seq },
        409 => IngestOutcome::Duplicate { seq },
        // 4xx is the coordinator refusing this envelope as it stands: a bad protocol
        // version, a `from` the token does not authorize, a malformed shape. Retrying
        // an unchanged envelope cannot fix any of those.
        400..=499 => IngestOutcome::Rejected {
            code: extract(body, "code").unwrap_or_else(|| format!("http-{status}")),
            detail: extract(body, "detail")
                .or_else(|| extract(body, "error"))
                .unwrap_or_else(|| format!("HTTP {status}")),
        },
        // Server errors never reach here from [`HttpBus`], which reports them as
        // errors so the message is retried; a caller that does reach here with one is
        // told the message did not land.
        _ => IngestOutcome::Rejected {
            code: format!("http-{status}"),
            detail: "server error".to_string(),
        },
    }
}

/// Whether an ingest failure is the server failing rather than the message being wrong,
/// and so is worth retrying with the same envelope.
pub fn is_transient_status(status: u16) -> bool {
    status >= 500
}

fn extract(body: Option<&Value>, key: &str) -> Option<String> {
    body.and_then(|b| b.get(key))
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Parse a poll response.
///
/// Tolerant on purpose. §13.1 requires a poll operation returning envelopes after a
/// cursor together with the run's state, but it does not fix the JSON around them, and
/// this adapter must keep working against a coordinator that wraps its messages
/// differently. Three shapes are accepted:
///
/// * `{"state": ..., "seq": N, "messages": [<envelope>, ...]}`
/// * `{"messages": [{"seq": N, "envelope": {...}}, ...]}`
/// * a bare `[<envelope>, ...]`
///
/// An envelope that fails to deserialize is an error for the whole page rather than a
/// silent skip: dropping a message the coordinator considers delivered would leave the
/// worker permanently missing it, and §5 forbids rejecting a message merely for having
/// an unregistered `kind`, so a failure here is a real disagreement worth surfacing.
pub fn parse_poll_page(value: &Value) -> Result<PollPage> {
    let (items, cursor, state) = match value {
        Value::Array(items) => (items.clone(), None, None),
        Value::Object(map) => {
            let items = map
                .get("messages")
                .or_else(|| map.get("envelopes"))
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default();
            let cursor = map.get("seq").or_else(|| map.get("cursor")).and_then(Value::as_u64);
            let state = map.get("state").and_then(Value::as_str).map(str::to_string);
            (items, cursor, state)
        }
        other => return Err(anyhow!("poll response is neither an object nor an array: {other}")),
    };

    let mut messages = Vec::with_capacity(items.len());
    for item in items {
        let (seq, raw) = match item.get("envelope") {
            Some(inner) => (item.get("seq").and_then(Value::as_u64), inner.clone()),
            None => (item.get("seq").and_then(Value::as_u64), item),
        };
        let envelope: Envelope =
            serde_json::from_value(raw).context("a delivered message is not a HACP envelope")?;
        messages.push(Delivered { seq, envelope });
    }
    Ok(PollPage { cursor, state, messages })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn envelope_json() -> Value {
        json!({
            "protocol": "HACP/1.1",
            "message_id": "m-1",
            "run_id": "run-1",
            "from": "urn:hacp:coordinator:hive",
            "to": "urn:hacp:agent:a-3f0c",
            "kind": "run.started",
            "timestamp": "2026-09-04T10:00:00Z",
            "body": {"goal": "g", "participants": []}
        })
    }

    #[test]
    fn parses_the_documented_page_shape() {
        let page = parse_poll_page(&json!({
            "state": "working",
            "seq": 7,
            "messages": [envelope_json()]
        }))
        .expect("page parses");
        assert_eq!(page.cursor, Some(7));
        assert_eq!(page.state.as_deref(), Some("working"));
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.messages[0].envelope.kind.as_str(), "run.started");
    }

    #[test]
    fn parses_per_message_sequence_numbers() {
        let page = parse_poll_page(&json!({
            "messages": [{"seq": 12, "envelope": envelope_json()}]
        }))
        .expect("page parses");
        assert_eq!(page.messages[0].seq, Some(12));
    }

    #[test]
    fn parses_a_bare_array() {
        let page = parse_poll_page(&json!([envelope_json()])).expect("page parses");
        assert_eq!(page.messages.len(), 1);
        assert_eq!(page.cursor, None);
    }

    #[test]
    fn unknown_kinds_and_unknown_body_fields_survive_the_page() {
        // §5: unknown kinds are delivered, never rejected, and unknown body fields
        // round-trip. The adapter is the last place that could quietly lose either.
        let mut raw = envelope_json();
        raw["kind"] = json!("some.future.kind");
        raw["body"] = json!({"known": 1, "unknown_field": {"nested": true}});
        let page = parse_poll_page(&json!({"messages": [raw]})).expect("page parses");
        let env = &page.messages[0].envelope;
        assert_eq!(env.kind.as_str(), "some.future.kind");
        assert_eq!(env.body["unknown_field"]["nested"], json!(true));
    }

    #[test]
    fn classifies_ingest_outcomes() {
        assert_eq!(classify_ingest(200, Some(&json!({"seq": 3}))), IngestOutcome::Accepted { seq: Some(3) });
        assert_eq!(classify_ingest(409, None), IngestOutcome::Duplicate { seq: None });
        assert_eq!(
            classify_ingest(200, Some(&json!({"status": "duplicate", "seq": 9}))),
            IngestOutcome::Duplicate { seq: Some(9) }
        );
        match classify_ingest(403, Some(&json!({"code": "role-mismatch", "detail": "from is not yours"}))) {
            IngestOutcome::Rejected { code, detail } => {
                assert_eq!(code, "role-mismatch");
                assert_eq!(detail, "from is not yours");
            }
            other => panic!("expected a rejection, got {other:?}"),
        }
        assert!(classify_ingest(200, None).is_settled());
        assert!(!classify_ingest(400, None).is_settled());
        assert!(is_transient_status(503));
        assert!(!is_transient_status(409));
    }
}
