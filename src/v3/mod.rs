//! The V3 relay: a 1-controller:1-device pairing protocol ported from
//! `v3-server.ts`. A controller ("web") and a DG-LAB APP ("app") each
//! connect, get a UUID `clientId`, and pair up (either by the app
//! connecting with `?targetId=<web's id>` / `?tid=` / a URL path tail, or
//! by the web side sending an explicit `bind` frame). Once paired, the
//! web side can adjust strength, clear a channel, or send a pulse
//! waveform, which the app reports back on with `feedback-*`/`strength-*`
//! messages; either side disconnecting tears down the pairing and closes
//! the other. See [`config::Config`] for env vars, [`protocol`] for wire
//! parsing/validation, [`pulse`] for waveform packetization, [`state`]
//! for the shared pairing/pulse-timer bookkeeping, and [`handler`] for
//! the connection lifecycle and message routing.

pub mod config;
pub mod handler;
pub mod heartbeat;
pub mod protocol;
pub mod pulse;
pub mod state;

use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;

use crate::logging::{log_v3, LogLevel};

/// Builds the Hub and router without binding or spawning background
/// tasks, so integration tests can drive a fresh instance against an
/// ephemeral port with their own `Config`.
pub fn build(config: Arc<config::Config>) -> (Arc<state::Hub>, Router) {
    let hub = Arc::new(state::Hub::new());
    let state = handler::AppState {
        hub: hub.clone(),
        config,
    };
    (hub, handler::router(state))
}

pub async fn serve() -> std::io::Result<()> {
    serve_with(Arc::new(config::Config::from_env())).await
}

/// Same as [`serve`], but takes an already-built `Config` -- so callers
/// that need the port (e.g. `main.rs` wiring the control panel to this
/// same instance) can hold onto it without reading the env twice.
pub async fn serve_with(config: Arc<config::Config>) -> std::io::Result<()> {
    let (hub, router) = build(config.clone());

    let listener = TcpListener::bind(("0.0.0.0", config.port)).await?;
    heartbeat::spawn(hub, config.heartbeat_ms);
    log_v3(LogLevel::Info, format!("service started port={}", config.port));

    axum::serve(listener, router).await
}
