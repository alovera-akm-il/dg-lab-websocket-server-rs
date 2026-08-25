//! The background task that drives the session timer: walks its
//! precomputed schedule (see [`super::session`]), sleeping exactly to
//! each checkpoint in turn and firing the matching webhook/log event,
//! until it reaches the final `Ended` checkpoint. See
//! `docs/dg-lab-panel-feature-requests.md`'s Session Timer section for
//! the full design.
//!
//! Unlike playlists/ramps, the session timer is panel-wide, not
//! per-channel -- there's only ever one running at a time.

use std::sync::Arc;
use std::time::Duration;

use serde_json::{Value, json};
use tokio_util::sync::CancellationToken;

use crate::v3::protocol::Channel;

use super::session::{Checkpoint, ScheduledEvent};
use super::state::PanelState;

pub async fn run(
    panel: Arc<PanelState>,
    schedule: Vec<Checkpoint>,
    start_elapsed: u32,
    total_seconds: u32,
    token: CancellationToken,
) {
    let mut elapsed = start_elapsed;

    for checkpoint in schedule {
        let wait = checkpoint.at_seconds.saturating_sub(elapsed);
        if wait > 0 {
            tokio::select! {
                () = tokio::time::sleep(Duration::from_secs(wait.into())) => {}
                () = token.cancelled() => {
                    return; // paused or stopped externally -- state already correct
                }
            }
        }
        elapsed = checkpoint.at_seconds;

        if matches!(checkpoint.event, ScheduledEvent::Ended) {
            // Only clear state if nothing else has already taken over
            // the session slot -- a fresh `POST /api/session/timer`
            // cancels this same token when it replaces us; if that
            // raced with us reaching the end at the same instant,
            // leave the replacement's state alone rather than
            // clobbering it (same guard `ramp_runner` uses for the
            // identical class of race).
            if !token.is_cancelled()
                && let Some((config, elapsed_at_stop)) = panel.session_stop()
            {
                fire_event(
                    &panel,
                    &ScheduledEvent::Ended,
                    elapsed_at_stop,
                    total_seconds,
                );
                if config.auto_stop_playlists_at_end {
                    panel.playlist_stop(Channel::A);
                    panel.playlist_stop(Channel::B);
                }
            }
            return;
        }

        panel.session_advance();
        fire_event(&panel, &checkpoint.event, elapsed, total_seconds);
    }
}

fn fire_event(panel: &PanelState, event: &ScheduledEvent, elapsed: u32, total_seconds: u32) {
    let remaining = total_seconds.saturating_sub(elapsed);
    let (event_name, label, message) = match event {
        ScheduledEvent::CheckIn => (
            "session.check_in",
            Value::Null,
            "Session check-in".to_string(),
        ),
        ScheduledEvent::PhaseGate { label } => (
            "session.phase_gate",
            json!(label),
            format!("Session phase gate: {label}"),
        ),
        ScheduledEvent::Ending => (
            "session.ending",
            Value::Null,
            "Session ending soon (5 min warning)".to_string(),
        ),
        ScheduledEvent::Ended => ("session.ended", Value::Null, "Session ended".to_string()),
    };
    panel.log_with(
        message,
        json!({
            "event": event_name,
            "elapsedSeconds": elapsed,
            "remainingSeconds": remaining,
            "label": label,
        }),
    );
}
