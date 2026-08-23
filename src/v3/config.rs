//! V3 env-driven configuration, mirroring the top-of-file constants in
//! v3-server.ts. See the README for the full env var table.

use crate::env::{i64_from_env, u16_from_env, u64_from_env};

pub struct Config {
    /// `PORT` -- default 10002 (see [`Config::from_env`]).
    pub port: u16,
    /// `HEARTBEAT_INTERVAL` -- how often every open connection is sent a
    /// `heartbeat` frame.
    pub heartbeat_ms: u64,
    /// `IDLE_TIMEOUT` -- how long an unpaired connection may sit idle
    /// before it's closed.
    pub idle_timeout_ms: u64,
    /// `DEFAULT_PUNISHMENT_TIME` -- pulse packets sent per second,
    /// clamped to `[1, 10]` (see [`super::pulse::normalize_sends_per_second`]).
    pub default_punishment_time: i64,
    /// `DEFAULT_PUNISHMENT_DURATION` -- fallback waveform duration in
    /// seconds when a `clientMsg` frame omits/has an invalid `time`.
    pub default_punishment_duration: i64,
}

/// Short wait before replacing an in-flight pulse sequence on the same
/// channel, not env-configurable (matches `PULSE_REPLACE_DELAY_MS`).
pub const PULSE_REPLACE_DELAY_MS: u64 = 150;
/// WS close code used when a connection's requested targetId is invalid.
pub const CLOSE_INVALID_TARGET_ID: u16 = 4001;

impl Config {
    pub fn from_env() -> Self {
        Config {
            // Offset +3 from the TS server's default 9999 so both can run
            // side-by-side on one machine without a port clash.
            port: u16_from_env("PORT", 10_002),
            heartbeat_ms: u64_from_env("HEARTBEAT_INTERVAL", 60_000),
            idle_timeout_ms: u64_from_env("IDLE_TIMEOUT", 5 * 60_000),
            default_punishment_time: i64_from_env("DEFAULT_PUNISHMENT_TIME", 1),
            default_punishment_duration: i64_from_env("DEFAULT_PUNISHMENT_DURATION", 5),
        }
    }
}
