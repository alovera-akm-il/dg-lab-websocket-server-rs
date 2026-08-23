//! V4 native-WS ping/missed-pong task, mirroring `startWsPing`/
//! `pingConnections`.
//!
//! There's no direct axum equivalent of Bun's `ws.terminate()` (instant
//! TCP kill, no close handshake): at the missed-pong threshold, this
//! just cancels the connection's shutdown token without ever building a
//! `Message::Close` -- the peer sees the TCP connection die, not a clean
//! WS close frame.

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::Message;

use crate::logging::{LogLevel, log_v4};

use super::state::{Hub, PingAction};

pub fn spawn(hub: Arc<Hub>, interval_ms: u64, max_missed_pongs: u32) {
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
        ticker.tick().await; // don't fire immediately on startup
        loop {
            ticker.tick().await;
            for (client_id, tx, shutdown_token, action) in hub.tick_pings(max_missed_pongs) {
                match action {
                    PingAction::Terminate => {
                        log_v4(
                            LogLevel::Warn,
                            format!(
                                "WS liveness check timed out connection={client_id} missed_pongs={max_missed_pongs}"
                            ),
                        );
                        shutdown_token.cancel();
                    }
                    PingAction::SendPing => {
                        let _ = tx.send(Message::Ping(Vec::new().into()));
                    }
                }
            }
        }
    });
}
