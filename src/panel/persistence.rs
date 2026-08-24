//! Tiny JSON-file persistence for the one piece of panel data that needs
//! to survive a process restart to be useful: playlist templates (see
//! [`super::templates`]). Everything else in [`super::state::PanelState`]
//! is deliberately in-memory-only, resetting on restart the same way a
//! relay reconnect resets pairing state -- see
//! `docs/dg-lab-panel-feature-requests.md`'s "one thing all six share"
//! note on why templates are the exception.
//!
//! Plain synchronous `std::fs`, not `tokio::fs`/`spawn_blocking`: these
//! files are tiny (a handful of KB at most) and only ever written on an
//! explicit operator action (saving/deleting a template), never on a hot
//! path, so the brief blocking cost inside an async handler is an
//! acceptable tradeoff against the ceremony a fully async version would
//! need for something this small.

use std::path::PathBuf;

use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::logging::{LogLevel, log_panel};

/// `PANEL_DATA_DIR` -- default `panel-data`, mirroring `LOG_DIR`'s "one
/// env var, one plain value" convention (see `src/logging/mod.rs`).
fn data_dir() -> PathBuf {
    std::env::var("PANEL_DATA_DIR")
        .unwrap_or_else(|_| "panel-data".to_string())
        .into()
}

/// Reads `<data_dir>/<filename>` and parses it as JSON, defaulting (and
/// logging a warning rather than failing startup) if the file is
/// missing or unparseable -- a hand-edited or corrupt file shouldn't
/// stop the panel from starting, just start that store empty.
pub fn load_json<T: DeserializeOwned + Default>(filename: &str) -> T {
    let path = data_dir().join(filename);
    match std::fs::read_to_string(&path) {
        Ok(text) => match serde_json::from_str(&text) {
            Ok(value) => value,
            Err(err) => {
                log_panel(
                    LogLevel::Warn,
                    format!("{}: failed to parse, starting empty: {err}", path.display()),
                );
                T::default()
            }
        },
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => T::default(),
        Err(err) => {
            log_panel(
                LogLevel::Warn,
                format!("{}: failed to read, starting empty: {err}", path.display()),
            );
            T::default()
        }
    }
}

/// Writes `value` to `<data_dir>/<filename>` atomically: serializes to a
/// sibling `.tmp` file, then renames it over the real path -- rename is
/// atomic on the same filesystem, so a crash or a concurrent read
/// mid-write never observes a partially-written file. Failures are
/// logged, not propagated -- a save failure (e.g. a read-only
/// filesystem) shouldn't fail the request that triggered it; the
/// in-memory state stays authoritative until the next successful save.
pub fn save_json<T: Serialize>(filename: &str, value: &T) {
    let dir = data_dir();
    if let Err(err) = std::fs::create_dir_all(&dir) {
        log_panel(
            LogLevel::Warn,
            format!("{}: failed to create data dir: {err}", dir.display()),
        );
        return;
    }
    let path = dir.join(filename);
    let tmp_path = dir.join(format!("{filename}.tmp"));
    let text = match serde_json::to_string_pretty(value) {
        Ok(t) => t,
        Err(err) => {
            log_panel(
                LogLevel::Warn,
                format!("{}: failed to serialize: {err}", path.display()),
            );
            return;
        }
    };
    if let Err(err) = std::fs::write(&tmp_path, text) {
        log_panel(
            LogLevel::Warn,
            format!("{}: failed to write: {err}", tmp_path.display()),
        );
        return;
    }
    if let Err(err) = std::fs::rename(&tmp_path, &path) {
        log_panel(
            LogLevel::Warn,
            format!("{}: failed to finalize save: {err}", path.display()),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde::Deserialize;
    use std::collections::HashMap;

    #[derive(Debug, Default, Clone, PartialEq, Serialize, Deserialize)]
    struct Sample {
        items: HashMap<String, u32>,
    }

    /// Serializes env var access across these tests -- `data_dir()` reads
    /// `PANEL_DATA_DIR` from the process-wide environment, which `std::env`
    /// doesn't isolate per-test, so two tests setting it concurrently would
    /// otherwise race.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn missing_file_loads_the_default() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile_dir("voidless-missing");
        unsafe {
            std::env::set_var("PANEL_DATA_DIR", &dir);
        }
        let loaded: Sample = load_json("does-not-exist.json");
        assert_eq!(loaded, Sample::default());
        unsafe {
            std::env::remove_var("PANEL_DATA_DIR");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_then_load_round_trips() {
        let _guard = ENV_LOCK.lock().unwrap();
        let dir = tempfile_dir("voidless-roundtrip");
        unsafe {
            std::env::set_var("PANEL_DATA_DIR", &dir);
        }
        let mut sample = Sample::default();
        sample.items.insert("a".to_string(), 1);
        save_json("sample.json", &sample);
        let loaded: Sample = load_json("sample.json");
        assert_eq!(loaded, sample);
        // No leftover .tmp file after a successful save.
        assert!(!dir.join("sample.json.tmp").exists());
        unsafe {
            std::env::remove_var("PANEL_DATA_DIR");
        }
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn tempfile_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }
}
