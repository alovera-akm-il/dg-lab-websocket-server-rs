# DG-LAB Panel Feature Requests

## Implementation research

Findings below are checked directly against the current code
(`src/panel/*.rs`, `docs/api.md`, `docs/architecture.md`), not just the
request text — each feature has an "Implementation notes" subsection with
what it hooks into, what's genuinely new work, and where the request's
spec runs into something the real wire protocol or the current
architecture doesn't support as written. A mockup of every proposed UI
addition, laid directly into the real panel page (dashed/badged cards
mark what's new; solid ones are shipped as-is today), is at
<https://claude.ai/code/artifact/ba9d3adc-941f-4fb2-9a94-f819270bfaea>.
**Historical, not current:** all six features have since shipped, and a
couple of details (notably Recipes — no live "save current session as…"
capture, no button-map switching; see its "Status: implemented" note
below) diverged from that mockup during implementation. Left as-is as a
record of the original proposal, not updated to match the final build.

**One thing all six share:** nothing in the panel today survives a process
restart except `PANEL_WEBHOOK_URL`'s *initial* value (re-read from the
environment at startup) — playlists, limits, and the runtime webhook URL
all live in `PanelState`'s in-memory `Mutex<Inner>` and reset to empty the
moment the process restarts. That's fine for state that's naturally
session-scoped (a playlist you built for tonight), but three of these
requests (Templates, Button Mapping, Recipes) are explicitly things you
build once and expect to still be there next week — "stored server-side"
in the request text implies surviving a restart, not just surviving a
relay reconnect. There's no persistence layer to reuse for that yet, so
all three need one. Rather than three bespoke ad hoc file-IO
implementations, the recommendation below is a single small
`src/panel/persistence.rs` (read/write one JSON file per store, atomic
via write-to-tmp-then-rename, directory from a new `PANEL_DATA_DIR` env
var — same shape as `LOG_DIR`) that Templates, Button Mapping, and
Recipes all sit on top of. **Open question for you:** is a JSON file on
disk the right model, or would you rather these lived somewhere else
(SQLite, a directory the Python client also writes into directly)? The
notes below assume the JSON file approach since it's the smallest addition
consistent with how this codebase already handles config
(`LOG_DIR`/`PANEL_WEBHOOK_URL`/etc. are all "one env var, one plain
value"), but it's worth confirming before building.

**Recommended build order:** 1 (Templates) → 2 (Ramps) → 3 (Session
Timer) → 5 (Button Mapping, `pattern` only — see its notes) → 6
(Recipes, which is pure composition over 1/2/3/5 and has nothing to build
until they exist) → 4 (File-Based Event Log) can slot in anywhere, it's
independent of the other five.

---

## 1. Named Playlist Templates

**Status: implemented.** `src/panel/templates.rs` + `src/panel/persistence.rs`;
endpoints and JSON shapes below match what shipped, including the
query-params-to-JSON-body revision — see `docs/api.md`'s "Templates"
section for the final reference.

Saveable, reusable playlist definitions. Define a playlist once, give it a name, then load it into either channel with one call.

### API

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/templates` | List all template names |
| `GET` | `/api/templates/{name}` | Get one template by name |
| `POST` | `/api/templates/{name}` | Save current channel's queue as template (body: `{sourceChannel: "A"\|"B"}`) |
| `DELETE` | `/api/templates/{name}` | Remove a template |
| `POST` | `/api/playlist/{channel}/load-template` | Replace channel queue with template (body: `{name, shuffle}`) |

### `POST /api/playlist/{channel}/load-template` request body

Revised from the original query-param sketch (`?name={name}&shuffle={bool}`)
to a JSON body, for consistency with every other panel endpoint that takes
input — see the Implementation notes below.

```json
{"name": "Mara's Edge Test", "shuffle": false}
```

`name` must match an existing template (`404` otherwise); `shuffle` sets
the loaded queue's shuffle flag going forward, independent of whatever the
template itself was saved with. `loopPlayback` is *not* here — it comes in
with the template (see `settings` below) rather than being overridable
per-load, since there's no stated use case for wanting a different loop
setting than the one the template was saved with, the way there is for
shuffle (e.g. "load this fixed sequence, but shuffle differently tonight").

### Template JSON (stored server-side)

```json
{
  "name": "Mara's Edge Test",
  "items": [
    {"kind":"pulse","waveform":"coyote-extrusion","duration":{"mode":"fixed","seconds":20}},
    {"kind":"gap","duration":{"mode":"random","min":8,"max":15}},
    {"kind":"pulse","waveform":"coyote-climb","duration":{"mode":"fixed","seconds":30}}
  ],
  "settings": {"shuffle":false,"loopPlayback":true}
}
```

### Use Case
Build torture recipes in Python, save them as templates, then load into A and B before a session. No rebuilding queues every time.

### Implementation notes

Straightforward — this is the smallest of the six, and mostly wiring
existing pieces together. A template is exactly a `Vec<PlaylistEntry>` plus
the two settings fields, which `src/panel/playlist.rs` already models; the
JSON shape in the request is already what `handler.rs`'s
`playlist_entry_json`/`playlist_json` produce today, field for field.

- `POST /api/templates/{name}` reads the source channel's *current* queue
  (`PlaylistQueue::entries()`, already exposed) and writes it into the new
  template store — no new playlist-side code, just a snapshot-and-save.
- `POST /api/playlist/{channel}/load-template` needs one new
  `PlaylistQueue` method, something like `load(entries, shuffle,
  loop_playback)`, that replaces the queue wholesale. It should stop
  playback first if the channel is currently playing — swapping entry ids
  out from under a live runner task would leave `current_id` pointing at
  an entry that no longer exists. Reuse `stop()`'s reset for this rather
  than inventing new state.
- **Decided:** `load-template` takes a JSON body, `{"name": "...",
  "shuffle": false}`, not the original query-param sketch. Query params
  would've been a deviation from every other panel endpoint, which take a
  JSON body even for one field (`/api/reconnect` is the only other
  no-body endpoint, and it takes *no* params at all) — see the request
  body section above.
- Needs the shared file-persistence module (see above) — a
  `HashMap<String, Template>` loaded at startup and rewritten on every
  save/delete.

**UI:** yes — see the mockup's Pulse Waveform card. A "Templates" row sits
directly above each channel's queue: a name picker (reusing the existing
`.preset-picker` dropdown component, same visual language as the waveform
preset picker right above it) plus **Load** and **Save current as…**
buttons.

---

## 2. Strength Ramp Profiles

**Status: implemented.** `src/panel/ramp.rs` + `src/panel/ramp_runner.rs`;
the design below (1-second tick, peak-value limit check, per-step
random-walk clamping, no pause/resume) matches what shipped — see
`docs/api.md`'s "Ramps" section for the final endpoint/SSE reference.

Programmatic curves that adjust strength over time without sending individual `/api/strength` calls. The panel runs the schedule internally.

### API

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/ramp` | Start a ramp profile on a channel |
| `POST` | `/api/ramp/stop` | Stop active ramp on a channel |

### Request Body Examples

**Linear ramp:**
```json
{
  "channel": "A",
  "profile": "linear",
  "from": 10,
  "to": 40,
  "overSeconds": 600
}
```

**Random walk:**
```json
{
  "channel": "B",
  "profile": "random-walk",
  "base": 30,
  "variance": 15,
  "stepSeconds": 30,
  "durationSeconds": 600
}
```

**Hold steady:**
```json
{
  "channel": "A",
  "profile": "hold",
  "value": 25,
  "durationSeconds": 300
}
```

### SSE Addition

`rampA` / `rampB` objects in `/events`:
```json
{
  "target": 40,
  "current": 23,
  "remainingSeconds": 340,
  "profile": "linear"
}
```

### Override
Sending any manual `/api/strength` command cancels the active ramp on that channel.

### Use Case
Start a session with "ramp both channels from 10 to 35 over 15 minutes" and the panel handles gradual escalation while focusing on playlist timing.

### Implementation notes

This reuses the exact architecture the playlist feature already
established — a background `tokio::spawn`ed task per channel, driven by a
`CancellationToken` the HTTP handlers cancel to stop/override it, mutating
`PanelState` and calling `notify_changed()` (`playlist_runner.rs` is a
near-literal template for a `ramp_runner.rs`). No new wire-protocol code
either: the ramp runner just calls `commands::strength_frame`/
`v4_commands::strength_frame` — the exact functions `POST /api/strength`
already calls — on a timer instead of a button click.

- **Neither protocol has a "smooth curve" primitive.** V3's only strength
  commands are Inc/Dec/Set-exact (`docs/api.md`'s "Strength control"
  section); V4's closest match, `SetTempIntensity`, is a single value that
  auto-reverts to `0` when its task ends — not a ramp. So "ramping" here
  necessarily means: the panel computes the intended value on a fixed tick
  (recommend every 1s) and sends a `Set` the same way a manual click does,
  same as the request's own examples imply (`overSeconds`/`stepSeconds`
  are already framed as discrete steps, not continuous). Worth confirming
  this matches what you had in mind, since "ramp" can also imply something
  smoother than a value updating once a second.
- **Interaction with the existing upper limit** (`/api/limit`): `POST
  /api/ramp` should run the same limit check `post_strength` already does
  before it starts — reject a `linear`/`hold` target above the
  configured `limitA`/`limitB` with `400`, same as a manual Set would be
  rejected. `random-walk`'s `base + variance` should be clamped into the
  limit each step rather than rejected outright, since a wandering value
  is expected to occasionally want to exceed a range — clamping keeps the
  walk going instead of just stopping it.
- **Override, per the request:** a manual `/api/strength` call cancels the
  channel's active ramp. Mirrors `cancel_playlist_token` exactly — before
  `post_strength` sends, check for (and cancel) a live ramp token on that
  channel.
- `rampA`/`rampB` in `/events` follow the same `snapshot_json` pattern
  every other per-channel field already does — `null` when no ramp is
  active.

**UI:** yes — see the Strength card. Each channel gets a compact ramp row
under its strength dial (originally a linear slider; re-skinned to a
circular gauge later, the ramp row itself unaffected): a profile toggle
(Linear / Random walk / Hold — same `.mode-toggle` component the Pulse
Waveform card
already uses for Single/Playlist), the profile's specific fields, and
while one's running, a small progress bar plus live current→target
readout and a Cancel button, replacing the picker.

---

## 3. Session Timer with Check-in Gates

**Status: implemented.** `src/panel/session.rs` + `src/panel/session_runner.rs`;
the design below (a precomputed sorted schedule; pause/resume via captured
elapsed time, no schedule recomputation) matches what shipped — see
`docs/api.md`'s "Session timer" section for the final endpoint/SSE
reference. Unit tests covering the schedule builder and pause/resume are
in place and passing.

**Update:** the runner originally slept in one stretch straight to each
checkpoint (see the "one tick-driving improvement" bullet below) and only
broadcast on `/events` when a checkpoint fired. In practice that meant the
panel's countdown display froze between checkpoints — which can be many
minutes apart — instead of counting down live like the ramp progress bar
does. Fixed by reinstating a 1-second wakeup (`PanelState::session_heartbeat`,
called from the runner's wait loop) that broadcasts a fresh snapshot
without mutating any state; `elapsed`/`remaining` were already computed
live from a stored deadline, so the heartbeat's only job is giving
subscribers something to re-render from every second.

Built-in session timer that fires webhook/SSE events at configurable checkpoints.

### API

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/session/timer` | Configure and start timer |
| `POST` | `/api/session/timer/pause` | Pause timer |
| `POST` | `/api/session/timer/play` | Resume timer |
| `POST` | `/api/session/end` | End timer early |

### Request Body

```json
{
  "durationSeconds": 3600,
  "checkInEverySeconds": 900,
  "phaseGates": [
    {"atSeconds": 300, "label": "warmup-complete"},
    {"atSeconds": 1200, "label": "midpoint"},
    {"atSeconds": 3000, "label": "final-stretch"}
  ],
  "autoStopPlaylistsAtEnd": true
}
```

### Events Fired

- `session.started`
- `session.check_in` (every 15 min)
- `session.phase_gate` (at labeled checkpoints)
- `session.ending` (5 min warning)
- `session.ended`

All events include: `elapsedSeconds`, `remainingSeconds`, `label`.

### SSE Addition

`sessionTimer` field:
```json
{
  "state": "running",
  "elapsed": 845,
  "remaining": 2755,
  "nextGateLabel": "midpoint",
  "nextGateAt": 1200
}
```

### Use Case
Set a 60-minute session with 15-minute check-ins. Panel reminds sub to report color/arousal/discomfort via webhook. Can also gate playlist transitions.

### Implementation notes

Third instance of the same background-task-plus-`CancellationToken`
pattern as playlists and ramps — by this point it's clearly this app's
standard shape for "a timed process that drives state and fires events,"
which is a good sign the architecture generalizes rather than each
feature needing its own bespoke plumbing.

- One tick-driving choice that didn't survive contact with the UI: the
  original runner computed the time until the *next* thing that matters
  (soonest of: next check-in, next phase gate, or the end) and `sleep`
  exactly that long, same as `playlist_runner` sleeping the entry's exact
  resolved duration rather than polling — fewer wakeups (an hour-long
  session only needs ~7 instead of 3600), but it also meant the only
  thing telling `/events` subscribers to re-render was a checkpoint
  firing, so the countdown display would sit frozen for however long the
  gap between checkpoints was. Reinstated a 1-second wakeup after the
  fact (`PanelState::session_heartbeat`, see the Update note above) —
  the extra wakeups turned out to matter more than the efficiency
  argument against them, the same way `ramp_runner` already accepted
  that tradeoff for its own progress bar.
- `pause`/`play` need to capture and restore "time until next event" the
  same way `PlaylistQueue::pause` captures `remaining` — direct parallel,
  same bug class (don't just re-derive from a stale start-time).
- `autoStopPlaylistsAtEnd` is one line: call the already-existing
  `PanelState::playlist_stop` for both channels when the timer ends
  naturally. No new playlist-side code.
- The five webhook events (`session.started`, `session.check_in`, etc.)
  are a direct fit for the existing `log_with(line, extra)` two-argument
  pattern — every current event type already goes through it, so this is
  additive, not a new mechanism.
- Timer config is inherently session-scoped (you set a new one each
  session), so unlike Templates/Button Mapping/Recipes this one doesn't
  need the persistence layer — in-memory `PanelState` fields are enough,
  consistent with how playlists themselves already work.

**UI:** yes — see the mockup's new Session card. Countdown, a
horizontal strip of phase-gate markers (past ones dimmed, the next one
highlighted with its label and ETA), and Pause/Resume/End controls.

---

## 4. File-Based Event Log

**Status: implemented.** `src/panel/event_log.rs`; hooks into
`PanelState::log_with` as the single third sink, exactly as planned
below, with its own background writer task communicated with over an
`mpsc` channel. See `docs/api.md`'s "File-based event log" section for
the final endpoint/format reference. Unit tests (filename templating,
line format, directory creation + write round-trip via a real tokio
temp-dir test) are in place and passing; live end-to-end verification
against a running panel has not been run yet.

Every webhook event also gets appended to a local JSONL file on disk.

### Config API

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/session/log-config` | Configure logging |
| `POST` | `/api/session/start` | Start a new log file (manual session start) |

### Request Body

```json
{
  "enabled": true,
  "directory": "/home/amouroug/.openclaw/workspace/logs/sessions",
  "filenameFormat": "dg-lab-{YYYY-MM-DD}_{HH-mm-ss}.jsonl",
  "maxFileSizeMb": 50,
  "retentionDays": 30
}
```

### File Format

One JSON object per line:
```json
{"timestamp":"2026-08-23T23:01:00Z","event":"button_feedback","protocol":"v4","code":3,"channel":"A","shape":"star","strengthA":37,"strengthB":43}
{"timestamp":"2026-08-23T23:01:15Z","event":"playlist","channel":"A","phase":"playing","currentId":"uuid","remainingMs":12500}
{"timestamp":"2026-08-23T23:01:15Z","event":"strength","channel":"A","op":"inc","value":1,"resultingStrength":38}
```

### Rotation

- New file per session (auto-started at first playlist play, or explicit `POST /api/session/start`)
- Old files auto-deleted after retention period

### Use Case
Post-session analysis with `jq`: average strength over time, button press count, which playlist items caused strongest reactions.

### Implementation notes

Good architectural fit, with one real gap between the request's sample
output and what's actually available today.

- **The fit:** `PanelState::log_with(line, extra)` is already the single
  choke point every loggable event passes through — it feeds the in-memory
  ring buffer *and* the webhook today, so a file writer is a third sink
  bolted on at that one spot, exactly how the webhook itself was added.
  Nothing else in the codebase needs to change to wire this in.
- **The gap:** several of the request's sample JSONL lines carry fields
  the panel doesn't currently produce as structured data at that call
  site. `{"event":"strength","channel":"A","op":"inc","value":1,"resultingStrength":38}`
  — today's strength-change log lines are plain human-readable strings
  with no `extra` payload at all (`state.panel.log(format!("Sent: {text}"))`
  in `send_frame`), and playlist events
  (`"Playlist channel {}: added an entry"`, etc.) are the same — plain
  `log()`, no `log_with()`. Hitting the request's exact shape means adding
  `extra` payloads at several more call sites (`handler.rs`'s playlist
  handlers, `send_frame`), not just plumbing a new sink onto what already
  exists. Worth flagging since it's a legitimate chunk of the work, easy
  to undercount if you only look at where the file-writer itself plugs in.
- **New dependency:** `tokio`'s `fs` feature isn't enabled (see
  `Cargo.toml` — only `rt-multi-thread, macros, net, time, sync, signal`),
  so async file writes need that feature added, or the writes need to go
  through `spawn_blocking` with `std::fs`. Either is fine; the `fs`
  feature is the smaller diff.
- Filename templating (`{YYYY-MM-DD}_{HH-mm-ss}`) is a small hand-rolled
  substitution over `chrono::Utc::now()` (already a dependency, already
  used in `webhook.rs`) — no new crate needed.
- Size-based rollover and retention sweeps are the same shape as
  `src/logging/mod.rs`'s existing `LOG_DIR`/`LOG_MAX_TOTAL_BYTES` handling
  for the main process log, just against a different directory and a
  per-session filename instead of `flexi_logger`'s numbered backups —
  that module is a reasonable reference for the rollover logic, even
  though it can't be reused directly (this needs a distinct file *per
  session*, which `flexi_logger` doesn't model).
- Session-file lifecycle ties to feature 3: "auto-started at first
  playlist play, or `POST /api/session/start`" — recommend a small
  independent state machine (current file handle, path, bytes written)
  rather than coupling it into `session.rs`'s own state.

**UI:** none proposed. This is a set-it-once operations feature, and the
existing precedent for that kind of config in this app (`LOG_DIR`,
`LOG_MAX_TOTAL_BYTES`) has zero UI presence — env vars plus the two config
endpoints for the Python client is consistent with that, and a full
settings form for something you configure once and forget would be more
UI than the feature is worth. If it'd be useful later, the smallest
addition would be a one-line status in the existing Log card's header
("recording to `dg-lab-2026-08-23_23-01-00.jsonl`") — not proposed in the
mockup, but cheap to add if you want it.

---

## 5. Configurable Button Mapping

**Status: implemented, `pattern` only** (per this section's own
Implementation notes below — `shortPress`/`doublePress`/`longPress`
were not built, since neither protocol reports press duration or click
count on the wire). `src/panel/button_map.rs`; dispatched from
`relay_client.rs`/`v4_client.rs` right after button-feedback decoding,
persisted like templates. See `docs/api.md`'s "Button mapping" section
for the final action list/reference. Unit tests (the full request
shape round-tripping through JSON, per-target channel resolution) are
in place and passing; live end-to-end verification (an actual button
press dispatching an action against a running panel) has not been run
yet.

**UI revised after initial ship:** the original raw-JSON-textarea UI
(see "UI" below, written when this was first scoped) was later replaced
with the full 10-row visual mapper that paragraph flagged as the bigger
alternative — one row per button-shape combination, an action dropdown
per row, and a target/amount field that only appears for actions that
take one. Still no shortPress/doublePress/longPress UI, for the same
wire-protocol reason above. Saving omits rows left at "None" rather than
writing all 10 keys, so the stored `button-map.json` stays exactly as
sparse as a hand-written one.

Server-side actions assigned to physical button presses on the DG-LAB device.

### Current State
Button events fire to webhook as hints. Panel should be able to execute actions directly.

### API

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/button-map` | Configure button actions |
| `GET` | `/api/button-map` | Read current mapping |

### Request Body

```json
{
  "defaultChannel": "A",
  "shortPress": {"action": "playlist_toggle", "target": "A"},
  "doublePress": {"action": "strength_delta", "channel": "both", "delta": 5},
  "longPress": {"action": "emergency_clear"},
  "pattern": {
    "A-circle": {"action": "playlist_pause", "target": "A"},
    "A-triangle": {"action": "strength_inc", "channel": "A", "amount": 1},
    "A-square": {"action": "strength_dec", "channel": "A", "amount": 1},
    "B-circle": {"action": "playlist_pause", "target": "B"},
    "B-hexagon": {"action": "ramp_cancel", "channel": "both"}
  }
}
```

### Supported Actions

| Action | Params | Description |
|--------|--------|-------------|
| `playlist_play` | `target: "A"\|"B"` | Start/resume playlist |
| `playlist_pause` | `target: "A"\|"B"` | Pause playlist |
| `playlist_stop` | `target: "A"\|"B"` | Stop and reset playlist |
| `playlist_toggle` | `target: "A"\|"B"` | Play if stopped/paused, pause if playing |
| `strength_inc` | `channel`, `amount` | Increase strength |
| `strength_dec` | `channel`, `amount` | Decrease strength |
| `strength_set` | `channel`, `value` | Set exact strength |
| `strength_delta` | `channel`, `delta` | Add/subtract delta |
| `emergency_clear` | — | Clear both channels, stop all playlists |
| `ramp_cancel` | `channel` | Cancel active ramp |
| `webhook_only` | — | Current behavior — just fire webhook |
| `none` | — | Ignore this button press |

### Persistence
Saved in panel state, survives reconnect.

### Use Case
Tap device button to pause A's playlist without reaching for phone. Double-tap for quick +5 escalation. Long-press as panic button.

### Implementation notes

Good news first: every action in the "Supported Actions" table already
maps onto a function that exists in this codebase today —
`playlist_play`/`pause`/`stop`, `commands::strength_frame`/
`v4_commands::strength_frame` (Inc/Dec/Set), `clear_frame`. Button mapping
is almost entirely wiring, not new device-control logic (the one
exception, `ramp_cancel`, depends on feature 2 shipping first).

- **Hook point:** right after `state.set_button_action(...)` — there are
  exactly two call sites, one in `relay_client.rs` (V3) and one in
  `v4_client.rs` (V4), both already decoding the press into `(channel,
  shape)` via the shared `decode_button_feedback`. A new
  `button_map::dispatch(&state, channel, shape)` call added at both spots
  looks up the configured action and executes it through the existing
  functions above.
- **`pattern` (per-button mapping) is fully implementable as specified** —
  it's a direct 1:1 lookup on `(channel, shape)`, which is exactly what
  the wire protocol already delivers per tap.
- **`shortPress`/`doublePress`/`longPress` are not derivable from the wire
  protocol as written, and this is worth resolving before building.**
  V3's `feedback-<n>` and V4's `custom.action` are each a single discrete
  event per physical tap — there is no press-duration signal anywhere in
  either protocol (confirmed against `docs/api.md`'s "Device feedback"
  section and both handlers' code: the device sends one message, `n`
  0-9, full stop). "Long press" has literally nothing to detect. "Double
  press" *could* be inferred client-side by timing between two
  consecutive button events, but the request's shape doesn't say which
  of the 10 button codes triggers it — is a double-tap on *any* button a
  `doublePress`, and if channel A's circle is tapped twice fast while B's
  triangle is `pattern`-mapped to something else, which action fires?
  That's a real ambiguity in the spec, not an implementation detail.
  Recommend shipping `pattern` only for now (which already covers the
  request's own primary use case — "tap to pause A's playlist") and
  treating short/double/long-press as a separate follow-up once there's a
  concrete answer to "double-press *of what*."
- **Persistence:** needs to survive a restart to be useful ("configure
  once"), same as Templates — sits on the shared persistence module
  proposed above.

**UI:** a **minimal** one — deliberately not a full 10-slot visual mapper.
Building a form for 10 button slots × action + params each is a
disproportionate amount of new UI for a config surface the request
itself says will mainly be driven from the Python client. The mockup
instead shows a small card with a raw JSON textarea + Save/Reload,
following the exact precedent this app already uses for the custom
waveform field (paste raw data, no bespoke form per field) — same
visual language, much less to build, still gives an in-browser fallback
for one-off edits. Flagging this scope call explicitly in case you'd
rather have the full visual mapper — it's a bigger but buildable option.

---

## 6. Session Presets / Recipes (Bonus)

**Status: implemented.** `src/panel/recipe.rs` (data model + persistence,
`recipes.json` under `PANEL_DATA_DIR`), wired into `src/panel/state.rs`
(`recipe_names`/`recipe_get`/`recipe_save`/`recipe_delete`, mirroring
Templates exactly) and `src/panel/handler.rs` (the six endpoints below plus
`POST /api/session/stop`), with a Recipes card in
`src/panel/assets/index.html` (list + Start/Edit/Delete buttons, plus
the emergency-stop/pause/resume buttons) paired next to Session Timer,
per the mockup. Matches the design here with two deliberate trims, both
documented inline in `recipe.rs` and in `docs/api.md`'s "Session presets /
recipes" section: there's no `buttonMap` field (Feature 5 has one active
map, not a named collection to reference by name), and saving a recipe
takes an explicit JSON body rather than snapshotting "current session
state" (a live playlist queue has no template name to snapshot back into
once loaded). `POST .../start` validates everything — the recipe's own
numbers and that every referenced template exists — before starting
anything, so a bad recipe fails cleanly rather than partially applying.
Unit tests (round-trip serialization matching the request's own JSON
shape, and `validate_self`) are in place and passing; integration-level/
live-UI verification have not been run yet.

**UI revised after initial ship:** the original raw-JSON-textarea UI was
later replaced with a visual builder -- a toggle-able section per
optional recipe piece (timer, playlist A/B, ramp A/B), each its own
sub-form (playlist sections get a live template-name dropdown from
`GET /api/templates` instead of a free-typed name; ramp sections reuse
the same profile-picker fields the Strength card's live ramp config
uses). One scope cut: phase gates aren't editable in the builder (no
gate-list sub-editor was built) -- editing and resaving a recipe that
already has some preserves them as-is rather than dropping them, but
adding/removing gates still needs the Python client or a raw `POST`.
`POST /api/session/recipes/{name}` itself is unchanged -- still an
explicit JSON body; only what edits that body in-browser changed.

Combine templates + ramps + timer into one named session definition.

### API

| Method | Path | Description |
|--------|------|-------------|
| `GET` | `/api/session/recipes` | List recipes |
| `GET` | `/api/session/recipes/{name}` | Get one recipe |
| `POST` | `/api/session/recipes/{name}` | Save current session config as recipe |
| `DELETE` | `/api/session/recipes/{name}` | Remove recipe |
| `POST` | `/api/session/recipes/{name}/start` | Start the full session |
| `POST` | `/api/session/stop` | Emergency stop everything |

### Recipe JSON

```json
{
  "name": "Mara's Standard Warmup",
  "timer": {
    "durationSeconds": 1800,
    "checkInEverySeconds": 900
  },
  "playlistA": {
    "template": "warmup-steady",
    "shuffle": false
  },
  "playlistB": {
    "template": "warmup-tease",
    "shuffle": false
  },
  "rampA": {
    "profile": "linear",
    "from": 10,
    "to": 30,
    "overSeconds": 600
  },
  "rampB": {
    "profile": "hold",
    "value": 15,
    "durationSeconds": 1800
  },
  "buttonMap": "default-warmup"
}
```

### Use Case
One call — `POST /api/session/recipes/maras-warmup/start` — and the entire session orchestrates itself. Mara just monitors and intervenes.

### Implementation notes

Pure composition — this is the one feature with essentially zero new
device-control logic, which is why it's recommended last. A recipe is a
bundle of references to blocks features 1/2/3/5 already build:
`POST /api/session/recipes/{name}/start` just calls, in sequence, the
already-existing load-template + play (×2 channels), ramp-start (×2),
session-timer-start, and button-map-switch endpoints/functions. Nothing
here needs new wire-protocol code or new background-task machinery.

- `POST /api/session/stop` ("emergency stop everything") is the same
  composition in reverse: `playlist_stop` (×2), `clear_frame` (×2), ramp
  cancel (×2), session timer end — all functions that exist once features
  1-3 land.
- Same shared persistence module as Templates/Button Mapping.
- Genuinely has nothing to build until 1, 2, 3, and 5 (or at least the
  subset a given recipe references) exist — building this first would
  mean stubbing out everything it composes over, twice.

**UI:** yes — see the mockup's new Recipes card, at the top of the page
next to Session (these two are the "whole-session" controls, so they sit
together rather than beside a single-channel card like Templates does).
A list of saved recipe names, each with a **Start** button, plus **Save
current session as…**.

---

## 7. Subjective Check-in Logging

**Status: implemented**, per Mara's answers below. `POST /api/session/checkin`
in `src/panel/handler.rs` validates the body exactly as specified (`color`
required, `arousal` required 1–10, `discomfort` defaults to `"none"`,
`notes` optional), logs it as `subjective.check_in` via the existing
`log_with`, and stores it on `PanelState` (`CheckIn` in `src/panel/state.rs`)
for `/events`' new `lastCheckIn` field. The Session timer card shows a
"Last check-in" status line, plus a submission form (color toggle, arousal,
discomfort, notes) that posts to this endpoint directly — added after the
initial ship, which only had the read-only status line and no way to
actually log one from the panel itself. See `docs/api.md`'s "Subjective
check-in" section for the final reference. Unit tests (`record_check_in_
populates_the_snapshot`) are in place and passing.

**API:**

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/session/checkin` | Log a subjective check-in |

**Request Body:**

```json
{
  "color": "green" | "yellow" | "red",
  "arousal": 1-10,
  "discomfort": "none" | "mild" | "moderate" | "severe",
  "notes": "Optional freeform notes"
}
```

**Use Case:**

During sessions, the sub reports color/arousal/discomfort at check-in gates. This data is currently lost to chat logs. Logging it into the event stream would let us correlate "arousal 6, color green" with "strengthA:50, waveform:Breathing" for post-session analysis.

**Implementation Notes:**

- The request body is a direct fit for the existing `log_with` two-argument pattern. Every current event type already goes through it, so this is additive, not a new mechanism.
- The Python client would expose this as a single command during sessions, e.g., `dg_lab_panel_client.py checkin color=green arousal=6`.

**Needs clarification before implementation:**

- Are all four fields required on every check-in, or can e.g. `arousal` be logged alone without `color`/`discomfort`?
- Is `arousal` strictly required to be an integer 1–10 — rejected with `400` outside that range, or clamped? Are `color`/`discomfort` validated against the listed values (`400` on anything else), or accepted as free-form strings?
- What `event` name should the logged line's `extra.event` field use? Existing events use a dotted taxonomy (`session.started`, `session.check_in`, `button_feedback`) — this needs one for consistency.
- Does a check-in need to show up live anywhere (the `/events` snapshot, a panel UI element), or is it log/webhook-only, as the request's own framing ("logged into the event stream," "post-session analysis") implies?

---

## 8. Electrode Contact Quality Alert

**Status: implemented**, per Mara's answers below. Purely automatic and
V4-only, as clarified — no endpoint. `state.rs`'s `v4_set_device`/
`v4_update_device` now detect a `normal` <-> issue transition on
`channelAStatus`/`channelBStatus` (`detect_contact_transition`) with a
500ms per-channel debounce, firing both `"entered"` and `"resolved"`;
`v4_client.rs` logs each as a `contact_issue` event via `log_with`. The
five documented status codes don't map one-to-one onto the request's
four `issue` values — `contact_issue_str`'s mapping (documented inline
and in `docs/api.md`) is a judgment call, not something the clarification
round settled explicitly. See `docs/api.md`'s "Electrode contact quality
alert" section for the final reference. Unit tests (transition detection,
debounce behavior, the status-code mapping, and one end-to-end
`v4_update_device` case) are in place and passing; integration-level/
live-device verification have not been run yet.

**API:**

| Event | Fields | Fires when |
|-------|--------|-------------|
| `contact_issue` | `channel: "A" | "B"`, `issue: "loose" | "damaged" | "open_circuit" | "unknown"` | When electrode contact quality changes from `normal` to anything else. |

**Use Case:**

Safety-critical. Tonight's pad displacement was reported late. The panel already receives `channelAStatus`/`channelBStatus` (open circuit, normal, damaged, etc.) from the device. An immediate alert when status changes from `normal` to anything else would trigger an immediate alert — no waiting for the sub to notice.

**Implementation Notes:**

- The event is a direct fit for the existing `log_with` two-argument pattern. Every current event type already goes through it, so this is additive, not a new mechanism.
- The Python client would expose this as a single command during sessions, e.g., `dg_lab_panel_client.py alert contact_issue channel=A issue=loose`.

**Needs clarification before implementation:**

- The API table lists an *event*, not a `Method`/`Path` the way every other feature does — it's unclear how this actually gets triggered. The Use Case ("no waiting for the sub to notice") reads as the panel detecting the transition itself, automatically, from `channelAStatus`/`channelBStatus` data it already has; the Implementation Notes' example Python command (`alert contact_issue channel=A issue=loose`) reads as a human/client manually reporting it after noticing. These are two different features — which is wanted, or both?
- If automatic: this can only ever fire over V4. `channelAStatus`/`channelBStatus` are V4-only fields in the current code (`src/panel/state.rs`) — V3 reports no contact-quality signal on the wire at all. Is a V4-only alert acceptable, or does V3 need a different detection path?
- Should it also fire on recovery (issue → `normal`), not just `normal` → issue?
- Device status can flap. Is any debounce/rate-limit wanted, or should every transition alert immediately, however brief?

---

## 9. Per-Channel Intensity Calibration

**Status: implemented**, per Mara's answers below, with one correction
made along the way. `src/panel/calibration.rs` holds the data model
(`raw = (logical + offset) * gain`, gain in `[0.1, 5.0]`, offset in
`[-50, 50]`) and persistence (`calibration.json` under `PANEL_DATA_DIR`).
`handler.rs::post_strength`, `handler.rs::post_ramp`'s peak-value check,
`ramp_runner.rs::send_set`, and `button_map.rs`'s `strength_set` action
all calibrate their logical target before sending — `op: "inc"/"dec"`
and `strength_inc`/`_dec`/`_delta` deliberately don't (relative nudges to
raw current strength, no logical target to convert). `/events` gained
`logicalStrengthA`/`_B` and `calibration`; the Strength card shows both
values per channel, plus a small gain/offset form.

**Correction to answer #6** ("reject any combination that would produce
a negative wire value or a value > 200"): implementing this literally as
a *save-time* check against the full 0-200 logical domain would reject
Mara's own example calibration (`gain: 2.0`) — `200 * 2.0 = 400` already
overflows past 200, even though the same calibration is exactly what her
"ramp both to 40" example needs and is perfectly safe there. Implemented
instead as a *per-command* check against the actual value being sent
(`calibration::apply_checked`), at every call site listed above — this
satisfies the same underlying safety goal (the device never receives a
negative value or one above 200) without rejecting calibrations that are
only unsafe for logical inputs nobody's actually sending. Flagged here
rather than silently reinterpreted, since it changes what "reject" means
relative to the literal answer.

See `docs/api.md`'s "Per-channel intensity calibration" section for the
final reference. Unit tests cover `calibration.rs`'s own logic
(order-of-operations, the inverse for display, the save-time vs.
per-command validation split) and `PanelState`'s calibration accessors;
matching the rest of this codebase, `handler.rs`/`button_map.rs` have no
direct unit tests of their own (see e.g. Features 7/8's notes), so the
four call sites that apply calibration when sending a command
(`post_strength`, `post_ramp`'s peak check, `ramp_runner::send_set`,
`button_map`'s `strength_set` action) are exercised only by inspection
and by the shared `calibration::apply_checked` they all call, not by a
dedicated integration test per call site. Integration-level/live-UI
verification have not been run yet either.

**API:**

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/calibration` | Configure per-channel intensity calibration |

**Request Body:**

```json
{
  "channelA": {
    "gain": 1.0,
    "offset": 0
  },
  "channelB": {
    "gain": 1.0,
    "offset": 0
  }
}
```

**Use Case:**

Based on tonight: perineum at 25 feels roughly equivalent to inner thigh at 50. A panel-level gain/offset per channel (e.g., "Channel B actual = requested × 2.0") would let me write recipes saying "ramp both to 40" and have them feel matched, rather than manually compensating every time.

**Implementation Notes:**

- The calibration is applied when the panel sends strength commands. It does not affect the device's own reported `strengthA`/`strengthB` or the panel's soft limits.
- The Python client would expose this as a single command, e.g., `dg_lab_panel_client.py calibration channelA_gain=2.0 channelB_offset=5`.

**Needs clarification before implementation:**

- Order of operations: is the calibrated wire value `requested × gain + offset`, or `(requested + offset) × gain`? The example ("Channel B actual = requested × 2.0") only demonstrates gain alone.
- Does the configured upper limit (`POST /api/limit`) check the requested (pre-calibration) value or the calibrated (actual wire) value? Safety-relevant either way: a gain above 1 could let real output exceed the configured limit if the check runs before calibration is applied.
- On V4, `POST /api/strength`'s `op: "set"` is already emulated as a delta from the device's last known reported strength (see ["Strength control"](../docs/api.md)). If the API speaks in pre-calibration ("logical") units, computing that delta correctly means inverting the calibration to find the right raw wire delta — the request doesn't address this at all, and it's the trickiest part of this feature.
- The device reports its *actual* (post-calibration, raw) strength back on every status update, and that's what the panel currently displays and returns in `/events`. Should the display/API also become calibration-aware (show a back-converted "logical" value), or is seeing "80" on screen after asking for "40" (gain 2.0) accepted as expected behavior?
- Should calibration persist across a panel restart, like templates/button-map/recipes do — or reset each session, like the upper limit currently does?
- Should `gain`/`offset` be range-validated (e.g. reject negative gain, or a combination that could produce a negative wire value)?

---

## 10. True Session Pause/Resume with State Preservation

**Status: implemented**, per Mara's answers below. `POST /api/session/pause`/
`resume` in `src/panel/handler.rs` compose playlist-pause, session-timer-
pause, and a new ramp-pause capability (`src/panel/ramp.rs`'s
`RampSnapshot.paused` + `RunnerTick::resume_from`, `PanelState::ramp_pause`/
`ramp_resume`) with an active strength zero-and-restore, all as answered.
See `docs/api.md`'s "Global pause/resume" section for the final reference.

**Correction to answer #4** ("deleted template while paused"): this
scenario can't actually occur with the current architecture. A playlist's
queue is fully in-memory once loaded (`PlaylistQueue.entries()`) —
pausing/resuming playback (`playlist_pause`/`playlist_play`) never
re-resolves a template by name, so there is nothing for a deleted
template to invalidate mid-pause. This was a premise in my own
clarifying question, not something the implementation needed to work
around — no warning-logging or skip-on-resume behavior was built for it,
since there's no code path that would ever need it.

**Two safety bugs found and fixed while implementing** (not present in
the answers — found by tracing through the composed behavior):

1. `POST /api/session/stop` (emergency stop) and button-mapped
   `emergency_clear` didn't clear a pending `POST /api/session/pause`
   strength capture. Sequence: pause (strength captured, zeroed) →
   emergency stop (channels cleared, but the capture was untouched) →
   later, an unrelated `POST /api/session/resume` call → the stale
   pre-pause value gets restored, silently un-zeroing a channel that was
   supposed to be *finally* stopped. Fixed by having both emergency-stop
   paths also end the pending capture, matching the "not resumable"
   framing in `docs/api.md`.
2. A manual strength override during a pause (`POST /api/strength`, a
   button-mapped strength action, or starting a fresh `POST /api/ramp`)
   didn't end the pending capture either — a later resume would silently
   overwrite the operator's deliberate manual adjustment (or a fresh
   ramp's own progress) with the stale pre-pause value. Fixed by having
   all of those "take over" the pending capture the same way they
   already take over an active ramp.

Unit tests cover `RunnerTick::resume_from` (exact continuation for
`linear`/`hold`, the `random-walk` approximation), `PanelState`'s
`ramp_pause`/`ramp_resume`/`playlist_is_paused`/pre-pause-strength
accessors (including a regression test for bug 2 above), matching the
rest of this codebase; `handler.rs`/`button_map.rs` have no direct unit
tests of their own (see Features 7-9's notes) so bug 1's fix is
exercised only by inspection. Integration-level/live-UI verification
have not been run yet.

**API:**

| Method | Path | Description |
|--------|------|-------------|
| `POST` | `/api/session/pause` | Pause the session, preserving state |
| `POST` | `/api/session/resume` | Resume the paused session |

**Use Case:**

Tonight's ramp cancellation forced a restart from scratch. A real pause needs to capture: ramp position (current value and remaining time), playlist entry position and remaining ms, timer elapsed time, and device strengths. Resume restores all of it. Currently pause just freezes the clock — it doesn't snapshot the world.

**Implementation Notes:**

- The pause state is stored in `PanelState` and survives a panel restart.
- The Python client would expose this as two commands, e.g., `dg_lab_panel_client.py session pause` and `dg_lab_panel_client.py session resume`.

**Needs clarification before implementation:**

- Playlists and the session timer already have real, position-preserving pause/resume today (`POST /api/playlist/{channel}/pause` captures remaining ms; `POST /api/session/timer/pause`/`play` captures elapsed time and resumes the schedule). **Ramps are the one piece that doesn't** — `src/panel/ramp.rs`'s own design docs call this out as a deliberate prior decision ("there's no pause/resume, only start/stop… unlike playlists there's no captured 'remaining' state to restore on resume"). Is the actual ask here specifically to add that missing ramp-pause capability (and then have one endpoint call all three sub-systems' pause/resume together), or to change how playlist/session pause already behave too?
- "Capture… device strengths" — does pausing mean *actively commanding the device's strength down* (and restoring it on resume), or just *recording* what the strength was at pause time for logging/display? These have very different safety implications and need to be explicit before this touches a live device.
- "Survives a panel restart" is a materially bigger ask than the rest of this feature: it means a new persisted store (like templates/recipes), *and* the panel correctly re-establishing real device state after a crash/restart — not just an in-process pause that's lost if the server restarts. Is persisted, restart-surviving pause actually needed, or is an in-memory pause (lost on restart, like the rest of session state today) acceptable?
- If a paused playlist's queue was loaded from a template that gets edited or deleted while paused, what should `resume` do?

---

## Clarifications — Mara’s Answers

### Persistence Format (Open Question from Introduction)

**JSON file on disk is correct.** One file per store under `PANEL_DATA_DIR` — same shape as `LOG_DIR`. SQLite is overkill for key-value template/recipe data; the atomic write-to-tmp-then-rename pattern already used in `persistence.rs` is sufficient. Confirming this approach.

---

### Feature 7 — Subjective Check-in Logging

1. **Required fields:** `color` and `arousal` are mandatory — reject with `400` if either is missing. `discomfort` defaults to `"none"` if omitted. `notes` is fully optional.
2. **Validation:** `arousal` must be an integer 1–10 — reject with `400` if outside range or non-integer. `color` must be exactly `"green"`, `"yellow"`, or `"red"` — `400` otherwise. `discomfort` must be exactly `"none"`, `"mild"`, `"moderate"`, or `"severe"` — `400` otherwise. Strict validation prevents garbage data in the event stream.
3. **Event name:** Use `subjective.check_in` — follows the existing dotted taxonomy (`session.started`, `button_feedback`, etc.).
4. **Live display:** Yes. Add `lastCheckIn` to the `/events` SSE snapshot showing the most recent check-in data (timestamp, color, arousal, discomfort, notes). Webhook fires with the same payload. Panel UI gets a small status line in the Session card: **Last check-in:** Green / 6 / none — timestamp. Log/webhook is primary; live display is secondary but useful.

---

### Feature 8 — Electrode Contact Quality Alert

1. **Automatic detection.** The panel detects transitions from `channelAStatus`/`channelBStatus` data it already receives. The Python client example in the Implementation Notes was misleading — this is **not** a manual-reporting endpoint. The feature is purely automatic.
2. **V4-only is acceptable.** V3 reports no contact-quality signal on the wire; this is a documented limitation, not a gap to fill. If V3 ever adds status reporting, we extend then. For now, alert only when a V4 device is connected and status is available.
3. **Fire on both directions:** Alert on `normal → issue` and on `issue → normal`. Include a `status` field in the event payload: `"entered"` or `"resolved"`. Both directions matter for post-session analysis.
4. **Debounce:** 500 ms minimum between alerts on the same channel. If the status flaps faster than that, log the transition internally but suppress duplicate webhook/SSE noise. One alert per 500 ms window per channel.

---

### Feature 9 — Per-Channel Intensity Calibration

1. **Order of operations:** `(requested + offset) × gain`. Offset applies first, then gain. Example: if perineum needs a +5 baseline shift to match thigh sensation at equal recipe values, set `offset: 5`, `gain: 1.0`.
2. **Upper limit check:** Runs on the **calibrated (actual wire)** value. Safety-critical — if a recipe requests 50 and calibration would send 100, the panel rejects with `400` before the device sees it. The limit is a hard ceiling on physical output, not on logical recipe values.
3. **V4 `set` emulation:** This is the trickiest part. For V4 `op: "set"`, the panel must compute the logical-to-raw mapping, then derive the delta from the device’s last known **raw** strength. If no raw baseline is known (device just connected), reject with `409` — same as current behavior. Document this as a known V4 calibration limitation.
4. **Display:** Show **both** values in `/events` and panel UI: `strengthA` (raw actual, as today) and `logicalStrengthA` (back-converted). The UI labels must make clear which is which — e.g., **A: 80 (logical: 40)**. I want to see both. The raw value matters for safety; the logical value matters for recipe debugging.
5. **Persistence:** Yes, persists across restart. Calibration is a hardware-zone characteristic (thigh vs. perineum sensitivity), not session-scoped. Store it alongside templates/recipes in the persistence layer.
6. **Validation:** `gain` must be ≥ 0.1 and ≤ 5.0. `offset` must be ≥ -50 and ≤ 50. Reject any combination that would produce a negative wire value or a value > 200 (the device’s maximum). `400` on any violation.

---

### Feature 10 — True Session Pause/Resume with State Preservation

1. **Scope:** Add ramp-pause capability specifically. Then have `POST /api/session/pause` call all three subsystems’ pause in sequence: playlist pause (already works), timer pause (already works), ramp pause (new). Do **not** change existing playlist/timer behavior — they’re already correct.
2. **Pausing strength behavior:** On pause, **actively command strength to 0 on both channels** and record the pre-pause strengths for restore on resume. Safety first — never leave a device running at pause. On resume, restore channels to their recorded pre-pause strengths, then resume ramps/playlists/timer.
3. **Persistence:** In-memory only. Acceptable if lost on restart. If the panel restarts during pause, that’s effectively an emergency stop anyway — channels at 0, all state lost. I do **not** need restart-surviving pause state.
4. **Deleted template while paused:** On resume, skip the playlist (leave it stopped) and restore ramps/timer only. Log a warning: *"Template 'warmup-steady' no longer exists; playlist skipped on resume."* Do not fail the entire resume over a missing playlist — the ramp and timer are the critical pieces.

---

## Notes

- All endpoints return standard panel status codes: `200 OK`, `400` bad request, `409` conflict (no device, empty queue, etc.), `503` relay not ready.
- These are feature requests for the Rust panel at `~/dg-lab-websocket-server-rs`. Build in whatever order you prefer.
- Once implemented, the Python client (`dg_lab_panel_client.py`) will be updated to speak the new endpoints.
