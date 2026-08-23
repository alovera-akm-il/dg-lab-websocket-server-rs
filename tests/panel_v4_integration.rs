//! End-to-end check of the panel's V4 support: spins up a real V4 relay
//! and a panel connected to it, drives a simulated APP through the whole
//! V4 handshake (hello -> controller_attached -> devices.snapshot), and
//! confirms `POST /api/strength`/`/api/pulse`/`/api/clear` on the panel's
//! HTTP API produce the exact `device.op`/`device.op.clear` wire frames
//! a real V4 controller would send, and that a `custom.action` event from
//! the APP shows up as the panel's tracked button feedback.

use std::sync::Arc;
use std::time::Duration;

use dg_lab_websocket_server_rs::{panel, v3, v4};
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

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
    let config = Arc::new(panel::config::Config { port: 0, public_ws_base: None, webhook_url: None });
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
    let msg = tokio::time::timeout(Duration::from_secs(2), ws.next())
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
async fn panel_drives_a_v4_device_through_the_full_handshake_and_command_set() {
    let v3_port = spawn_v3().await;
    let v4_port = spawn_v4().await;
    let (panel_base, panel_state) = spawn_panel(v3_port, v4_port).await;

    let v4_controller_id = wait_for_v4_controller_id(&panel_state).await;

    // Simulate the DG-LAB 4 APP attaching to the panel's V4 controller.
    let (mut app, _) = connect_async(format!("ws://127.0.0.1:{v4_port}/?tid={v4_controller_id}"))
        .await
        .expect("APP connects");
    let hello = recv_json(&mut app).await;
    assert_eq!(hello["type"], "hello");
    let attached = recv_until(&mut app, |v| v["type"] == "controller_attached").await;
    assert_eq!(attached["clientId"], v4_controller_id);

    // Per dglab-kit's documented flow, the APP immediately reports its
    // devices once attached.
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
    assert_eq!(panel_state.snapshot().active_protocol.map(|p| p.as_str()), Some("v4"));

    let http = reqwest::Client::new();

    // Strength increase -> AddIntensity (t:3) with v:1.
    let res = http
        .post(format!("{panel_base}/api/strength"))
        .json(&json!({"channel": "A", "op": "inc"}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success(), "strength inc should succeed once a V4 device is active");

    // Per the V4 wire protocol, the relay strips the outer `clientId`
    // when forwarding controller -> device (a device only ever has one
    // controller, so it doesn't need to be told who sent it) -- see
    // docs/api.md's V4 section.
    let frame = recv_until(&mut app, |v| {
        v["type"] == "message" && v["data"]["m"] == "device.op" && v["data"]["data"]["t"] == 3
    })
    .await;
    assert_eq!(frame.get("clientId"), None);
    assert_eq!(frame["data"]["data"], json!({"s": "slot1", "t": 3, "c": 0, "p": 1, "v": 1}));

    // Pulse -> AppendPulseData (t:0), duration converted from seconds to ms.
    let res = http
        .post(format!("{panel_base}/api/pulse"))
        .json(&json!({"channel": "B", "time": 2, "waveform": r#"A:["0A0A0A0A0A0A0A0A"]"#}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());

    let frame = recv_until(&mut app, |v| v["data"]["data"]["t"] == 0).await;
    assert_eq!(
        frame["data"]["data"],
        json!({"s": "slot1", "t": 0, "c": 1, "p": 1, "d": 2000, "v": ["0A0A0A0A0A0A0A0A"]})
    );

    // Clear -> device.op.clear.
    let res = http.post(format!("{panel_base}/api/clear")).json(&json!({"channel": "A"})).send().await.unwrap();
    assert!(res.status().is_success());
    let frame = recv_until(&mut app, |v| v["data"]["m"] == "device.op.clear").await;
    assert_eq!(frame["data"]["data"], json!({"s": "slot1", "c": 0}));

    // custom.action -> tracked as button feedback.
    app.send(Message::text(json!({"type": "message", "data": {"t": "ev", "ev": "custom.action", "action": 2}}).to_string()))
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if panel_state.snapshot().last_button_action == Some(2) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("button feedback never recorded");

    // APP disconnects -> panel clears the tracked device and active protocol.
    drop(app);
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if panel_state.snapshot().active_protocol.is_none() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("active protocol never cleared after APP disconnect");
}
