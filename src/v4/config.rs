//! V4 env-driven configuration, mirroring the top-of-file constants in
//! v4-server.ts. See the README for the full env var table.

use crate::env::{i64_from_env, u16_from_env};

pub struct Config {
    /// `PORT` -- default 10001 (see [`Config::from_env`]).
    pub port: u16,
    /// `HEARTBEAT_INTERVAL` -- how often a bare `{type:'heartbeat'}` is
    /// broadcast to every live connection.
    pub heartbeat_ms: u64,
    /// `WS_PING_INTERVAL` -- how often a native WS ping is sent to every
    /// live connection.
    pub ws_ping_ms: u64,
    /// `MAX_MISSED_WS_PONGS` -- consecutive missed pongs before a
    /// connection is abruptly terminated.
    pub max_missed_ws_pongs: u32,
    /// `IDLE_TIMEOUT` -- how long a controller may have zero attached
    /// devices before it's closed.
    pub idle_timeout_ms: u64,
    /// `PREFIX` -- the sole upgrade-eligible path, normalized by
    /// [`normalize_prefix`].
    pub prefix: String,
}

impl Config {
    pub fn from_env() -> Self {
        Config {
            port: u16_from_env("PORT", 10_001),
            heartbeat_ms: crate::env::u64_from_env("HEARTBEAT_INTERVAL", 30_000),
            ws_ping_ms: crate::env::u64_from_env("WS_PING_INTERVAL", 10_000),
            max_missed_ws_pongs: i64_from_env("MAX_MISSED_WS_PONGS", 3).max(0) as u32,
            idle_timeout_ms: crate::env::u64_from_env("IDLE_TIMEOUT", 5 * 60_000),
            prefix: normalize_prefix(std::env::var("PREFIX").ok().as_deref(), "/"),
        }
    }
}

/// Mirrors `prefixFromEnv`: trim, force a leading slash, then strip
/// trailing slashes except for the bare root path.
pub fn normalize_prefix(raw: Option<&str>, fallback: &str) -> String {
    let trimmed = raw.map(str::trim).unwrap_or("");
    if trimmed.is_empty() {
        return fallback.to_string();
    }

    let prefixed = if trimmed.starts_with('/') {
        trimmed.to_string()
    } else {
        format!("/{trimmed}")
    };

    if prefixed.len() > 1 {
        prefixed.trim_end_matches('/').to_string()
    } else {
        prefixed
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_or_empty_uses_fallback() {
        assert_eq!(normalize_prefix(None, "/"), "/");
        assert_eq!(normalize_prefix(Some(""), "/"), "/");
        assert_eq!(normalize_prefix(Some("   "), "/"), "/");
    }

    #[test]
    fn bare_root_stays_root() {
        assert_eq!(normalize_prefix(Some("/"), "/"), "/");
    }

    #[test]
    fn forces_leading_slash() {
        assert_eq!(normalize_prefix(Some("v4"), "/"), "/v4");
    }

    #[test]
    fn strips_trailing_slashes_except_root() {
        assert_eq!(normalize_prefix(Some("/v4/"), "/"), "/v4");
        assert_eq!(normalize_prefix(Some("/relay/v4///"), "/"), "/relay/v4");
    }

    #[test]
    fn already_well_formed_is_unchanged() {
        assert_eq!(normalize_prefix(Some("/relay/v4"), "/"), "/relay/v4");
    }
}
