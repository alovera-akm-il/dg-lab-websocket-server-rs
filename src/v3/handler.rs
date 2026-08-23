//! V3 connection lifecycle and message routing, mirroring `onOpen`,
//! `onMessage`, `onClose` and their private handlers on `V3SocketServer`.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use axum::Router;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::{FromRequestParts, Request, State};
use axum::http::{StatusCode, Uri};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::logging::{LogLevel, log_v3};

use super::config::{CLOSE_INVALID_TARGET_ID, Config, PULSE_REPLACE_DELAY_MS};
use super::protocol::{self, Channel, Frame};
use super::pulse;
use super::state::Hub;

#[derive(Clone)]
pub struct AppState {
    pub hub: Arc<Hub>,
    pub config: Arc<Config>,
}

pub fn router(state: AppState) -> Router {
    // V3 has no path restriction -- any path (including the root) is
    // upgrade-eligible, since the path tail doubles as an implicit
    // targetId.
    Router::new().fallback(handle_upgrade).with_state(state)
}

async fn handle_upgrade(State(state): State<AppState>, req: Request) -> Response {
    let (mut parts, _body) = req.into_parts();
    match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(ws) => {
            let target_id = extract_target_id(&parts.uri);
            ws.on_upgrade(move |socket| handle_socket(socket, state, target_id))
        }
        Err(_) => upgrade_required_response(),
    }
}

fn upgrade_required_response() -> Response {
    let body = json!({
        "ok": false,
        "error": "websocket_required",
        "protocol": "DG-LAB WebSocket V3",
    })
    .to_string();
    Response::builder()
        .status(StatusCode::UPGRADE_REQUIRED)
        .header("content-type", "application/json; charset=utf-8")
        .header("access-control-allow-origin", "*")
        .body(axum::body::Body::from(body))
        .expect("static header names/values are valid")
}

/// `?targetId=` -> `?tid=` -> URL path tail, mirroring `handleFetch`.
/// Each candidate is trimmed before its "is it present" check (a
/// deliberate simplification vs. JS's `||` chain, which only treats a
/// literal empty string as falsy and would let a whitespace-only query
/// value win before falling through to the invalid-target close path --
/// here it's just skipped instead, ending up as "no target" instead of
/// "invalid target").
fn extract_target_id(uri: &Uri) -> Option<String> {
    let query: HashMap<String, String> = uri
        .query()
        .and_then(|q| serde_urlencoded::from_str(q).ok())
        .unwrap_or_default();

    let path_tail = uri.path().strip_prefix('/').unwrap_or(uri.path());
    let candidates = [
        query.get("targetId").map(String::as_str),
        query.get("tid").map(String::as_str),
        Some(path_tail),
    ];

    candidates
        .into_iter()
        .flatten()
        .map(str::trim)
        .find(|s| !s.is_empty())
        .map(str::to_string)
}

async fn handle_socket(socket: WebSocket, state: AppState, target_id: Option<String>) {
    let client_id = Uuid::new_v4().to_string();
    let (mut ws_sender, mut ws_receiver) = socket.split();
    let (tx, mut rx) = mpsc::unbounded_channel::<Message>();

    let writer_task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            if ws_sender.send(msg).await.is_err() {
                break;
            }
        }
    });

    log_v3(LogLevel::Debug, target_id.clone().unwrap_or_default());

    if let Some(tid) = &target_id
        && !state.hub.is_available_target(tid)
    {
        log_v3(
            LogLevel::Warn,
            format!("rejected connection: invalid targetId={tid}"),
        );
        close_invalid_target(&tx, "", tid);
        drop(tx);
        let _ = writer_task.await;
        return;
    }

    let (idle_token, shutdown_token) = state.hub.register(client_id.clone(), tx.clone());
    spawn_idle_timer(
        state.hub.clone(),
        state.config.idle_timeout_ms,
        client_id.clone(),
        idle_token,
        tx.clone(),
        shutdown_token.clone(),
    );

    send_frame(
        &tx,
        json!({"type":"bind","clientId":client_id,"targetId":"","message":"targetId"}),
    );

    if let Some(tid) = target_id {
        let result = state.hub.pair(&tid, &client_id);
        if !result.ok {
            log_v3(
                LogLevel::Warn,
                format!(
                    "rejected connection: targetId={tid} appId={client_id} code={}",
                    result.code
                ),
            );
            state.hub.unregister(&client_id);
            close_invalid_target(&tx, &client_id, &tid);
            drop(tx);
            let _ = writer_task.await;
            return;
        }

        let bind_msg =
            json!({"type":"bind","clientId":tid,"targetId":client_id,"message":result.code});
        send_to_client(&state.hub, &tid, bind_msg.clone());
        send_frame(&tx, bind_msg);
    }

    log_v3(
        LogLevel::Info,
        format!("new WebSocket connection: {client_id}"),
    );

    loop {
        tokio::select! {
            _ = shutdown_token.cancelled() => break,
            maybe_msg = ws_receiver.next() => {
                match maybe_msg {
                    Some(Ok(Message::Text(text))) => on_message(&state, &client_id, &text),
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

fn close_invalid_target(tx: &mpsc::UnboundedSender<Message>, client_id: &str, target_id: &str) {
    send_frame(
        tx,
        json!({"type":"error","clientId":client_id,"targetId":target_id,"message":CLOSE_INVALID_TARGET_ID.to_string()}),
    );
    let _ = tx.send(Message::Close(Some(CloseFrame {
        code: CLOSE_INVALID_TARGET_ID,
        reason: "invalid_target_id".into(),
    })));
}

fn spawn_idle_timer(
    hub: Arc<Hub>,
    timeout_ms: u64,
    client_id: String,
    idle_token: CancellationToken,
    tx: mpsc::UnboundedSender<Message>,
    shutdown_token: CancellationToken,
) {
    tokio::spawn(async move {
        tokio::select! {
            _ = idle_token.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                if hub.is_connected(&client_id) && !hub.is_bound(&client_id) {
                    send_frame(&tx, json!({"type":"error","clientId":client_id,"targetId":"","message":"idle_timeout"}));
                    let _ = tx.send(Message::Close(Some(CloseFrame { code: 1000, reason: "idle_timeout".into() })));
                    shutdown_token.cancel();
                    log_v3(LogLevel::Warn, format!("closed idle unpaired connection: {client_id}"));
                }
            }
        }
    });
}

fn on_close(state: &AppState, client_id: &str) {
    let outcome = state.hub.remove(client_id);
    if outcome.paired_id.is_none() {
        log_v3(LogLevel::Info, format!("[disconnect] {client_id}"));
        return;
    }

    if let Some((_partner_id, partner_tx, partner_shutdown)) = outcome.partner {
        let break_client_id = outcome
            .web_id
            .clone()
            .unwrap_or_else(|| outcome.paired_id.clone().unwrap());
        let break_target_id = outcome
            .app_id
            .clone()
            .unwrap_or_else(|| client_id.to_string());
        send_frame(
            &partner_tx,
            json!({"type":"break","clientId":break_client_id,"targetId":break_target_id,"message":"209"}),
        );
        let _ = partner_tx.send(Message::Close(Some(CloseFrame {
            code: 1000,
            reason: "partner_disconnected".into(),
        })));
        partner_shutdown.cancel();
    }

    log_v3(
        LogLevel::Info,
        format!(
            "[disconnect] {client_id}, paired with: {}",
            outcome.paired_id.unwrap()
        ),
    );
}

// ---- message routing ------------------------------------------------

fn on_message(state: &AppState, client_id: &str, text: &str) {
    log_v3(
        LogLevel::Debug,
        format!("received message [{client_id}]: {text}"),
    );

    let frame = match protocol::parse_frame(text) {
        Ok(f) => f,
        Err(code) => {
            log_v3(
                LogLevel::Warn,
                format!("malformed message [{client_id}]: code={code}"),
            );
            send_error(&state.hub, client_id, "", "", code);
            return;
        }
    };

    if !protocol::validate_source(&frame, client_id) {
        log_v3(
            LogLevel::Warn,
            format!(
                "illegal message source [{client_id}]: clientId={} targetId={}",
                frame.client_id, frame.target_id
            ),
        );
        send_error(
            &state.hub,
            client_id,
            &frame.client_id,
            &frame.target_id,
            "404",
        );
        return;
    }

    if protocol::type_is(&frame.type_, "bind") {
        handle_bind(state, client_id, &frame);
        return;
    }
    if protocol::is_app_report_message(&frame.message) {
        forward_message(state, client_id, &frame);
        return;
    }

    match protocol::numeric_type(&frame.type_) {
        Some(route_type @ 1..=3) => {
            handle_strength_adjust(state, client_id, &frame, route_type);
            return;
        }
        Some(4) => {
            handle_custom_strength(state, client_id, &frame);
            return;
        }
        _ => {}
    }

    if protocol::type_is(&frame.type_, "clientMsg") {
        handle_client_message(state, client_id, &frame);
        return;
    }
    if protocol::type_is(&frame.type_, "heartbeat") {
        return;
    }

    forward_message(state, client_id, &frame);
}

fn handle_bind(state: &AppState, sender_id: &str, frame: &Frame) {
    let result = state.hub.pair(&frame.client_id, &frame.target_id);
    log_v3(
        LogLevel::Debug,
        format!(
            "bind request [{sender_id}]: web={} app={} code={}",
            frame.client_id, frame.target_id, result.code
        ),
    );
    let response = json!({"type":"bind","clientId":frame.client_id,"targetId":frame.target_id,"message":result.code});

    if !result.ok {
        log_v3(
            LogLevel::Warn,
            format!(
                "bind failed: {} <-> {}, code={}",
                frame.client_id, frame.target_id, result.code
            ),
        );
        send_to_client(&state.hub, sender_id, response);
        return;
    }

    send_to_client(&state.hub, &frame.client_id, response.clone());
    if frame.target_id != frame.client_id {
        send_to_client(&state.hub, &frame.target_id, response);
    }
}

fn handle_strength_adjust(state: &AppState, sender_id: &str, frame: &Frame, route_type: i64) {
    if sender_id != frame.client_id {
        log_v3(
            LogLevel::Warn,
            format!(
                "illegal strength-adjust source: sender={sender_id} clientId={} targetId={}",
                frame.client_id, frame.target_id
            ),
        );
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "404",
        );
        return;
    }
    if !state.hub.is_paired(&frame.client_id, &frame.target_id) {
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "402",
        );
        return;
    }
    let Some(channel) = protocol::normalize_channel(frame.channel.as_ref(), Some(Channel::A))
    else {
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "406",
        );
        return;
    };

    let send_type = route_type - 1;
    let strength = if route_type == 3 {
        protocol::normalize_number(frame.strength.as_ref(), 0)
    } else {
        1
    };
    let message = format!("strength-{}+{send_type}+{strength}", channel.number());
    let sent = send_to_client(
        &state.hub,
        &frame.target_id,
        json!({"type":"msg","clientId":frame.client_id,"targetId":frame.target_id,"message":message}),
    );
    if !sent {
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "404",
        );
    }
}

fn handle_custom_strength(state: &AppState, sender_id: &str, frame: &Frame) {
    if sender_id != frame.client_id {
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "404",
        );
        return;
    }
    if !state.hub.is_paired(&frame.client_id, &frame.target_id) {
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "402",
        );
        return;
    }
    let Some(channel) = protocol::normalize_channel(frame.channel.as_ref(), None) else {
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "406",
        );
        return;
    };

    if frame.message.contains("clear") {
        let message = format!("clear-{}", channel.number());
        let sent = send_to_client(
            &state.hub,
            &frame.target_id,
            json!({"type":"msg","clientId":frame.client_id,"targetId":frame.target_id,"message":message}),
        );
        if !sent {
            send_error(
                &state.hub,
                sender_id,
                &frame.client_id,
                &frame.target_id,
                "404",
            );
            return;
        }
        state.hub.clear_pulse_slot(&frame.client_id, channel);
        send_to_client(
            &state.hub,
            sender_id,
            notify_done(&frame.client_id, &frame.target_id),
        );
        return;
    }

    let strength = protocol::normalize_number(frame.strength.as_ref(), 0);
    let message = format!("strength-{}+2+{strength}", channel.number());
    let sent = send_to_client(
        &state.hub,
        &frame.target_id,
        json!({"type":"msg","clientId":frame.client_id,"targetId":frame.target_id,"message":message}),
    );
    if !sent {
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "404",
        );
    }
}

fn handle_client_message(state: &AppState, sender_id: &str, frame: &Frame) {
    if sender_id != frame.client_id {
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "404",
        );
        return;
    }
    if !state.hub.is_paired(&frame.client_id, &frame.target_id) {
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "402",
        );
        return;
    }
    let Some(channel) = protocol::normalize_channel(frame.channel.as_ref(), None) else {
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "406",
        );
        return;
    };
    if !state.hub.is_connected(&frame.target_id) {
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "404",
        );
        return;
    }

    let time = protocol::normalize_positive_integer(
        frame.time.as_ref(),
        state.config.default_punishment_duration,
    );
    let sends_per_second = pulse::normalize_sends_per_second(state.config.default_punishment_time);
    let interval_ms = (1000.0 / sends_per_second as f64).max(1.0) as u64;
    let sequence = pulse::build_pulse_sequence(&frame.message, channel, time, sends_per_second);

    let packet_count = sequence.packet_count;
    let total_frames = sequence.total_frames;
    let parsed = sequence.parsed;

    queue_pulse(
        state.clone(),
        frame.client_id.clone(),
        frame.target_id.clone(),
        channel,
        sequence.messages,
        interval_ms,
        sender_id.to_string(),
    );

    log_v3(
        LogLevel::Info,
        format!(
            "[{}] waveform message sent: channel {}, packets={packet_count}, duration={time}s{}",
            frame.client_id,
            channel.letter(),
            match (parsed, total_frames) {
                (true, Some(n)) => format!(", total frames={n}"),
                _ => ", raw passthrough format".to_string(),
            }
        ),
    );
}

fn forward_message(state: &AppState, sender_id: &str, frame: &Frame) {
    if !state.hub.is_paired(&frame.client_id, &frame.target_id) {
        log_v3(
            LogLevel::Warn,
            format!(
                "forward message: not paired: clientId={} targetId={} type={:?}",
                frame.client_id, frame.target_id, frame.type_
            ),
        );
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "402",
        );
        return;
    }

    let recipient_id: &str = if sender_id == frame.client_id {
        &frame.target_id
    } else {
        &frame.client_id
    };
    let should_swap = protocol::type_is(&frame.type_, "msg") && state.hub.is_app(sender_id);
    let (out_client_id, out_target_id): (Value, Value) = if should_swap {
        (json!(sender_id), json!(recipient_id))
    } else {
        (json!(frame.client_id), json!(frame.target_id))
    };

    let sent = send_to_client(
        &state.hub,
        recipient_id,
        json!({"type":frame.type_.clone(),"clientId":out_client_id,"targetId":out_target_id,"message":frame.message}),
    );

    if !sent {
        log_v3(
            LogLevel::Warn,
            format!(
                "forward message: recipient not found: sender={sender_id} recipient={recipient_id}"
            ),
        );
        send_error(
            &state.hub,
            sender_id,
            &frame.client_id,
            &frame.target_id,
            "404",
        );
        return;
    }

    log_v3(
        LogLevel::Debug,
        format!(
            "forwarded message: sender={sender_id} recipient={recipient_id} type={:?} message={}",
            frame.type_, frame.message
        ),
    );
}

// ---- pulse waveform sequencing ---------------------------------------

fn queue_pulse(
    state: AppState,
    client_id: String,
    target_id: String,
    channel: Channel,
    messages: Vec<String>,
    interval_ms: u64,
    source_id: String,
) {
    let (token, had_existing) = state.hub.start_pulse_slot(&client_id, channel);

    if had_existing {
        log_v3(
            LogLevel::Info,
            format!(
                "[{client_id}:{}] clearing existing timer, preparing to send new message",
                channel.letter()
            ),
        );
        send_to_client(
            &state.hub,
            &target_id,
            json!({"type":"msg","clientId":client_id,"targetId":target_id,"message":format!("clear-{}", channel.number())}),
        );
        send_to_client(
            &state.hub,
            &source_id,
            json!({"type":"notify","clientId":client_id,"targetId":target_id,"message":format!("当前通道{}有正在发送的消息，覆盖之前的消息", channel.letter())}),
        );

        tokio::spawn(async move {
            tokio::select! {
                _ = token.cancelled() => {}
                _ = tokio::time::sleep(Duration::from_millis(PULSE_REPLACE_DELAY_MS)) => {
                    start_pulse(state, client_id, target_id, channel, messages, interval_ms, source_id, token);
                }
            }
        });
        return;
    }

    start_pulse(
        state,
        client_id,
        target_id,
        channel,
        messages,
        interval_ms,
        source_id,
        token,
    );
}

#[allow(clippy::too_many_arguments)]
fn start_pulse(
    state: AppState,
    client_id: String,
    target_id: String,
    channel: Channel,
    messages: Vec<String>,
    interval_ms: u64,
    source_id: String,
    token: CancellationToken,
) {
    let key = format!("{client_id}:{}", channel.letter());
    let mut iter = messages.into_iter();

    let Some(first) = iter.next() else {
        log_v3(
            LogLevel::Warn,
            format!("[{key}] waveform message is empty, stopping"),
        );
        state.hub.finish_pulse_slot(&client_id, channel);
        send_to_client(&state.hub, &source_id, notify_done(&client_id, &target_id));
        return;
    };

    let remaining: Vec<String> = iter.collect();
    let total = 1 + remaining.len();
    send_pulse_packet(&state.hub, &key, &target_id, &client_id, &first, 1, total);

    if remaining.is_empty() {
        log_v3(
            LogLevel::Info,
            format!("[{key}] message send complete (single packet)"),
        );
        state.hub.finish_pulse_slot(&client_id, channel);
        send_to_client(&state.hub, &source_id, notify_done(&client_id, &target_id));
        return;
    }

    log_v3(
        LogLevel::Info,
        format!("[{key}] sending message, remaining={}", remaining.len()),
    );

    tokio::spawn(async move {
        let mut idx = 0usize;
        loop {
            tokio::select! {
                _ = token.cancelled() => return,
                _ = tokio::time::sleep(Duration::from_millis(interval_ms)) => {
                    if !state.hub.is_connected(&target_id) {
                        log_v3(LogLevel::Warn, format!("[{key}] target connection closed, stopping"));
                        state.hub.finish_pulse_slot(&client_id, channel);
                        return;
                    }
                    let Some(message) = remaining.get(idx) else {
                        log_v3(LogLevel::Warn, format!("[{key}] waveform message sequence exhausted, stopping"));
                        state.hub.finish_pulse_slot(&client_id, channel);
                        send_to_client(&state.hub, &source_id, notify_done(&client_id, &target_id));
                        return;
                    };
                    send_pulse_packet(&state.hub, &key, &target_id, &client_id, message, idx + 2, total);
                    idx += 1;
                    if idx >= remaining.len() {
                        log_v3(LogLevel::Info, format!("[{key}] message send complete"));
                        state.hub.finish_pulse_slot(&client_id, channel);
                        send_to_client(&state.hub, &source_id, notify_done(&client_id, &target_id));
                        return;
                    }
                }
            }
        }
    });
}

fn send_pulse_packet(
    hub: &Hub,
    key: &str,
    target_id: &str,
    client_id: &str,
    message: &str,
    index: usize,
    total: usize,
) {
    let sent = send_to_client(
        hub,
        target_id,
        json!({"type":"msg","clientId":client_id,"targetId":target_id,"message":message}),
    );
    if sent {
        log_v3(
            LogLevel::Info,
            format!("[{key}] sent packet {index}/{total}: {message}"),
        );
    }
}

fn notify_done(client_id: &str, target_id: &str) -> Value {
    json!({"type":"notify","clientId":client_id,"targetId":target_id,"message":"发送完毕"})
}

// ---- low-level send helpers -------------------------------------------

fn send_frame(tx: &mpsc::UnboundedSender<Message>, value: Value) -> bool {
    tx.send(Message::Text(value.to_string().into())).is_ok()
}

fn send_to_client(hub: &Hub, client_id: &str, value: Value) -> bool {
    match hub.sender(client_id) {
        Some(tx) => send_frame(&tx, value),
        None => false,
    }
}

fn send_error(hub: &Hub, reply_to: &str, client_id: &str, target_id: &str, code: &str) {
    log_v3(
        LogLevel::Debug,
        format!(
            "sending error response: clientId={} targetId={} code={code}",
            if client_id.is_empty() { "-" } else { client_id },
            if target_id.is_empty() { "-" } else { target_id },
        ),
    );
    send_to_client(
        hub,
        reply_to,
        json!({"type":"error","clientId":client_id,"targetId":target_id,"message":code}),
    );
}
