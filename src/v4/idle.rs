//! V4 controller idle-timeout task, mirroring `startIdleTimer`.
//!
//! A controller with zero attached devices is closed after
//! `IDLE_TIMEOUT` ms. `CancellationToken`s are one-shot, so every
//! (re)start of this timer -- initial registration, or a device count
//! returning to zero -- spawns a fresh task against a fresh token (see
//! `state::Hub::remove_connection`, which mints the replacement token).

use std::sync::Arc;
use std::time::Duration;

use axum::extract::ws::{CloseFrame, Message};
use serde_json::json;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use crate::logging::{LogLevel, log_v4};

use super::handler::CLOSE_IDLE_TIMEOUT;
use super::state::Hub;

pub fn spawn(
    hub: Arc<Hub>,
    timeout_ms: u64,
    controller_id: String,
    idle_token: CancellationToken,
    tx: mpsc::UnboundedSender<Message>,
) {
    tokio::spawn(async move {
        tokio::select! {
            _ = idle_token.cancelled() => {}
            _ = tokio::time::sleep(Duration::from_millis(timeout_ms)) => {
                let _ = tx.send(Message::Text(json!({"type":"idle_timeout"}).to_string().into()));
                let _ = tx.send(Message::Close(Some(CloseFrame {
                    code: CLOSE_IDLE_TIMEOUT,
                    reason: "idle_timeout".into(),
                })));
                if let Some(shutdown) = hub.shutdown_token_of(&controller_id) {
                    shutdown.cancel();
                }
                log_v4(LogLevel::Warn, format!("controller idle timeout controller={controller_id}"));
            }
        }
    });
}
