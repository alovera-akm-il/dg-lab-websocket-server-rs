//! V3 heartbeat broadcast, mirroring `startHeartbeat`.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::Message;
use serde_json::json;

use crate::logging::{LogLevel, log_v3};

use super::state::Hub;

pub fn spawn(hub: Arc<Hub>, interval_ms: u64) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
        ticker.tick().await; // don't fire immediately on startup
        loop {
            ticker.tick().await;
            let connections = hub.all_connections();
            log_v3(
                LogLevel::Debug,
                format!("sending heartbeat, connections={}", connections.len()),
            );
            for (id, tx, partner_id) in connections {
                let value = json!({
                    "type": "heartbeat",
                    "clientId": id,
                    "targetId": partner_id.unwrap_or_default(),
                    "message": "200",
                });
                let _ = tx.send(Message::Text(value.to_string().into()));
            }
        }
    });
    log_v3(
        LogLevel::Info,
        format!("heartbeat started interval={interval_ms}ms"),
    );
}
