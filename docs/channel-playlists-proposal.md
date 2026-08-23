# Proposal: per-channel pulse playlists

**Status:** design exploration, not implemented. Archived here for
reference if/when this gets built.

**Mockup:** https://claude.ai/code/artifact/a6ce293a-7b19-4b52-94f7-09a7b5308b8c

## Motivation

Today each channel's Pulse waveform card only supports a single-shot
trigger: pick one preset (or paste custom frames), set a duration, hit
"Trigger preset". Running a longer session means the operator manually
re-triggering the next waveform every time the current one ends. A
playlist lets a channel run an unattended sequence instead.

## Concept

Each channel (A/B) gets a **Single / Playlist** mode toggle next to its
existing controls. In Playlist mode:

- Presets (or custom frames) are added to an ordered, per-channel queue,
  each with its own duration. Queue items are drag-reorderable and
  removable.
- Playback advances automatically when an item's duration elapses, in
  list order or shuffled, optionally looping the whole list.
- **Random duration** — a per-channel toggle with a min–max range that,
  when on, re-rolls the actual play duration for every item each time it
  plays, instead of using the item's set duration.
- **Silent gaps** are their own queue entries (not a blanket per-channel
  setting): a dedicated "+ Add gap" control drops a gap into the queue at
  a chosen position, with its own independent randomize toggle (separate
  from the item one). A gap pauses stimulation for its length, then
  resumes the same playlist position automatically — no re-trigger
  needed.
- "Single" mode reverts to today's one-shot trigger, unchanged.

The mockup shows both states: Channel A mid-playback (random item
duration on, a randomized gap queued further down); Channel B paused
inside a fixed-duration gap, about to resume, with a second fixed gap
queued later.

## Proposed backend approach

Run playback **server-side** rather than sequencing it from the
browser with chained `setTimeout`s:

- A small per-channel task owned by `PanelState`, spawned when a
  playlist starts and cancelled on stop — the same
  spawn/`CancellationToken` pattern already used for the V3/V4 relay
  client tasks and idle timers.
- The task walks the queue (applying shuffle/random-duration/gap
  resolution as it goes), calling the same `strength_frame`/
  `pulse_frame`/`clear_frame` builders `commands.rs`/`v4_commands.rs`
  already expose for `/api/pulse` — no new wire-protocol work, just a
  new caller.
- Playback state (current index, remaining time, playing/paused/in-gap)
  lives in `PanelState` and is pushed over the existing `/events` SSE
  stream like every other piece of panel state, instead of the browser
  owning it.

This keeps playback running if the operator's tab is closed and keeps
every open panel view in sync, consistent with how the rest of the
panel is already server-authoritative — a real gap in the
`setTimeout`-in-the-browser alternative, which loses state the moment
the tab closes and can't be observed from a second viewer.

## Open questions

- Persistence: should a built playlist survive a panel restart, or is
  it in-memory/per-process like the rest of `PanelState`?
- New HTTP surface: likely `POST /api/playlist/{A,B}` to replace the
  queue, `POST /api/playlist/{A,B}/play|pause|stop`, and item
  add/remove/reorder — needs a shape worked out before implementation.
- V4 has no server-side notion of "paused" for a running task; a gap
  would need to be implemented as simply not sending the next
  `device.op` until the gap elapses, rather than anything the relay
  itself understands.
