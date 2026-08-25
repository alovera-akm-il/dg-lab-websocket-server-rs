//! Configurable button mapping: assigns a server-side action to a
//! physical button-shape press, dispatched the instant `relay_client.rs`
//! (V3) or `v4_client.rs` (V4) decode one -- Feature 5 from
//! `docs/dg-lab-panel-feature-requests.md`. Persisted like templates
//! (`super::persistence`), since "configure once" is the whole point.
//!
//! **Scope: `pattern` (per-button) mapping only.** The original request
//! also proposed `shortPress`/`doublePress`/`longPress`, but neither
//! protocol reports press duration or click count anywhere on the wire
//! (`docs/api.md`'s "Device feedback" section: one message per tap,
//! full stop) -- there's nothing to detect them from, and the request
//! doesn't say which of the 10 button codes a "double press" would even
//! apply to. `pattern` alone already covers the request's own primary
//! use case ("tap to pause A's playlist without reaching for the
//! phone").
//!
//! Every supported action reuses a function that already exists
//! elsewhere in the panel (`PanelState::playlist_play`/`pause`/`stop`,
//! `commands::strength_frame`/`v4_commands::strength_frame`,
//! `PanelState::ramp_cancel`) -- this module is almost entirely wiring,
//! not new device-control logic, mirroring how little of
//! `ramp_runner.rs` turned out to be genuinely new wire-protocol code
//! either.

use std::collections::HashMap;
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tokio_tungstenite::tungstenite::Message as WsMessage;

use crate::v3::protocol::Channel;

use super::calibration;
use super::commands::{self, StrengthOp};
use super::persistence;
use super::playlist_runner;
use super::state::{ActiveTarget, PanelState};
use super::v4_commands;

const FILE: &str = "button-map.json";

pub fn load() -> ButtonMap {
    persistence::load_json(FILE)
}

pub fn save(map: &ButtonMap) {
    persistence::save_json(FILE, map);
}

/// Which channel(s) an action applies to. The original request's
/// examples use `"both"` only for `strength_delta`/`ramp_cancel`, but
/// there's no reason the other actions couldn't accept it too (e.g.
/// mapping one button to pause both playlists at once), so it's
/// accepted uniformly across every action rather than modeled as two
/// different target types.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChannelTarget {
    // Per-variant renames, not `rename_all`: the request's own examples
    // mix casing -- individual channels stay uppercase ("A"/"B"),
    // matching every other channel spelling in this codebase (wire
    // protocols, `commands::parse_channel`), while "both" (a concept
    // unique to this feature, no prior convention) is lowercase.
    #[serde(rename = "A")]
    A,
    #[serde(rename = "B")]
    B,
    #[serde(rename = "both")]
    Both,
}

impl ChannelTarget {
    pub fn channels(self) -> &'static [Channel] {
        match self {
            ChannelTarget::A => &[Channel::A],
            ChannelTarget::B => &[Channel::B],
            ChannelTarget::Both => &[Channel::A, Channel::B],
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum ButtonAction {
    PlaylistPlay {
        target: ChannelTarget,
    },
    PlaylistPause {
        target: ChannelTarget,
    },
    PlaylistStop {
        target: ChannelTarget,
    },
    PlaylistToggle {
        target: ChannelTarget,
    },
    /// `strengthInc`/`strengthDec`/`strengthDelta` all resolve to the
    /// same underlying operation (a signed delta added to the
    /// best-known current strength) -- kept as three separate variants
    /// for fidelity to the request's own action table, which names them
    /// separately, rather than collapsing them into one.
    StrengthInc {
        channel: ChannelTarget,
        amount: i64,
    },
    StrengthDec {
        channel: ChannelTarget,
        amount: i64,
    },
    StrengthSet {
        channel: ChannelTarget,
        value: i64,
    },
    StrengthDelta {
        channel: ChannelTarget,
        delta: i64,
    },
    EmergencyClear,
    RampCancel {
        channel: ChannelTarget,
    },
    WebhookOnly,
    None,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ButtonMap {
    #[serde(default)]
    pub pattern: HashMap<String, ButtonAction>,
}

/// Looks up and executes whatever action is mapped to `"{channel}-
/// {shape}"` (e.g. `"A-circle"`), if any. Called right after
/// `PanelState::set_button_action` in both `relay_client.rs` and
/// `v4_client.rs`, which already decode a raw press into this same
/// `(channel, shape)` pair via `decode_button_feedback` -- a no-op if
/// nothing is mapped to that key, which is the common case for anyone
/// who hasn't configured a button map at all.
pub fn dispatch(panel: &Arc<PanelState>, channel: &str, shape: &str) {
    let key = format!("{channel}-{shape}");
    let Some(action) = panel.button_map_action_for(&key) else {
        return;
    };
    panel.log(format!("Button map: \"{key}\" -> {action:?}"));
    match action {
        ButtonAction::PlaylistPlay { target } => {
            for &ch in target.channels() {
                start_playlist(panel, ch);
            }
        }
        ButtonAction::PlaylistPause { target } => {
            for &ch in target.channels() {
                panel.playlist_pause(ch);
            }
        }
        ButtonAction::PlaylistStop { target } => {
            for &ch in target.channels() {
                panel.playlist_stop(ch);
            }
        }
        ButtonAction::PlaylistToggle { target } => {
            for &ch in target.channels() {
                if panel.playlist_is_playing(ch) {
                    panel.playlist_pause(ch);
                } else {
                    start_playlist(panel, ch);
                }
            }
        }
        ButtonAction::StrengthInc { channel, amount } => {
            for &ch in channel.channels() {
                apply_strength_delta(panel, ch, amount);
            }
        }
        ButtonAction::StrengthDec { channel, amount } => {
            for &ch in channel.channels() {
                apply_strength_delta(panel, ch, -amount);
            }
        }
        ButtonAction::StrengthSet { channel, value } => {
            // `value` is a *logical* target (Feature 9) -- calibrated
            // into a raw wire value here, unlike `StrengthInc`/`_Dec`/
            // `_Delta` below, which nudge the raw current strength
            // directly and deliberately bypass calibration (see
            // `calibration`'s module docs).
            for &ch in channel.channels() {
                match calibration::apply_checked(panel.calibration_for(ch), value) {
                    Ok(raw) => apply_strength_target(panel, ch, raw),
                    Err(message) => panel.log(format!(
                        "Button map: channel {} {message} -- skipped",
                        commands::channel_str(ch)
                    )),
                }
            }
        }
        ButtonAction::StrengthDelta { channel, delta } => {
            for &ch in channel.channels() {
                apply_strength_delta(panel, ch, delta);
            }
        }
        ButtonAction::EmergencyClear => {
            for ch in [Channel::A, Channel::B] {
                send_clear(panel, ch);
                panel.playlist_stop(ch);
                panel.ramp_cancel(ch);
                // Also ends any in-progress `POST /api/session/pause`
                // cycle (Feature 10) for this channel -- same fix as
                // `handler::post_session_stop_all`, for the same reason:
                // an emergency clear is meant to be final, and shouldn't
                // leave a stale pre-pause value for a later `POST
                // /api/session/resume` to silently restore.
                panel.take_pre_pause_strength(ch);
            }
        }
        ButtonAction::RampCancel { channel } => {
            for &ch in channel.channels() {
                panel.ramp_cancel(ch);
            }
        }
        ButtonAction::WebhookOnly | ButtonAction::None => {
            // `button_feedback` already fired above `dispatch` is called
            // (see the call sites in relay_client.rs/v4_client.rs) --
            // both of these are intentionally inert here.
        }
    }
}

fn start_playlist(panel: &Arc<PanelState>, channel: Channel) {
    if let Ok((step, token)) = panel.playlist_play(channel) {
        tokio::spawn(playlist_runner::run(panel.clone(), channel, token, step));
    }
}

/// Resolves a signed delta against the best-known current strength and
/// applies it -- a no-op (logged, not an error a caller needs to
/// handle) if no baseline is known yet, since there's nothing to add a
/// delta to.
fn apply_strength_delta(panel: &PanelState, channel: Channel, delta: i64) {
    let Some(current) = panel.strength_and_limit(channel).0 else {
        panel.log(format!(
            "Button map: channel {} strength not known yet -- delta skipped",
            commands::channel_str(channel)
        ));
        return;
    };
    apply_strength_target(panel, channel, current + delta);
}

/// Sets `channel`'s strength to exactly `target`, respecting the
/// configured upper limit and reusing the exact wire-frame-building
/// `POST /api/strength`'s `op: "set"` uses (including V4's "no known
/// baseline" rejection) -- a self-contained implementation rather than
/// calling into `handler::post_strength`, the same "not going through
/// the HTTP layer" shape `ramp_runner::send_set` already uses for the
/// identical class of problem.
fn apply_strength_target(panel: &PanelState, channel: Channel, target: i64) {
    // A button-triggered strength change also ends any in-progress
    // `POST /api/session/pause` cycle for this channel (Feature 10),
    // same as `handler::post_strength` -- otherwise a later `POST
    // /api/session/resume` could silently overwrite this with the stale
    // pre-pause value.
    panel.take_pre_pause_strength(channel);
    let (current, limit) = panel.strength_and_limit(channel);
    if let Some(limit) = limit
        && target > limit
    {
        panel.log(format!(
            "Button map: channel {} target {target} would exceed the configured upper limit of {limit} -- skipped",
            commands::channel_str(channel)
        ));
        return;
    }
    let Ok((active, tx)) = panel.active_target_and_outbound() else {
        panel.log("Button map: strength command skipped -- no device paired or relay not ready");
        return;
    };
    let frame = match &active {
        ActiveTarget::V3 {
            controller_id,
            device_id,
        } => Some(commands::strength_frame(
            controller_id,
            device_id,
            channel,
            StrengthOp::Set(target),
        )),
        ActiveTarget::V4 { device_id, slot_id } => v4_commands::strength_frame(
            device_id,
            slot_id,
            channel,
            StrengthOp::Set(target),
            current,
        ),
    };
    let Some(frame) = frame else {
        panel.log("Button map: current V4 strength not known yet -- Set skipped");
        return;
    };
    let text = frame.to_string();
    if tx.send(WsMessage::Text(text.clone().into())).is_ok() {
        panel.log(format!("Button map sent: {text}"));
        panel.apply_optimistic_strength(channel, target);
    } else {
        panel.log("Button map: failed to send to relay");
    }
}

fn send_clear(panel: &PanelState, channel: Channel) {
    let Ok((active, tx)) = panel.active_target_and_outbound() else {
        return;
    };
    let frame = match &active {
        ActiveTarget::V3 {
            controller_id,
            device_id,
        } => commands::clear_frame(controller_id, device_id, channel),
        ActiveTarget::V4 { device_id, slot_id } => {
            v4_commands::clear_frame(device_id, slot_id, channel)
        }
    };
    let _ = tx.send(WsMessage::Text(frame.to_string().into()));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_target_resolves_to_the_expected_channel_list() {
        assert_eq!(ChannelTarget::A.channels(), &[Channel::A]);
        assert_eq!(ChannelTarget::B.channels(), &[Channel::B]);
        assert_eq!(ChannelTarget::Both.channels(), &[Channel::A, Channel::B]);
    }

    #[test]
    fn button_map_round_trips_through_json_matching_the_request_shape() {
        let json_str = r#"{
            "pattern": {
                "A-circle": {"action": "playlist_toggle", "target": "A"},
                "A-triangle": {"action": "strength_inc", "channel": "A", "amount": 1},
                "B-hexagon": {"action": "ramp_cancel", "channel": "both"},
                "B-star": {"action": "emergency_clear"},
                "B-square": {"action": "none"}
            }
        }"#;
        let map: ButtonMap = serde_json::from_str(json_str).unwrap();
        assert_eq!(map.pattern.len(), 5);
        assert!(matches!(
            map.pattern.get("A-circle"),
            Some(ButtonAction::PlaylistToggle {
                target: ChannelTarget::A
            })
        ));
        assert!(matches!(
            map.pattern.get("B-hexagon"),
            Some(ButtonAction::RampCancel {
                channel: ChannelTarget::Both
            })
        ));
        assert!(matches!(
            map.pattern.get("B-star"),
            Some(ButtonAction::EmergencyClear)
        ));

        // Round-trips back out the same shape it came in.
        let reserialized = serde_json::to_string(&map).unwrap();
        let reparsed: ButtonMap = serde_json::from_str(&reserialized).unwrap();
        assert_eq!(reparsed.pattern.len(), 5);
    }

    #[test]
    fn unmapped_key_is_a_silent_no_op() {
        let map = ButtonMap::default();
        assert!(!map.pattern.contains_key("A-circle"));
    }
}
