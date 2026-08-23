//! End-to-end check that the panel's webhook actually fires an HTTP POST
//! when a real event happens: spins up a V3 relay, a panel pointed at
//! it, and a tiny mock HTTP receiver, then drives a simulated device
//! through pairing and confirms the receiver gets the expected payload.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::routing::post;
use axum::{Json, Router};
use dg_lab_websocket_server_rs::{panel, v3, v4};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::net::TcpListener;
use tokio::sync::mpsc;
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

/// A minimal mock webhook receiver: every JSON POST body it gets is
/// pushed onto the returned channel.
async fn spawn_webhook_receiver() -> (String, mpsc::UnboundedReceiver<Value>) {
    let (tx, rx) = mpsc::unbounded_channel();
    let router = Router::new().route(
        "/hook",
        post(
            |State(tx): State<mpsc::UnboundedSender<Value>>, Json(body): Json<Value>| async move {
                let _ = tx.send(body);
            },
        ),
    ).with_state(tx);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    (format!("http://{addr}/hook"), rx)
}

async fn wait_for_controller_id(panel_state: &panel::state::PanelState) -> String {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let Some(id) = panel_state.snapshot().controller_id {
                return id;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("panel never obtained a controller id")
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

async fn recv_webhook_event(rx: &mut mpsc::UnboundedReceiver<Value>, event: &str) -> Value {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let payload = rx.recv().await.expect("webhook receiver channel closed");
            if payload.get("event").and_then(Value::as_str) == Some(event) {
                return payload;
            }
        }
    })
    .await
    .unwrap_or_else(|_| panic!("never received a webhook payload with event={event}"))
}

#[tokio::test(flavor = "multi_thread")]
async fn webhook_fires_with_structured_payloads_for_pairing_and_button_feedback() {
    let v3_port = spawn_v3().await;
    let v4_port = spawn_v4().await;
    let (panel_base, panel_state) = spawn_panel(v3_port, v4_port).await;
    let (hook_url, mut hook_rx) = spawn_webhook_receiver().await;

    let http = reqwest::Client::new();
    let res = http
        .post(format!("{panel_base}/api/webhook"))
        .json(&json!({"url": hook_url}))
        .send()
        .await
        .unwrap();
    assert!(res.status().is_success());

    let controller_id = wait_for_controller_id(&panel_state).await;

    // A "controller_connected" webhook should already have fired for the
    // panel's own initial connection to V3 -- but that happened before
    // the webhook URL was configured, so we don't expect to see it.
    // Simulate a device pairing with the panel's controller id instead.
    let (mut device, _) = connect_async(format!("ws://127.0.0.1:{v3_port}/{controller_id}"))
        .await
        .expect("device connects");
    let self_bind = recv_until(&mut device, |v| {
        v["type"] == "bind" && v["message"] == "targetId"
    })
    .await;
    let device_id = self_bind["clientId"].as_str().unwrap().to_string();
    recv_until(&mut device, |v| {
        v["type"] == "bind" && v["message"] == "200"
    })
    .await;

    let paired_payload = recv_webhook_event(&mut hook_rx, "paired").await;
    assert_eq!(paired_payload["deviceId"], device_id);
    assert!(
        paired_payload["message"]
            .as_str()
            .unwrap()
            .contains(&device_id)
    );

    // Simulate a shape-button press report from the device (per
    // dglab-kit's format, forwarded verbatim by V3's app-report path).
    device
        .send(Message::text(
            json!({"type":"msg","clientId":device_id,"targetId":controller_id,"message":"feedback-7"}).to_string(),
        ))
        .await
        .unwrap();

    let feedback_payload = recv_webhook_event(&mut hook_rx, "button_feedback").await;
    assert_eq!(feedback_payload["code"], 7);
    assert_eq!(feedback_payload["channel"], "B");
    assert_eq!(feedback_payload["shape"], "square");

    // And a device status report.
    device
        .send(Message::text(
            json!({"type":"msg","clientId":device_id,"targetId":controller_id,"message":"strength-12+34+56+78"}).to_string(),
        ))
        .await
        .unwrap();
    let status_payload = recv_webhook_event(&mut hook_rx, "device_status").await;
    assert_eq!(status_payload["strengthA"], 12);
    assert_eq!(status_payload["strengthB"], 34);
    assert_eq!(status_payload["softLimitA"], 56);
    assert_eq!(status_payload["softLimitB"], 78);

    // Disconnecting the device should fire a device_disconnected event.
    drop(device);
    let disconnect_payload = recv_webhook_event(&mut hook_rx, "device_disconnected").await;
    assert!(
        disconnect_payload["message"]
            .as_str()
            .unwrap()
            .contains("disconnected")
    );
}
