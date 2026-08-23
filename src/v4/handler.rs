//! V4 connection lifecycle and message routing, mirroring `onOpen`,
//! `onMessage`, `onClose` and their private handlers on `RelayServer`.

use std::collections::HashMap;
use std::sync::Arc;

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;

use crate::logging::{LogLevel, log_v4};

use super::config::Config;
use super::state::{CloseOutcome, Hub};

const CLOSE_CONTROLLER_DISCONNECTED: u16 = 4000;
const CLOSE_CONTROLLER_NOT_FOUND: u16 = 4001;
pub const CLOSE_IDLE_TIMEOUT: u16 = 4002;
const CLIENT_ID_BYTES: usize = 4;

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub config: Arc<Config>,
}

pub fn router(state: AppState) -> Router {
    let prefix = state.config.prefix.clone();
    Router::new()
        .route(&prefix, get(handle_upgrade))
        .fallback(|| async { (StatusCode::NOT_FOUND, "Not Found") })
        .with_state(state)
}

async fn handle_upgrade(State(state): State<AppState>, req: Request) -> Response {
    let (mut parts, _body) = req.into_parts();
    match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(ws) => {
            let tid = extract_tid(parts.uri.query());
            ws.on_upgrade(move |socket| handle_socket(socket, state, tid))
        }
        Err(_) => (StatusCode::UPGRADE_REQUIRED, "WebSocket upgrade required").into_response(),
    }
}

/// `?targetId=` -> `?tid=` only -- unlike V3, there is no path-tail
/// fallback.
fn extract_tid(query: Option<&str>) -> Option<String> {
    let query: HashMap<String, String> = query
        .and_then(|q| serde_urlencoded::from_str(q).ok())
        .unwrap_or_default();
    query.get("targetId").or_else(|| query.get("tid")).cloned()
}

fn generate_client_id(hub: &Hub) -> String {
    loop {
        let mut bytes = [0u8; CLIENT_ID_BYTES];
        rand::fill(&mut bytes);
        let candidate = bytes.iter().map(|b| format!("{b:02x}")).collect::<String>();
        if !hub.id_in_use(&candidate) {
            return candidate;
        }
    }
}

async fn handle_socket(socket: WebSocket, state: AppState, tid: Option<String>) {
    let client_id = generate_client_id(&state.hub);
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    send_frame(&tx, json!({"type":"hello","clientId":client_id}));
    let shutdown_token = state.hub.register_connection(client_id.clone(), tx.clone());

    if let Some(tid) = tid {
        if !state.hub.attach_device(&tid, &client_id) {
            send_frame(&tx, json!({"type":"error","code":"controller_not_found"}));
            let _ = tx.send(Message::Close(Some(CloseFrame {
                code: CLOSE_CONTROLLER_NOT_FOUND,
                reason: "controller_not_found".into(),
            })));
            log_v4(
                LogLevel::Warn,
                format!(
                    "device rejected device={client_id} target={tid} reason=controller_not_found"
                ),
            );
            state.hub.remove_connection(&client_id);
            drop(tx);
            let _ = writer_task.await;
            return;
        }

        send_frame(&tx, json!({"type":"controller_attached","clientId":tid}));
        if let Some(controller_tx) = state.hub.sender_of(&tid) {
            send_frame(
                &controller_tx,
                json!({"type":"client_attached","clientId":client_id}),
            );
        }
        log_v4(
            LogLevel::Info,
            format!("device attached device={client_id} controller={tid}"),
        );
    } else {
        let idle_token = state.hub.register_controller(client_id.clone());
        super::idle::spawn(
            state.hub.clone(),
            state.config.idle_timeout_ms,
            client_id.clone(),
            idle_token,
            tx.clone(),
        );
        log_v4(
            LogLevel::Info,
            format!("controller connected controller={client_id}"),
        );
    }

    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => break,
            maybe_msg = ws_receiver.next() => {
                match maybe_msg {
                    Some(Ok(Message::Text(text))) => on_message(&state, &client_id, &text),
                    Some(Ok(Message::Pong(_))) => state.hub.reset_missed_pongs(&client_id),
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(_)) => {}
                    Some(Err(_)) => break,
                }
            }
        }
    }

    on_close(&state, &client_id);
    drop(tx);
    let _ = writer_task.await;
}

fn on_close(state: &AppState, client_id: &str) {
    match state.hub.remove_connection(client_id) {
        CloseOutcome::WasController { devices } => {
            for (device_id, device_tx, device_shutdown) in &devices {
                send_frame(
                    device_tx,
                    json!({"type":"controller_disconnected","clientId":client_id}),
                );
                let _ = device_tx.send(Message::Close(Some(CloseFrame {
                    code: CLOSE_CONTROLLER_DISCONNECTED,
                    reason: "controller_disconnected".into(),
                })));
                device_shutdown.cancel();
                log_v4(
                    LogLevel::Info,
                    format!("evicted device device={device_id} controller={client_id}"),
                );
            }
            log_v4(
                LogLevel::Info,
                format!(
                    "controller disconnected controller={client_id} evicted={}",
                    devices.len()
                ),
            );
        }
        CloseOutcome::WasDevice {
            controller,
            restart_idle,
        } => {
            if let Some((controller_id, controller_tx)) = controller {
                send_frame(
                    &controller_tx,
                    json!({"type":"client_disconnected","clientId":client_id}),
                );
                if let Some(idle_token) = restart_idle {
                    super::idle::spawn(
                        state.hub.clone(),
                        state.config.idle_timeout_ms,
                        controller_id.clone(),
                        idle_token,
                        controller_tx,
                    );
                }
                log_v4(
                    LogLevel::Info,
                    format!("device disconnected device={client_id} controller={controller_id}"),
                );
            }
        }
        CloseOutcome::WasUnknown => {}
    }
}

// ---- message routing --------------------------------------------------

fn on_message(state: &AppState, client_id: &str, text: &str) {
    let parsed: Value = match serde_json::from_str(text) {
        Ok(v) => v,
        Err(_) => {
            log_v4(
                LogLevel::Warn,
                format!("invalid WS JSON connection={client_id}"),
            );
            return;
        }
    };

    if handle_ping_message(state, client_id, &parsed) {
        return;
    }

    let Some(obj) = parsed.as_object() else {
        log_v4(
            LogLevel::Debug,
            format!("ignoring WS frame connection={client_id} type=-"),
        );
        return;
    };
    if obj.get("type").and_then(Value::as_str) != Some("message") {
        let type_desc = obj.get("type").and_then(Value::as_str).unwrap_or("-");
        log_v4(
            LogLevel::Debug,
            format!("ignoring WS frame connection={client_id} type={type_desc}"),
        );
        return;
    }
    let data = obj.get("data").cloned().unwrap_or(Value::Null);

    if state.hub.is_controller(client_id) {
        let Some(device_id) = obj.get("clientId").and_then(Value::as_str) else {
            log_v4(
                LogLevel::Warn,
                format!("WS target missing controller={client_id}"),
            );
            send_to_client(
                &state.hub,
                client_id,
                json!({"type":"error","code":"bad_request","message":"message.clientId is required"}),
            );
            return;
        };

        match state.hub.device_sender_under(client_id, device_id) {
            Some(device_tx) => {
                send_frame(&device_tx, json!({"type":"message","data":data}));
                log_v4(
                    LogLevel::Info,
                    format!("WS forwarded controller={client_id} device={device_id}"),
                );
            }
            None => {
                log_v4(
                    LogLevel::Warn,
                    format!("WS device not found controller={client_id} device={device_id}"),
                );
                send_to_client(
                    &state.hub,
                    client_id,
                    json!({"type":"error","code":"client_not_found","clientId":device_id}),
                );
            }
        }
        return;
    }

    match state
        .hub
        .controller_of(client_id)
        .and_then(|cid| state.hub.sender_of(&cid).map(|tx| (cid, tx)))
    {
        Some((controller_id, controller_tx)) => {
            send_frame(
                &controller_tx,
                json!({"type":"message","clientId":client_id,"data":data}),
            );
            log_v4(
                LogLevel::Info,
                format!("WS reported device={client_id} controller={controller_id}"),
            );
        }
        None => {
            log_v4(
                LogLevel::Warn,
                format!("WS controller missing device={client_id}"),
            );
        }
    }
}

/// App-level JSON `{"type":"ping"}`/`{"type":"pong"}` handshake, unrelated
/// to native WS ping/pong frames. Returns `true` if the message was
/// consumed here.
fn handle_ping_message(state: &AppState, client_id: &str, value: &Value) -> bool {
    let Some(obj) = value.as_object() else {
        return false;
    };
    match obj.get("type").and_then(Value::as_str) {
        Some("pong") => true,
        Some("ping") => {
            let ts = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_millis() as u64)
                .unwrap_or(0);
            send_to_client(&state.hub, client_id, json!({"type":"pong","ts":ts}));
            true
        }
        _ => false,
    }
}

// ---- low-level send helpers -------------------------------------------

fn send_frame(tx: &mpsc::UnboundedSender<Message>, value: Value) -> bool {
    tx.send(Message::Text(value.to_string().into())).is_ok()
}

fn send_to_client(hub: &Hub, client_id: &str, value: Value) -> bool {
    match hub.sender_of(client_id) {
        Some(tx) => send_frame(&tx, value),
        None => false,
    }
}
