//! Per-channel pulse playlists: an ordered queue of waveform presets/
//! custom waveforms and silent gaps that plays automatically, optionally
//! shuffled and/or looped, with each entry's own duration either fixed or
//! randomized independently. Pure data model and state-machine logic only
//! -- no tokio, no I/O -- see [`super::playlist_runner`] for the
//! background task that actually drives a queue, and
//! `docs/channel-playlists-implementation.md` for the full design
//! rationale.
//!
//! **On `Phase`:** a silent gap is not a distinct pause state -- it's
//! just an [`EntryKind::Gap`] entry being "played" like any other, so the
//! queue reports [`Phase::Playing`] the whole time it's silently counting
//! down (the runner task is alive and the deadline is ticking). Only an
//! operator-initiated pause (or a fresh/stopped queue) is
//! [`Phase::Paused`]/[`Phase::Stopped`]. Callers distinguish "playing a
//! pulse" from "in a gap" via [`PlaylistQueue::current_kind`], not `Phase`.

use std::collections::HashSet;
use std::time::{Duration, Instant};

use rand::seq::SliceRandom;
use uuid::Uuid;

/// How long a queue entry should run for when it's played.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum DurationSpec {
    Fixed(u32),
    /// Inclusive range in seconds, re-rolled every time the entry plays
    /// (not just once at add-time) -- see [`Self::resolve`].
    Random {
        min: u32,
        max: u32,
    },
}

impl DurationSpec {
    /// Resolves to a concrete duration. Called fresh each time an entry
    /// is about to start playing (see [`PlaylistQueue::enter_at_cursor`]),
    /// never precomputed for the whole queue -- that's what makes a
    /// `Random` entry re-roll on every pass through a looped/shuffled
    /// queue instead of picking one value forever. A backwards range
    /// (`min > max`) resolves to `min`, treating it as effectively fixed
    /// rather than panicking.
    pub fn resolve(self) -> Duration {
        let secs = match self {
            DurationSpec::Fixed(s) => s,
            DurationSpec::Random { min, max } if min < max => rand::random_range(min..=max),
            DurationSpec::Random { min, .. } => min,
        };
        Duration::from_secs(secs.max(1) as u64)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum EntryKind {
    /// A bundled preset id or a raw custom waveform string -- exactly
    /// what `POST /api/pulse`'s `waveform` field already accepts. Kept
    /// unresolved (not yet expanded to actual frame data) until play
    /// time, since which preset table applies can't change but whether
    /// the string is even valid for the currently-active protocol can.
    Pulse { waveform: String },
    /// A silent period: the runner sends one `clear` frame on entry and
    /// then nothing further until the duration elapses.
    Gap,
}

impl EntryKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            EntryKind::Pulse { .. } => "pulse",
            EntryKind::Gap => "gap",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PlaylistEntry {
    pub id: Uuid,
    pub kind: EntryKind,
    pub duration: DurationSpec,
}

impl PlaylistEntry {
    fn new(kind: EntryKind, duration: DurationSpec) -> Self {
        PlaylistEntry {
            id: Uuid::new_v4(),
            kind,
            duration,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Stopped,
    Playing,
    Paused,
}

impl Phase {
    pub fn as_str(self) -> &'static str {
        match self {
            Phase::Stopped => "stopped",
            Phase::Playing => "playing",
            Phase::Paused => "paused",
        }
    }
}

/// What the runner should do right now.
pub enum Step {
    Run {
        entry_id: Uuid,
        kind: EntryKind,
        duration: Duration,
    },
    /// The queue is empty, or playback reached the end with looping off.
    Stop,
}

#[derive(Debug)]
pub enum PlayError {
    /// Nothing to play -- the queue has no entries.
    Empty,
    /// Already playing; not an error the caller needs to act on, but
    /// distinct from `Empty` so the HTTP layer can respond accordingly.
    AlreadyPlaying,
}

/// A cloned, self-contained snapshot of one channel's playlist -- see
/// [`PlaylistQueue::snapshot`].
pub struct PlaylistSnapshot {
    pub entries: Vec<PlaylistEntry>,
    pub shuffle: bool,
    pub loop_playback: bool,
    pub phase: Phase,
    pub current_id: Option<Uuid>,
    pub remaining_ms: Option<u64>,
    /// The *resolved* duration this run of the current entry, distinct
    /// from its `DurationSpec` (which, for `Random`, is a range, not a
    /// concrete value) -- what a progress bar needs as the denominator
    /// alongside `remaining_ms`.
    pub current_duration_ms: Option<u64>,
}

pub struct PlaylistQueue {
    entries: Vec<PlaylistEntry>,
    shuffle: bool,
    loop_playback: bool,
    phase: Phase,
    /// Snapshot of entry ids in play order for the current lap --
    /// (re)computed when playback starts from `Stopped`, or wraps after
    /// the last entry with `loop_playback` on. Storing ids rather than
    /// `entries` indices means removing an unrelated entry mid-playback
    /// can never invalidate this list.
    play_order: Vec<Uuid>,
    /// Position within `play_order`.
    cursor: usize,
    /// The entry currently playing/paused, held as a value snapshot (not
    /// re-looked-up by id) so editing or removing *other* entries never
    /// disturbs what's in flight.
    current: Option<(Uuid, EntryKind, Duration)>,
    /// Wall-clock deadline for `current`'s duration. `None` while paused.
    deadline: Option<Instant>,
    /// Time left on `current` when paused; consumed by the next resume.
    remaining: Option<Duration>,
}

impl Default for PlaylistQueue {
    fn default() -> Self {
        PlaylistQueue {
            entries: Vec::new(),
            shuffle: false,
            loop_playback: false,
            phase: Phase::Stopped,
            play_order: Vec::new(),
            cursor: 0,
            current: None,
            deadline: None,
            remaining: None,
        }
    }
}

impl PlaylistQueue {
    pub fn new() -> Self {
        Self::default()
    }

    // ---- queue editing (always allowed, whatever the phase) ----------

    pub fn entries(&self) -> &[PlaylistEntry] {
        &self.entries
    }

    pub fn add(&mut self, kind: EntryKind, duration: DurationSpec) -> Uuid {
        let entry = PlaylistEntry::new(kind, duration);
        let id = entry.id;
        self.entries.push(entry);
        id
    }

    /// Removes one entry by id. Returns whether it was found. Safe to
    /// call on the currently-playing entry -- see the module docs on why
    /// `current` is a value snapshot, not a live reference.
    pub fn remove(&mut self, id: Uuid) -> bool {
        let before = self.entries.len();
        self.entries.retain(|e| e.id != id);
        self.entries.len() != before
    }

    /// Replaces entry order with `order`, which must name each of the
    /// queue's current entries exactly once. Returns `false` (no change
    /// made) if `order` doesn't match the current id set. Only reorders
    /// `entries` itself -- an in-progress lap's already-computed
    /// `play_order` is untouched, so reordering while playing changes the
    /// upcoming order without disturbing what's currently playing.
    pub fn reorder(&mut self, order: &[Uuid]) -> bool {
        if order.len() != self.entries.len() {
            return false;
        }
        let mut seen = HashSet::with_capacity(order.len());
        if order.iter().any(|id| !seen.insert(*id)) {
            return false; // duplicate id in the request
        }
        let mut reordered = Vec::with_capacity(order.len());
        for id in order {
            match self.entries.iter().position(|e| e.id == *id) {
                Some(pos) => reordered.push(self.entries.remove(pos)),
                None => return false,
            }
        }
        self.entries = reordered;
        true
    }

    pub fn set_settings(&mut self, shuffle: bool, loop_playback: bool) {
        self.shuffle = shuffle;
        self.loop_playback = loop_playback;
    }

    // ---- playback control ---------------------------------------------

    /// Starts playback from a stopped queue, or resumes a paused one
    /// (with whatever time was left on the entry it paused mid-way
    /// through) -- the one place "resumes exactly where it left off"
    /// becomes literal rather than just "resumes the next entry".
    pub fn play(&mut self) -> Result<Step, PlayError> {
        match self.phase {
            Phase::Playing => Err(PlayError::AlreadyPlaying),
            Phase::Paused => {
                let (id, kind, _) = self
                    .current
                    .clone()
                    .expect("Paused always has a captured current entry");
                let remaining = self.remaining.take().unwrap_or(Duration::from_secs(1));
                self.phase = Phase::Playing;
                self.deadline = Some(Instant::now() + remaining);
                self.current = Some((id, kind.clone(), remaining));
                Ok(Step::Run {
                    entry_id: id,
                    kind,
                    duration: remaining,
                })
            }
            Phase::Stopped => {
                if self.entries.is_empty() {
                    return Err(PlayError::Empty);
                }
                self.play_order = self.entries.iter().map(|e| e.id).collect();
                if self.shuffle {
                    self.play_order.shuffle(&mut rand::rng());
                }
                self.cursor = 0;
                self.phase = Phase::Playing;
                Ok(self.enter_at_cursor())
            }
        }
    }

    /// Called by the runner once its current entry's resolved duration
    /// elapses naturally (not via pause/stop). Moves to the next entry,
    /// wrapping (and reshuffling, if `shuffle`) when `loop_playback` is
    /// on, or stopping at the end of the list otherwise.
    pub fn advance(&mut self) -> Step {
        if self.phase != Phase::Playing {
            return Step::Stop;
        }
        self.cursor += 1;
        self.enter_at_cursor()
    }

    /// Captures how much time was left on the current entry and marks
    /// the queue paused. No-op (returns `false`) unless currently
    /// playing -- pausing an already-paused or stopped queue is fine to
    /// call but changes nothing.
    pub fn pause(&mut self) -> bool {
        if self.phase != Phase::Playing {
            return false;
        }
        let now = Instant::now();
        self.remaining = Some(
            self.deadline
                .map(|d| d.saturating_duration_since(now))
                .unwrap_or(Duration::from_secs(1)),
        );
        self.deadline = None;
        self.phase = Phase::Paused;
        true
    }

    /// Resets to the top of the queue. Always succeeds, including when
    /// already stopped -- a safety-relevant action shouldn't fail on a
    /// double-press.
    pub fn stop(&mut self) {
        self.phase = Phase::Stopped;
        self.current = None;
        self.deadline = None;
        self.remaining = None;
        self.play_order.clear();
        self.cursor = 0;
    }

    // ---- snapshot reads --------------------------------------------

    pub fn phase(&self) -> Phase {
        self.phase
    }

    pub fn shuffle_enabled(&self) -> bool {
        self.shuffle
    }

    pub fn loop_playback(&self) -> bool {
        self.loop_playback
    }

    pub fn current_id(&self) -> Option<Uuid> {
        self.current.as_ref().map(|(id, ..)| *id)
    }

    pub fn current_kind(&self) -> Option<&EntryKind> {
        self.current.as_ref().map(|(_, kind, _)| kind)
    }

    /// The resolved duration this run of the current entry -- the
    /// denominator a progress bar needs alongside `remaining_ms`, distinct
    /// from the entry's `DurationSpec` (a `Random` one is a range, not a
    /// concrete value already-played-out duration).
    pub fn current_duration_ms(&self) -> Option<u64> {
        self.current
            .as_ref()
            .map(|(_, _, duration)| duration.as_millis() as u64)
    }

    /// Time left on the current entry, whether actively ticking down
    /// (`Playing`) or frozen (`Paused`). `None` when stopped.
    pub fn remaining_ms(&self) -> Option<u64> {
        match self.phase {
            Phase::Playing => self
                .deadline
                .map(|d| d.saturating_duration_since(Instant::now()).as_millis() as u64),
            Phase::Paused => self.remaining.map(|r| r.as_millis() as u64),
            Phase::Stopped => None,
        }
    }

    /// A cloned, self-contained read of everything the panel's `/events`
    /// SSE snapshot needs to render this queue -- see
    /// [`super::state::PanelState::snapshot`].
    pub fn snapshot(&self) -> PlaylistSnapshot {
        PlaylistSnapshot {
            entries: self.entries.clone(),
            shuffle: self.shuffle,
            loop_playback: self.loop_playback,
            phase: self.phase,
            current_id: self.current_id(),
            remaining_ms: self.remaining_ms(),
            current_duration_ms: self.current_duration_ms(),
        }
    }

    // ---- internal -----------------------------------------------------

    /// Looks up the entry named by `play_order[cursor]`; if it no longer
    /// exists (removed while queued), advances past it and tries again --
    /// wrapping/reshuffling per `loop_playback` exactly like reaching the
    /// natural end would. Resolves a fresh duration for whatever it
    /// lands on. Leaves the queue `Stopped` and returns `Step::Stop` if
    /// nothing playable remains.
    fn enter_at_cursor(&mut self) -> Step {
        loop {
            if self.cursor >= self.play_order.len() {
                if !self.loop_playback || self.entries.is_empty() {
                    self.stop();
                    return Step::Stop;
                }
                self.play_order = self.entries.iter().map(|e| e.id).collect();
                if self.shuffle {
                    self.play_order.shuffle(&mut rand::rng());
                }
                self.cursor = 0;
                continue;
            }

            let id = self.play_order[self.cursor];
            match self.entries.iter().find(|e| e.id == id) {
                Some(entry) => {
                    let duration = entry.duration.resolve();
                    let kind = entry.kind.clone();
                    self.current = Some((id, kind.clone(), duration));
                    self.deadline = Some(Instant::now() + duration);
                    return Step::Run {
                        entry_id: id,
                        kind,
                        duration,
                    };
                }
                None => {
                    self.cursor += 1;
                    continue;
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pulse(waveform: &str, secs: u32) -> (EntryKind, DurationSpec) {
        (
            EntryKind::Pulse {
                waveform: waveform.to_string(),
            },
            DurationSpec::Fixed(secs),
        )
    }

    #[test]
    fn duration_spec_random_stays_within_range() {
        for _ in 0..200 {
            let d = DurationSpec::Random { min: 3, max: 8 }.resolve();
            assert!(d >= Duration::from_secs(3) && d <= Duration::from_secs(8));
        }
    }

    #[test]
    fn duration_spec_backwards_range_resolves_to_min() {
        assert_eq!(
            DurationSpec::Random { min: 8, max: 3 }.resolve(),
            Duration::from_secs(8)
        );
    }

    #[test]
    fn play_on_empty_queue_is_an_error() {
        let mut q = PlaylistQueue::new();
        assert!(matches!(q.play(), Err(PlayError::Empty)));
    }

    #[test]
    fn play_second_time_while_playing_is_already_playing_error() {
        let mut q = PlaylistQueue::new();
        let (kind, dur) = pulse("A", 5);
        q.add(kind, dur);
        assert!(q.play().is_ok());
        assert!(matches!(q.play(), Err(PlayError::AlreadyPlaying)));
    }

    #[test]
    fn plays_entries_in_order_without_loop() {
        let mut q = PlaylistQueue::new();
        let (k1, d1) = pulse("A", 5);
        let id1 = q.add(k1, d1);
        let (k2, d2) = pulse("B", 5);
        let id2 = q.add(k2, d2);

        match q.play().unwrap() {
            Step::Run { entry_id, .. } => assert_eq!(entry_id, id1),
            Step::Stop => panic!("expected Run"),
        }
        assert_eq!(q.phase(), Phase::Playing);

        match q.advance() {
            Step::Run { entry_id, .. } => assert_eq!(entry_id, id2),
            Step::Stop => panic!("expected Run"),
        }

        assert!(matches!(q.advance(), Step::Stop));
        assert_eq!(q.phase(), Phase::Stopped);
        assert_eq!(q.current_id(), None);
    }

    #[test]
    fn loop_playback_wraps_back_to_the_start() {
        let mut q = PlaylistQueue::new();
        let (k1, d1) = pulse("A", 5);
        let id1 = q.add(k1, d1);
        let (k2, d2) = pulse("B", 5);
        let id2 = q.add(k2, d2);
        q.set_settings(false, true);

        assert!(matches!(q.play(), Ok(Step::Run { .. })));
        assert!(matches!(q.advance(), Step::Run { .. })); // -> id2
        match q.advance() {
            // wraps back to id1
            Step::Run { entry_id, .. } => assert_eq!(entry_id, id1),
            Step::Stop => panic!("expected wrap to the first entry"),
        }
        assert_eq!(q.phase(), Phase::Playing);
        let _ = id2;
    }

    #[test]
    fn shuffle_produces_a_permutation_of_every_entry_not_a_fixed_order() {
        let mut q = PlaylistQueue::new();
        let mut ids = Vec::new();
        for i in 0..8 {
            let (k, d) = pulse(&format!("W{i}"), 1);
            ids.push(q.add(k, d));
        }
        q.set_settings(true, false);

        let mut seen = Vec::new();
        let mut step = q.play().unwrap();
        while let Step::Run { entry_id, .. } = step {
            seen.push(entry_id);
            step = q.advance();
        }
        let mut sorted_seen = seen.clone();
        sorted_seen.sort();
        let mut sorted_ids = ids.clone();
        sorted_ids.sort();
        assert_eq!(
            sorted_seen, sorted_ids,
            "shuffle must visit every entry exactly once per lap"
        );
    }

    #[test]
    fn pause_then_play_resumes_the_same_entry_with_less_time_left() {
        let mut q = PlaylistQueue::new();
        let (k, d) = pulse("A", 100);
        let id = q.add(k, d);
        let Step::Run { duration, .. } = q.play().unwrap() else {
            panic!("expected Run");
        };
        assert_eq!(duration, Duration::from_secs(100));

        assert!(q.pause());
        assert_eq!(q.phase(), Phase::Paused);
        assert_eq!(q.current_id(), Some(id)); // still tracked while paused

        match q.play().unwrap() {
            Step::Run {
                entry_id, duration, ..
            } => {
                assert_eq!(entry_id, id);
                assert!(duration <= Duration::from_secs(100));
            }
            Step::Stop => panic!("expected Run"),
        }
        assert_eq!(q.phase(), Phase::Playing);
    }

    #[test]
    fn pause_while_not_playing_is_a_no_op() {
        let mut q = PlaylistQueue::new();
        assert!(!q.pause());
        assert_eq!(q.phase(), Phase::Stopped);
    }

    #[test]
    fn stop_is_idempotent_and_resets_to_the_top() {
        let mut q = PlaylistQueue::new();
        let (k1, d1) = pulse("A", 5);
        q.add(k1, d1);
        let (k2, d2) = pulse("B", 5);
        let id2 = q.add(k2, d2);

        q.play().unwrap();
        q.advance(); // now on id2
        assert_eq!(q.current_id(), Some(id2));

        q.stop();
        assert_eq!(q.phase(), Phase::Stopped);
        assert_eq!(q.current_id(), None);
        q.stop(); // idempotent, no panic
        assert_eq!(q.phase(), Phase::Stopped);

        // A fresh play starts from the top again, not from where it stopped.
        match q.play().unwrap() {
            Step::Run { entry_id, .. } => assert_ne!(entry_id, id2),
            Step::Stop => panic!("expected Run"),
        }
    }

    #[test]
    fn removing_the_currently_playing_entry_does_not_disrupt_it() {
        let mut q = PlaylistQueue::new();
        let (k1, d1) = pulse("A", 5);
        let id1 = q.add(k1, d1);
        let (k2, d2) = pulse("B", 5);
        let id2 = q.add(k2, d2);

        match q.play().unwrap() {
            Step::Run { entry_id, .. } => assert_eq!(entry_id, id1),
            Step::Stop => panic!("expected Run"),
        }

        assert!(q.remove(id1));
        // Still reported as current/playing -- the runner already holds
        // this entry's value and isn't disturbed by the removal.
        assert_eq!(q.current_id(), Some(id1));
        assert_eq!(q.phase(), Phase::Playing);

        // Advancing skips the now-gone id1 and lands on id2.
        match q.advance() {
            Step::Run { entry_id, .. } => assert_eq!(entry_id, id2),
            Step::Stop => panic!("expected Run"),
        }
    }

    #[test]
    fn reorder_changes_upcoming_order_without_touching_current_playback() {
        let mut q = PlaylistQueue::new();
        let (k1, d1) = pulse("A", 5);
        let id1 = q.add(k1, d1);
        let (k2, d2) = pulse("B", 5);
        let id2 = q.add(k2, d2);

        match q.play().unwrap() {
            Step::Run { entry_id, .. } => assert_eq!(entry_id, id1),
            Step::Stop => panic!("expected Run"),
        }

        // Reorder to put id2 first -- doesn't affect the in-progress lap.
        assert!(q.reorder(&[id2, id1]));
        assert_eq!(q.entries()[0].id, id2);
        assert_eq!(q.current_id(), Some(id1));

        // The next `advance` still follows the *lap's* snapshot order,
        // which was captured before the reorder -- id2 next either way
        // here since it was the only other entry, but the entries() Vec
        // itself reflects the new order immediately.
        assert!(matches!(q.advance(), Step::Run { .. }));
    }

    #[test]
    fn reorder_rejects_a_mismatched_id_set() {
        let mut q = PlaylistQueue::new();
        let (k1, d1) = pulse("A", 5);
        let id1 = q.add(k1, d1);
        let (k2, d2) = pulse("B", 5);
        q.add(k2, d2);

        assert!(!q.reorder(&[id1])); // missing one id
        assert!(!q.reorder(&[id1, id1])); // right length, but a duplicate standing in for the missing id
        assert!(!q.reorder(&[id1, id1, id1])); // wrong length + duplicate
        assert!(!q.reorder(&[id1, Uuid::new_v4()])); // unknown id
    }
}
