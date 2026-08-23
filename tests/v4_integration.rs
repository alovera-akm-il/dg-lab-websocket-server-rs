//! Integration happy-path + idle-timeout test for the V4 server, driven
//! over a real loopback TCP connection with `tokio-tungstenite`.

use std::sync::Arc;
use std::time::Duration;

use dg_lab_websocket_server_rs::v4;
use futures_util::{SinkExt, StreamExt};
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::{connect_async, MaybeTlsStream, WebSocketStream};

type WsStream = WebSocketStream<MaybeTlsStream<tokio::net::TcpStream>>;

async fn spawn_server(idle_timeout_ms: u64) -> String {
    let config = Arc::new(v4::config::Config {
        port: 0,
        heartbeat_ms: 60_000,
        ws_ping_ms: 60_000,
        max_missed_ws_pongs: 100,
        idle_timeout_ms,
        prefix: "/".to_string(),
    });
    let (_hub, router) = v4::build(config);
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

#[tokio::test(flavor = "multi_thread")]
async fn controller_device_attach_and_bidirectional_forward() {
    let base = spawn_server(300_000).await;

    let (mut controller, _) = connect_async(&base).await.expect("controller connects");
    let hello = recv_json(&mut controller).await;
    assert_eq!(hello["type"], "hello");
    let controller_id = hello["clientId"].as_str().unwrap().to_string();

    let (mut device, _) = connect_async(format!("{base}/?tid={controller_id}"))
        .await
        .expect("device connects");
    let device_hello = recv_json(&mut device).await;
    let device_id = device_hello["clientId"].as_str().unwrap().to_string();

    let attached = recv_json(&mut device).await;
    assert_eq!(attached["type"], "controller_attached");
    assert_eq!(attached["clientId"], controller_id);

    let client_attached = recv_json(&mut controller).await;
    assert_eq!(client_attached["type"], "client_attached");
    assert_eq!(client_attached["clientId"], device_id);

    // controller -> device: no id fields on the device-bound envelope.
    controller
        .send(Message::text(
            json!({"type":"message","clientId":device_id,"data":{"op":"example","value":1}}).to_string(),
        ))
        .await
        .unwrap();
    let forwarded = recv_json(&mut device).await;
    assert_eq!(forwarded["type"], "message");
    assert!(forwarded.get("clientId").is_none());
    assert_eq!(forwarded["data"]["value"], 1);

    // device -> controller: envelope carries the device's clientId.
    device
        .send(Message::text(json!({"type":"message","data":{"op":"report","value":2}}).to_string()))
        .await
        .unwrap();
    let reported = recv_json(&mut controller).await;
    assert_eq!(reported["type"], "message");
    assert_eq!(reported["clientId"], device_id);
    assert_eq!(reported["data"]["value"], 2);
}

#[tokio::test(flavor = "multi_thread")]
async fn controller_with_no_devices_closes_on_idle_timeout() {
    let base = spawn_server(200).await;

    let (mut controller, _) = connect_async(&base).await.expect("connects");
    let _hello = recv_json(&mut controller).await;

    let idle_notice = recv_json(&mut controller).await;
    assert_eq!(idle_notice["type"], "idle_timeout");

    let close = tokio::time::timeout(Duration::from_secs(2), controller.next())
        .await
        .expect("timed out waiting for close")
        .expect("stream ended")
        .expect("websocket error");
    match close {
        Message::Close(Some(frame)) => assert_eq!(frame.code, 4002u16.into()),
        other => panic!("expected a close frame with code 4002, got {other:?}"),
    }
}
