//! The background task that actually drives one channel's pulse playlist
//! while it's playing: sends the right frame for the current queue entry
//! (a resolved waveform for [`EntryKind::Pulse`], one `clear` frame for
//! [`EntryKind::Gap`]), waits out that entry's resolved duration (racing
//! a cancellation token so pause/stop can interrupt it), and repeats via
//! [`PanelState::playlist_advance`] until the queue naturally stops. See
//! `docs/channel-playlists-implementation.md` for the full design.
//!
//! Deliberately does no pause/stop bookkeeping itself: the HTTP handlers
//! for those already mutate the queue's state directly (`handler.rs`),
//! and only use this task's cancellation token to make an in-flight wait
//! exit promptly. Cancellation always means "stop running, state is
//! already correct" here -- never "figure out why and update state" --
//! which avoids a whole class of races between this task noticing it was
//! cancelled and whichever handler cancelled it.

use std::sync::Arc;
use std::time::Duration;

use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use crate::v3::protocol::Channel;

use super::playlist::{EntryKind, Step};
use super::state::{ActiveTarget, PanelState};
use super::{commands, presets, v4_commands};

/// Runs `channel`'s playlist starting from `first` (the step
/// [`PanelState::playlist_play`] already decided on) until it stops or
/// `token` is cancelled by a pause/stop request.
pub async fn run(panel: Arc<PanelState>, channel: Channel, token: CancellationToken, first: Step) {
    let mut step = first;
    loop {
        let (kind, duration) = match step {
            Step::Run { kind, duration, .. } => (kind, duration),
            Step::Stop => return,
        };

        dispatch(&panel, channel, &kind, duration).await;

        tokio::select! {
            () = tokio::time::sleep(duration) => {
                step = panel.playlist_advance(channel);
            }
            () = token.cancelled() => {
                return;
            }
        }
    }
}

/// Sends the one frame `kind` calls for on entry -- a pulse waveform, or
/// a `clear` to actually silence the channel for a gap (withholding new
/// frames alone would leave a previous item's last queued output
/// running). Failures (no device paired, relay not ready, waveform
/// invalid for the active protocol) are logged and otherwise ignored --
/// the entry's resolved duration still elapses either way, so playback
/// keeps its own clock rather than getting stuck retrying.
async fn dispatch(panel: &PanelState, channel: Channel, kind: &EntryKind, duration: Duration) {
    let (target, tx) = match panel.active_target_and_outbound() {
        Ok(pair) => pair,
        Err((_, message)) => {
            panel.log(format!(
                "Playlist channel {}: {message} -- entry skipped",
                commands::channel_str(channel)
            ));
            return;
        }
    };

    let frame = match kind {
        EntryKind::Pulse { waveform } => {
            let resolved = presets::find(waveform)
                .map(|p| p.waveform_string())
                .unwrap_or_else(|| waveform.clone());
            match &target {
                ActiveTarget::V3 {
                    controller_id,
                    device_id,
                } => Some(commands::pulse_frame(
                    controller_id,
                    device_id,
                    channel,
                    duration.as_secs() as i64,
                    &resolved,
                )),
                ActiveTarget::V4 { device_id, slot_id } => v4_commands::pulse_frame(
                    device_id,
                    slot_id,
                    channel,
                    duration.as_millis() as i64,
                    &resolved,
                ),
            }
        }
        EntryKind::Gap => Some(match &target {
            ActiveTarget::V3 {
                controller_id,
                device_id,
            } => commands::clear_frame(controller_id, device_id, channel),
            ActiveTarget::V4 { device_id, slot_id } => {
                v4_commands::clear_frame(device_id, slot_id, channel)
            }
        }),
    };

    let Some(frame) = frame else {
        panel.log(format!(
            "Playlist channel {}: waveform isn't valid in V4's frame-array format -- entry skipped",
            commands::channel_str(channel)
        ));
        return;
    };

    let text = frame.to_string();
    if tx.send(WsMessage::Text(text.clone().into())).is_err() {
        panel.log(format!(
            "Playlist channel {}: failed to send to relay -- entry skipped",
            commands::channel_str(channel)
        ));
        return;
    }
    panel.log(format!("Playlist sent: {text}"));
}
