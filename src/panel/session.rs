//! Session timer with check-in gates: a single, panel-wide timer (not
//! per-channel, unlike playlists/ramps) that fires webhook/log events at
//! configurable checkpoints -- recurring check-ins, one-off labeled
//! phase gates, a fixed 5-minute-before-the-end warning, and the final
//! end. Pure data model and schedule logic only -- no tokio, no I/O --
//! see [`super::session_runner`] for the background task that actually
//! drives it, mirroring [`super::playlist`]/[`super::playlist_runner`]'s
//! split.
//!
//! **Why a precomputed schedule, not a 1-second poll:** every checkpoint
//! (check-in multiples, phase gates, the ending warning, the end) is
//! known in full the instant the timer starts, so the whole run is
//! flattened into one sorted `Vec<Checkpoint>` up front -- the runner
//! then just sleeps exactly to each one in turn (`docs/dg-lab-panel-
//! feature-requests.md`'s Session Timer section), the same "sleep the
//! exact needed duration" discipline `playlist_runner` already uses,
//! rather than waking every second to check nothing happened.
//!
//! **No pause/resume state to "capture and restore" beyond elapsed
//! time:** unlike a playlist entry's `DurationSpec` (which re-rolls a
//! random duration on each play), every checkpoint's `at_seconds` is
//! fixed from the moment the timer starts, so resuming just means
//! continuing to walk the same precomputed schedule from wherever the
//! cursor stopped -- see [`SessionTimer::pause`]/[`SessionTimer::resume`].

use std::time::Instant;

#[derive(Debug, Clone)]
pub struct PhaseGate {
    pub at_seconds: u32,
    pub label: String,
}

/// One entry in the flattened, sorted schedule a running session walks
/// through in order.
#[derive(Debug, Clone)]
pub enum ScheduledEvent {
    CheckIn,
    PhaseGate {
        label: String,
    },
    /// Fixed 5-minutes-before-the-end warning -- only scheduled if
    /// `duration_seconds > 300`, see [`SessionConfig::build_schedule`].
    Ending,
    /// Always the last entry in the schedule, at `duration_seconds`.
    Ended,
}

#[derive(Debug, Clone)]
pub struct Checkpoint {
    pub at_seconds: u32,
    pub event: ScheduledEvent,
}

const ENDING_WARNING_SECONDS: u32 = 300;

#[derive(Debug, Clone)]
pub struct SessionConfig {
    pub duration_seconds: u32,
    /// `0` disables recurring check-ins entirely (rather than being an
    /// error -- a session with only phase gates and no periodic
    /// check-in is a legitimate configuration).
    pub check_in_every_seconds: u32,
    pub phase_gates: Vec<PhaseGate>,
    pub auto_stop_playlists_at_end: bool,
}

impl SessionConfig {
    pub fn validate(&self) -> Result<(), String> {
        if self.duration_seconds == 0 {
            return Err("durationSeconds must be at least 1".to_string());
        }
        for gate in &self.phase_gates {
            if gate.label.trim().is_empty() {
                return Err("phase gate label must not be empty".to_string());
            }
            if gate.at_seconds >= self.duration_seconds {
                return Err(format!(
                    "phase gate \"{}\" atSeconds must be before durationSeconds",
                    gate.label
                ));
            }
        }
        Ok(())
    }

    /// Flattens every checkpoint (phase gates, check-in multiples, the
    /// ending warning, the end) into one list sorted by `at_seconds`.
    /// `Ended` is always present and always last.
    pub fn build_schedule(&self) -> Vec<Checkpoint> {
        let mut checkpoints: Vec<Checkpoint> = self
            .phase_gates
            .iter()
            .map(|g| Checkpoint {
                at_seconds: g.at_seconds,
                event: ScheduledEvent::PhaseGate {
                    label: g.label.clone(),
                },
            })
            .collect();

        if self.check_in_every_seconds > 0 {
            let mut t = self.check_in_every_seconds;
            while t < self.duration_seconds {
                checkpoints.push(Checkpoint {
                    at_seconds: t,
                    event: ScheduledEvent::CheckIn,
                });
                t += self.check_in_every_seconds;
            }
        }

        if self.duration_seconds > ENDING_WARNING_SECONDS {
            checkpoints.push(Checkpoint {
                at_seconds: self.duration_seconds - ENDING_WARNING_SECONDS,
                event: ScheduledEvent::Ending,
            });
        }

        checkpoints.push(Checkpoint {
            at_seconds: self.duration_seconds,
            event: ScheduledEvent::Ended,
        });

        checkpoints.sort_by_key(|c| c.at_seconds);
        checkpoints
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionPhase {
    Stopped,
    Running,
    Paused,
}

impl SessionPhase {
    pub fn as_str(self) -> &'static str {
        match self {
            SessionPhase::Stopped => "stopped",
            SessionPhase::Running => "running",
            SessionPhase::Paused => "paused",
        }
    }
}

/// A read-only view of the timer for `/events` -- see
/// [`SessionTimer::snapshot`].
#[derive(Debug, Clone)]
pub struct SessionSnapshot {
    pub state: SessionPhase,
    pub elapsed_secs: u32,
    pub remaining_secs: u32,
    pub duration_seconds: u32,
    pub check_in_every_seconds: u32,
    pub phase_gates: Vec<PhaseGate>,
    pub next_gate_label: Option<String>,
    pub next_gate_at: Option<u32>,
    pub auto_stop_playlists_at_end: bool,
}

pub struct SessionTimer {
    config: Option<SessionConfig>,
    schedule: Vec<Checkpoint>,
    /// Index into `schedule` of the next checkpoint the runner hasn't
    /// fired yet.
    cursor: usize,
    phase: SessionPhase,
    /// Elapsed seconds accumulated from all *previous* running
    /// intervals -- i.e. as of the last pause, or `0` if never paused.
    elapsed_base: u32,
    /// Wall-clock instant the current running interval began (or
    /// resumed) -- `None` while paused/stopped. Live elapsed while
    /// `Running` is `elapsed_base + deadline.elapsed()`, same pattern
    /// `PlaylistQueue` uses for its own live countdown.
    deadline: Option<Instant>,
}

impl Default for SessionTimer {
    fn default() -> Self {
        SessionTimer {
            config: None,
            schedule: Vec::new(),
            cursor: 0,
            phase: SessionPhase::Stopped,
            elapsed_base: 0,
            deadline: None,
        }
    }
}

impl SessionTimer {
    pub fn new() -> Self {
        Self::default()
    }

    /// Configures and starts a fresh session, replacing any existing
    /// one outright (starting a new one always wins, same as
    /// `PanelState::ramp_start`). Returns the full schedule for the
    /// caller to spawn a runner with.
    pub fn start(&mut self, config: SessionConfig) -> Vec<Checkpoint> {
        let schedule = config.build_schedule();
        self.config = Some(config);
        self.schedule = schedule.clone();
        self.cursor = 0;
        self.phase = SessionPhase::Running;
        self.elapsed_base = 0;
        self.deadline = Some(Instant::now());
        schedule
    }

    /// Captures elapsed time and marks paused. No-op (returns `false`)
    /// unless currently running.
    pub fn pause(&mut self) -> bool {
        if self.phase != SessionPhase::Running {
            return false;
        }
        self.elapsed_base = self.live_elapsed_secs();
        self.deadline = None;
        self.phase = SessionPhase::Paused;
        true
    }

    /// Resumes a paused session. Returns the *remaining* schedule (from
    /// the cursor onward), the elapsed seconds to resume counting from,
    /// and the configured total duration -- everything a fresh runner
    /// needs. `None` if not currently paused.
    pub fn resume(&mut self) -> Option<(Vec<Checkpoint>, u32, u32)> {
        if self.phase != SessionPhase::Paused {
            return None;
        }
        self.phase = SessionPhase::Running;
        self.deadline = Some(Instant::now());
        let total = self.config.as_ref()?.duration_seconds;
        Some((
            self.schedule[self.cursor..].to_vec(),
            self.elapsed_base,
            total,
        ))
    }

    /// Stops the session (explicit early end, or the runner reaching
    /// its final checkpoint) and returns the config that was active
    /// plus the elapsed seconds at the moment of stopping, so the
    /// caller can log an accurate `session.ended` event and honor
    /// `autoStopPlaylistsAtEnd`. `None` if nothing was running.
    pub fn stop(&mut self) -> Option<(SessionConfig, u32)> {
        let elapsed = self.live_elapsed_secs();
        let config = self.config.take()?;
        self.schedule.clear();
        self.cursor = 0;
        self.phase = SessionPhase::Stopped;
        self.elapsed_base = 0;
        self.deadline = None;
        Some((config, elapsed))
    }

    /// Called by the runner after firing the checkpoint at the current
    /// cursor, to move past it -- keeps a subsequent pause/resume
    /// resuming from the right point.
    pub fn advance(&mut self) {
        if self.cursor < self.schedule.len() {
            self.cursor += 1;
        }
    }

    pub fn phase(&self) -> SessionPhase {
        self.phase
    }

    pub fn live_elapsed_secs(&self) -> u32 {
        match (self.phase, self.deadline) {
            (SessionPhase::Running, Some(deadline)) => {
                let live = self.elapsed_base + deadline.elapsed().as_secs() as u32;
                match &self.config {
                    Some(c) => live.min(c.duration_seconds),
                    None => live,
                }
            }
            _ => self.elapsed_base,
        }
    }

    pub fn snapshot(&self) -> Option<SessionSnapshot> {
        let config = self.config.as_ref()?;
        let elapsed = self.live_elapsed_secs();
        let remaining = config.duration_seconds.saturating_sub(elapsed);
        let next_gate = self.schedule[self.cursor..]
            .iter()
            .find_map(|c| match &c.event {
                ScheduledEvent::PhaseGate { label } => Some((c.at_seconds, label.clone())),
                _ => None,
            });
        Some(SessionSnapshot {
            state: self.phase,
            elapsed_secs: elapsed,
            remaining_secs: remaining,
            duration_seconds: config.duration_seconds,
            check_in_every_seconds: config.check_in_every_seconds,
            phase_gates: config.phase_gates.clone(),
            next_gate_at: next_gate.as_ref().map(|(at, _)| *at),
            next_gate_label: next_gate.map(|(_, l)| l),
            auto_stop_playlists_at_end: config.auto_stop_playlists_at_end,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn gate(at: u32, label: &str) -> PhaseGate {
        PhaseGate {
            at_seconds: at,
            label: label.to_string(),
        }
    }

    #[test]
    fn build_schedule_orders_every_checkpoint_kind_and_always_ends_with_ended() {
        let config = SessionConfig {
            duration_seconds: 1200,
            check_in_every_seconds: 300,
            phase_gates: vec![gate(300, "warmup-complete"), gate(900, "midpoint")],
            auto_stop_playlists_at_end: false,
        };
        let schedule = config.build_schedule();
        let ats: Vec<u32> = schedule.iter().map(|c| c.at_seconds).collect();
        assert_eq!(ats, vec![300, 300, 600, 900, 900, 900, 1200]);
        assert!(matches!(
            schedule.last().unwrap().event,
            ScheduledEvent::Ended
        ));
        assert_eq!(schedule.last().unwrap().at_seconds, 1200);
    }

    #[test]
    fn ending_warning_is_only_scheduled_for_sessions_longer_than_five_minutes() {
        let long = SessionConfig {
            duration_seconds: 600,
            check_in_every_seconds: 0,
            phase_gates: vec![],
            auto_stop_playlists_at_end: false,
        };
        assert!(
            long.build_schedule()
                .iter()
                .any(|c| matches!(c.event, ScheduledEvent::Ending))
        );

        let short = SessionConfig {
            duration_seconds: 200,
            check_in_every_seconds: 0,
            phase_gates: vec![],
            auto_stop_playlists_at_end: false,
        };
        assert!(
            !short
                .build_schedule()
                .iter()
                .any(|c| matches!(c.event, ScheduledEvent::Ending))
        );
    }

    #[test]
    fn zero_check_in_interval_disables_recurring_check_ins() {
        let config = SessionConfig {
            duration_seconds: 1200,
            check_in_every_seconds: 0,
            phase_gates: vec![],
            auto_stop_playlists_at_end: false,
        };
        assert!(
            !config
                .build_schedule()
                .iter()
                .any(|c| matches!(c.event, ScheduledEvent::CheckIn))
        );
    }

    #[test]
    fn validate_rejects_a_gate_at_or_past_the_duration() {
        let config = SessionConfig {
            duration_seconds: 600,
            check_in_every_seconds: 0,
            phase_gates: vec![gate(600, "too-late")],
            auto_stop_playlists_at_end: false,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn validate_rejects_zero_duration() {
        let config = SessionConfig {
            duration_seconds: 0,
            check_in_every_seconds: 0,
            phase_gates: vec![],
            auto_stop_playlists_at_end: false,
        };
        assert!(config.validate().is_err());
    }

    #[test]
    fn start_then_stop_reports_the_config_and_elapsed_time() {
        let mut timer = SessionTimer::new();
        assert!(timer.snapshot().is_none());

        let config = SessionConfig {
            duration_seconds: 600,
            check_in_every_seconds: 0,
            phase_gates: vec![],
            auto_stop_playlists_at_end: true,
        };
        timer.start(config);
        assert_eq!(timer.phase(), SessionPhase::Running);
        assert!(timer.snapshot().is_some());

        let (stopped_config, elapsed) = timer.stop().expect("was running");
        assert!(stopped_config.auto_stop_playlists_at_end);
        assert_eq!(elapsed, 0); // stopped immediately, no time elapsed
        assert_eq!(timer.phase(), SessionPhase::Stopped);
        assert!(timer.snapshot().is_none());
    }

    #[test]
    fn pause_then_resume_continues_from_the_same_cursor() {
        let mut timer = SessionTimer::new();
        let config = SessionConfig {
            duration_seconds: 600,
            check_in_every_seconds: 100,
            phase_gates: vec![],
            auto_stop_playlists_at_end: false,
        };
        timer.start(config);
        timer.advance(); // simulate the runner having fired the first check-in

        assert!(timer.pause());
        assert_eq!(timer.phase(), SessionPhase::Paused);

        let (remaining, elapsed, total) = timer.resume().expect("was paused");
        assert_eq!(total, 600);
        assert_eq!(elapsed, 0); // no wall-clock time actually passed in this test
        // The first entry (100s check-in) was already advanced past.
        assert_eq!(remaining.first().unwrap().at_seconds, 200);
        assert_eq!(timer.phase(), SessionPhase::Running);
    }

    #[test]
    fn pause_while_not_running_is_a_no_op() {
        let mut timer = SessionTimer::new();
        assert!(!timer.pause());
    }

    #[test]
    fn next_gate_in_snapshot_skips_check_ins_and_finds_the_first_upcoming_gate() {
        let mut timer = SessionTimer::new();
        let config = SessionConfig {
            duration_seconds: 1200,
            check_in_every_seconds: 100,
            phase_gates: vec![gate(300, "warmup-complete"), gate(900, "midpoint")],
            auto_stop_playlists_at_end: false,
        };
        timer.start(config);
        let snap = timer.snapshot().unwrap();
        assert_eq!(snap.next_gate_at, Some(300));
        assert_eq!(snap.next_gate_label.as_deref(), Some("warmup-complete"));
    }
}
