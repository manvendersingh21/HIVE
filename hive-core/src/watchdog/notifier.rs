//! iMessage notification via macOS `osascript` — sends alerts to the
//! operator's phone when the safety interceptor pauses a high-risk action.
//!
//! Both a synchronous and an asynchronous entry point are provided: the CLI
//! approval gate ([`send_imessage_alert_sync`]) runs on a blocking thread;
//! the agent loop and supervision path ([`send_imessage_alert`]) are async.
//!
//! Notification is **best-effort**: a missing `PHONE_NUMBER`, a non-macOS
//! host, or a Messages.app failure all log a warning and return `Ok(())`
//! rather than aborting the safety flow. The gate itself (stdin prompt) is
//! the hard stop; iMessage is the "you left the terminal" nudge.

use std::env;

use tracing::{info, warn};

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
}
