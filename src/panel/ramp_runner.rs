//! The background task that drives one channel's active strength ramp:
//! computes the value to command on each 1-second tick
//! ([`ramp::RunnerTick`]), sends a `Set` frame through the exact same
//! functions `POST /api/strength` uses whenever that value actually
//! changes (not every tick -- see [`send_set`]), and updates
//! [`PanelState`] so `/events` can report progress. See
//! `docs/dg-lab-panel-feature-requests.md`'s Strength Ramp Profiles
//! section for the full design.
//!
//! There's no pause/resume for a ramp, only start/stop -- a manual
//! `/api/strength` call or `POST /api/ramp/stop` cancels it outright
//! (see `PanelState::ramp_cancel`) -- so unlike the playlist runner,
//! this task owns all of its own stepping state (`ramp::RunnerTick`)
//! locally rather than needing to save/restore it across a pause.

use std::sync::Arc;

use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use crate::v3::protocol::Channel;

use super::calibration;
use super::commands::{self, StrengthOp};
use super::ramp::{RampProfile, RunnerTick};
use super::state::{ActiveTarget, PanelState};
use super::v4_commands;

pub async fn run(
    panel: Arc<PanelState>,
    channel: Channel,
    profile: RampProfile,
    token: CancellationToken,
) {
    let mut tick = RunnerTick::new(profile);
    let mut last_sent: Option<i64> = None;

    loop {
        let limit = panel.strength_and_limit(channel).1;
        let Some(value) = tick.step(profile, limit) else {
            // Natural end -- only clear state if nothing else has
            // already taken over this channel's ramp slot. A fresh
            // `POST /api/ramp` cancels this same token when it
            // replaces us; if that raced with us reaching the end at
            // the same instant, leave its state alone rather than
            // clobbering the replacement.
            if !token.is_cancelled() {
                panel.ramp_clear(channel);
            }
            return;
        };

        if last_sent != Some(value) {
            send_set(&panel, channel, value).await;
            last_sent = Some(value);
        }
        panel.ramp_tick(
            channel,
            value,
            profile.target(Some(tick.rolled)),
            tick.remaining_secs(profile),
        );

        tokio::select! {
            () = tokio::time::sleep(RunnerTick::tick_duration()) => {
                tick.elapsed_secs += 1;
            }
            () = token.cancelled() => {
                return;
            }
        }
    }
}

/// Sends one `Set` command through the exact functions `POST
/// /api/strength` already uses, converting `value` -- the ramp's own
/// *logical* target, per `profile`'s configured curve -- into the raw
/// wire value via the channel's calibration (Feature 9) first, the same
/// as every other absolute-target strength command. Failures (no device
/// paired, relay not ready, or -- V4 only -- no known baseline strength
/// to compute a delta from yet) are logged and the tick is simply
/// skipped, the same policy `playlist_runner::dispatch` already uses
/// for the identical class of problem: the ramp's own clock keeps
/// advancing regardless of whether this particular tick could actually
/// reach the device, rather than getting stuck retrying.
async fn send_set(panel: &PanelState, channel: Channel, value: i64) {
    let (target, tx) = match panel.active_target_and_outbound() {
        Ok(pair) => pair,
        Err((_, message)) => {
            panel.log(format!(
                "Ramp channel {}: {message} -- tick skipped",
                commands::channel_str(channel)
            ));
            return;
        }
    };
    let current = panel.strength_and_limit(channel).0;
    let raw_value = match calibration::apply_checked(panel.calibration_for(channel), value) {
        Ok(v) => v,
        Err(message) => {
            panel.log(format!(
                "Ramp channel {}: {message} -- tick skipped",
                commands::channel_str(channel)
            ));
            return;
        }
    };
    let frame = match &target {
        ActiveTarget::V3 {
            controller_id,
            device_id,
        } => Some(commands::strength_frame(
            controller_id,
            device_id,
            channel,
            StrengthOp::Set(raw_value),
        )),
        ActiveTarget::V4 { device_id, slot_id } => v4_commands::strength_frame(
            device_id,
            slot_id,
            channel,
            StrengthOp::Set(raw_value),
            current,
        ),
    };
    let Some(frame) = frame else {
        panel.log(format!(
            "Ramp channel {}: current V4 strength not known yet -- tick skipped",
            commands::channel_str(channel)
        ));
        return;
    };
    let text = frame.to_string();
    if tx.send(WsMessage::Text(text.clone().into())).is_err() {
        panel.log(format!(
            "Ramp channel {}: failed to send to relay -- tick skipped",
            commands::channel_str(channel)
        ));
        return;
    }
    panel.apply_optimistic_strength(channel, raw_value);
    panel.log(format!("Ramp sent: {text}"));
}
