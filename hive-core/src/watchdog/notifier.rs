//! Alert delivery for the watchdog — fans a single [`Alert`] out to every
//! channel the operator has configured: iMessage via macOS `osascript`,
//! [ntfy.sh](https://ntfy.sh) push notifications, and a generic webhook
//! (Slack/Discord/anything that takes JSON).
//!
//! Notification is **best-effort**: a missing `PHONE_NUMBER`, a non-macOS
//! host, an unreachable ntfy server, a dead webhook — all of these log a
//! warning and return `Ok(())` rather than aborting the safety flow. The
//! gate itself (stdin prompt) is the hard stop; every channel here is a
//! "you left the terminal" nudge, and channels are independent — one
//! failing must never prevent the others from firing.

use std::env;
use std::time::Duration;

use hive_common::config::NotificationConfig;
use hive_common::Severity;
use serde_json::{json, Value};
use tracing::{info, warn};

/// How long any single HTTP notification is allowed to take. The safety
/// path calls into this synchronously from the interceptor loop, so a
/// hanging DNS lookup or a stalled TCP connect to a notification endpoint
/// must not be able to wedge it.
const HTTP_TIMEOUT: Duration = Duration::from_secs(5);

/// Default ntfy.sh host used when a topic is given without a scheme.
const NTFY_DEFAULT_BASE: &str = "https://ntfy.sh";

/// A single alert to deliver across every configured channel.
#[derive(Debug, Clone)]
pub struct Alert {
    pub severity: Severity,
    /// Short one-line summary, used as the notification title.
    pub title: String,
    /// Human-readable explanation of why the alert fired.
    pub reason: String,
    /// Session or incident identity (e.g. session id, hostname+pid) so the
    /// operator can tell which of several supervised sessions paused.
    pub session: String,
    /// Incident id, if one was recorded — used to build a review link
    /// against `web_base_url` (e.g. `{web_base_url}/incidents/{id}`).
    pub incident_id: Option<String>,
}

impl Alert {
    /// The review link for this alert, if it carries an incident id.
    fn review_url(&self, web_base_url: &str) -> Option<String> {
        self.incident_id
            .as_ref()
            .map(|id| format!("{}/incidents/{id}", web_base_url.trim_end_matches('/')))
    }

    /// One-line body shared by every text-based channel (iMessage, ntfy,
    /// webhook fallback). Kept separate from the JSON payload the webhook
    /// sends so a Slack/Discord/ntfy client always has readable prose even
    /// if it ignores the structured fields.
    fn body(&self, web_base_url: &str) -> String {
        let mut s = format!(
            "[{}] {}: {} (session: {})",
            self.severity, self.title, self.reason, self.session
        );
        if let Some(url) = self.review_url(web_base_url) {
            s.push_str(&format!(" — {url}"));
        }
        s
    }
}

/// Fans one [`Alert`] out to every channel configured in
/// [`NotificationConfig`]. Construct once from config and reuse — the
/// underlying `reqwest::Client` pools connections.
pub struct Notifier {
    config: NotificationConfig,
    http: reqwest::Client,
}

impl Notifier {
    /// Build a notifier from config. Never fails: an unbuildable HTTP
    /// client (should not happen with the fixed timeout/TLS backend here)
    /// falls back to `reqwest::Client::new()` rather than propagating an
    /// error into what is supposed to be a best-effort path.
    pub fn from_config(config: NotificationConfig) -> Self {
        let http = reqwest::Client::builder()
            .timeout(HTTP_TIMEOUT)
            .build()
            .unwrap_or_else(|e| {
                warn!("failed to build notifier HTTP client, using default: {e}");
                reqwest::Client::new()
            });
        Self { config, http }
    }

    /// Deliver `alert` to every configured channel. Every channel is
    /// independent and best-effort: a failure on one (network down, bad
    /// topic, unreachable webhook) is logged and does not stop the others
    /// from being attempted, and this function itself never returns an
    /// error — there is nothing upstream that should treat a failed
    /// notification as fatal.
    pub async fn notify(&self, alert: &Alert) {
        let body = alert.body(&self.config.web_base_url);

        send_imessage_alert(&body).await.ok();

        if let Some(topic) = &self.config.ntfy_topic {
            self.send_ntfy(topic, alert).await;
        }

        if let Some(webhook_url) = &self.config.webhook_url {
            self.send_webhook(webhook_url, alert).await;
        }
    }

    async fn send_ntfy(&self, topic: &str, alert: &Alert) {
        let url = ntfy_url(topic);
        let priority = ntfy_priority(alert.severity);
        let body = alert.body(&self.config.web_base_url);

        let result = self
            .http
            .post(&url)
            .header("Title", sanitize_header_value(&alert.title))
            .header("Priority", priority.to_string())
            .header("Tags", "warning")
            .body(body)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                info!("ntfy alert delivered");
            }
            Ok(resp) => {
                warn!("ntfy at {} responded with status {}", redact_url_host(&url), resp.status());
            }
            Err(_e) => {
                // Redacted for the same reason as the webhook below, one step
                // weaker: on the public server an ntfy topic is the whole
                // access control. Anyone who reads the topic out of a log can
                // subscribe to every future alert — including the session
                // names and rule matches they carry — and publish fake ones.
                warn!("failed to deliver ntfy alert to {}", redact_url_host(&url));
            }
        }
    }

    async fn send_webhook(&self, webhook_url: &str, alert: &Alert) {
        let payload = webhook_payload(alert, &self.config.web_base_url);

        let result = self
            .http
            .post(webhook_url)
            .json(&payload)
            .send()
            .await;

        match result {
            Ok(resp) if resp.status().is_success() => {
                info!("webhook alert delivered to {}", redact_url_host(webhook_url));
            }
            Ok(resp) => {
                warn!(
                    "webhook to {} responded with status {}",
                    redact_url_host(webhook_url),
                    resp.status()
                );
            }
            Err(_e) => {
                // Never format the underlying reqwest error: it can echo the
                // request URL back verbatim, and a webhook URL is a bearer
                // credential (Slack/Discord accept the request from anyone
                // who holds the URL). Log only the host.
                warn!(
                    "failed to deliver webhook alert to {}",
                    redact_url_host(webhook_url)
                );
            }
        }
    }
}

/// Resolve an ntfy topic into a POST URL. A bare topic name is posted to
/// the public `ntfy.sh` service; a value that already looks like a URL
/// (self-hosted ntfy, or a reverse-proxied instance) is used unchanged so
/// operators aren't forced onto the public server.
fn ntfy_url(topic: &str) -> String {
    if topic.starts_with("http://") || topic.starts_with("https://") {
        topic.to_string()
    } else {
        format!("{NTFY_DEFAULT_BASE}/{topic}")
    }
}

/// Map watchdog [`Severity`] onto ntfy's 1-5 priority scale.
///
/// ntfy's default priority (3) does not raise a notification-not-a-sound
/// on most clients, and 1 is silently low-priority — neither is right for
/// "an autonomous agent just tried something dangerous". So `Low` starts
/// at ntfy default (3) rather than the bottom of the scale, `Medium` and
/// `High` step up one each, and `Critical` takes ntfy's max (5, which also
/// triggers the "urgent" red styling and bypasses most phones' silent
/// mode) — the one case where waking the operator up is the point.
fn ntfy_priority(severity: Severity) -> u8 {
    match severity {
        Severity::Low => 3,
        Severity::Medium => 4,
        Severity::High => 4,
        Severity::Critical => 5,
    }
}

/// Build the JSON body posted to the configured webhook.
///
/// Slack incoming webhooks read `{"text": "..."}`; Discord webhooks read
/// `{"content": "..."}`. Rather than making the operator declare which
/// flavor of webhook they're pointing at, send both keys in one payload —
/// each service ignores the field it doesn't recognize — plus the
/// structured fields underneath, so a generic consumer (a custom
/// dashboard, a log sink) doesn't have to parse prose back out.
fn webhook_payload(alert: &Alert, web_base_url: &str) -> Value {
    let body = alert.body(web_base_url);
    json!({
        "text": body,
        "content": body,
        "severity": alert.severity.to_string(),
        "reason": alert.reason,
        "session": alert.session,
        "incident_id": alert.incident_id,
        "url": alert.review_url(web_base_url),
    })
}

/// Strip characters that would break an HTTP header value (ntfy reads
/// `Title`/`Tags` as raw header bytes, and a summary can contain anything
/// an agent printed). Newlines are the only thing that actually breaks
/// header framing; replace them with a space rather than rejecting the
/// whole notification over cosmetic content.
fn sanitize_header_value(s: &str) -> String {
    s.replace(['\n', '\r'], " ")
}

/// Reduce a URL to a loggable form: scheme + host only. Webhook URLs
/// (Slack, Discord) are bearer secrets — the path/query holds the actual
/// token — so nothing past the host may ever reach a log line or error
/// message.
fn redact_url_host(url: &str) -> String {
    match reqwest::Url::parse(url) {
        Ok(parsed) => match parsed.host_str() {
            Some(host) => format!("{}://{host}", parsed.scheme()),
            None => "<unparseable-host>".to_string(),
        },
        Err(_) => "<invalid-url>".to_string(),
    }
}

/// Resolve the operator's phone number from the `PHONE_NUMBER` environment
/// variable. Returns `None` (with a logged warning) if unset or empty.
fn resolve_phone() -> Option<String> {
    match env::var("PHONE_NUMBER") {
        Ok(p) if !p.is_empty() => Some(p),
        _ => {
            warn!("PHONE_NUMBER env var not set — skipping iMessage notification");
            None
        }
    }
}

/// Build the AppleScript snippet that sends an iMessage.
fn build_applescript(phone: &str, message: &str) -> String {
    // Escape double-quotes and backslashes inside the message so the
    // AppleScript string literal is valid.
    let escaped = message.replace('\\', "\\\\").replace('"', "\\\"");
    format!(
        r#"tell application "Messages" to send "{escaped}" to buddy "{phone}" of (1st account whose service type = iMessage)"#,
    )
}

/// Format the notification message from an action summary.
fn format_message(summary: &str) -> String {
    format!(
        "⚠️ High-Risk Action Paused: {summary}. \
         Please review the terminal to approve or abort."
    )
}

// ── Async variant (for agent loop / supervision) ────────────────────────

/// Send an iMessage alert to the operator via macOS Messages.app.
///
/// Requires:
/// - macOS with Messages.app signed into iMessage
/// - `PHONE_NUMBER` environment variable set
///
/// Returns `Ok(())` on success **or** if notification cannot be sent
/// (missing env var, not macOS, osascript failure). Notification is
/// advisory; the safety gate does not depend on it.
pub async fn send_imessage_alert(summary: &str) -> anyhow::Result<()> {
    let phone = match resolve_phone() {
        Some(p) => p,
        None => return Ok(()),
    };

    let message = format_message(summary);
    let script = build_applescript(&phone, &message);

    info!("Sending iMessage alert to {phone}");

    let output = tokio::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            info!("iMessage alert sent successfully");
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            warn!("osascript exited with {}: {stderr}", out.status);
        }
        Err(e) => {
            warn!("Failed to run osascript: {e}");
        }
    }

    Ok(())
}

// ── Sync variant (for CLI approval gate on blocking thread) ─────────────

/// Synchronous version of [`send_imessage_alert`] for use in contexts where
/// a tokio runtime may not be available or blocking is acceptable (e.g. the
/// CLI approval gate, which is already blocking on stdin).
pub fn send_imessage_alert_sync(summary: &str) {
    let phone = match resolve_phone() {
        Some(p) => p,
        None => return,
    };

    let message = format_message(summary);
    let script = build_applescript(&phone, &message);

    info!("Sending iMessage alert to {phone} (sync)");

    let output = std::process::Command::new("osascript")
        .arg("-e")
        .arg(&script)
        .output();

    match output {
        Ok(out) if out.status.success() => {
            info!("iMessage alert sent successfully (sync)");
        }
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            warn!("osascript exited with {}: {stderr}", out.status);
        }
        Err(e) => {
            warn!("Failed to run osascript (sync): {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn applescript_escapes_quotes_and_backslashes() {
        // The message is embedded in an AppleScript string literal, so every backslash
        // and quote in it has to survive as data. A bare `"` would end the literal early
        // and turn the rest of an attacker-influenced command summary into script.
        let script = build_applescript("+1234567890", r#"rm "a\b""#);

        assert!(script.contains(r#"rm \"a\\b\""#), "got: {script}");
        assert!(script.contains("+1234567890"));

        // The message contributes no unescaped quote, so the literal the message sits in
        // still ends where we put its closing quote.
        let literal = script
            .split_once(r#"send ""#)
            .and_then(|(_, rest)| rest.split_once(r#"" to buddy"#))
            .expect("the literal is delimited as written")
            .0;
        assert!(!contains_unescaped_quote(literal), "message escaped its literal: {literal}");
    }

    /// Whether `s` holds a `"` preceded by an even number of backslashes — i.e. one that
    /// AppleScript would read as ending the string.
    fn contains_unescaped_quote(s: &str) -> bool {
        let mut backslashes = 0usize;
        for c in s.chars() {
            match c {
                '\\' => backslashes += 1,
                '"' if backslashes % 2 == 0 => return true,
                _ => backslashes = 0,
            }
        }
        false
    }

    #[test]
    fn message_format_includes_summary() {
        let msg = format_message("rm -rf /");
        assert!(msg.contains("rm -rf /"));
        assert!(msg.starts_with("⚠️"));
        assert!(msg.contains("approve or abort"));
    }

    #[tokio::test]
    async fn missing_phone_number_returns_ok() {
        env::remove_var("PHONE_NUMBER");
        let result = send_imessage_alert("test action").await;
        assert!(result.is_ok());
    }

    #[test]
    fn missing_phone_number_sync_does_not_panic() {
        env::remove_var("PHONE_NUMBER");
        // Should not panic or hang
        send_imessage_alert_sync("test action");
    }

    fn test_alert() -> Alert {
        Alert {
            severity: Severity::High,
            title: "Destructive command".to_string(),
            reason: "rm -rf detected".to_string(),
            session: "session-123".to_string(),
            incident_id: Some("inc-42".to_string()),
        }
    }

    #[test]
    fn ntfy_url_prefixes_bare_topic() {
        assert_eq!(ntfy_url("my-topic"), "https://ntfy.sh/my-topic");
    }

    #[test]
    fn ntfy_url_passes_through_full_url() {
        assert_eq!(
            ntfy_url("https://ntfy.example.internal/team-alerts"),
            "https://ntfy.example.internal/team-alerts"
        );
        assert_eq!(
            ntfy_url("http://localhost:8090/topic"),
            "http://localhost:8090/topic"
        );
    }

    #[test]
    fn severity_maps_to_ntfy_priority() {
        assert_eq!(ntfy_priority(Severity::Low), 3);
        assert_eq!(ntfy_priority(Severity::Medium), 4);
        assert_eq!(ntfy_priority(Severity::High), 4);
        assert_eq!(ntfy_priority(Severity::Critical), 5);
    }

    #[test]
    fn webhook_payload_contains_slack_and_discord_keys_and_structured_fields() {
        let alert = test_alert();
        let payload = webhook_payload(&alert, "http://localhost:8080");

        assert!(payload.get("text").is_some(), "missing Slack `text` key");
        assert!(payload.get("content").is_some(), "missing Discord `content` key");
        assert_eq!(payload["severity"], "HIGH");
        assert_eq!(payload["reason"], "rm -rf detected");
        assert_eq!(payload["session"], "session-123");
        assert_eq!(payload["incident_id"], "inc-42");
        assert_eq!(payload["url"], "http://localhost:8080/incidents/inc-42");

        // Both prose fields carry the same human-readable body.
        assert_eq!(payload["text"], payload["content"]);
    }

    #[test]
    fn webhook_payload_omits_url_without_incident_id() {
        let mut alert = test_alert();
        alert.incident_id = None;
        let payload = webhook_payload(&alert, "http://localhost:8080");
        assert!(payload["url"].is_null());
    }

    #[test]
    fn webhook_url_never_appears_in_logs_or_errors() {
        let secret_url = "https://hooks.slack.com/services/T00/B00/super-secret-token-xyz";
        let redacted = redact_url_host(secret_url);

        assert!(!redacted.contains("T00"));
        assert!(!redacted.contains("B00"));
        assert!(!redacted.contains("super-secret-token-xyz"));
        assert_eq!(redacted, "https://hooks.slack.com");
    }

    #[test]
    fn sanitize_header_value_strips_newlines() {
        let s = sanitize_header_value("line one\nline two\r\nline three");
        assert!(!s.contains('\n'));
        assert!(!s.contains('\r'));
    }

    #[tokio::test]
    async fn notifier_with_no_channels_is_a_noop() {
        env::remove_var("PHONE_NUMBER");
        let notifier = Notifier::from_config(NotificationConfig::default());
        // Should return without attempting any network call and without panicking.
        notifier.notify(&test_alert()).await;
    }
}
