//! Integration happy-path (+ one failure path) test for the V3 server,
//! driven over a real loopback TCP connection with `tokio-tungstenite`.

use std::sync::Arc;
use std::time::Duration;

use dg_lab_websocket_server_rs::v3;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn spawn_server() -> String {
    let config = Arc::new(v3::config::Config {
        port: 0,
        heartbeat_ms: 60_000,
        idle_timeout_ms: 300_000,
        default_punishment_time: 1,
        default_punishment_duration: 5,
    });
    let (_hub, router) = v3::build(config);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("ws://{addr}")
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

/// Reads frames until one satisfies `predicate`, discarding the rest
/// (heartbeats etc. never fire in these short-lived tests, but this
/// keeps assertions robust to frame ordering).
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
async fn pair_strength_and_pulse_happy_path() {
    let base = spawn_server().await;

    let (mut web, _) = connect_async(&base).await.expect("web connects");
    let web_bind = recv_json(&mut web).await;
    assert_eq!(web_bind["type"], "bind");
    assert_eq!(web_bind["message"], "targetId");
    let web_id = web_bind["clientId"].as_str().unwrap().to_string();

    // App side attaches via the URL path tail, auto-pairing with web_id.
    let (mut app, _) = connect_async(format!("{base}/{web_id}")).await.expect("app connects");
    let app_bind = recv_json(&mut app).await;
    let app_id = app_bind["clientId"].as_str().unwrap().to_string();

    let web_paired = recv_until(&mut web, |v| v["type"] == "bind" && v["message"] == "200").await;
    assert_eq!(web_paired["targetId"], app_id);
    let app_paired = recv_until(&mut app, |v| v["type"] == "bind" && v["message"] == "200").await;
    assert_eq!(app_paired["clientId"], web_id);

    // type 3: set channel A strength to 20. Note: "message" must NOT
    // start with "feedback"/"strength" or it'd be intercepted by the
    // app-report forward path ahead of the numeric-type routing.
    web.send(Message::text(
        json!({"type":3,"clientId":web_id,"targetId":app_id,"channel":"A","strength":20,"message":""})
            .to_string(),
    ))
    .await
    .unwrap();
    let strength = recv_until(&mut app, |v| v["type"] == "msg").await;
    assert_eq!(strength["message"], "strength-1+2+20");

    // clientMsg raw-fallback pulse: short duration so it finishes fast.
    web.send(Message::text(
        json!({"type":"clientMsg","clientId":web_id,"targetId":app_id,"channel":"A","time":1,"message":"legacywave"})
            .to_string(),
    ))
    .await
    .unwrap();
    let pulse = recv_until(&mut app, |v| v["type"] == "msg" && v["message"] != "strength-1+2+20").await;
    assert_eq!(pulse["message"], "pulse-legacywave");
    let done = recv_until(&mut web, |v| v["type"] == "notify").await;
    assert_eq!(done["message"], "发送完毕");
}

#[tokio::test(flavor = "multi_thread")]
async fn pairing_to_nonexistent_target_closes_with_4001() {
    let base = spawn_server().await;

    let (mut app, _) = connect_async(format!("{base}/does-not-exist")).await.expect("connects");
    let error = recv_json(&mut app).await;
    assert_eq!(error["type"], "error");
    assert_eq!(error["message"], "4001");

    let close = tokio::time::timeout(Duration::from_secs(2), app.next())
        .await
        .expect("timed out waiting for close")
        .expect("stream ended")
        .expect("websocket error");
    match close {
        Message::Close(Some(frame)) => assert_eq!(frame.code, 4001u16.into()),
        other => panic!("expected a close frame with code 4001, got {other:?}"),
    }
}
