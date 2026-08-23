//! Outbound webhook notifications: POSTs a JSON payload to a
//! user-configured URL whenever a panel event occurs, so external
//! tooling (a Discord/Slack incoming webhook, your own server, IFTTT,
//! ...) can react without polling the `/events` SSE stream.
//!
//! Delivery is fire-and-forget from the caller's perspective -- each
//! send spawns its own task with a short timeout, and failures are only
//! logged to the process's own stdout (`log_panel`), never through
//! [`crate::panel::state::PanelState::log`], which would itself trigger
//! another webhook attempt and risk a feedback loop on a
//! persistently-failing URL.

use std::sync::OnceLock;
use std::time::Duration;

use serde_json::{json, Value};

use crate::logging::{log_panel, LogLevel};

const TIMEOUT: Duration = Duration::from_secs(5);

fn client() -> &'static reqwest::Client {
    static CLIENT: OnceLock<reqwest::Client> = OnceLock::new();
    CLIENT.get_or_init(reqwest::Client::new)
}

/// A URL is only accepted if it looks like an absolute HTTP(S) URL --
/// this is a personal single-user tool, not a validated allowlist, so
/// the check is just enough to catch obvious typos before they're saved.
pub fn is_plausible_url(url: &str) -> bool {
    url.starts_with("http://") || url.starts_with("https://")
}

/// Fires a webhook POST if `url` is set. `message` is the same
/// human-readable line the panel's own log gets; `extra` (if not
/// `Value::Null`) is merged into the JSON body as additional structured
/// fields for event types the panel can classify (button feedback,
/// device status, pairing, ...). Never blocks the caller.
pub fn notify(url: Option<&str>, message: &str, extra: Value) {
    let Some(url) = url else { return };
    let mut body = json!({
        "message": message,
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    });
    if let Value::Object(extra_fields) = extra
        && let Value::Object(body_fields) = &mut body
    {
        body_fields.extend(extra_fields);
    }

    let url = url.to_string();
    tokio::spawn(async move {
        let result = client().post(&url).timeout(TIMEOUT).json(&body).send().await;
        match result {
            Ok(response) if !response.status().is_success() => {
                log_panel(
                    LogLevel::Warn,
                    format!("webhook delivery to {url} returned status {}", response.status()),
                );
            }
            Err(err) => {
                log_panel(LogLevel::Warn, format!("webhook delivery to {url} failed: {err}"));
            }
            Ok(_) => {}
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_plausible_url_requires_http_scheme() {
        assert!(is_plausible_url("https://example.com/hook"));
        assert!(is_plausible_url("http://127.0.0.1:9000/hook"));
        assert!(!is_plausible_url("example.com/hook"));
        assert!(!is_plausible_url("ftp://example.com"));
        assert!(!is_plausible_url(""));
    }
}
