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

**UI:** yes — see the mockup's Strength card. Each channel gets a compact
ramp row under its existing slider: a profile toggle (Linear / Random
walk / Hold — same `.mode-toggle` component the Pulse Waveform card
already uses for Single/Playlist), the profile's specific fields, and
while one's running, a small progress bar plus live current→target
readout and a Cancel button, replacing the picker.

---

## 3. Session Timer with Check-in Gates

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

- One tick-driving improvement over a naive 1s poll: rather than waking
  every second, compute the time until the *next* thing that matters
  (soonest of: next check-in, next phase gate, or the end) and `sleep`
  exactly that long, same as `playlist_runner` sleeping the entry's exact
  resolved duration rather than polling. Simpler to reason about and
  doesn't wake the task 3600 times for an hour-long session that only
  needs ~7 wakeups.
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

## Notes

- All endpoints return standard panel status codes: `200 OK`, `400` bad request, `409` conflict (no device, empty queue, etc.), `503` relay not ready.
- These are feature requests for the Rust panel at `~/dg-lab-websocket-server-rs`. Build in whatever order you prefer.
- Once implemented, the Python client (`dg_lab_panel_client.py`) will be updated to speak the new endpoints.
