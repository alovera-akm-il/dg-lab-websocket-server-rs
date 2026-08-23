//! The V4 relay: a 1-controller:N-device protocol ported from
//! `v4-server.ts`. A controller connects first and gets a short
//! `clientId`; devices then attach with `?targetId=<controller's id>` /
//! `?tid=`. Once attached, the controller can send arbitrary `data`
//! payloads to a specific device by id, and devices report back with
//! `data` payloads the controller receives tagged with the sending
//! device's id. See [`config::Config`] for env vars, [`state`] for the
//! shared controller/device bookkeeping, [`handler`] for the connection
//! lifecycle and message routing, and [`idle`]/[`ping`] for the
//! zero-devices idle timeout and native WS ping/missed-pong termination.

pub mod config;
pub mod handler;
pub mod heartbeat;
pub mod idle;
pub mod ping;
pub mod state;

use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;

use crate::logging::{log_v4, LogLevel};

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
/// that need the port/prefix (e.g. `main.rs` wiring the control panel's
/// V4 leg to this same instance) can hold onto it without reading the
/// env twice.
pub async fn serve_with(config: Arc<config::Config>) -> std::io::Result<()> {
    let (hub, router) = build(config.clone());

    let listener = TcpListener::bind(("0.0.0.0", config.port)).await?;
    heartbeat::spawn(hub.clone(), config.heartbeat_ms);
    ping::spawn(hub, config.ws_ping_ms, config.max_missed_ws_pongs);
    log_v4(
        LogLevel::Info,
        format!("service started port={} path={}", config.port, config.prefix),
    );

    axum::serve(listener, router).await
}
