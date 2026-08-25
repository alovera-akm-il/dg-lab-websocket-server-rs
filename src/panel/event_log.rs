//! File-based event log: appends every event `PanelState::log_with`
//! already sees (the same superset the webhook fires for) to a local
//! JSONL file, one JSON object per line -- Feature 4 from
//! `docs/dg-lab-panel-feature-requests.md`. Runs as its own background
//! task (see [`run`]) owning the current file handle and rotation state
//! serially, communicated with over an `mpsc` channel -- the same
//! "one actor task, no shared mutex for its own internal state" shape
//! the V3/V4 relay connections already use for their writer halves.
//!
//! **"Session" here means a recording session (one physical file), not
//! the panel-wide session *timer* (`super::session`) -- the two share
//! the word by coincidence in the original request, not by design; they
//! don't interact.** A session's file opens on whichever comes first:
//! an explicit [`EventLogMsg::StartSession`], or the first event logged
//! while enabled and no file is currently open -- deliberately more
//! general than the original request's literal "auto-started at first
//! playlist play" (which would miss any pairing/strength/etc. activity
//! that happens before the first playlist play), and it needs no hook
//! anywhere outside this module: any `PanelState::log_with` call
//! already routes through [`EventLogMsg::Append`], which opens a file
//! itself if one isn't open yet. There's no explicit "end session" in
//! the original request, so a session's file
//! simply keeps being appended to (rolling to a fresh file only when it
//! crosses `max_file_size_mb`) until the next explicit `StartSession`
//! forces a new one, or the process restarts.
//!
//! Retention is swept once whenever a new file opens (not on a separate
//! timer) -- simple and sufficient for a personal tool: any `.jsonl`
//! file in the configured directory older than `retention_days`
//! (by filesystem mtime, not by parsing its name -- a hand-picked
//! `filenameFormat` can't always be parsed back into a date) is deleted.

use std::path::PathBuf;
use std::time::{Duration, SystemTime};

use serde::Deserialize;
use serde_json::Value;
use tokio::fs::{self, File, OpenOptions};
use tokio::io::AsyncWriteExt;
use tokio::sync::mpsc;

use crate::logging::{LogLevel, log_panel};

#[derive(Debug, Clone, Deserialize)]
pub struct EventLogConfig {
    pub enabled: bool,
    pub directory: PathBuf,
    #[serde(rename = "filenameFormat")]
    pub filename_format: String,
    /// `0` disables size-based rotation entirely -- the file just keeps
    /// growing until the next `StartSession`.
    #[serde(rename = "maxFileSizeMb")]
    pub max_file_size_mb: u64,
    /// `0` disables the retention sweep entirely -- nothing is ever
    /// auto-deleted.
    #[serde(rename = "retentionDays")]
    pub retention_days: u32,
}

impl EventLogConfig {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.directory.as_os_str().is_empty() {
            return Err("directory must not be empty");
        }
        if self.filename_format.trim().is_empty() {
            return Err("filenameFormat must not be empty");
        }
        Ok(())
    }

    fn disabled() -> Self {
        EventLogConfig {
            enabled: false,
            directory: PathBuf::new(),
            filename_format: String::new(),
            max_file_size_mb: 0,
            retention_days: 0,
        }
    }
}

pub enum EventLogMsg {
    Configure(EventLogConfig),
    StartSession,
    /// One already-classified event -- `message`/`extra` are exactly
    /// what `PanelState::log_with` receives; this task builds the same
    /// `{message, timestamp, ...extra}` shape `webhook::notify`'s
    /// payloads use, independently (a few duplicated lines rather than
    /// threading a shared builder through both modules for something
    /// this small).
    Append {
        message: String,
        extra: Value,
    },
}

/// Renders `{YYYY-MM-DD}`/`{HH-mm-ss}` placeholders against `now` --
/// the only two the original request's example uses. Any other text in
/// `format` (including no placeholder at all, which just names the same
/// file every session) passes through unchanged.
fn render_filename(format: &str, now: chrono::DateTime<chrono::Utc>) -> String {
    format
        .replace("{YYYY-MM-DD}", &now.format("%Y-%m-%d").to_string())
        .replace("{HH-mm-ss}", &now.format("%H-%M-%S").to_string())
}

struct OpenFile {
    file: File,
    bytes_written: u64,
}

async fn open_new_file(config: &EventLogConfig) -> Option<OpenFile> {
    if let Err(err) = fs::create_dir_all(&config.directory).await {
        log_panel(
            LogLevel::Warn,
            format!(
                "event log: failed to create directory {}: {err}",
                config.directory.display()
            ),
        );
        return None;
    }
    let filename = render_filename(&config.filename_format, chrono::Utc::now());
    let path = config.directory.join(filename);
    match OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .await
    {
        Ok(file) => {
            // A re-used filename (two sessions starting within the same
            // rendered timestamp) appends rather than overwrites -- ask
            // for its current length so size-based rotation still
            // triggers at the right point instead of resetting to 0.
            let bytes_written = fs::metadata(&path).await.map(|m| m.len()).unwrap_or(0);
            Some(OpenFile {
                file,
                bytes_written,
            })
        }
        Err(err) => {
            log_panel(
                LogLevel::Warn,
                format!("event log: failed to open {}: {err}", path.display()),
            );
            None
        }
    }
}

/// Deletes any `.jsonl` file in `config.directory` whose modified time
/// is older than `retention_days` -- a no-op if retention is disabled
/// (`0`) or the directory can't be listed (e.g. doesn't exist yet).
async fn sweep_retention(config: &EventLogConfig) {
    if config.retention_days == 0 {
        return;
    }
    let cutoff = SystemTime::now() - Duration::from_secs(u64::from(config.retention_days) * 86_400);
    let Ok(mut entries) = fs::read_dir(&config.directory).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
            continue;
        }
        let Ok(metadata) = entry.metadata().await else {
            continue;
        };
        if let Ok(modified) = metadata.modified()
            && modified < cutoff
        {
            let _ = fs::remove_file(&path).await;
        }
    }
}

fn build_line(message: &str, extra: Value) -> String {
    let mut obj = serde_json::json!({
        "message": message,
        "timestamp": chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
    });
    if let Value::Object(extra_fields) = extra
        && let Value::Object(body_fields) = &mut obj
    {
        body_fields.extend(extra_fields);
    }
    obj.to_string()
}

/// The event log's background task -- owns the current file handle and
/// rotation bookkeeping for its whole lifetime, driven entirely by
/// messages from [`super::state::PanelState`] (see
/// `PanelState::install_event_log_sender`). Runs until `rx` closes
/// (panel shutdown).
pub async fn run(mut rx: mpsc::UnboundedReceiver<EventLogMsg>) {
    let mut config = EventLogConfig::disabled();
    let mut current: Option<OpenFile> = None;

    while let Some(msg) = rx.recv().await {
        match msg {
            EventLogMsg::Configure(new_config) => {
                if !new_config.enabled {
                    current = None; // closes the file handle (Drop)
                }
                config = new_config;
            }
            EventLogMsg::StartSession => {
                if config.enabled {
                    current = open_new_file(&config).await;
                    sweep_retention(&config).await;
                }
            }
            EventLogMsg::Append { message, extra } => {
                if !config.enabled {
                    continue;
                }
                if current.is_none() {
                    current = open_new_file(&config).await;
                    sweep_retention(&config).await;
                }
                let max_bytes = config.max_file_size_mb * 1024 * 1024;
                if let Some(open) = &current
                    && max_bytes > 0
                    && open.bytes_written >= max_bytes
                {
                    current = open_new_file(&config).await;
                }
                let Some(open) = &mut current else { continue };
                let line = build_line(&message, extra);
                let written = open.file.write_all(line.as_bytes()).await.is_ok()
                    && open.file.write_all(b"\n").await.is_ok();
                if written {
                    open.bytes_written += line.len() as u64 + 1;
                } else {
                    log_panel(LogLevel::Warn, "event log: write failed, dropping line");
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn render_filename_substitutes_both_known_placeholders() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-08-23T23:01:15Z")
            .unwrap()
            .with_timezone(&chrono::Utc);
        assert_eq!(
            render_filename("dg-lab-{YYYY-MM-DD}_{HH-mm-ss}.jsonl", now),
            "dg-lab-2026-08-23_23-01-15.jsonl"
        );
    }

    #[test]
    fn render_filename_without_placeholders_passes_through() {
        let now = chrono::Utc::now();
        assert_eq!(render_filename("fixed-name.jsonl", now), "fixed-name.jsonl");
    }

    #[test]
    fn build_line_merges_extra_fields_alongside_message_and_timestamp() {
        let line = build_line(
            "Session check-in",
            serde_json::json!({"event": "session.check_in", "elapsedSeconds": 5}),
        );
        let parsed: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(parsed["message"], "Session check-in");
        assert_eq!(parsed["event"], "session.check_in");
        assert_eq!(parsed["elapsedSeconds"], 5);
        assert!(parsed["timestamp"].is_string());
    }

    #[test]
    fn validate_rejects_empty_directory_or_filename_format() {
        let mut config = EventLogConfig {
            enabled: true,
            directory: PathBuf::new(),
            filename_format: "x.jsonl".to_string(),
            max_file_size_mb: 0,
            retention_days: 0,
        };
        assert!(config.validate().is_err());
        config.directory = PathBuf::from("/tmp/somewhere");
        config.filename_format = "  ".to_string();
        assert!(config.validate().is_err());
        config.filename_format = "x.jsonl".to_string();
        assert!(config.validate().is_ok());
    }

    #[tokio::test]
    async fn open_new_file_creates_the_directory_and_writes_lines_that_round_trip() {
        let dir = std::env::temp_dir().join(format!("dglab-eventlog-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir).await;
        let config = EventLogConfig {
            enabled: true,
            directory: dir.clone(),
            filename_format: "session.jsonl".to_string(),
            max_file_size_mb: 0,
            retention_days: 0,
        };

        let (tx, rx) = mpsc::unbounded_channel();
        let handle = tokio::spawn(run(rx));
        tx.send(EventLogMsg::Configure(config)).unwrap();
        tx.send(EventLogMsg::StartSession).unwrap();
        tx.send(EventLogMsg::Append {
            message: "hello".to_string(),
            extra: Value::Null,
        })
        .unwrap();
        drop(tx); // closes the channel, letting `run` return
        handle.await.unwrap();

        let contents = fs::read_to_string(dir.join("session.jsonl")).await.unwrap();
        let line = contents.lines().next().expect("one line written");
        let parsed: Value = serde_json::from_str(line).unwrap();
        assert_eq!(parsed["message"], "hello");

        let _ = fs::remove_dir_all(&dir).await;
    }
}
