//! Builds the exact V3 wire frames a real controller would send --
//! reuses [`crate::v3::protocol`]'s channel normalization directly so
//! the panel accepts the same channel spellings the relay itself does.

use serde_json::{Value, json};

use crate::v3::protocol::{Channel, normalize_channel};

pub fn parse_channel(raw: &str) -> Option<Channel> {
    normalize_channel(Some(&Value::String(raw.to_string())), None)
}

pub fn channel_str(channel: Channel) -> &'static str {
    match channel {
        Channel::A => "A",
        Channel::B => "B",
    }
}

#[derive(Clone, Copy)]
pub enum StrengthOp {
    Inc,
    Dec,
    Set(i64),
}

/// `message` is `"set channel"` for all three ops, matching dglab-kit's
/// own `addStrength`/`reduceStrength`/`setStrength` convention -- the V3
/// server ignores this field's content for numeric-type routing, so this
/// is purely for wire-level fidelity with a real controller.
pub fn strength_frame(
    controller_id: &str,
    device_id: &str,
    channel: Channel,
    op: StrengthOp,
) -> Value {
    match op {
        StrengthOp::Inc => json!({
            "type": 1, "clientId": controller_id, "targetId": device_id,
            "channel": channel_str(channel), "message": "set channel",
        }),
        StrengthOp::Dec => json!({
            "type": 2, "clientId": controller_id, "targetId": device_id,
            "channel": channel_str(channel), "message": "set channel",
        }),
        StrengthOp::Set(value) => json!({
            "type": 3, "clientId": controller_id, "targetId": device_id,
            "channel": channel_str(channel), "strength": value, "message": "set channel",
        }),
    }
}

pub fn clear_frame(controller_id: &str, device_id: &str, channel: Channel) -> Value {
    json!({
        "type": 4, "clientId": controller_id, "targetId": device_id,
        "channel": channel_str(channel), "message": "clear",
    })
}

pub fn pulse_frame(
    controller_id: &str,
    device_id: &str,
    channel: Channel,
    time: i64,
    waveform: &str,
) -> Value {
    json!({
        "type": "clientMsg", "clientId": controller_id, "targetId": device_id,
        "channel": channel_str(channel), "time": time, "message": waveform,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_channel_accepts_known_spellings() {
        assert_eq!(parse_channel("A"), Some(Channel::A));
        assert_eq!(parse_channel("a"), Some(Channel::A));
        assert_eq!(parse_channel("1"), Some(Channel::A));
        assert_eq!(parse_channel("B"), Some(Channel::B));
        assert_eq!(parse_channel("nonsense"), None);
    }

    #[test]
    fn strength_frames_match_the_v3_wire_shape() {
        assert_eq!(
            strength_frame("c1", "d1", Channel::A, StrengthOp::Inc),
            json!({"type":1,"clientId":"c1","targetId":"d1","channel":"A","message":"set channel"})
        );
        assert_eq!(
            strength_frame("c1", "d1", Channel::B, StrengthOp::Set(20)),
            json!({"type":3,"clientId":"c1","targetId":"d1","channel":"B","strength":20,"message":"set channel"})
        );
    }

    #[test]
    fn clear_frame_matches_the_v3_wire_shape() {
        assert_eq!(
            clear_frame("c1", "d1", Channel::A),
            json!({"type":4,"clientId":"c1","targetId":"d1","channel":"A","message":"clear"})
        );
    }

    #[test]
    fn pulse_frame_matches_the_v3_wire_shape() {
        assert_eq!(
            pulse_frame("c1", "d1", Channel::A, 3, "X:[\"0A0A0A0A0A0A0A0A\"]"),
            json!({"type":"clientMsg","clientId":"c1","targetId":"d1","channel":"A","time":3,"message":"X:[\"0A0A0A0A0A0A0A0A\"]"})
        );
    }
}
