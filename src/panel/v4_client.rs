//! Connects to the local V4 relay as an ordinary controller (no target on
//! connect) -- runs alongside [`super::relay_client`]'s V3 connection,
//! independently and simultaneously. See `panel::state`'s module docs for
//! how the panel decides which of the two currently drives the shared
//! strength/pulse controls. Reconnects forever (fixed 2s backoff, or
//! immediately if [`PanelState::v4_request_reconnect`] was called), each
//! attempt yielding a brand-new V4 `clientId` and thus a new pairing QR.

use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::logging::{LogLevel, log_panel};

use super::relay_client::decode_button_feedback;
use super::state::{PanelState, Protocol};

pub async fn run(v4_port: u16, prefix: String, state: Arc<PanelState>) {
    loop {
        let token = state.v4_reconnect_token();
        state.v4_begin_connecting();
        state.log(format!("Connecting to V4 relay on port {v4_port}..."));

        match connect_and_run(v4_port, &prefix, &state, &token).await {
            Ok(()) => {}
            Err(err) => {
                log_panel(LogLevel::Warn, format!("V4 relay connection error: {err}"));
                state.log_with(
                    format!("V4 relay connection error: {err}"),
                    json!({"event": "relay_error", "protocol": "v4", "error": err.to_string()}),
                );
            }
        }

        state.v4_set_disconnected();
        state.log_with(
            "Disconnected from V4 relay, reconnecting...",
            json!({"event": "relay_disconnected", "protocol": "v4"}),
        );

        tokio::select! {
            _ = token.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_secs(2)) => {}
        }
    }
}

/// `prefix` always starts with `/` (see `v4::config::normalize_prefix`);
/// avoid a doubled slash when it's the bare root.
fn connect_path(prefix: &str) -> &str {
    if prefix == "/" { "" } else { prefix }
}

async fn connect_and_run(
    v4_port: u16,
    prefix: &str,
    state: &Arc<PanelState>,
    token: &tokio_util::sync::CancellationToken,
) -> Result<(), tokio_tungstenite::tungstenite::Error> {
    let url = format!("ws://127.0.0.1:{v4_port}{}", connect_path(prefix));
    let (ws_stream, _response) = tokio_tungstenite::connect_async(url).await?;
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

    // See relay_client::connect_and_run's docs on why this must happen
    // before dropping our own `tx` and awaiting the writer task's drain.
    state.v4_clear_outbound();
    drop(tx);
    let _ = writer_task.await;
    Ok(())
}

fn handle_frame(state: &Arc<PanelState>, text: &str, tx: &mpsc::UnboundedSender<WsMessage>) {
    let Ok(value) = serde_json::from_str::<Value>(text) else {
        return;
    };
    let frame_type = value.get("type").and_then(Value::as_str);

    match frame_type {
        Some("hello") => {
            if let Some(client_id) = value.get("clientId").and_then(Value::as_str) {
                state.log_with(
                    format!("V4 connected. Controller id: {client_id}"),
                    json!({"event": "controller_connected", "protocol": "v4", "controllerId": client_id}),
                );
                state.v4_set_connected(client_id.to_string(), tx.clone());
            }
        }
        Some("client_attached") => {
            if let Some(client_id) = value.get("clientId").and_then(Value::as_str) {
                state.log_with(
                    format!("V4 APP attached: {client_id}"),
                    json!({"event": "paired", "protocol": "v4", "deviceId": client_id}),
                );
                state.v4_set_app_attached(client_id.to_string());
            }
        }
        Some("client_disconnected") => {
            state.log_with(
                "V4 APP disconnected",
                json!({"event": "device_disconnected", "protocol": "v4"}),
            );
            state.v4_clear_device();
        }
        Some("idle_timeout") => {
            state.log_with(
                "V4 connection idle-timed-out (no APP attached in time)",
                json!({"event": "relay_error", "protocol": "v4", "error": "idle_timeout"}),
            );
        }
        Some("error") => {
            let code = value
                .get("code")
                .and_then(Value::as_str)
                .unwrap_or("unknown");
            state.log_with(
                format!("V4 error: {code}"),
                json!({"event": "error", "protocol": "v4", "code": code}),
            );
        }
        Some("message") => handle_app_message(state, &value),
        Some("heartbeat") | Some("pong") => {} // don't spam the visible log
        _ => {}
    }
}

/// The outer `{"type":"message","clientId":<appId>,"data":{...}}`
/// envelope's `data` carries the actual RPC/event payload -- see
/// `dglab-kit`'s documented V4 schema (`docs/api.md`'s V4 section).
fn handle_app_message(state: &Arc<PanelState>, envelope: &Value) {
    let Some(data) = envelope.get("data") else {
        return;
    };
    match data.get("t").and_then(Value::as_str) {
        Some("ev") => handle_app_event(state, data),
        Some("resp") => {
            // device.op only resolves once a task completes/is cleared/
            // replaced/cancelled -- the panel fires commands without
            // waiting on this, so there's nothing to correlate. Logged
            // for visibility only.
            if let Some(error) = data.get("error").and_then(Value::as_str) {
                state.log(format!("V4 command failed: {error}"));
            }
        }
        _ => {}
    }
}

fn handle_app_event(state: &Arc<PanelState>, data: &Value) {
    match data.get("ev").and_then(Value::as_str) {
        Some("devices.snapshot") => {
            if let Some(device) = data
                .get("devices")
                .and_then(Value::as_array)
                .and_then(|d| d.first())
            {
                apply_device(state, device);
            }
        }
        Some("devices.patch") => {
            if let Some(device) = data
                .get("added")
                .and_then(Value::as_array)
                .and_then(|d| d.first())
            {
                apply_device(state, device);
            }
        }
        Some("slots.patch") => {
            if let Some(slots) = data.get("slots").and_then(Value::as_array) {
                for slot in slots {
                    let Some(slot_id) = slot.get("slotId").and_then(Value::as_str) else {
                        continue;
                    };
                    let (strength_a, strength_b) = extract_intensities(slot.get("props"));
                    state.v4_update_device(slot_id, strength_a, strength_b);
                }
            }
        }
        Some("custom.action") => {
            if let Some(action) = data.get("action").and_then(Value::as_i64) {
                let (channel, shape) = decode_button_feedback(action);
                state.log_with(
                    format!("V4 button feedback: action {action}"),
                    json!({"event": "button_feedback", "protocol": "v4", "code": action, "channel": channel, "shape": shape}),
                );
                state.set_button_action(Protocol::V4, action);
            }
        }
        _ => {}
    }
}

fn apply_device(state: &Arc<PanelState>, device: &Value) {
    let Some(slot_id) = device.get("slotId").and_then(Value::as_str) else {
        return;
    };
    let name = device
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("device")
        .to_string();
    let (strength_a, strength_b) = extract_intensities(device.get("props"));
    state.log_with(
        format!("V4 device available: {name} ({slot_id})"),
        json!({"event": "device_status", "protocol": "v4", "slotId": slot_id, "name": name}),
    );
    state.v4_set_device(slot_id.to_string(), name, strength_a, strength_b);
}

/// Reads `props.intensityA`/`props.intensityB` -- the documented Coyote
/// `props` field names under V4 (`docs/api.md`), distinct from V3's
/// `strength-<a>+<b>+...` naming for the same concept.
fn extract_intensities(props: Option<&Value>) -> (Option<i64>, Option<i64>) {
    let a = props
        .and_then(|p| p.get("intensityA"))
        .and_then(Value::as_i64);
    let b = props
        .and_then(|p| p.get("intensityB"))
        .and_then(Value::as_i64);
    (a, b)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connect_path_avoids_double_slash_at_root() {
        assert_eq!(connect_path("/"), "");
        assert_eq!(connect_path("/v4"), "/v4");
    }

    #[test]
    fn extract_intensities_reads_documented_field_names() {
        let props = json!({"intensityA": 12, "intensityB": 34, "power": 90});
        assert_eq!(extract_intensities(Some(&props)), (Some(12), Some(34)));
        assert_eq!(extract_intensities(None), (None, None));
        assert_eq!(extract_intensities(Some(&json!({}))), (None, None));
    }

    #[tokio::test]
    async fn hello_sets_connected_state() {
        let state = Arc::new(PanelState::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_frame(
            &state,
            &json!({"type": "hello", "clientId": "c1"}).to_string(),
            &tx,
        );
        let snap = state.snapshot();
        assert_eq!(snap.v4_controller_id.as_deref(), Some("c1"));
    }

    #[tokio::test]
    async fn devices_snapshot_populates_tracked_device_and_activates() {
        let state = Arc::new(PanelState::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_frame(
            &state,
            &json!({"type": "hello", "clientId": "c1"}).to_string(),
            &tx,
        );
        handle_frame(
            &state,
            &json!({"type": "client_attached", "clientId": "app1"}).to_string(),
            &tx,
        );
        let msg = json!({
            "type": "message",
            "clientId": "app1",
            "data": {"t": "ev", "ev": "devices.snapshot", "devices": [
                {"slotId": "slot1", "name": "Coyote", "type": "COYOTE_030", "props": {"intensityA": 5, "intensityB": 6}}
            ]},
        });
        handle_frame(&state, &msg.to_string(), &tx);

        let snap = state.snapshot();
        assert_eq!(snap.v4_device_slot_id.as_deref(), Some("slot1"));
        assert_eq!(snap.strength_a, Some(5));
        assert_eq!(snap.strength_b, Some(6));
        assert!(matches!(
            state.active_target(),
            Some(super::super::state::ActiveTarget::V4 { .. })
        ));
    }

    #[tokio::test]
    async fn custom_action_records_button_feedback_when_active() {
        let state = Arc::new(PanelState::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_frame(
            &state,
            &json!({"type": "hello", "clientId": "c1"}).to_string(),
            &tx,
        );
        handle_frame(
            &state,
            &json!({"type": "client_attached", "clientId": "app1"}).to_string(),
            &tx,
        );
        let snapshot_msg = json!({
            "type": "message", "clientId": "app1",
            "data": {"t": "ev", "ev": "devices.snapshot", "devices": [{"slotId": "slot1", "name": "Coyote"}]},
        });
        handle_frame(&state, &snapshot_msg.to_string(), &tx);

        let action_msg = json!({
            "type": "message", "clientId": "app1",
            "data": {"t": "ev", "ev": "custom.action", "action": 3},
        });
        handle_frame(&state, &action_msg.to_string(), &tx);
        assert_eq!(state.snapshot().last_button_action, Some(3));
    }

    #[tokio::test]
    async fn client_disconnected_clears_the_device() {
        let state = Arc::new(PanelState::new());
        let (tx, _rx) = mpsc::unbounded_channel();
        handle_frame(
            &state,
            &json!({"type": "hello", "clientId": "c1"}).to_string(),
            &tx,
        );
        handle_frame(
            &state,
            &json!({"type": "client_attached", "clientId": "app1"}).to_string(),
            &tx,
        );
        handle_frame(
            &state,
            &json!({"type": "client_disconnected", "clientId": "app1"}).to_string(),
            &tx,
        );

        let snap = state.snapshot();
        assert_eq!(snap.v4_device_id, None);
        assert_eq!(snap.active_protocol, None);
    }
}
