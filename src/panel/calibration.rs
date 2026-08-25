//! Per-channel intensity calibration -- Feature 9 from
//! `docs/dg-lab-panel-feature-requests.md`. A hardware-zone
//! characteristic (e.g. "perineum at 25 feels like inner thigh at 50"),
//! not session state, so it's persisted like templates/recipes (see
//! [`super::persistence`]) rather than reset on reconnect.
//!
//! **Only applies to absolute-target operations.** Per Mara's
//! clarification, calibration transforms a *logical* target strength
//! into the *raw* wire value the device actually receives:
//! `raw = (logical + offset) * gain`. That's meaningful wherever an
//! absolute target is being set -- `POST /api/strength`'s `op: "set"`,
//! a ramp's per-tick target, `button_map`'s `strength_set` action -- but
//! **not** for relative nudges (`op: "inc"/"dec"`, and
//! `strength_inc`/`_dec`/`_delta`, which add a signed amount to the
//! *raw* current strength). Those already operate directly in wire
//! units with no "logical value" to calibrate in the first place, so
//! they're deliberately left untouched -- calibrating a relative nudge
//! would mean deciding whether `amount` itself is logical or raw, which
//! neither the request nor Mara's answers address, and native Inc/Dec
//! frames only support a wire-level +-1 anyway, giving nothing to
//! calibrate against.
//!
//! The configured upper limit (`POST /api/limit`) is a hard ceiling on
//! *physical* output (Mara's clarification #2), so every call site
//! already checks it against the raw, post-calibration value -- exactly
//! the same check it already did before calibration existed, since raw
//! values are what it always compared against.

use serde::{Deserialize, Serialize};

use super::persistence;

const FILE: &str = "calibration.json";

pub fn load() -> Calibration {
    persistence::load_json(FILE)
}

pub fn save(cal: &Calibration) {
    persistence::save_json(FILE, cal);
}

fn default_gain() -> f64 {
    1.0
}

#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct ChannelCalibration {
    #[serde(default = "default_gain")]
    pub gain: f64,
    #[serde(default)]
    pub offset: f64,
}

impl Default for ChannelCalibration {
    fn default() -> Self {
        ChannelCalibration {
            gain: 1.0,
            offset: 0.0,
        }
    }
}

impl ChannelCalibration {
    /// `gain` in [0.1, 5.0], `offset` in [-50, 50], per Mara's answer.
    ///
    /// **Deliberately does not check the resulting wire range here.**
    /// Mara's answer also says to "reject any combination that would
    /// produce a negative wire value or a value > 200" -- but a gain far
    /// from 1.0 is only unsafe for *some* logical inputs, not all: her
    /// own example (`gain: 2.0`, "ramp both to 40") is perfectly safe in
    /// day-to-day use (40 * 2.0 = 80), even though the same calibration
    /// would overflow past 200 for a logical value above 100. Rejecting
    /// the *config* at save time against the full 0-200 domain would
    /// reject that literal example. Instead, [`apply_checked`] enforces
    /// the wire-range rule per-command, against the actual value being
    /// sent -- see its docs.
    pub fn validate(&self) -> Result<(), String> {
        if !(0.1..=5.0).contains(&self.gain) {
            return Err("gain must be between 0.1 and 5.0".to_string());
        }
        if !(-50.0..=50.0).contains(&self.offset) {
            return Err("offset must be between -50 and 50".to_string());
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize)]
pub struct Calibration {
    #[serde(rename = "channelA", default)]
    pub channel_a: ChannelCalibration,
    #[serde(rename = "channelB", default)]
    pub channel_b: ChannelCalibration,
}

impl Calibration {
    pub fn validate(&self) -> Result<(), String> {
        self.channel_a
            .validate()
            .map_err(|e| format!("channelA: {e}"))?;
        self.channel_b
            .validate()
            .map_err(|e| format!("channelB: {e}"))?;
        Ok(())
    }
}

/// `raw = (logical + offset) * gain`, per Mara's decided order of
/// operations, rounded to the nearest integer wire value.
pub fn apply(cal: ChannelCalibration, logical: i64) -> i64 {
    ((logical as f64 + cal.offset) * cal.gain).round() as i64
}

/// The inverse of [`apply`] -- used only for display (`logicalStrengthA`/
/// `_B` in `/events`), turning the device's actual reported strength
/// back into the logical value it corresponds to. `gain` is validated
/// to always be >= 0.1, so this never divides by zero.
pub fn invert(cal: ChannelCalibration, raw: i64) -> i64 {
    ((raw as f64 / cal.gain) - cal.offset).round() as i64
}

/// [`apply`], plus Mara's clarification #6: reject a result that would
/// send a negative value or one above 200 (the device's own maximum) --
/// checked per-command, against this specific logical value, not at
/// save time against the whole config (see [`ChannelCalibration::validate`]'s
/// docs on why that would reject perfectly safe calibrations).
pub fn apply_checked(cal: ChannelCalibration, logical: i64) -> Result<i64, String> {
    let raw = apply(cal, logical);
    if raw < 0 {
        return Err(format!(
            "would send a negative value ({raw}) after calibration"
        ));
    }
    if raw > 200 {
        return Err(format!(
            "would send {raw} after calibration, above the device's 0-200 range"
        ));
    }
    Ok(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_follows_the_offset_then_gain_order() {
        // Perineum needs a +5 baseline shift, per Mara's own example.
        let cal = ChannelCalibration {
            gain: 1.0,
            offset: 5.0,
        };
        assert_eq!(apply(cal, 25), 30);

        let cal = ChannelCalibration {
            gain: 2.0,
            offset: 0.0,
        };
        assert_eq!(apply(cal, 40), 80);

        let cal = ChannelCalibration {
            gain: 2.0,
            offset: 5.0,
        };
        // (40 + 5) * 2.0 = 90, not 40*2.0 + 5 = 85 -- confirms offset
        // applies before gain, not after.
        assert_eq!(apply(cal, 40), 90);
    }

    #[test]
    fn invert_reverses_apply() {
        let cal = ChannelCalibration {
            gain: 2.0,
            offset: 5.0,
        };
        let raw = apply(cal, 40);
        assert_eq!(invert(cal, raw), 40);
    }

    #[test]
    fn default_calibration_is_a_no_op() {
        let cal = ChannelCalibration::default();
        assert_eq!(apply(cal, 40), 40);
        assert_eq!(invert(cal, 40), 40);
    }

    #[test]
    fn validate_rejects_out_of_range_gain_and_offset() {
        assert!(
            ChannelCalibration {
                gain: 0.05,
                offset: 0.0
            }
            .validate()
            .is_err()
        );
        assert!(
            ChannelCalibration {
                gain: 5.5,
                offset: 0.0
            }
            .validate()
            .is_err()
        );
        assert!(
            ChannelCalibration {
                gain: 1.0,
                offset: -51.0
            }
            .validate()
            .is_err()
        );
        assert!(
            ChannelCalibration {
                gain: 1.0,
                offset: 51.0
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn validate_accepts_a_gain_far_from_one_since_range_is_checked_per_command() {
        // Mara's own example -- would be rejected by a save-time full-
        // domain check, since 200 * 2.0 overflows past 200, even though
        // this calibration is perfectly safe for the values actually
        // used day to day (see `apply_checked_rejects_an_out_of_range_result`).
        assert!(
            ChannelCalibration {
                gain: 2.0,
                offset: 0.0
            }
            .validate()
            .is_ok()
        );
    }

    #[test]
    fn apply_checked_rejects_an_out_of_range_result() {
        let cal = ChannelCalibration {
            gain: 1.0,
            offset: -50.0,
        };
        assert!(apply_checked(cal, 0).is_err(), "0 - 50 is negative");

        let cal = ChannelCalibration {
            gain: 2.0,
            offset: 0.0,
        };
        assert!(apply_checked(cal, 40).is_ok(), "40 * 2.0 = 80, in range");
        assert!(
            apply_checked(cal, 200).is_err(),
            "200 * 2.0 = 400, out of range"
        );
    }

    #[test]
    fn validate_accepts_a_safe_combination() {
        assert!(
            ChannelCalibration {
                gain: 2.0,
                offset: 5.0
            }
            .validate()
            .is_ok()
        );
        assert!(Calibration::default().validate().is_ok());
    }

    #[test]
    fn calibration_json_shape_matches_the_request() {
        let json_str =
            r#"{"channelA": {"gain": 1.0, "offset": 0}, "channelB": {"gain": 2.0, "offset": 5}}"#;
        let cal: Calibration = serde_json::from_str(json_str).unwrap();
        assert_eq!(cal.channel_a.gain, 1.0);
        assert_eq!(cal.channel_b.gain, 2.0);
        assert_eq!(cal.channel_b.offset, 5.0);
    }

    #[test]
    fn omitted_gain_defaults_to_one() {
        let json_str = r#"{"channelA": {"offset": 5}, "channelB": {}}"#;
        let cal: Calibration = serde_json::from_str(json_str).unwrap();
        assert_eq!(cal.channel_a.gain, 1.0);
        assert_eq!(cal.channel_a.offset, 5.0);
        assert_eq!(cal.channel_b.gain, 1.0);
        assert_eq!(cal.channel_b.offset, 0.0);
    }
}
