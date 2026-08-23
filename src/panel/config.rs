//! Control panel env-driven configuration.

use crate::env::u16_from_env;

pub struct Config {
    /// `PANEL_PORT` -- default 40000.
    pub port: u16,
    /// `PANEL_PUBLIC_WS_BASE` -- optional override for the pairing QR's
    /// `scheme://host` (e.g. `wss://relay.example.com`), for deployments
    /// behind a reverse proxy/TLS. When unset, the scheme/host are
    /// derived per-request from the browser's `Host` header, which is
    /// enough to "just work" for local/LAN use.
    pub public_ws_base: Option<String>,
    /// `PANEL_WEBHOOK_URL` -- optional initial webhook URL (see
    /// [`super::webhook`]); can also be set/changed/cleared at runtime
    /// via `POST /api/webhook`, which is the more common path.
    pub webhook_url: Option<String>,
}

impl Config {
    pub fn from_env() -> Self {
        Config {
            port: u16_from_env("PANEL_PORT", 40_000),
            public_ws_base: non_empty_env("PANEL_PUBLIC_WS_BASE"),
            webhook_url: non_empty_env("PANEL_WEBHOOK_URL"),
        }
    }
}

fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}
