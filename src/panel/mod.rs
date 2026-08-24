//! A self-contained control panel webserver: shows pairing QR codes for a
//! phone's DG-LAB APP (one per protocol, V3 and V4), and gives buttons/
//! controls to actually drive whichever device is currently attached. It
//! does not talk to [`crate::v3`]'s or [`crate::v4`]'s `Hub`s in-process
//! -- [`relay_client`] and [`v4_client`] each connect to their local
//! relay over loopback TCP exactly like any real third-party controller
//! would, which keeps the panel fully decoupled and reuses the
//! already-tested V3/V4 servers unchanged, running both connections at
//! once (see [`state`]'s module docs for how the two are reconciled into
//! one set of shared controls). See [`state::PanelState`] for the shared
//! connection state, [`commands`]/[`v4_commands`] for the wire frames the
//! panel sends on each protocol, and [`handler`] for the HTTP surface
//! (the page, the `/events` SSE feed, and the command endpoints).

mod assets;
pub mod commands;
pub mod config;
pub mod handler;
pub mod network;
mod persistence;
pub mod playlist;
pub mod playlist_runner;
pub mod presets;
pub mod qrcode;
pub mod relay_client;
pub mod state;
pub mod templates;
pub mod v4_client;
pub mod v4_commands;
pub mod webhook;

use std::sync::Arc;

use axum::Router;
use tokio::net::TcpListener;

use crate::logging::{LogLevel, log_panel};

/// Builds the panel state (spawning its V3 and V4 relay-client tasks) and
/// router without binding a socket, so integration tests can drive a
/// fresh instance against ephemeral ports.
pub fn build(
    config: Arc<config::Config>,
    v3_port: u16,
    v4_port: u16,
    v4_prefix: String,
) -> (Arc<state::PanelState>, Router) {
    let panel = Arc::new(state::PanelState::new());
    if let Some(url) = &config.webhook_url {
        panel.set_webhook_url(Some(url.clone()));
    }
    tokio::spawn(relay_client::run(v3_port, panel.clone()));
    tokio::spawn(v4_client::run(v4_port, v4_prefix.clone(), panel.clone()));

    let lan_ip = network::detect_lan_ip().map(|ip| ip.to_string());
    match &lan_ip {
        Some(ip) => log_panel(
            LogLevel::Info,
            format!(
                "detected LAN IP {ip} (used for the pairing QR if the panel is viewed via localhost)"
            ),
        ),
        None => log_panel(
            LogLevel::Warn,
            "could not detect a LAN IP -- if pairing over WiFi doesn't work when viewing the panel via localhost, set PANEL_PUBLIC_WS_BASE",
        ),
    }

    let app_state = handler::AppState {
        panel: panel.clone(),
        config,
        v3_port,
        v4_port,
        v4_prefix,
        lan_ip,
    };
    (panel, handler::router(app_state))
}

pub async fn serve(v3_port: u16, v4_port: u16, v4_prefix: String) -> std::io::Result<()> {
    let config = Arc::new(config::Config::from_env());
    let (_panel, router) = build(config.clone(), v3_port, v4_port, v4_prefix);

    let listener = TcpListener::bind(("0.0.0.0", config.port)).await?;
    log_panel(
        LogLevel::Info,
        format!("control panel started on port {}", config.port),
    );

    axum::serve(listener, router).await
}
