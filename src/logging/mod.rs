//! Level-filtered logging mirroring the TS `log()` helpers.
//!
//! V3 lines look like `{iso8601} [{LEVEL}] [V3] {message}`.
//! V4 lines look like `[V4] {message}` — no timestamp, no level tag in the
//! text (level filtering still applies before printing).
//!
//! Every line is written to stdout AND appended to a self-managed,
//! size-capped rotating log file, via `flexi_logger` -- the app
//! previously only ever wrote to stdout, leaving disk usage entirely up
//! to however the process's output happened to be captured (shell
//! redirect, systemd, docker...) with no bound at all. [`init`] must run
//! once before any log call (normally the first line of `main()`);
//! `LOG_DIR` (default `logs`) picks the directory, `LOG_MAX_TOTAL_BYTES`
//! (default 500 MiB) picks the total on-disk budget across the active
//! file and its backups.
//!
//! We already build the exact line text ourselves (to match the two
//! formats above precisely) and already gate on level via
//! [`resolve_log_level`], so `flexi_logger` is used purely as a rotating
//! file writer plus stdout duplication: every emitted line goes through
//! `log::info!` at a fixed severity, with a custom format function that
//! writes the line verbatim (no added timestamp/level/target) rather than
//! `flexi_logger`'s own formatting or level filtering.

use std::cmp::Ordering;

use chrono::SecondsFormat;
use flexi_logger::{Cleanup, Criterion, DeferredNow, Duplicate, FileSpec, Logger, Naming, Record};

/// Rotated backups kept in addition to the active file (10 files total).
const LOG_BACKUP_COUNT: usize = 9;
const DEFAULT_LOG_MAX_TOTAL_BYTES: u64 = 500 * 1024 * 1024;

/// Starts process-wide logging. Idempotent to call at most once; calling
/// it again (e.g. from a second `main`-like entry point) would panic via
/// `flexi_logger`'s own global-logger-already-set error, so callers
/// should only ever invoke this from the real process entry point, not
/// from library code or tests.
pub fn init() {
    let dir = std::env::var("LOG_DIR").unwrap_or_else(|_| "logs".to_string());
    let max_total = crate::env::u64_from_env("LOG_MAX_TOTAL_BYTES", DEFAULT_LOG_MAX_TOTAL_BYTES);
    let max_file_bytes = (max_total / (LOG_BACKUP_COUNT as u64 + 1)).max(1);

    let file_spec = FileSpec::default()
        .directory(&dir)
        .basename("server")
        .suppress_timestamp()
        .suffix("log");

    let result = Logger::try_with_str("info")
        .expect("static log spec \"info\" always parses")
        .log_to_file(file_spec)
        .rotate(
            Criterion::Size(max_file_bytes),
            Naming::Numbers,
            Cleanup::KeepLogFiles(LOG_BACKUP_COUNT),
        )
        .duplicate_to_stdout(Duplicate::All)
        .format(raw_format)
        .start();

    if let Err(err) = result {
        eprintln!("could not open log directory {dir:?}, file logging disabled: {err}");
        // Fall back to stdout-only so output is never silently dropped.
        let _ = Logger::try_with_str("info")
            .expect("static log spec \"info\" always parses")
            .format(raw_format)
            .start();
    }
}

/// Writes `record.args()` verbatim -- no timestamp/level/target added by
/// `flexi_logger` itself, since every line we emit is already fully
/// formatted. `flexi_logger` appends the line ending after calling this,
/// so it must not add its own trailing newline.
fn raw_format(
    w: &mut dyn std::io::Write,
    _now: &mut DeferredNow,
    record: &Record,
) -> std::io::Result<()> {
    write!(w, "{}", record.args())
}

fn emit(line: &str) {
    log::info!("{line}");
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

impl LogLevel {
    fn weight(self) -> u8 {
        match self {
            LogLevel::Debug => 10,
            LogLevel::Info => 20,
            LogLevel::Warn => 30,
            LogLevel::Error => 40,
        }
    }

    fn parse(raw: &str) -> Option<LogLevel> {
        match raw.to_ascii_lowercase().as_str() {
            "debug" => Some(LogLevel::Debug),
            "info" => Some(LogLevel::Info),
            "warn" => Some(LogLevel::Warn),
            "error" => Some(LogLevel::Error),
            _ => None,
        }
    }
}

impl PartialOrd for LogLevel {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.weight().cmp(&other.weight()))
    }
}

/// Pure resolution of the active log level from raw env values, mirroring
/// `logLevelFromEnv()`. Kept separate from env reads for testability.
pub fn resolve_log_level(verbose: bool, log_level: Option<&str>) -> LogLevel {
    if verbose {
        return LogLevel::Debug;
    }
    log_level
        .and_then(LogLevel::parse)
        .unwrap_or(LogLevel::Info)
}

fn active_level() -> LogLevel {
    resolve_log_level(
        crate::env::bool_from_env("VERBOSE"),
        std::env::var("LOG_LEVEL").ok().as_deref(),
    )
}

fn should_log(level: LogLevel) -> bool {
    level.weight() >= active_level().weight()
}

pub fn log_v3(level: LogLevel, message: impl AsRef<str>) {
    if !should_log(level) {
        return;
    }
    let timestamp = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let tag = level_tag(level);
    emit(&format!("{timestamp} [{tag}] [V3] {}", message.as_ref()));
}

pub fn log_v4(level: LogLevel, message: impl AsRef<str>) {
    if !should_log(level) {
        return;
    }
    emit(&format!("[V4] {}", message.as_ref()));
}

/// Process-stdout diagnostics for the control panel (bind errors, relay
/// connect/reconnect). Distinct from `panel::state::PanelState`'s
/// in-memory log, which is the user-facing event feed shown in the
/// browser UI -- the two audiences overlap in content but serve
/// different purposes, same as V3's stdout logs vs. its `notify`/`error`
/// frames sent to clients.
pub fn log_panel(level: LogLevel, message: impl AsRef<str>) {
    if !should_log(level) {
        return;
    }
    let timestamp = chrono::Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true);
    let tag = level_tag(level);
    emit(&format!("{timestamp} [{tag}] [PANEL] {}", message.as_ref()));
}

fn level_tag(level: LogLevel) -> &'static str {
    match level {
        LogLevel::Debug => "DEBUG",
        LogLevel::Info => "INFO",
        LogLevel::Warn => "WARN",
        LogLevel::Error => "ERROR",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn verbose_forces_debug_regardless_of_log_level() {
        assert_eq!(resolve_log_level(true, Some("error")), LogLevel::Debug);
    }

    #[test]
    fn explicit_log_level_is_respected() {
        assert_eq!(resolve_log_level(false, Some("warn")), LogLevel::Warn);
        assert_eq!(resolve_log_level(false, Some("WARN")), LogLevel::Warn);
    }

    #[test]
    fn unset_or_invalid_defaults_to_info() {
        assert_eq!(resolve_log_level(false, None), LogLevel::Info);
        assert_eq!(resolve_log_level(false, Some("bogus")), LogLevel::Info);
    }

    #[test]
    fn ordering_matches_weight_table() {
        assert!(LogLevel::Debug < LogLevel::Info);
        assert!(LogLevel::Info < LogLevel::Warn);
        assert!(LogLevel::Warn < LogLevel::Error);
    }
}
