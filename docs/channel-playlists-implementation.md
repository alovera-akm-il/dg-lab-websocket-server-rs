# Implementation plan: per-channel pulse playlists

**Status:** design research, not implemented. Companion to
[`channel-playlists-proposal.md`](channel-playlists-proposal.md) (the UI
concept and mockup); this document works out the concrete Rust-side design
against the actual current code, so it can be picked up and built directly.

## Summary of the mechanism

Each channel (A/B) gets an ordered `Vec` of queue entries — pulse presets/
custom waveforms and silent gaps, interleaved in any order the operator
wants — plus play/pause/stop, loop, and shuffle. A background task per
channel walks the queue when playing, reusing the exact frame builders
`POST /api/pulse`/`/api/strength`/`/api/clear` already call. Playback state
lives in `PanelState` and rides the existing `/events` SSE broadcast, so it
survives closing the browser tab and stays in sync across every open panel
view — no new transport, no client-side timers driving anything.

## Data model

New module `src/panel/playlist.rs`:

```rust
use uuid::Uuid;
use crate::v3::protocol::Channel;

/// How long a queue entry should run for when it's played.
#[derive(Clone, Copy)]
pub enum DurationSpec {
    Fixed(u32),               // seconds
    Random { min: u32, max: u32 }, // inclusive range, re-rolled every play
}

impl DurationSpec {
    /// Resolves to a concrete duration -- called once, right before an
    /// entry starts playing, never precomputed for the whole queue. This
    /// is what makes "random duration" re-roll on every pass through a
    /// looped/shuffled queue instead of picking one value at add-time.
    pub fn resolve(self) -> std::time::Duration {
        let secs = match self {
            DurationSpec::Fixed(s) => s,
            DurationSpec::Random { min, max } => {
                rand::random_range(min..=max.max(min))
            }
        };
        std::time::Duration::from_secs(secs as u64)
    }
}

pub enum EntryKind {
    /// A bundled preset (`presets::find(id)`) or a raw custom waveform
    /// string, exactly what `POST /api/pulse`'s `waveform` field already
    /// accepts -- the queue stores the same string, resolved the same way
    /// `post_pulse` already resolves it.
    Pulse { waveform: String },
    /// A silent period: no frame is sent while this entry is "playing"
    /// (see the runner below for why a `clear` frame *is* sent, once, on
    /// entry).
    Gap,
}

pub struct PlaylistEntry {
    pub id: Uuid,
    pub kind: EntryKind,
    pub duration: DurationSpec,
}
```

Note what's *not* here: there is no separate global "random duration
enabled" or "silent gap enabled" flag. Each entry carries its own
`DurationSpec` and its own `kind` independently -- that's what makes "gaps
can be randomized independently of items, and added independently, at any
position" fall out for free instead of needing special-cased interaction
between two global toggles. The mockup's "Random duration" / "randomize gap
duration" toggles next to each "+ Add" / "+ Add gap" button are purely a UI
convenience that decides what `DurationSpec` gets attached to the *next*
entry added -- they don't need any server-side representation beyond that.

## Playback state

```rust
pub enum Phase {
    Stopped,
    Playing,
    /// Manually paused, or automatically inside a `Gap` entry -- both
    /// cases pause the same way (see the runner below); the mockup's
    /// "Pause button still shows Pause during a gap" reflects that a gap
    /// is just an automatic phase of an active playlist, not a distinct
    /// user-facing pause state.
    Paused,
}

pub struct PlaylistQueue {
    pub entries: Vec<PlaylistEntry>,
    pub shuffle: bool,
    pub loop_playback: bool,
    pub phase: Phase,
    /// Index into `entries` (in original, un-shuffled order) of what's
    /// currently playing/paused. `None` when stopped or the queue is empty.
    pub current: Option<usize>,
    /// Wall-clock deadline for the current entry, so the SSE snapshot can
    /// report a countdown -- `None` while paused (see the runner).
    pub deadline: Option<std::time::Instant>,
    /// Set on pause (manual or a gap); consumed by the next spawn to
    /// resume mid-entry instead of restarting it.
    pub remaining: Option<std::time::Duration>,
    /// When `shuffle` is on, the resolved play order for the current lap
    /// -- recomputed every time playback (re)starts or loops back to the
    /// top, so repeats aren't predictable across laps.
    pub shuffled_order: Option<Vec<usize>>,
}
```

`PanelState` gains `playlist_a: Mutex<PlaylistQueue>` and `playlist_b: ...`
(matching the existing `strength_a`/`strength_b`, `limit_a`/`limit_b`
per-channel-field convention already used throughout `state.rs`, rather than
a `HashMap<Channel, _>`), plus a `CancellationToken` per channel
(`playlist_token_a`/`_b`, same pattern as the existing `reconnect`/
`v4_reconnect` tokens) so Pause/Stop can interrupt a running task the same
way `request_reconnect()` already interrupts a relay connection.

## Refactor needed first

`handler.rs::active_target_and_outbound` (the helper every command endpoint
calls to resolve "which leg is active, and its live outbound sender") is
currently a free function taking the full `AppState`. The playlist runner
is a background task spawned from a `Play` request, not itself inside a
request handler, and only has (and only needs) `Arc<PanelState>`. Move that
helper onto `PanelState` itself:

```rust
impl PanelState {
    pub fn active_target_and_outbound(&self)
        -> Result<(ActiveTarget, mpsc::UnboundedSender<WsMessage>), (StatusCode, &'static str)>
    { ... } // body unchanged, just `self.` instead of `state.panel.`
}
```

`handler.rs`'s call sites become `state.panel.active_target_and_outbound()`.
Everything else the runner needs (`commands::pulse_frame`/`strength_frame`,
`v4_commands::pulse_frame`/`clear_frame`, `presets::find`) are already free
functions taking plain ids -- no further refactor needed there.

## The playback task

Spawned by `POST /api/playlist/{channel}/play`, one per channel, guarded by
that channel's `CancellationToken` (fresh token per spawn, same as the relay
reconnect tokens). Sketch:

```rust
async fn run(panel: Arc<PanelState>, channel: Channel, token: CancellationToken) {
    loop {
        let Some((entry_id, kind, deadline_dur)) = panel.playlist_advance_or_resume(channel) else {
            panel.playlist_set_stopped(channel); // queue empty or loop-off + end reached
            return;
        };

        match &kind {
            EntryKind::Pulse { waveform } => {
                if let Ok((target, tx)) = panel.active_target_and_outbound() {
                    send_pulse_via(&target, &tx, channel, waveform, deadline_dur);
                    // errors (no device paired, relay down) are logged and
                    // the entry is skipped at its resolved duration anyway
                    // -- see "active leg changes mid-playback" below.
                }
            }
            EntryKind::Gap => {
                // A gap must actually silence the channel, not just
                // withhold new frames -- otherwise a previous pulse item's
                // last queued frames keep the device buzzing into what's
                // supposed to be quiet. Reuses the exact `clear` frame
                // `POST /api/clear` already sends.
                if let Ok((target, tx)) = panel.active_target_and_outbound() {
                    send_clear_via(&target, &tx, channel);
                }
            }
        }

        panel.playlist_set_deadline(channel, deadline_dur);
        tokio::select! {
            _ = tokio::time::sleep_until(deadline_dur.into()) => {
                panel.playlist_advance_index(channel); // move to the next entry, or wrap/stop
            }
            _ = token.cancelled() => {
                panel.playlist_capture_remaining(channel); // pause: freeze position + time-left
                return;
            }
        }
    }
}
```

`playlist_advance_or_resume` is the one piece of real logic: on a fresh
`Play` from `Stopped`, it picks the first entry (or the first of a freshly
computed `shuffled_order`) and resolves a brand-new `DurationSpec`; on
`Play` after a `Pause` (manual or post-gap), it returns the *same* entry
with the previously-captured `remaining` duration instead of re-resolving
it -- this is what makes "resumes from where it stopped" literal rather
than just "resumes the next entry". `Stop` (distinct from `Pause`) clears
`current`/`remaining` entirely, so the next `Play` starts from the top of
the queue again.

**Why "pause" and "in a gap" share one mechanism:** a gap isn't a special
runner state, it's just an `EntryKind::Gap` entry being "played" — the
`Phase` reported to the UI is `Paused` either way (with a `reason` the SSE
payload can distinguish for the "manually paused" vs. "resumes in Ns"
copy the mockup shows), and the button that stops the whole engine still
reads "Pause" during a gap because the engine genuinely is still advancing
its own internal clock, just not emitting anything.

## HTTP API surface

All under `/api/playlist/{channel}` where `{channel}` is `a`/`b` (parsed with
the existing `commands::parse_channel`):

| Method + path | Body | Effect |
| --- | --- | --- |
| `POST /api/playlist/{channel}/items` | `{"kind":"pulse","waveform":"...", "duration":{"mode":"fixed","seconds":5}}` or `{"kind":"pulse",...,"duration":{"mode":"random","min":3,"max":8}}` or `{"kind":"gap","duration":{...}}` | Appends one entry, server-assigns its `id` (UUID) |
| `DELETE /api/playlist/{channel}/items/{id}` | — | Removes one entry by id (stable across reorders, unlike an index) |
| `POST /api/playlist/{channel}/reorder` | `{"order":["id1","id2",...]}` | Full replacement of entry order; rejected (`400`) if the id set doesn't match exactly |
| `POST /api/playlist/{channel}/settings` | `{"shuffle": bool, "loopPlayback": bool}` | Updates the two queue-level flags |
| `POST /api/playlist/{channel}/play` | — | Starts (or resumes) the runner |
| `POST /api/playlist/{channel}/pause` | — | Cancels the token; runner captures remaining time and exits |
| `POST /api/playlist/{channel}/stop` | — | Cancels the token *and* resets position to the top of the queue |

Same status-code conventions the existing endpoints use: `400` malformed
body/unknown channel, `409` nothing to play (empty queue) or already in the
requested phase, reusing `active_target_and_outbound`'s existing `409`/`503`
implicitly the first time the runner actually needs to send.

`waveform` on a `pulse` entry accepts the same two shapes `post_pulse`
already does -- a preset id (resolved via `presets::find`) or a raw custom
waveform string -- so entry creation validates it the same way `post_pulse`
does today (empty check; V4 additionally requires the `"<prefix>:[...]"`
frame-array shape, checked lazily at play time exactly like `post_pulse`
checks it today, since which protocol is active can change between adding
an entry and playing it).

## SSE snapshot additions

Extend `Snapshot`/`snapshot_json` with, per channel:

```json
"playlistA": {
  "entries": [{"id":"...", "kind":"pulse", "label":"Climb", "duration":{"mode":"fixed","seconds":6}}, ...],
  "shuffle": false,
  "loopPlayback": true,
  "phase": "playing",
  "currentId": "...",
  "remainingMs": 3120,
  "totalEntries": 4
}
```

`label` for a `pulse` entry is the preset's `label_en` if it resolved from
`presets::find`, else a truncated preview of the custom waveform string --
computed at snapshot time, not stored, so it never goes stale if presets
ever change. `remainingMs` is derived from `deadline` each time a snapshot
is rendered (not stored as a static number), the same way the rest of the
panel already treats `Snapshot` as a fresh read of `Inner` on every SSE
push via `notify_changed()`.

## Front-end wiring

Maps directly onto the mockup's elements in `src/panel/assets/`:

- `.mode-toggle` (Single/Playlist) — pure UI state, no new endpoint; when
  in "Single" the existing preset-trigger/custom-waveform controls show, as
  today.
- `.preset-picker` + duration input + dice toggle + "+ Add" — `POST
  .../items` with `kind:"pulse"`.
- The mirrored gap controls + "+ Add gap" — `POST .../items` with
  `kind:"gap"`.
- `.playlist-queue` list — rendered from `playlistA/B.entries` on every SSE
  push, same pattern `render(state)` already uses for the rest of the page
  in `app.js`; drag-reorder needs a minimal HTML5 drag-and-drop handler
  (`dragstart`/`dragover`/`drop`) that on drop POSTs the full new `order`
  array -- there's no existing drag interaction in `app.js` to reuse, this
  is genuinely new.
- Remove (`.item-remove`) — `DELETE .../items/{id}`.
- Play/Pause/Stop/Shuffle/Loop buttons — the four playback endpoints plus
  `.../settings`; button state (which icon shows, `.active` classes)
  derived from `phase`/`shuffle`/`loopPlayback` in the snapshot, exactly
  like every other control on the page already reflects server state
  rather than tracking its own.
- `.playing-progress`/`.gap-progress-fill` width — `1 -
  remainingMs/entry.duration_seconds*1000`, recomputed on each SSE push;
  since pushes only happen on state *change* (not a timer), the fill won't
  animate smoothly between pushes -- worth a client-side
  `requestAnimationFrame` interpolation between the last two known
  `remainingMs` readings if smooth motion matters, otherwise it'll visibly
  jump each time something else changes elsewhere on the page. Flagged as
  a follow-up polish item, not blocking.

## Edge cases

- **Active leg changes mid-playback** (device disconnects, the other
  protocol's device takes over): `active_target_and_outbound()` starts
  failing or returns a different `ActiveTarget`. The runner doesn't
  special-case this -- it just logs the send failure via
  `panel.log(...)` and continues its own clock, so playback effectively
  free-runs (advancing entries, doing nothing) until a device is active
  again, rather than crashing or silently wedging. Worth deciding whether
  this should auto-pause instead once actually built; noted as an open
  question below rather than decided here.
- **Removing the currently-playing entry**: the runner holds an entry
  *value*, not a live reference into the `Vec`, so an in-flight play isn't
  disrupted; the next `playlist_advance_index` simply won't find that id
  in the (now-shorter) list and moves on from its original position,
  clamped to the new length.
- **Reordering while playing**: changes the *upcoming* order only; the
  currently-playing entry keeps playing to its resolved deadline
  regardless of where it moved to.
- **`Play` on an empty queue**: `409`, no task spawned.
- **V4 has no native "paused" task state**: a gap (or a manual pause) is
  implemented purely as "the runner sends nothing," not anything the V4
  relay or `dglab-kit`'s RPC schema understands -- there's no `device.op`
  pause/resume call to make. This matches how `EntryKind::Gap` is already
  designed above (send one `clear`, then just wait).

## Testing plan

- **Unit tests in `playlist.rs`** (mirroring `v4_commands.rs`'s existing
  test style): `DurationSpec::resolve` stays within `[min, max]` over many
  samples; shuffle produces a permutation of the same index set, not a
  fixed one; advance/wrap/stop transitions at queue boundaries with/without
  `loop_playback`; pause-then-resume returns the same entry id with a
  shorter resolved duration than the original.
- **Integration test** (new `tests/panel_playlist_integration.rs`, same
  scaffolding as `tests/panel_v4_integration.rs`): build a 2-3 entry queue
  with short fixed durations (tens of milliseconds, not real seconds, to
  keep the test fast — same tolerance-based `recv_until`/`timeout` pattern
  already used elsewhere in that test file), hit `play`, and assert the
  simulated device receives frames in the expected order within a timing
  tolerance; a separate case covers a `Gap` entry actually producing a
  `device.op.clear`/V3 `clear-<n>` frame partway through.

## Open questions (carried over, plus new ones from this pass)

- Should losing the active device auto-pause the playlist instead of
  free-running silently until one returns (see "active leg changes
  mid-playback" above)?
- Persistence: in-memory only (lost on restart/panel process restart), like
  the rest of `PanelState`, or should queues be written to disk? Leaning
  in-memory-only for a first version, consistent with everything else here.
- Should `remainingMs`/progress bars interpolate client-side between SSE
  pushes for smoothness, or is a stepwise update acceptable? (see "Front-end
  wiring" above)
- Suggested build order if this moves forward: (1) `playlist.rs` data model
  - unit tests, no HTTP surface yet; (2) `PanelState` integration + the
  `active_target_and_outbound` refactor; (3) the runner task + play/pause/
  stop endpoints, tested via the integration test above; (4) items/reorder/
  settings endpoints; (5) front-end.
