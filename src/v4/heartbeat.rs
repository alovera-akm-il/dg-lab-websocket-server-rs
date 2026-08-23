//! V4 heartbeat broadcast, mirroring `startHeartbeat`/`broadcastHeartbeat`.
//! Sends a bare `{type:'heartbeat'}` (no other fields) to every live
//! connection -- unlike V3, this doesn't log anything (matching the TS
//! source, which has no logging in this path).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::Message;
use serde_json::json;

use super::state::Hub;

pub fn spawn(hub: Arc<Hub>, interval_ms: u64) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
        ticker.tick().await; // don't fire immediately on startup
        loop {
            ticker.tick().await;
            let payload = json!({"type":"heartbeat"}).to_string();
            for tx in hub.all_senders() {
                let _ = tx.send(Message::Text(payload.clone().into()));
            }
        }
    });
}
