//! Builds the exact V4 `device.op`/`device.op.clear` wire frames a real
//! V4 controller would send, per `dglab-kit`'s documented RPC schema --
//! not reverse-engineered: `dglab-kit` is the official SDK for the
//! DG-LAB 4 APP, and its README documents this schema directly for
//! cross-language implementers. See `docs/api.md` for the full
//! reference.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{json, Value};

use crate::v3::protocol::Channel;
use crate::v3::pulse::parse_pulse_message;

pub use super::commands::StrengthOp;

fn v4_channel(channel: Channel) -> u8 {
    match channel {
        Channel::A => 0,
        Channel::B => 1,
    }
}

/// Monotonic per-process `reqId` source. The panel never correlates
/// `device.op` responses back to a request (that RPC only resolves once
/// a task completes/is cleared/replaced/cancelled -- not on enqueue, so
/// there's nothing useful for a fire-and-forget command sender to wait
/// on), so all a `reqId` needs to do here is avoid ever repeating for the
/// same APP connection, which a process-wide counter guarantees with
/// room to spare.
fn next_request_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    COUNTER.fetch_add(1, Ordering::Relaxed).to_string()
}

fn envelope(device_id: &str, method: &str, data: Value) -> Value {
    json!({
        "type": "message",
        "clientId": device_id,
        "data": {"t": "req", "reqId": next_request_id(), "m": method, "data": data},
    })
}

/// Increase/decrease build an `AddIntensity` (`t:3`) task with a signed
/// delta. V4 has no action for setting an arbitrary absolute value --
/// `SetIntensity` (`t:7`) only ever resets to `0` -- so `Set(v)` is
/// implemented as an `AddIntensity` delta computed from `current`, the
/// best currently-known strength for this channel. Returns `None` for
/// `Set` if `current` is unknown (nothing to compute a delta from); the
/// caller should surface that as "current V4 strength unknown yet" rather
/// than silently sending a wrong delta.
pub fn strength_frame(
    device_id: &str,
    slot_id: &str,
    channel: Channel,
    op: StrengthOp,
    current: Option<i64>,
) -> Option<Value> {
    let delta = match op {
        StrengthOp::Inc => 1,
        StrengthOp::Dec => -1,
        StrengthOp::Set(target) => target - current?,
    };
    Some(envelope(
        device_id,
        "device.op",
        json!({"s": slot_id, "t": 3, "c": v4_channel(channel), "p": 1, "v": delta}),
    ))
}

pub fn clear_frame(device_id: &str, slot_id: &str, channel: Channel) -> Value {
    envelope(device_id, "device.op.clear", json!({"s": slot_id, "c": v4_channel(channel)}))
}

/// Builds an `AppendPulseData` (`t:0`) task from the same waveform text
/// format the panel's presets/custom field already use for V3
/// (`"<prefix>:[\"<16-hex-char frame>\",...]"`), reusing
/// `v3::pulse::parse_pulse_message` to extract just the hex frame list --
/// exactly what V4's `ver:3` frame format expects as `v`. Returns `None`
/// if `waveform` isn't in that shape: V4 has no raw-passthrough fallback
/// the way V3 does, so there's nothing sensible to send.
pub fn pulse_frame(device_id: &str, slot_id: &str, channel: Channel, duration_ms: i64, waveform: &str) -> Option<Value> {
    let frames = parse_pulse_message(waveform)?;
    Some(envelope(
        device_id,
        "device.op",
        json!({"s": slot_id, "t": 0, "c": v4_channel(channel), "p": 1, "d": duration_ms, "v": frames}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn inner_data(frame: &Value) -> &Value {
        &frame["data"]["data"]
    }

    #[test]
    fn envelope_shape_is_a_v4_message_frame_wrapping_a_req() {
        let frame = clear_frame("d1", "s1", Channel::A);
        assert_eq!(frame["type"], "message");
        assert_eq!(frame["clientId"], "d1");
        assert_eq!(frame["data"]["t"], "req");
        assert_eq!(frame["data"]["m"], "device.op.clear");
        assert!(frame["data"]["reqId"].is_string());
    }

    #[test]
    fn inc_dec_build_add_intensity_with_signed_delta() {
        let inc = strength_frame("d1", "s1", Channel::A, StrengthOp::Inc, None).unwrap();
        assert_eq!(inner_data(&inc), &json!({"s": "s1", "t": 3, "c": 0, "p": 1, "v": 1}));

        let dec = strength_frame("d1", "s1", Channel::B, StrengthOp::Dec, None).unwrap();
        assert_eq!(inner_data(&dec), &json!({"s": "s1", "t": 3, "c": 1, "p": 1, "v": -1}));
    }

    #[test]
    fn set_computes_delta_from_current_known_strength() {
        let frame = strength_frame("d1", "s1", Channel::A, StrengthOp::Set(30), Some(12)).unwrap();
        assert_eq!(inner_data(&frame)["v"], 18);

        // Target below current -> negative delta.
        let frame = strength_frame("d1", "s1", Channel::A, StrengthOp::Set(5), Some(12)).unwrap();
        assert_eq!(inner_data(&frame)["v"], -7);
    }

    #[test]
    fn set_without_a_known_current_strength_is_rejected() {
        assert!(strength_frame("d1", "s1", Channel::A, StrengthOp::Set(30), None).is_none());
    }

    #[test]
    fn clear_frame_targets_slot_and_channel() {
        let frame = clear_frame("d1", "s1", Channel::B);
        assert_eq!(inner_data(&frame), &json!({"s": "s1", "c": 1}));
    }

    #[test]
    fn pulse_frame_extracts_hex_frames_from_the_shared_waveform_format() {
        let frame = pulse_frame("d1", "s1", Channel::A, 3000, r#"A:["0A0A0A0A0A0A0A0A","0B0B0B0B0B0B0B0B"]"#).unwrap();
        assert_eq!(
            inner_data(&frame),
            &json!({"s": "s1", "t": 0, "c": 0, "p": 1, "d": 3000, "v": ["0A0A0A0A0A0A0A0A", "0B0B0B0B0B0B0B0B"]})
        );
    }

    #[test]
    fn pulse_frame_rejects_raw_legacy_strings() {
        assert!(pulse_frame("d1", "s1", Channel::A, 3000, "legacywave").is_none());
    }

    #[test]
    fn request_ids_never_repeat() {
        let a = next_request_id();
        let b = next_request_id();
        assert_ne!(a, b);
    }
}
