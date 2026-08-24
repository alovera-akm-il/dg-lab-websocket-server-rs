//! End-to-end check of the control panel's per-channel pulse playlists:
//! spins up a real V4 relay and a panel connected to it, drives a
//! simulated APP through the V4 handshake, builds a two-entry playlist
//! (one pulse preset, one silent gap) over the panel's HTTP API, hits
//! play, and confirms the simulated device receives the pulse's
//! `device.op` frame followed by the gap's `device.op.clear` frame, in
//! that order -- proof the runner task actually drives the queue rather
//! than just accepting the HTTP calls.

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
    for _ in 0..20 {
        let value = recv_json(ws).await;
        if predicate(&value) {
            return value;
        }
    }
    panic!("did not see a matching frame within 20 messages");
}

#[tokio::test(flavor = "multi_thread")]
async fn playlist_plays_a_pulse_entry_then_a_gap_in_order() {
    let v3_port = spawn_v3().await;
    let v4_port = spawn_v4().await;
    let (panel_base, panel_state) = spawn_panel(v3_port, v4_port).await;

    let v4_controller_id = wait_for_v4_controller_id(&panel_state).await;

    let (mut app, _) = connect_async(format!("ws://127.0.0.1:{v4_port}/?tid={v4_controller_id}"))
        .await
        .expect("APP connects");
    let hello = recv_json(&mut app).await;
    assert_eq!(hello["type"], "hello");
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

    let slot_id = wait_for_slot(&panel_state).await;
    assert_eq!(slot_id, "slot1");

    let http = reqwest::Client::new();

    // Build a two-entry playlist on channel A: one pulse preset, one
    // one-second silent gap.
    let res = http
        .post(format!("{panel_base}/api/playlist/a/items"))
        .json(&json!({
            "kind": "pulse",
            "waveform": r#"A:["0A0A0A0A0A0A0A0A"]"#,
            "duration": {"mode": "fixed", "seconds": 1},
        }))
        .send()
        .await
        .unwrap();
    assert!(
        res.status().is_success(),
        "adding a pulse entry should succeed"
    );

    let res = http
        .post(format!("{panel_base}/api/playlist/a/items"))
        .json(&json!({
            "kind": "gap",
            "duration": {"mode": "fixed", "seconds": 1},
        }))
        .send()
        .await
        .unwrap();
    assert!(
        res.status().is_success(),
        "adding a gap entry should succeed"
    );

    let res = http
        .post(format!("{panel_base}/api/playlist/a/play"))
        .send()
        .await
        .unwrap();
    assert!(
        res.status().is_success(),
        "play should succeed once a V4 device is active"
    );

    // The pulse entry -> AppendPulseData (t:0) for channel A (c:0), with
    // the resolved duration (1s) converted to ms.
    let frame = recv_until(&mut app, |v| v["data"]["data"]["t"] == 0).await;
    assert_eq!(
        frame["data"]["data"],
        json!({"s": "slot1", "t": 0, "c": 0, "p": 1, "d": 1000, "v": ["0A0A0A0A0A0A0A0A"]})
    );

    // Then the gap entry -> device.op.clear for the same channel, proving
    // the runner actually advanced the queue rather than stopping after
    // the first entry.
    let frame = recv_until(&mut app, |v| v["data"]["m"] == "device.op.clear").await;
    assert_eq!(frame["data"]["data"], json!({"s": "slot1", "c": 0}));
}

#[tokio::test(flavor = "multi_thread")]
async fn playlist_pause_and_resume_continues_from_where_it_stopped() {
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

    let http = reqwest::Client::new();
    http.post(format!("{panel_base}/api/playlist/b/items"))
        .json(&json!({
            "kind": "pulse",
            "waveform": r#"B:["0B0B0B0B0B0B0B0B"]"#,
            "duration": {"mode": "fixed", "seconds": 5},
        }))
        .send()
        .await
        .unwrap();

    http.post(format!("{panel_base}/api/playlist/b/play"))
        .send()
        .await
        .unwrap();
    recv_until(&mut app, |v| v["data"]["data"]["t"] == 0).await; // the pulse send

    let res = http
        .post(format!("{panel_base}/api/playlist/b/pause"))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());

    let snap = panel_state.snapshot();
    assert_eq!(snap.playlist_b.phase.as_str(), "paused");
    let remaining_after_pause = snap
        .playlist_b
        .remaining_ms
        .expect("a paused entry has captured remaining time");
    assert!(
        remaining_after_pause <= 5_000,
        "remaining time should be at most the full 5s duration"
    );

    // Resuming re-sends the same entry for whatever time was left --
    // there's no wire-level "resume" a device could understand, so the
    // runner re-issues the waveform with the shortened duration.
    let res = http
        .post(format!("{panel_base}/api/playlist/b/play"))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());
    assert_eq!(panel_state.snapshot().playlist_b.phase.as_str(), "playing");

    let frame = recv_until(&mut app, |v| v["data"]["data"]["t"] == 0).await;
    let resumed_ms = frame["data"]["data"]["d"].as_i64().unwrap();
    assert!(
        resumed_ms <= remaining_after_pause as i64 + 50,
        "resumed duration ({resumed_ms}ms) should not exceed what was captured at pause ({remaining_after_pause}ms)"
    );

    http.post(format!("{panel_base}/api/playlist/b/stop"))
        .send()
        .await
        .unwrap();
    assert_eq!(panel_state.snapshot().playlist_b.phase.as_str(), "stopped");
}
