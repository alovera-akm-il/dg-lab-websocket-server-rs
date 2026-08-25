//! End-to-end check of the control panel's strength ramp profiles:
//! spins up a real V4 relay and a panel connected to it, drives a
//! simulated APP through the V4 handshake, starts a short linear ramp
//! over the panel's HTTP API, and confirms the simulated device
//! receives `AddIntensity` frames whose deltas actually converge toward
//! the ramp's target -- proof the runner task drives the schedule
//! itself rather than just accepting the HTTP call. Also covers the
//! override (`/api/strength` cancels an active ramp), explicit stop,
//! and the upper-limit rejection.

use std::sync::Arc;
use std::time::Duration;

use dg_lab_websocket_server_rs::{panel, v3, v4};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn spawn_v3() -> u16 {
    let config = Arc::new(v3::config::Config {
        port: 0,
        heartbeat_ms: 60_000,
        idle_timeout_ms: 300_000,
        default_punishment_time: 1,
        default_punishment_duration: 5,
    });
    let (_hub, router) = v3::build(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    port
}

async fn spawn_v4() -> u16 {
    let config = Arc::new(v4::config::Config {
        port: 0,
        heartbeat_ms: 60_000,
        ws_ping_ms: 60_000,
        max_missed_ws_pongs: 3,
        idle_timeout_ms: 300_000,
        prefix: "/".to_string(),
    });
    let (_hub, router) = v4::build(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    port
}

async fn spawn_panel(v3_port: u16, v4_port: u16) -> (String, Arc<panel::state::PanelState>) {
    let config = Arc::new(panel::config::Config {
        port: 0,
        public_ws_base: None,
        webhook_url: None,
    });
    let (panel_state, router) = panel::build(config, v3_port, v4_port, "/".to_string());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}"), panel_state)
}

async fn wait_for_v4_controller_id(panel_state: &panel::state::PanelState) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(id) = panel_state.snapshot().v4_controller_id {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("panel never obtained a V4 controller id")
}

async fn wait_for_slot(panel_state: &panel::state::PanelState) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(id) = panel_state.snapshot().v4_device_slot_id {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("panel never tracked a V4 device slot")
}

async fn recv_json(ws: &mut WsStream) -> Value {
    let msg = tokio::time::timeout(Duration::from_secs(3), ws.next())
        .await
        .expect("timed out waiting for a message")
        .expect("stream ended")
        .expect("websocket error");
    match msg {
        Message::Text(text) => serde_json::from_str(&text).expect("valid JSON"),
        other => panic!("expected a text frame, got {other:?}"),
    }
}

async fn recv_until(ws: &mut WsStream, predicate: impl Fn(&Value) -> bool) -> Value {
    for _ in 0..40 {
        let value = recv_json(ws).await;
        if predicate(&value) {
            return value;
        }
    }
    panic!("did not see a matching frame within 40 messages");
}

/// Pairs a simulated V4 Coyote device (starting at 0 strength on both
/// channels) and returns the connected APP socket plus the panel's HTTP
/// base URL.
async fn pair_v4_device() -> (WsStream, String, Arc<panel::state::PanelState>) {
    let v3_port = spawn_v3().await;
    let v4_port = spawn_v4().await;
    let (panel_base, panel_state) = spawn_panel(v3_port, v4_port).await;

    let v4_controller_id = wait_for_v4_controller_id(&panel_state).await;
    let (mut app, _) = connect_async(format!("ws://127.0.0.1:{v4_port}/?tid={v4_controller_id}"))
        .await
        .expect("APP connects");
    recv_json(&mut app).await; // hello
    recv_until(&mut app, |v| v["type"] == "controller_attached").await;
    app.send(Message::text(
        json!({
            "type": "message",
            "data": {"t": "ev", "ev": "devices.snapshot", "devices": [
                {"slotId": "slot1", "name": "Coyote", "type": "COYOTE_030", "props": {"intensityA": 0, "intensityB": 0}}
            ]},
        })
        .to_string(),
    ))
    .await
    .unwrap();
    wait_for_slot(&panel_state).await;

    (app, panel_base, panel_state)
}

#[tokio::test(flavor = "multi_thread")]
async fn linear_ramp_sends_increasing_add_intensity_deltas_toward_the_target() {
    let (mut app, panel_base, panel_state) = pair_v4_device().await;
    let http = reqwest::Client::new();

    let res = http
        .post(format!("{panel_base}/api/ramp"))
        .json(&json!({"channel": "A", "profile": "linear", "from": 0, "to": 4, "overSeconds": 2}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "starting a ramp should succeed");

    // t=0 -> AddIntensity delta +0 relative to the known baseline (0),
    // proving the runner actually sent an initial tick immediately
    // rather than waiting a full second first.
    let frame = recv_until(&mut app, |v| v["data"]["data"]["t"] == 3).await;
    assert_eq!(frame["data"]["data"]["c"], 0); // channel A
    let mut cumulative = frame["data"]["data"]["v"].as_i64().unwrap();
    assert_eq!(cumulative, 0);

    // Channel A's snapshot should now show an active linear ramp.
    let snap = panel_state.snapshot();
    let ramp_a = snap.ramp_a.expect("ramp should be active on channel A");
    assert_eq!(ramp_a.profile.as_str(), "linear");
    assert_eq!(ramp_a.target, Some(4));

    // t=1 -> interpolating toward 4.
    let frame = recv_until(&mut app, |v| v["data"]["data"]["t"] == 3).await;
    cumulative += frame["data"]["data"]["v"].as_i64().unwrap();
    assert_eq!(cumulative, 2);

    // t=2 -- the tick *at* overSeconds -- must land exactly on the
    // configured target (4), not stop one short at value_at(1)=2. This
    // is the regression case for a real off-by-one: the runner used to
    // end the ramp the instant elapsed_secs reached overSeconds, so
    // this final tick never ran and the ramp never actually reached
    // its target.
    let frame = recv_until(&mut app, |v| v["data"]["data"]["t"] == 3).await;
    cumulative += frame["data"]["data"]["v"].as_i64().unwrap();
    assert_eq!(
        cumulative, 4,
        "the ramp must actually reach its configured target"
    );

    http.post(format!("{panel_base}/api/ramp/stop"))
        .json(&json!({"channel": "A"}))
        .send()
        .await
        .unwrap();
    assert!(panel_state.snapshot().ramp_a.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_manual_strength_command_overrides_the_active_ramp() {
    let (mut app, panel_base, panel_state) = pair_v4_device().await;
    let http = reqwest::Client::new();

    http.post(format!("{panel_base}/api/ramp"))
        .json(&json!({"channel": "B", "profile": "hold", "value": 20, "durationSeconds": 30}))
        .send()
        .await
        .unwrap();
    recv_until(&mut app, |v| v["data"]["data"]["c"] == 1).await; // the hold's initial send
    assert!(panel_state.snapshot().ramp_b.is_some());

    let res = http
        .post(format!("{panel_base}/api/strength"))
        .json(&json!({"channel": "B", "op": "inc"}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());

    assert!(
        panel_state.snapshot().ramp_b.is_none(),
        "a manual strength command should cancel the active ramp"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_ramp_target_above_the_configured_limit_is_rejected() {
    let (_app, panel_base, panel_state) = pair_v4_device().await;
    let http = reqwest::Client::new();

    http.post(format!("{panel_base}/api/limit"))
        .json(&json!({"channel": "A", "value": 30}))
        .send()
        .await
        .unwrap();

    let res = http
        .post(format!("{panel_base}/api/ramp"))
        .json(
            &json!({"channel": "A", "profile": "linear", "from": 10, "to": 40, "overSeconds": 10}),
        )
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::BAD_REQUEST);
    assert!(panel_state.snapshot().ramp_a.is_none());
}

#[tokio::test(flavor = "multi_thread")]
async fn a_zero_second_ramp_is_rejected_as_invalid() {
    let (_app, panel_base, _panel_state) = pair_v4_device().await;
    let http = reqwest::Client::new();

    let res = http
        .post(format!("{panel_base}/api/ramp"))
        .json(&json!({"channel": "A", "profile": "hold", "value": 20, "durationSeconds": 0}))
        .send()
        .await
        .unwrap();
    assert_eq!(res.status(), reqwest::StatusCode::BAD_REQUEST);
}
