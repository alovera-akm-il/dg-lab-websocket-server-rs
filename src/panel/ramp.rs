//! Strength ramp profiles: a programmatic curve that adjusts a channel's
//! strength over time without the operator sending individual
//! `/api/strength` calls -- the panel runs the schedule internally. Pure
//! data model and step logic only -- no tokio, no I/O -- see
//! [`super::ramp_runner`] for the background task that actually drives
//! one, mirroring [`super::playlist`]/[`super::playlist_runner`]'s split.
//!
//! **Neither wire protocol has a "smooth curve" primitive** -- V3's only
//! strength commands are Inc/Dec/Set-exact, and V4's closest match
//! (`SetTempIntensity`) is a single value that auto-reverts to `0` when
//! its task ends, not a ramp. So every profile here reduces to "the
//! panel computes the intended value on a fixed 1-second tick and sends
//! a `Set`, exactly like a manual click does" -- see `ramp_runner::run`.

use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum RampProfile {
    /// Linearly interpolates from `from` to `to` over `over_seconds`,
    /// clamping at `to` once elapsed time reaches (or exceeds) it.
    Linear {
        from: i64,
        to: i64,
        over_seconds: u32,
    },
    /// Wanders within `base` +/- `variance`, re-rolling a fresh step
    /// every `step_seconds`, for a total of `duration_seconds`.
    RandomWalk {
        base: i64,
        variance: i64,
        step_seconds: u32,
        duration_seconds: u32,
    },
    /// Holds a single value for `duration_seconds` -- functionally a
    /// one-shot Set that stays "active" (shows in `/events`, blocks a
    /// manual override the same way the other profiles do) for a fixed
    /// window instead of ending immediately.
    Hold { value: i64, duration_seconds: u32 },
}

impl RampProfile {
    pub fn as_str(self) -> &'static str {
        match self {
            RampProfile::Linear { .. } => "linear",
            RampProfile::RandomWalk { .. } => "random-walk",
            RampProfile::Hold { .. } => "hold",
        }
    }

    /// Total run length -- `over_seconds` for `Linear`, `duration_seconds`
    /// for the other two. The ramp ends naturally once elapsed time
    /// reaches this, the same way a playlist stops at the end of its
    /// queue.
    pub fn total_seconds(self) -> u32 {
        match self {
            RampProfile::Linear { over_seconds, .. } => over_seconds,
            RampProfile::RandomWalk {
                duration_seconds, ..
            } => duration_seconds,
            RampProfile::Hold {
                duration_seconds, ..
            } => duration_seconds,
        }
    }

    /// The highest value this profile could ever command -- what
    /// `PanelState::ramp_start` checks against the channel's configured
    /// upper limit before starting. `RandomWalk` has no fixed peak (it's
    /// checked per-step instead, by clamping -- see `ramp_runner::run`),
    /// so this is `None` for it.
    pub fn peak_value(self) -> Option<i64> {
        match self {
            RampProfile::Linear { from, to, .. } => Some(from.max(to)),
            RampProfile::Hold { value, .. } => Some(value),
            RampProfile::RandomWalk { .. } => None,
        }
    }

    /// Validates the profile's own numbers -- every duration must be at
    /// least 1 second (a 0-second ramp would end before its first tick
    /// ever ran, silently doing nothing) and `RandomWalk`'s `variance`
    /// must not be negative.
    pub fn validate(self) -> Result<(), &'static str> {
        match self {
            RampProfile::Linear {
                over_seconds: 0, ..
            } => Err("overSeconds must be at least 1"),
            RampProfile::RandomWalk {
                variance,
                step_seconds,
                duration_seconds,
                ..
            } => {
                if variance < 0 {
                    Err("variance must not be negative")
                } else if step_seconds == 0 {
                    Err("stepSeconds must be at least 1")
                } else if duration_seconds == 0 {
                    Err("durationSeconds must be at least 1")
                } else {
                    Ok(())
                }
            }
            RampProfile::Hold {
                duration_seconds: 0,
                ..
            } => Err("durationSeconds must be at least 1"),
            _ => Ok(()),
        }
    }

    /// The value this profile commands at `elapsed` seconds in, for
    /// `Linear`/`Hold` -- a pure function of elapsed time alone.
    /// `RandomWalk` isn't computed here: a step is an actual random
    /// draw with side effects (see `ramp_runner::run`'s own stepping
    /// logic), not a pure function of elapsed time the way the other
    /// two are.
    pub fn value_at(self, elapsed_secs: u32) -> i64 {
        match self {
            RampProfile::Linear {
                from,
                to,
                over_seconds,
            } => {
                let t = elapsed_secs.min(over_seconds) as i64;
                from + (to - from) * t / (over_seconds as i64)
            }
            RampProfile::Hold { value, .. } => value,
            RampProfile::RandomWalk { .. } => {
                unreachable!("RandomWalk is stepped by the runner, not value_at")
            }
        }
    }

    /// A reasonable value to show before the runner's first real tick
    /// has landed (within ~1s of starting) -- `value_at(0)` for
    /// `Linear`/`Hold`, or the un-rolled `base` for `RandomWalk` (same
    /// starting point `RunnerTick::new` uses).
    pub fn initial_value(self) -> i64 {
        match self {
            RampProfile::RandomWalk { base, .. } => base.max(0),
            _ => self.value_at(0),
        }
    }

    /// The "aiming for" value to report in `/events` -- see the module
    /// docs on `RampSnapshot::target`.
    pub fn target(self, rolled: Option<i64>) -> Option<i64> {
        match self {
            RampProfile::Linear { to, .. } => Some(to),
            RampProfile::Hold { value, .. } => Some(value),
            RampProfile::RandomWalk { .. } => rolled,
        }
    }
}

/// One channel's active ramp, as read by `/events` -- see
/// `PanelState::ramp_a`/`ramp_b`. `target`/`current` deliberately have
/// the same value for `Hold`/`RandomWalk` (both settle immediately, or
/// on each re-roll); they diverge meaningfully only for `Linear`, which
/// is the profile they were named for -- see the original feature
/// request's SSE example.
#[derive(Debug, Clone, Copy)]
pub struct RampSnapshot {
    pub profile: RampProfile,
    pub current: i64,
    pub target: Option<i64>,
    pub remaining_secs: u32,
}

/// One tick's worth of runner state -- what changes second to second
/// while a ramp is active, kept out of `RampSnapshot` (a read-only view)
/// and instead owned by the runner task itself, since none of it needs
/// to survive a cancellation the way playlist pause/resume state does --
/// there's no "resume a ramp" concept, only start/stop (see
/// `docs/dg-lab-panel-feature-requests.md`'s Ramp Profiles section).
pub struct RunnerTick {
    pub elapsed_secs: u32,
    pub rolled: i64,
    pub next_reroll_at: u32,
}

impl RunnerTick {
    pub fn new(profile: RampProfile) -> Self {
        let rolled = match profile {
            RampProfile::RandomWalk { base, .. } => base,
            _ => 0,
        };
        RunnerTick {
            elapsed_secs: 0,
            rolled,
            next_reroll_at: 0,
        }
    }

    /// Advances one second and returns the value to command at the new
    /// elapsed time, clamped into `limit` for `RandomWalk` only (see
    /// `RampProfile::peak_value`'s docs on why the other two profiles
    /// are checked upfront instead). `None` once the profile's total
    /// duration has elapsed -- the runner should stop.
    pub fn step(&mut self, profile: RampProfile, limit: Option<i64>) -> Option<i64> {
        if self.elapsed_secs >= profile.total_seconds() {
            return None;
        }
        let value = match profile {
            RampProfile::RandomWalk {
                base,
                variance,
                step_seconds,
                ..
            } => {
                if self.elapsed_secs >= self.next_reroll_at {
                    let delta = if variance > 0 {
                        rand::random_range(-variance..=variance)
                    } else {
                        0
                    };
                    self.rolled = (base + delta).max(0);
                    self.next_reroll_at = self.elapsed_secs + step_seconds.max(1);
                }
                match limit {
                    Some(l) => self.rolled.min(l),
                    None => self.rolled,
                }
            }
            _ => profile.value_at(self.elapsed_secs),
        };
        Some(value)
    }

    pub fn remaining_secs(&self, profile: RampProfile) -> u32 {
        profile.total_seconds().saturating_sub(self.elapsed_secs)
    }

    pub fn tick_duration() -> Duration {
        Duration::from_secs(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn linear_interpolates_and_clamps_at_the_end() {
        let p = RampProfile::Linear {
            from: 10,
            to: 40,
            over_seconds: 30,
        };
        assert_eq!(p.value_at(0), 10);
        assert_eq!(p.value_at(15), 25);
        assert_eq!(p.value_at(30), 40);
        assert_eq!(
            p.value_at(999),
            40,
            "elapsed past over_seconds clamps at `to`"
        );
    }

    #[test]
    fn linear_ramping_down_also_interpolates_correctly() {
        let p = RampProfile::Linear {
            from: 40,
            to: 10,
            over_seconds: 30,
        };
        assert_eq!(p.value_at(0), 40);
        assert_eq!(p.value_at(15), 25);
        assert_eq!(p.value_at(30), 10);
    }

    #[test]
    fn hold_is_constant_for_its_whole_duration() {
        let p = RampProfile::Hold {
            value: 25,
            duration_seconds: 300,
        };
        assert_eq!(p.value_at(0), 25);
        assert_eq!(p.value_at(150), 25);
        assert_eq!(p.value_at(299), 25);
    }

    #[test]
    fn peak_value_covers_both_ramp_directions() {
        assert_eq!(
            RampProfile::Linear {
                from: 10,
                to: 40,
                over_seconds: 1
            }
            .peak_value(),
            Some(40)
        );
        assert_eq!(
            RampProfile::Linear {
                from: 40,
                to: 10,
                over_seconds: 1
            }
            .peak_value(),
            Some(40),
            "a ramp-down still starts at (and briefly commands) the higher value"
        );
        assert_eq!(
            RampProfile::Hold {
                value: 25,
                duration_seconds: 1
            }
            .peak_value(),
            Some(25)
        );
        assert_eq!(
            RampProfile::RandomWalk {
                base: 30,
                variance: 15,
                step_seconds: 1,
                duration_seconds: 1
            }
            .peak_value(),
            None
        );
    }

    #[test]
    fn zero_duration_profiles_are_rejected() {
        assert!(
            RampProfile::Linear {
                from: 10,
                to: 40,
                over_seconds: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            RampProfile::Hold {
                value: 25,
                duration_seconds: 0
            }
            .validate()
            .is_err()
        );
        assert!(
            RampProfile::RandomWalk {
                base: 30,
                variance: 15,
                step_seconds: 0,
                duration_seconds: 10
            }
            .validate()
            .is_err()
        );
        assert!(
            RampProfile::RandomWalk {
                base: 30,
                variance: 15,
                step_seconds: 10,
                duration_seconds: 0
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn negative_variance_is_rejected() {
        assert!(
            RampProfile::RandomWalk {
                base: 30,
                variance: -1,
                step_seconds: 10,
                duration_seconds: 10
            }
            .validate()
            .is_err()
        );
    }

    #[test]
    fn random_walk_stays_within_variance_and_reuses_the_value_between_steps() {
        let p = RampProfile::RandomWalk {
            base: 30,
            variance: 5,
            step_seconds: 3,
            duration_seconds: 9,
        };
        let mut tick = RunnerTick::new(p);
        let first = tick.step(p, None).unwrap();
        assert!((25..=35).contains(&first));
        tick.elapsed_secs += 1;
        let second = tick.step(p, None).unwrap();
        assert_eq!(second, first, "no re-roll before step_seconds elapses");
        tick.elapsed_secs += 1;
        let third = tick.step(p, None).unwrap();
        assert_eq!(third, first, "still within the same 3s step");
        tick.elapsed_secs += 1; // elapsed_secs == 3 -> new step
        let fourth = tick.step(p, None).unwrap();
        assert!((25..=35).contains(&fourth));
    }

    #[test]
    fn random_walk_steps_are_clamped_into_a_configured_limit() {
        let p = RampProfile::RandomWalk {
            base: 100,
            variance: 20,
            step_seconds: 1,
            duration_seconds: 5,
        };
        let mut tick = RunnerTick::new(p);
        for _ in 0..5 {
            let value = tick.step(p, Some(80)).unwrap();
            assert!(value <= 80, "step {value} exceeded the configured limit");
            tick.elapsed_secs += 1;
        }
    }

    #[test]
    fn step_returns_none_once_the_total_duration_elapses() {
        let p = RampProfile::Hold {
            value: 25,
            duration_seconds: 2,
        };
        let mut tick = RunnerTick::new(p);
        assert!(tick.step(p, None).is_some());
        tick.elapsed_secs = 2;
        assert!(tick.step(p, None).is_none());
    }
}
