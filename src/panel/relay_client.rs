//! Connects to the local V3 relay as an ordinary controller ("web" side)
//! client -- exactly what any real third-party controller would do,
//! over loopback TCP. Reconnects forever (fixed 2s backoff, or
//! immediately if [`PanelState::request_reconnect`] was called), each
//! attempt yielding a brand-new V3 `clientId` and thus a new pairing QR.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::logging::{log_panel, LogLevel};

use super::state::{PanelState, Protocol};

pub async fn run(v3_port: u16, state: Arc<PanelState>) {
    loop {
        let token = state.reconnect_token();
        state.begin_connecting();
        state.log(format!("Connecting to V3 relay on port {v3_port}..."));

        match connect_and_run(v3_port, &state, &token).await {
            Ok(()) => {}
            Err(err) => {
                log_panel(LogLevel::Warn, format!("relay connection error: {err}"));
                state.log_with(
                    format!("Relay connection error: {err}"),
                    json!({"event": "relay_error", "protocol": "v3", "error": err.to_string()}),
                );
            }
        }

        state.set_disconnected();
        state.log_with(
            "Disconnected from relay, reconnecting...",
            json!({"event": "relay_disconnected", "protocol": "v3"}),
        );

        tokio::select! {
            _ = token.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

async fn connect_and_run(
    v3_port: u16,
    state: &Arc<PanelState>,
    token: &tokio_util::sync::CancellationToken,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let (ws_stream, _response) =
        tokio_tungstenite::connect_async(format!("ws://127.0.0.1:{v3_port}")).await?;
    let (mut sender, mut receiver) = ws_stream.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<WsMessage>();

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    loop {
        tokio::select! {
            _ = token.cancelled() => {
                let _ = tx.send(WsMessage::Close(None));
                break;
            }
            maybe_msg = receiver.next() => {
                match maybe_msg {
                    Some(Ok(WsMessage::Text(text))) => handle_frame(state, &text, &tx),
                    Some(Ok(WsMessage::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    // Drop the clone PanelState is holding (see `clear_outbound`'s docs)
    // before dropping our own and awaiting the writer task's drain --
    // otherwise the writer task can never observe every sender gone.
    state.clear_outbound();
    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

fn handle_frame(state: &Arc<PanelState>, text: &str, tx: &mpsc::UnboundedSender<WsMessage>) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let frame_type = value.get("type").and_then(Value::as_str);
    let message = value.get("message").and_then(Value::as_str).unwrap_or("");

    match frame_type {
        Some("bind") if message == "targetId" => {
            if let Some(client_id) = value.get("clientId").and_then(Value::as_str) {
                state.log_with(
                    format!("Connected. Controller id: {client_id}"),
                    json!({"event": "controller_connected", "protocol": "v3", "controllerId": client_id}),
                );
                state.set_connected(client_id.to_string(), tx.clone());
            }
        }
        Some("bind") if message == "200" => {
            if let Some(target_id) = value.get("targetId").and_then(Value::as_str) {
                state.log_with(
                    format!("Paired with device {target_id}"),
                    json!({"event": "paired", "protocol": "v3", "deviceId": target_id}),
                );
                state.set_paired(target_id.to_string());
            }
        }
        Some("bind") => {
            state.log_with(
                format!("Bind failed: code {message}"),
                json!({"event": "bind_failed", "protocol": "v3", "code": message}),
            );
        }
        Some("break") => {
            state.log_with("Device disconnected", json!({"event": "device_disconnected", "protocol": "v3"}));
            state.clear_device();
        }
        Some("notify") => {
            state.log(format!("Notify: {}", translate_notify(message)));
        }
        Some("error") => {
            state.log_with(format!("Error: {message}"), json!({"event": "error", "protocol": "v3", "code": message}));
        }
        Some("msg") if message.starts_with("feedback") => {
            match parse_action_message(message) {
                Some(action) => {
                    let (channel, shape) = decode_button_feedback(action);
                    state.log_with(
                        format!("Button feedback: action {action}"),
                        json!({"event": "button_feedback", "protocol": "v3", "code": action, "channel": channel, "shape": shape}),
                    );
                    state.set_button_action(Protocol::V3, action);
                }
                None => state.log(format!("Feedback: {message}")),
            }
        }
        Some("msg") if message.starts_with("strength") => {
            match parse_device_message(message) {
                Some(report) => {
                    state.log_with(
                        format!(
                            "Status: A={} (limit {}) B={} (limit {})",
                            report.strength_a, report.soft_limit_a, report.strength_b, report.soft_limit_b
                        ),
                        json!({
                            "event": "device_status",
                            "protocol": "v3",
                            "strengthA": report.strength_a,
                            "strengthB": report.strength_b,
                            "softLimitA": report.soft_limit_a,
                            "softLimitB": report.soft_limit_b,
                        }),
                    );
                    state.set_device_strength(
                        report.strength_a,
                        report.strength_b,
                        report.soft_limit_a,
                        report.soft_limit_b,
                    );
                }
                None => state.log(format!("Feedback: {message}")),
            }
        }
        Some("heartbeat") => {} // don't spam the visible log
        _ => {}
    }
}

/// Maps a `feedback-<n>` button-press code to which channel/shape button
/// on the DG-LAB APP's "Socket Control" screen was tapped. Also reused
/// for V4's `custom.action` (see [`super::v4_client`]) -- dglab-kit's own
/// README documents `custom.action` (V4) and `feedback-*` (V3) as the
/// same underlying concept, surfaced through the same `action` callback.
///
/// **Not documented anywhere** (checked `dglab-kit`'s source thoroughly:
/// no mention of "shape"/"pattern"/"circle"/"triangle"/"square"/"star"/
/// "hexagon"). This mapping was determined empirically, live, against a
/// real device: tapping through both rows of 5 shape icons left-to-right
/// produced codes 0-4 for one row and 5-9 for the other, in ascending
/// order matching tap order. Treat it as a best-effort label, not a
/// verified spec -- if it turns out wrong for your hardware/APP version,
/// this is the only place it needs to change.
pub(super) fn decode_button_feedback(code: i64) -> (Option<&'static str>, Option<&'static str>) {
    const SHAPES: [&str; 5] = ["circle", "triangle", "square", "star", "hexagon"];
    if !(0..10).contains(&code) {
        return (None, None);
    }
    let channel = if code < 5 { "A" } else { "B" };
    let shape = SHAPES[(code % 5) as usize];
    (Some(channel), Some(shape))
}

/// V3's wire protocol sends two `notify` messages in Chinese, verbatim
/// from the reference server's documented spec (`发送完毕`, and a
/// per-channel overwrite notice) -- kept byte-for-byte faithful to that
/// spec on the wire (see `v3::handler::notify_done` and the overwrite
/// notice in `queue_pulse`). This translates them for the panel's own
/// English log/UI only; the V3 relay itself is untouched. Anything not
/// recognized is shown as-is rather than dropped.
fn translate_notify(message: &str) -> String {
    if message == "发送完毕" {
        return "Waveform sequence complete".to_string();
    }
    if let Some(rest) = message.strip_prefix("当前通道")
        && let Some(channel) = rest.strip_suffix("有正在发送的消息，覆盖之前的消息")
    {
        return format!("Channel {channel} already has a waveform in flight -- replacing it with the new one");
    }
    message.to_string()
}

struct DeviceStrengthReport {
    strength_a: i64,
    strength_b: i64,
    soft_limit_a: i64,
    soft_limit_b: i64,
}

fn digits_only(s: &str) -> Option<i64> {
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// A physical button press on the device. Mirrors dglab-kit's
/// `parseActionMessage`: `/^feedback-(\d+)$/` (full match, digits only).
fn parse_action_message(message: &str) -> Option<i64> {
    digits_only(message.strip_prefix("feedback-")?)
}

/// The device's current per-channel strength and configured soft limit.
/// Mirrors dglab-kit's `parseDeviceMessage`:
/// `/^strength-(\d+)\+(\d+)\+(\d+)\+(\d+)$/`.
fn parse_device_message(message: &str) -> Option<DeviceStrengthReport> {
    let rest = message.strip_prefix("strength-")?;
    let parts: Vec<&str> = rest.split('+').collect();
    let [a, b, soft_a, soft_b] = parts.as_slice() else {
        return None;
    };
    Some(DeviceStrengthReport {
        strength_a: digits_only(a)?,
        strength_b: digits_only(b)?,
        soft_limit_a: digits_only(soft_a)?,
        soft_limit_b: digits_only(soft_b)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_notify_handles_both_known_v3_wire_strings() {
        assert_eq!(translate_notify("发送完毕"), "Waveform sequence complete");
        assert_eq!(
            translate_notify("当前通道A有正在发送的消息，覆盖之前的消息"),
            "Channel A already has a waveform in flight -- replacing it with the new one"
        );
        assert_eq!(
            translate_notify("当前通道B有正在发送的消息，覆盖之前的消息"),
            "Channel B already has a waveform in flight -- replacing it with the new one"
        );
    }

    #[test]
    fn translate_notify_passes_through_unrecognized_text() {
        assert_eq!(translate_notify("something else"), "something else");
    }

    #[test]
    fn parse_action_message_requires_full_digit_match() {
        assert_eq!(parse_action_message("feedback-1"), Some(1));
        assert_eq!(parse_action_message("feedback-42"), Some(42));
        assert_eq!(parse_action_message("feedback-"), None);
        assert_eq!(parse_action_message("feedback-1a"), None);
        assert_eq!(parse_action_message("feedback"), None);
        assert_eq!(parse_action_message("strength-1+2+3+4"), None);
    }

    #[test]
    fn parse_device_message_requires_exactly_four_digit_groups() {
        let report = parse_device_message("strength-10+20+50+60").unwrap();
        assert_eq!(report.strength_a, 10);
        assert_eq!(report.strength_b, 20);
        assert_eq!(report.soft_limit_a, 50);
        assert_eq!(report.soft_limit_b, 60);

        assert!(parse_device_message("strength-1+2+3").is_none());
        assert!(parse_device_message("strength-1+2+3+4+5").is_none());
        assert!(parse_device_message("strength-1+2+3+").is_none());
        assert!(parse_device_message("strength-a+2+3+4").is_none());
        assert!(parse_device_message("feedback-1").is_none());
    }

    #[test]
    fn decode_button_feedback_matches_the_empirically_confirmed_mapping() {
        assert_eq!(decode_button_feedback(0), (Some("A"), Some("circle")));
        assert_eq!(decode_button_feedback(1), (Some("A"), Some("triangle")));
        assert_eq!(decode_button_feedback(2), (Some("A"), Some("square")));
        assert_eq!(decode_button_feedback(3), (Some("A"), Some("star")));
        assert_eq!(decode_button_feedback(4), (Some("A"), Some("hexagon")));
        assert_eq!(decode_button_feedback(5), (Some("B"), Some("circle")));
        assert_eq!(decode_button_feedback(9), (Some("B"), Some("hexagon")));
        assert_eq!(decode_button_feedback(10), (None, None));
        assert_eq!(decode_button_feedback(-1), (None, None));
    }
}
