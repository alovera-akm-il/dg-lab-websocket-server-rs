//! HTTP surface for the control panel: the page itself, an SSE event
//! feed, and the command endpoints that drive whichever device is
//! currently active, over V3 (via [`super::relay_client`]) or V4 (via
//! [`super::v4_client`]) -- see `state`'s module docs for how the active
//! leg is chosen.

use std::convert::Infallible;
use std::sync::Arc;

use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::sse::{Event, KeepAlive, Sse};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use futures_util::StreamExt;
use futures_util::stream::{self, Stream};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use super::config::Config;
use super::state::{ActiveTarget, PanelState, Snapshot};
use super::{assets, commands, network, presets, qrcode, v4_commands, webhook};

#[derive(Clone)]
pub struct AppState {
    pub panel: Arc<PanelState>,
    pub config: Arc<Config>,
    pub v3_port: u16,
    pub v4_port: u16,
    pub v4_prefix: String,
    /// This machine's detected LAN IP (see [`network::detect_lan_ip`]),
    /// computed once at startup. Used only as a fallback in
    /// `resolve_ws_base` when the panel is viewed via a loopback host.
    pub lan_ip: Option<String>,
}

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/assets/{*path}", get(static_asset))
        .route("/events", get(sse_handler))
        .route("/api/presets", get(get_presets))
        .route("/api/qr/{protocol}", get(get_qr))
        .route("/api/strength", post(post_strength))
        .route("/api/clear", post(post_clear))
        .route("/api/pulse", post(post_pulse))
        .route("/api/limit", post(post_limit))
        .route("/api/webhook", post(post_webhook))
        .route("/api/reconnect", post(post_reconnect))
        .with_state(state)
}

async fn index() -> Response {
    assets::serve("index.html")
}

async fn static_asset(Path(path): Path<String>) -> Response {
    assets::serve(&path)
}

/// Derives the `scheme`/`host` to embed in the pairing QR.
///
/// 1. An explicit `PANEL_PUBLIC_WS_BASE` override always wins.
/// 2. Otherwise, read this request's own `Host` header (port stripped --
///    the V3 port is used instead). This is right whenever the panel is
///    opened from another device on the LAN using this machine's real
///    address, which is the common case and needs zero configuration.
/// 3. But if that header is a loopback address (`localhost` /
///    `127.0.0.1` / `::1` -- i.e. the panel is being viewed on the same
///    machine the server runs on), fall back to the LAN IP detected at
///    startup instead. A QR encoding `ws://localhost:...` would be
///    useless to a phone on WiFi, which resolves "localhost" to itself,
///    not to this machine.
fn resolve_ws_base(state: &AppState, headers: &HeaderMap) -> (String, String) {
    if let Some(base) = &state.config.public_ws_base
        && let Some((scheme, rest)) = base.split_once("://")
    {
        let host = rest.trim_end_matches('/');
        if !scheme.is_empty() && !host.is_empty() {
            return (scheme.to_string(), host.to_string());
        }
    }

    let host_header = headers
        .get(axum::http::header::HOST)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("127.0.0.1");
    let host = network::strip_port(host_header);

    if network::is_loopback_host(host)
        && let Some(lan_ip) = &state.lan_ip
    {
        return ("ws".to_string(), lan_ip.clone());
    }

    ("ws".to_string(), host.to_string())
}

/// Builds the V3 pairing QR's SVG markup and the deep link it encodes,
/// for a known controller id.
fn build_v3_qr(scheme: &str, host: &str, v3_port: u16, controller_id: &str) -> (String, String) {
    let url = qrcode::ws_url(scheme, host, v3_port, controller_id);
    let link = qrcode::pairing_deep_link(&url);
    let svg =
        qrcode::render_svg(&link).unwrap_or_else(|_| "<p>QR generation failed</p>".to_string());
    (svg, link)
}

/// Same as [`build_v3_qr`] but for V4's `?tid=` pairing form.
fn build_v4_qr(
    scheme: &str,
    host: &str,
    v4_port: u16,
    v4_prefix: &str,
    controller_id: &str,
) -> (String, String) {
    let url = qrcode::v4_ws_url(scheme, host, v4_port, v4_prefix, controller_id);
    let link = qrcode::v4_pairing_deep_link(&url);
    let svg =
        qrcode::render_svg(&link).unwrap_or_else(|_| "<p>QR generation failed</p>".to_string());
    (svg, link)
}

#[allow(clippy::too_many_arguments)]
fn snapshot_json(
    snapshot: &Snapshot,
    scheme: &str,
    host: &str,
    v3_port: u16,
    v4_port: u16,
    v4_prefix: &str,
) -> Value {
    let (qr_svg, pair_url) = match &snapshot.controller_id {
        Some(id) => {
            let (svg, link) = build_v3_qr(scheme, host, v3_port, id);
            (Some(svg), Some(link))
        }
        None => (None, None),
    };
    let (qr_svg_v4, pair_url_v4) = match &snapshot.v4_controller_id {
        Some(id) => {
            let (svg, link) = build_v4_qr(scheme, host, v4_port, v4_prefix, id);
            (Some(svg), Some(link))
        }
        None => (None, None),
    };

    json!({
        "status": snapshot.status.as_str(),
        "controllerId": snapshot.controller_id,
        "deviceId": snapshot.device_id,
        "v4Status": snapshot.v4_status.as_str(),
        "v4ControllerId": snapshot.v4_controller_id,
        "v4DeviceId": snapshot.v4_device_id,
        "v4DeviceName": snapshot.v4_device_name,
        "activeProtocol": snapshot.active_protocol.map(|p| p.as_str()),
        "strengthA": snapshot.strength_a,
        "strengthB": snapshot.strength_b,
        "softLimitA": snapshot.soft_limit_a,
        "softLimitB": snapshot.soft_limit_b,
        "lastButtonAction": snapshot.last_button_action,
        "limitA": snapshot.limit_a,
        "limitB": snapshot.limit_b,
        "webhookUrl": snapshot.webhook_url,
        "log": snapshot.log,
        "qrSvg": qr_svg,
        "pairUrl": pair_url,
        "qrSvgV4": qr_svg_v4,
        "pairUrlV4": pair_url_v4,
    })
}

async fn sse_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
    let (scheme, host) = resolve_ws_base(&state, &headers);
    let v3_port = state.v3_port;
    let v4_port = state.v4_port;
    let v4_prefix = state.v4_prefix.clone();
    let panel = state.panel.clone();
    let rx = panel.subscribe();

    let render = {
        let panel = panel.clone();
        let scheme = scheme.clone();
        let host = host.clone();
        move || {
            Event::default()
                .json_data(snapshot_json(
                    &panel.snapshot(),
                    &scheme,
                    &host,
                    v3_port,
                    v4_port,
                    &v4_prefix,
                ))
                .expect("snapshot JSON is always serializable")
        }
    };

    let initial = stream::once({
        let render = render.clone();
        async move { Ok(render()) }
    });

    let updates = stream::unfold((rx, render), move |(mut rx, render)| async move {
        loop {
            match rx.recv().await {
                Ok(()) => return Some((Ok(render()), (rx, render))),
                Err(broadcast::error::RecvError::Closed) => return None,
                Err(broadcast::error::RecvError::Lagged(_)) => continue,
            }
        }
    });

    Sse::new(initial.chain(updates)).keep_alive(KeepAlive::default())
}

async fn get_presets() -> Json<Value> {
    let list: Vec<Value> = presets::PRESETS
        .iter()
        .map(|p| {
            json!({
                "id": p.id,
                "label": p.label_en,
                "family": p.family.as_str(),
                "waveform": p.waveform_string(),
            })
        })
        .collect();
    Json(json!(list))
}

/// `GET /api/qr/v3` or `GET /api/qr/v4` -- the pairing QR for one
/// protocol on demand, independent of the `/events` SSE stream (e.g. for
/// a client that just wants to fetch/display/print the current code
/// without holding a live connection open). Uses the same per-request
/// `Host`-header resolution as the page's own embedded QRs, so it reflects
/// whatever address this request came in on.
async fn get_qr(
    State(state): State<AppState>,
    Path(protocol): Path<String>,
    headers: HeaderMap,
) -> Response {
    let (scheme, host) = resolve_ws_base(&state, &headers);
    let snapshot = state.panel.snapshot();

    let built = match protocol.as_str() {
        "v3" => snapshot
            .controller_id
            .as_deref()
            .map(|id| build_v3_qr(&scheme, &host, state.v3_port, id)),
        "v4" => snapshot
            .v4_controller_id
            .as_deref()
            .map(|id| build_v4_qr(&scheme, &host, state.v4_port, &state.v4_prefix, id)),
        _ => return error_response(StatusCode::BAD_REQUEST, "protocol must be \"v3\" or \"v4\""),
    };

    match built {
        Some((qr_svg, pair_url)) => {
            Json(json!({"qrSvg": qr_svg, "pairUrl": pair_url})).into_response()
        }
        None => error_response(
            StatusCode::SERVICE_UNAVAILABLE,
            "controller id not known yet -- that protocol's relay connection isn't established",
        ),
    }
}

#[derive(Deserialize)]
struct StrengthBody {
    channel: String,
    op: String,
    value: Option<i64>,
}

async fn post_strength(State(state): State<AppState>, Json(body): Json<StrengthBody>) -> Response {
    let Some(channel) = commands::parse_channel(&body.channel) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid channel");
    };
    let op = match body.op.as_str() {
        "inc" => commands::StrengthOp::Inc,
        "dec" => commands::StrengthOp::Dec,
        "set" => commands::StrengthOp::Set(body.value.unwrap_or(0)),
        _ => return error_response(StatusCode::BAD_REQUEST, "invalid op"),
    };

    // Dec is never blocked by an upper limit (it only ever reduces
    // strength). Inc's predicted result depends on the last known
    // strength -- if that's unknown (e.g. right after pairing, before
    // any status report or panel command has established a baseline),
    // the check can't be made and the command is allowed through rather
    // than blocked on a guess.
    let (current, limit) = state.panel.strength_and_limit(channel);
    let predicted = match op {
        commands::StrengthOp::Set(v) => Some(v),
        commands::StrengthOp::Inc => current.map(|c| c + 1),
        commands::StrengthOp::Dec => None,
    };
    if let (Some(limit), Some(predicted)) = (limit, predicted)
        && predicted > limit
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            &format!(
                "channel {} would reach {predicted}, above the configured upper limit of {limit}",
                commands::channel_str(channel)
            ),
        );
    }

    let (target, tx) = match active_target_and_outbound(&state) {
        Ok(pair) => pair,
        Err((status, message)) => return error_response(status, message),
    };
    let frame = match &target {
        ActiveTarget::V3 {
            controller_id,
            device_id,
        } => commands::strength_frame(controller_id, device_id, channel, op),
        ActiveTarget::V4 { device_id, slot_id } => {
            match v4_commands::strength_frame(device_id, slot_id, channel, op, current) {
                Some(frame) => frame,
                None => {
                    return error_response(
                        StatusCode::CONFLICT,
                        "current V4 strength not known yet -- try +/- first, or wait for a status report, before using Set",
                    );
                }
            }
        }
    };

    let response = send_frame(&state, &tx, frame);
    if response.status() == StatusCode::OK
        && let Some(new_value) = predicted
    {
        state.panel.apply_optimistic_strength(channel, new_value);
    }
    response
}

#[derive(Deserialize)]
struct LimitBody {
    channel: String,
    value: Option<i64>,
}

async fn post_limit(State(state): State<AppState>, Json(body): Json<LimitBody>) -> Response {
    let Some(channel) = commands::parse_channel(&body.channel) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid channel");
    };
    if let Some(v) = body.value
        && v < 0
    {
        return error_response(StatusCode::BAD_REQUEST, "limit must not be negative");
    }

    state.panel.set_limit(channel, body.value);
    let description = body
        .value
        .map_or_else(|| "cleared".to_string(), |v| v.to_string());
    state.panel.log(format!(
        "Upper limit for channel {} set to {description}",
        commands::channel_str(channel)
    ));
    StatusCode::OK.into_response()
}

#[derive(Deserialize)]
struct WebhookBody {
    url: Option<String>,
}

async fn post_webhook(State(state): State<AppState>, Json(body): Json<WebhookBody>) -> Response {
    let url = body
        .url
        .map(|u| u.trim().to_string())
        .filter(|u| !u.is_empty());
    if let Some(url) = &url
        && !webhook::is_plausible_url(url)
    {
        return error_response(
            StatusCode::BAD_REQUEST,
            "url must start with http:// or https://",
        );
    }

    let description = url.clone().unwrap_or_else(|| "cleared".to_string());
    state.panel.set_webhook_url(url);
    state.panel.log(format!("Webhook URL set to {description}"));
    StatusCode::OK.into_response()
}

#[derive(Deserialize)]
struct ClearBody {
    channel: String,
}

async fn post_clear(State(state): State<AppState>, Json(body): Json<ClearBody>) -> Response {
    let Some(channel) = commands::parse_channel(&body.channel) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid channel");
    };
    let (target, tx) = match active_target_and_outbound(&state) {
        Ok(pair) => pair,
        Err((status, message)) => return error_response(status, message),
    };
    let frame = match &target {
        ActiveTarget::V3 {
            controller_id,
            device_id,
        } => commands::clear_frame(controller_id, device_id, channel),
        ActiveTarget::V4 { device_id, slot_id } => {
            v4_commands::clear_frame(device_id, slot_id, channel)
        }
    };
    send_frame(&state, &tx, frame)
}

#[derive(Deserialize)]
struct PulseBody {
    channel: String,
    time: Option<i64>,
    waveform: String,
}

async fn post_pulse(State(state): State<AppState>, Json(body): Json<PulseBody>) -> Response {
    let Some(channel) = commands::parse_channel(&body.channel) else {
        return error_response(StatusCode::BAD_REQUEST, "invalid channel");
    };
    if body.waveform.trim().is_empty() {
        return error_response(StatusCode::BAD_REQUEST, "waveform must not be empty");
    }
    let time = body.time.unwrap_or(3);
    // Resolve preset ID to waveform data, or pass through raw custom waveform
    let waveform = presets::find(&body.waveform)
        .map(|p| p.waveform_string())
        .unwrap_or_else(|| body.waveform.clone());

    let (target, tx) = match active_target_and_outbound(&state) {
        Ok(pair) => pair,
        Err((status, message)) => return error_response(status, message),
    };
    let frame = match &target {
        ActiveTarget::V3 {
            controller_id,
            device_id,
        } => commands::pulse_frame(controller_id, device_id, channel, time, &waveform),
        ActiveTarget::V4 { device_id, slot_id } => {
            match v4_commands::pulse_frame(device_id, slot_id, channel, time * 1000, &waveform) {
                Some(frame) => frame,
                None => {
                    return error_response(
                        StatusCode::BAD_REQUEST,
                        "waveform must be in the \"<prefix>:[...]\" frame-array format for V4 (raw legacy strings aren't supported)",
                    );
                }
            }
        }
    };
    send_frame(&state, &tx, frame)
}

async fn post_reconnect(State(state): State<AppState>) -> Response {
    state.panel.request_reconnect();
    state.panel.v4_request_reconnect();
    state
        .panel
        .log("Manual reconnect requested (both protocols)");
    StatusCode::OK.into_response()
}

/// Resolves which leg is currently active and its live outbound sender
/// together, since every command endpoint needs both -- `409` if no
/// device is paired on either leg, `503` if that leg's relay connection
/// isn't currently ready to send. Kept as a small `(status, message)`
/// error rather than a built `Response` so this `Result` stays cheap to
/// pass around (clippy's `result_large_err`).
fn active_target_and_outbound(
    state: &AppState,
) -> Result<(ActiveTarget, mpsc::UnboundedSender<WsMessage>), (StatusCode, &'static str)> {
    let Some(target) = state.panel.active_target() else {
        return Err((StatusCode::CONFLICT, "no device paired"));
    };
    let tx = match &target {
        ActiveTarget::V3 { .. } => state.panel.outbound(),
        ActiveTarget::V4 { .. } => state.panel.v4_outbound(),
    };
    match tx {
        Some(tx) => Ok((target, tx)),
        None => Err((
            StatusCode::SERVICE_UNAVAILABLE,
            "relay connection not ready",
        )),
    }
}

fn send_frame(state: &AppState, tx: &mpsc::UnboundedSender<WsMessage>, frame: Value) -> Response {
    let text = frame.to_string();
    if tx.send(WsMessage::Text(text.clone().into())).is_err() {
        return error_response(StatusCode::SERVICE_UNAVAILABLE, "failed to send to relay");
    }
    state.panel.log(format!("Sent: {text}"));
    StatusCode::OK.into_response()
}

fn error_response(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({"error": message}))).into_response()
}
