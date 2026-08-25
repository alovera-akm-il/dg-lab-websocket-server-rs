# API reference

Three independent APIs, one per server. See [architecture.md](architecture.md)
for how they relate and [sequence-diagrams.md](sequence-diagrams.md) for
message flows over time.

- [V3 relay (WebSocket)](#v3-relay-websocket) — `:10002` by default
- [V4 relay (WebSocket)](#v4-relay-websocket) — `:10001` by default
- [Control panel (HTTP)](#control-panel-http) — `:40000` by default

All JSON examples below are exact wire shapes taken directly from the source
(`src/v3/handler.rs`, `src/v3/protocol.rs`, `src/v4/handler.rs`,
`src/panel/handler.rs`, `src/panel/commands.rs`), not paraphrased.

---

## V3 relay (WebSocket)

1 controller ("web") : 1 device ("app") pairing relay. Every message is a
JSON object; there is no binary framing.

### Connecting and pairing

There is no fixed path — **any** path is upgrade-eligible, because the path
tail doubles as an implicit `targetId`. On connect, the server always
replies first with the connection's own id:

```json
{"type":"bind","clientId":"<uuid>","targetId":"","message":"targetId"}
```

To pair, the connecting side supplies a target id one of three ways, checked
in this order:

1. `?targetId=<id>` query parameter
2. `?tid=<id>` query parameter
3. The URL path tail (e.g. `ws://host:10002/<id>`) — this is what the
   control panel's pairing QR uses

If a `targetId` was supplied at connect time and pairing succeeds, **both**
sides receive:

```json
{"type":"bind","clientId":"<controllerId>","targetId":"<deviceId>","message":"200"}
```

Pairing can also be requested explicitly after connecting, by either side
sending a `bind` frame:

```json
{"type":"bind","clientId":"<controllerId>","targetId":"<deviceId>","message":""}
```

`clientId`/`targetId` here name the *intended* web/app pair, not necessarily
the sender — but the sender's own real connection id must equal one of them
(`validate_source`), or the request is rejected with error code `404`.

**Bind result codes** (the `message` field of the `bind` response):

| Code | Meaning |
| --- | --- |
| `200` | Paired (or already paired with this exact partner — idempotent) |
| `400` | Rejected: one of the two ids is already bound to someone else |
| `401` | Rejected: self-pairing, or one of the ids isn't a connected client |

A connection whose `targetId` (from the query/path, not a `bind` frame) is
already bound to someone else, or doesn't exist, is rejected at connect time
with:

```json
{"type":"error","clientId":"","targetId":"<targetId>","message":"4001"}
```

followed by a WebSocket close with code `4001`.

### Strength control (`type` 1–3)

Sent by the controller only. `channel` accepts `1`/`"1"`/`"A"`/`"a"` for
channel A, `2`/`"2"`/`"B"`/`"b"` for channel B; omitted defaults to A (an
explicit invalid value is rejected outright, never silently defaulted).

| `type` | Meaning | Extra fields |
| --- | --- | --- |
| `1` | Increase by 1 | — |
| `2` | Decrease by 1 | — |
| `3` | Set to an exact value | `strength: <number>` |

```json
{"type":1,"clientId":"<controllerId>","targetId":"<deviceId>","channel":"A","message":"set channel"}
```

Forwarded to the device as:

```json
{"type":"msg","clientId":"<controllerId>","targetId":"<deviceId>","message":"strength-<channel#>+<sendType>+<value>"}
```

where `channel# ` is `1`/`2` and `sendType = type - 1` (so `0`=inc, `1`=dec,
`2`=set with `value` = the requested `strength`; inc/dec always carry
`value=1`).

### Clear / custom strength (`type` 4)

```json
{"type":4,"clientId":"<controllerId>","targetId":"<deviceId>","channel":"A","message":"clear"}
```

If `message` contains the substring `"clear"`, the device receives
`"clear-<channel#>"` and any in-flight pulse sequence on that channel is
cancelled; the controller then receives a `notify` (see
[below](#wire-fidelity-note-two-chinese-strings)). Otherwise this is a
"custom strength" set: the device receives `"strength-<channel#>+2+<strength>"`
using the frame's `strength` field.

### Pulse waveform (`clientMsg`)

```json
{"type":"clientMsg","clientId":"<controllerId>","targetId":"<deviceId>","channel":"A","time":3,"message":"A:[\"0A0A0A0A0A0A0A0A\"]"}
```

- `message` is either `"<prefix>:<JSON array of 16-hex-char frame strings>"`
  (parsed and repacketized — the channel letter in the *output* always
  matches the resolved `channel` field, not the prefix in `message`) or, if
  that shape doesn't parse, treated as a raw legacy string and passed
  through as `pulse-<message>` unmodified, repeated for the packet count.
- `time` (seconds) defaults to `DEFAULT_PUNISHMENT_DURATION` (env, default
  `5`) if omitted or non-positive.
- Packets are sent at a rate of `DEFAULT_PUNISHMENT_TIME` per second (env,
  clamped to `[1, 10]`), each packet:

  ```json
  {"type":"msg","clientId":"<controllerId>","targetId":"<deviceId>","message":"pulse-A:[\"0A0A0A0A0A0A0A0A\"]"}
  ```

- If a pulse is already running on the same (controller, channel), the
  running one is cancelled, the device gets an immediate `clear-<channel#>`,
  the controller gets a `notify` warning (see below), and the new sequence
  starts after a fixed 150ms delay.
- When the sequence completes (or is replaced/cancelled), the controller
  receives a `notify` "done" frame (see below).

### Device feedback (device → controller)

The device reports two kinds of message, forwarded to the controller
unmodified (matched by prefix, not parsed/routed like controller commands):

```json
{"type":"msg","clientId":"<deviceId>","targetId":"<controllerId>","message":"feedback-<n>"}
```

`n` is `0`–`9`, one per physical shape button on the DG-LAB APP's control
screen. **This channel/shape mapping is not documented in any known official
spec** — it was determined empirically against real hardware (see the doc
comment on `decode_button_feedback` in `src/panel/relay_client.rs`):

| `n` | Channel | Shape |
| --- | --- | --- |
| 0 | A | circle |
| 1 | A | triangle |
| 2 | A | square |
| 3 | A | star |
| 4 | A | hexagon |
| 5 | B | circle |
| 6 | B | triangle |
| 7 | B | square |
| 8 | B | star |
| 9 | B | hexagon |

```json
{"type":"msg","clientId":"<deviceId>","targetId":"<controllerId>","message":"strength-<a>+<b>+<softLimitA>+<softLimitB>"}
```

The device's current strength on each channel, plus its own
device/app-configured soft limit per channel (V3 has no wire command to set
this remotely — see the panel's [Upper limit](#upper-limit-1) feature for the
application-level alternative).

### Wire-fidelity note: two Chinese strings

Two `notify` messages are sent verbatim in Chinese, matching the reference
TypeScript server's spec byte-for-byte — this project deliberately does not
translate them on the wire, since third-party controllers built against the
original spec may pattern-match on the literal string:

```json
{"type":"notify","clientId":"<controllerId>","targetId":"<deviceId>","message":"发送完毕"}
```
Sent when a pulse sequence finishes sending (all packets dispatched, or the
target disconnected mid-stream).

```json
{"type":"notify","clientId":"<controllerId>","targetId":"<deviceId>","message":"当前通道A有正在发送的消息，覆盖之前的消息"}
```
Sent when a new pulse sequence pre-empts one already running on the same
channel (`A`/`B` substituted for the actual channel). The control panel
translates both of these for its own display only (`relay_client::translate_notify`);
the bytes on the wire are untouched.

### Errors (`type: "error"`)

```json
{"type":"error","clientId":"<clientId-or-empty>","targetId":"<targetId-or-empty>","message":"<code>"}
```

| Code | Meaning |
| --- | --- |
| `403` | Malformed frame (invalid JSON, not an object, missing/invalid `type`/`clientId`/`targetId`/`message`, or empty id) |
| `404` | Illegal source (sender isn't `clientId` or `targetId`), or the intended recipient isn't currently connected |
| `402` | The `clientId`/`targetId` pair named in the frame isn't actually paired |
| `406` | An explicitly-present `channel` field has an unrecognized value |
| `idle_timeout` | Connection stayed unpaired past `IDLE_TIMEOUT` — followed by a close with code `1000` |

### Disconnection

When either side of a pairing disconnects, the other receives:

```json
{"type":"break","clientId":"<the-other-sides-id>","targetId":"<the-closer's-id>","message":"209"}
```

followed by a WebSocket close (code `1000`, reason `partner_disconnected`).

### Heartbeat

Every connection receives `{"type":"heartbeat"}` every `HEARTBEAT_INTERVAL`
ms (default `60000`); no response is required or expected.

---

## V4 relay (WebSocket)

1 controller : N devices relay. The relay itself treats payloads under
`data` as fully opaque — it doesn't parse or validate them, only routes by
device id — but they aren't actually freeform in practice; see
["The `data` schema real DG-LAB 4 APPs use"](#the-data-schema-real-dg-lab-4-apps-use)
below for what a real controller/APP exchange actually puts there.

### Connecting

Only under the configured `PREFIX` path (default `/`) — any other path is
`404`. No `targetId`/`tid` query param → registers as a **controller**; with
one → attaches as a **device** under that controller.

Every connection first receives:

```json
{"type":"hello","clientId":"<8-hex-char id>"}
```

**Controller** (no target): starts a zero-devices idle timer (`IDLE_TIMEOUT`,
default 5 min), cancelled as soon as any device attaches and restarted if
the device count drops back to zero.

**Device** (`?targetId=<controllerId>` or `?tid=`): if the controller id
doesn't exist, the device gets

```json
{"type":"error","code":"controller_not_found"}
```

followed by a close (code `4001`). On success, the device gets

```json
{"type":"controller_attached","clientId":"<controllerId>"}
```

and the controller gets

```json
{"type":"client_attached","clientId":"<deviceId>"}
```

### Sending data

Controller → device (`clientId` names the target device):

```json
{"type":"message","clientId":"<deviceId>","data":{"anything":"here"}}
```

The device receives `{"type":"message","data":{...}}` (no `clientId` — it
only ever has one controller). If `clientId` is missing, the controller gets
`{"type":"error","code":"bad_request","message":"message.clientId is required"}`;
if the named device isn't attached under this controller,
`{"type":"error","code":"client_not_found","clientId":"<deviceId>"}`.

Device → controller (no `clientId` needed — the hub already knows which
controller owns this device):

```json
{"type":"message","data":{"anything":"here"}}
```

The controller receives `{"type":"message","clientId":"<deviceId>","data":{...}}`.

### The `data` schema real DG-LAB 4 APPs use

The relay itself never looks inside `data` — but it isn't actually
freeform in practice. [`dglab-kit`](https://github.com/dungeonlab-open/dglab-kit),
the official SDK for the DG-LAB 4 APP, documents a full RPC schema it puts
there, and the control panel's V4 support (`src/panel/v4_commands.rs`,
`src/panel/v4_client.rs`) implements this schema, not an invented one.
Three frame kinds, tagged by `t`:

```json
{"t":"req","reqId":"<id>","m":"<method>","data":{...}}   // controller -> device
{"t":"resp","reqId":"<id>","result":{...}}                // device -> controller, on success
{"t":"resp","reqId":"<id>","error":"<code>"}               // device -> controller, on failure
{"t":"ev","ev":"<name>",...}                                // device -> controller, unprompted
```

**RPC methods**: `devices.get` (no params → `{"devices":[...]}`, the
current device list on demand), `ping` (no params → the device's local
timestamp, for RTT measurement), `device.op` (enqueue a device action,
below), `device.op.clear` (cancel queued/running actions).

**`device.op` request** — `data` is:

```json
{"s": "<slotId>", "t": <ActionType>, "c": 0 | 1, "p": 0 | 1 | 2, "d": <ms>, "im": <bool>, "v": <depends on t>}
```

`s`=target device's slotId, `c`=channel (`0`=A, `1`=B), `p`=priority
(default 1), `d`=duration ms (default 0 = not time-limited), `im`=replace
any already-queued task of the same device/channel/type. `t` selects the
action and what `v` means:

| `t` | Action | `v` | Lifecycle |
| --- | --- | --- | --- |
| `0` | `AppendPulseData` | `number[][] \| string[]` — waveform frames (`ver:3` hex-string form, e.g. `"0A0A0A0A00000000"`, is what the panel sends) | continuous |
| `3` | `AddIntensity` | signed relative delta | one-shot |
| `4` | `SetTempIntensity` | temporary strength value, auto-reverts to `0` when the task ends | continuous |
| `5` | `SetMute` | `boolean` | one-shot |
| `7` | `SetIntensity` | must be `0` — **V4 has no action for an arbitrary absolute value** | one-shot |

`device.op` doesn't respond on enqueue — only once the task completes, is
cleared, replaced, or cancelled (connection drop). The panel doesn't wait
on this; it fires the request and moves on, so nothing here is required
for the panel's own command flow to work. On completion:

```json
{"t":"resp","reqId":"<id>","result":{"type":<ActionType>,"reason":"completed"|"cleared"|"replaced"|"cancelled","slotId":"<id>","channel":0|1}}
```

**`device.op.clear` request** — `data` (all optional): `{"s":"<slotId>","c":0|1}`.
Omit `s` to clear every device's tasks; `s` alone clears one device's every
channel; `s`+`c` clears one channel. Always resolves `{}` on success.

**Events** (`t:"ev"`, unprompted):

| `ev` | Fields | Fires when |
| --- | --- | --- |
| `devices.snapshot` | `devices: [{slotId, name, type, props?, slotState?}]` | Immediately after `controller_attached` — the APP's full device list, even if empty |
| `devices.patch` | `added?: [...]` (full entries), `removed?: [slotId,...]` | The APP's device list changes |
| `slots.patch` | `slots: [{slotId, props?, slotState?}]` | Per-device state changed — `props`/`slotState` here are *deltas*, only the changed fields |
| `custom.action` | `action: 0-9` | An on-screen/physical button press — dglab-kit documents this as the same underlying concept as V3's `feedback-*`, just under a different event name |

For a Coyote device (`type: "COYOTE_030"`), `props` includes
`intensityA`/`intensityB` (current per-channel strength — what the panel
reads) alongside `power` (battery %), `channelAStatus`/`channelBStatus`,
and others; there's no single documented soft-limit field the way V3 has
one. Full field references for every supported device type are in
`dglab-kit`'s README under "V4 设备 props / slotState 字段参考".

### App-level ping (independent of native WS ping/pong)

```json
{"type":"ping"}
```
→
```json
{"type":"pong","ts":<unix-ms>}
```

Separately, the server also sends native WebSocket ping frames every
`WS_PING_INTERVAL` ms (default `10000`); a connection that misses
`MAX_MISSED_WS_PONGS` (default `3`) consecutive native pongs is terminated
(close code `4002`, reused for the idle timeout too).

### Disconnection

- Controller disconnects → every attached device gets
  `{"type":"controller_disconnected","clientId":"<controllerId>"}` then a
  close (code `4000`).
- Device disconnects → its controller gets
  `{"type":"client_disconnected","clientId":"<deviceId>"}`; if that was the
  controller's last device, its zero-devices idle timer restarts.

### Heartbeat

Every connection receives `{"type":"heartbeat"}` every `HEARTBEAT_INTERVAL`
ms (default `30000`).

---

## Control panel (HTTP)

All request/response bodies are JSON except where noted. There is no
authentication — the panel is intended for trusted-network / localhost use.

| Method | Path | Purpose |
| --- | --- | --- |
| `GET` | `/` | The panel page (`assets/index.html`) |
| `GET` | `/assets/{file}` | The panel's CSS/JS (`style.css`, `app.js`) -- see [architecture.md](architecture.md) on how these are embedded |
| `GET` | `/events` | Server-Sent Events stream of the panel's live state |
| `GET` | `/api/presets` | The bundled pulse-waveform preset catalog |
| `GET` | `/api/qr/{v3\|v4}` | The pairing QR for one protocol, on demand |
| `POST` | `/api/strength` | Increase / decrease / set a channel's strength |
| `POST` | `/api/clear` | Clear a channel (cancels any in-flight pulse) |
| `POST` | `/api/pulse` | Send a pulse waveform |
| `POST` | `/api/limit` | Set/clear a channel's operator upper limit |
| `POST` | `/api/ramp` | Start (or replace) a strength ramp on a channel |
| `POST` | `/api/ramp/stop` | Stop a channel's active ramp |
| `POST` | `/api/session/timer` | Configure and start the session timer |
| `POST` | `/api/session/timer/pause` | Pause the session timer |
| `POST` | `/api/session/timer/play` | Resume the session timer |
| `POST` | `/api/session/end` | End the session timer early |
| `POST` | `/api/session/log-config` | Configure the file-based event log |
| `POST` | `/api/session/start` | Force a fresh event-log session file |
| `POST` | `/api/session/stop` | Emergency stop: clear both channels, stop playlists/ramps/timer |
| `POST` | `/api/session/checkin` | Log a subjective check-in (color/arousal/discomfort/notes) |
| `GET` | `/api/session/recipes` | List saved recipe names |
| `GET` | `/api/session/recipes/{name}` | Get one recipe by name |
| `POST` | `/api/session/recipes/{name}` | Save (create or replace) a recipe |
| `DELETE` | `/api/session/recipes/{name}` | Remove a recipe |
| `POST` | `/api/session/recipes/{name}/start` | Start every piece a recipe defines |
| `GET` | `/api/button-map` | Read the current button mapping |
| `POST` | `/api/button-map` | Replace the whole button mapping |
| `POST` | `/api/webhook` | Set/clear the outbound webhook URL |
| `POST` | `/api/reconnect` | Force a fresh relay connection (new controller id/QR) |
| `POST` | `/api/playlist/{channel}/items` | Add a pulse or gap entry to that channel's playlist |
| `DELETE` | `/api/playlist/{channel}/items/{id}` | Remove one entry |
| `POST` | `/api/playlist/{channel}/reorder` | Reorder entries |
| `POST` | `/api/playlist/{channel}/settings` | Set shuffle / loop-playback |
| `POST` | `/api/playlist/{channel}/play` | Start (or resume) playback |
| `POST` | `/api/playlist/{channel}/pause` | Pause playback, keeping position |
| `POST` | `/api/playlist/{channel}/stop` | Stop and reset to the start of the queue |
| `POST` | `/api/playlist/{channel}/load-template` | Replace a channel's queue with a saved template |
| `GET` | `/api/templates` | List saved template names |
| `GET` | `/api/templates/{name}` | Get one template by name |
| `POST` | `/api/templates/{name}` | Save a channel's current queue as a template |
| `DELETE` | `/api/templates/{name}` | Remove a template |

### `GET /events` (SSE)

Emits one event immediately on connect (a full snapshot), then one more
every time anything changes. Each event's `data` is:

```json
{
  "status": "connecting" | "waiting_for_device" | "paired" | "disconnected",
  "controllerId": "<uuid>" | null,
  "deviceId": "<uuid>" | null,
  "v4Status": "connecting" | "waiting_for_device" | "paired" | "disconnected",
  "v4ControllerId": "<8-hex-char id>" | null,
  "v4DeviceId": "<APP's V4 connection id>" | null,
  "v4DeviceName": "<string>" | null,
  "activeProtocol": "v3" | "v4" | null,
  "strengthA": <number> | null,
  "strengthB": <number> | null,
  "softLimitA": <number> | null,
  "softLimitB": <number> | null,
  "lastButtonAction": <0-9> | null,
  "battery": <0-100> | null,
  "channelAStatus": <0-4> | null,
  "channelAStatusLabel": "no output" | "open circuit" | "normal" | "damaged" | "masked" | "unknown" | null,
  "channelBStatus": <0-4> | null,
  "channelBStatusLabel": "..." | null,
  "channelAOverheat": <bool> | null,
  "channelAOverheatPercent": <0-100> | null,
  "channelBOverheat": <bool> | null,
  "channelBOverheatPercent": <0-100> | null,
  "limitA": <number> | null,
  "limitB": <number> | null,
  "webhookUrl": "<string>" | null,
  "log": ["<line>", "..."],
  "qrSvg": "<svg>...</svg>" | null,
  "pairUrl": "https://www.dungeon-lab.com/app-download.php#DGLAB-SOCKET#ws://..." | null,
  "qrSvgV4": "<svg>...</svg>" | null,
  "pairUrlV4": "https://dungeon-lab.cn/s/?v=1&action=socket&url=..." | null,
  "playlistA": <PlaylistSnapshot, see below>,
  "playlistB": <PlaylistSnapshot, see below>,
  "rampA": <ramp object, see "Ramps" below> | null,
  "rampB": <ramp object, see "Ramps" below> | null,
  "sessionTimer": <session timer object, see "Session timer" below> | null
}
```

`qrSvg`/`pairUrl` are `null` until `controllerId` is known (i.e. before the
panel's own V3 connection completes); `qrSvgV4`/`pairUrlV4` likewise wait on
`v4ControllerId`. Both QRs' embedded host is derived per-connection from that
request's `Host` header (see [architecture.md](architecture.md) and the
README's "Pairing over WiFi" section) — two browsers viewing the panel from
different addresses can see different QR codes for the same underlying
controller ids.

`battery`/`channelAStatus`/`channelBStatus`/`channelAOverheat*`/`channelBOverheat*`
are V4-only, and further Coyote-only within V4 (`dglab-kit` documents them
under `COYOTE_020`/`COYOTE_030`'s `props`/`slotState`; other device types
simply never report them, so these stay `null`) — see the [`data` schema
section](#the-data-schema-real-dg-lab-4-apps-use). `battery` is `props.power`;
`channelAStatus`/`channelBStatus` are `props.channelAStatus`/`channelBStatus`,
decoded into `channelAStatusLabel`/`channelBStatusLabel` by
`v4_client::channel_status_label`; the overheat fields come from
`slotState.channelA/B.comfortLimit.overheat`/`overheatPercent`. Each field
updates independently and is only ever overwritten by a snapshot/patch that
actually includes it — a `slots.patch` delta that omits `channelAStatus`
leaves the last-known value in place rather than clearing it — but all of
them reset to `null` together when the V4 device disconnects.

`activeProtocol` is which leg — if either — currently drives
`strengthA`/`strengthB`/`softLimitA`/`softLimitB`/`lastButtonAction` and the
`/api/strength`, `/api/clear`, `/api/pulse` endpoints below: whichever
protocol's device paired most recently, falling back to the other protocol's
device if it's still paired when the active one disconnects, else `null`. V3
and V4 pairing are tracked fully independently and can both be live at once
(the panel runs both relay connections simultaneously) — only one drives the
shared controls at a time. See [architecture.md](architecture.md) and
`src/panel/state.rs`'s module docs for the exact rules and their rationale.

### `GET /api/presets`

```json
[
  {"id": "coyote-...", "label": "...", "family": "coyote" | "ovc", "waveform": "A:[\"...\",...]"}
]
```

44 entries total (24 Coyote, 20 OVC), sourced from `dglab-kit`'s
`COYOTE_WAVEFORMS`/`OVC_WAVEFORMS` and cross-verified byte-for-byte
(`src/panel/presets.rs`). `waveform` is ready to pass straight through as
`POST /api/pulse`'s `waveform` field.

### `GET /api/qr/{protocol}`

`{protocol}` is `v3` or `v4`. Returns that protocol's pairing QR on demand,
independent of holding an `/events` SSE connection open (e.g. for a client
that just wants to fetch, display, or print the current code):

```json
{"qrSvg": "<svg>...</svg>", "pairUrl": "https://..."}
```

Same `Host`-header-based host resolution as the embedded QRs in `/events`
(see above) — the returned QR reflects whichever address this specific
request came in on. `400` if `{protocol}` isn't `v3`/`v4`; `503` if that
protocol's controller id isn't known yet (its relay connection hasn't
completed).

### `POST /api/strength`, `/api/clear`, `/api/pulse`

These three route transparently to whichever protocol's device is
currently active (`activeProtocol` in `/events` — see above): a V3 device
gets `commands::*_frame`'s V3 wire shapes ([above](#v3-relay-websocket)); a
V4 device gets `device.op`/`device.op.clear` RPC requests
([above](#v4-relay-websocket)), built by `src/panel/v4_commands.rs`
following `dglab-kit`'s documented schema.

```json
// POST /api/strength
{"channel": "A" | "B" | "a" | "b" | "1" | "2", "op": "inc" | "dec" | "set", "value": <number, required for "set">}

// POST /api/clear
{"channel": "A" | "B"}

// POST /api/pulse
{"channel": "A" | "B", "time": <seconds, default 3>, "waveform": "<preset string or custom frame data>"}
```

Common status codes:

- `200 OK` — command sent.
- `400` — invalid channel/op, empty `waveform`, or (`inc`/`set` only) the
  predicted resulting strength would exceed that channel's configured
  [upper limit](#upper-limit-1).
- `409` — no device currently paired on either protocol.
- `503` — the active protocol's relay connection isn't currently ready to
  send.

V4-specific cases:

- **`POST /api/strength` with `op: "set"`**: V4's wire protocol has no
  action for setting an arbitrary absolute strength — only a relative
  `AddIntensity` delta or resetting to exactly `0` via `SetIntensity`, see
  [the `data` schema section above](#the-data-schema-real-dg-lab-4-apps-use).
  The panel emulates "set" as an `AddIntensity` delta computed from the
  last known strength. If no baseline is known yet (nothing received from
  this device's `devices.snapshot`/`slots.patch` since it attached),
  there's nothing to compute a delta from and the request is rejected
  with **`409`** rather than guessing.
- **`POST /api/pulse`**: V4 has no raw-legacy-string fallback the way V3
  does — `waveform` must parse as the `"<prefix>:[...]"` frame-array
  format (same parser V3 uses, `v3::pulse::parse_pulse_message`) or the
  request is rejected with **`400`**. `time` (seconds) is converted to
  milliseconds for V4's `d` field.

### `POST /api/limit`

<a id="upper-limit-1"></a>
Sets an application-level safety cap: the panel refuses any `inc`/`set`
command on that channel that would push its (best-known) strength above this
value. This is **not** a device-level setting — V3 has no wire command to
change the device's own configured limit remotely; the device's own limit is
only ever visible read-only, as `softLimitA`/`softLimitB` in `/events`.

```json
{"channel": "A" | "B", "value": <number> | null}
```
`value: null` (or omitted) clears the limit. `400` if `value` is negative.
Survives the panel's own relay reconnects (it's operator configuration, not
device state).

### Ramps

A strength ramp is a curve the panel drives on a fixed 1-second tick,
sending a `Set` command (the same one `POST /api/strength`'s `op: "set"`
sends) each time the computed value actually changes, rather than the
operator sending individual commands — `src/panel/ramp.rs`/
`src/panel/ramp_runner.rs`. Neither wire protocol has a smooth-curve
primitive, so this is genuinely what "ramping" means here: a value
recomputed once a second, not a continuous device-side effect. There's no
pause/resume, only start/stop — a manual `POST /api/strength` on that
channel cancels its active ramp outright (see below).

Three profiles, chosen by the body's `profile` field:

```json
// linear -- interpolates from `from` to `to` over `overSeconds`, then ends
{"channel": "A", "profile": "linear", "from": 10, "to": 40, "overSeconds": 600}

// random-walk -- wanders within `base` +/- `variance`, re-rolling every
// `stepSeconds`, for a total of `durationSeconds`
{"channel": "B", "profile": "random-walk", "base": 30, "variance": 15, "stepSeconds": 30, "durationSeconds": 600}

// hold -- holds `value` steady for `durationSeconds` (a Set that stays
// "active" -- shown in /events, blocks a manual override the same way
// the other profiles do -- rather than ending immediately)
{"channel": "A", "profile": "hold", "value": 25, "durationSeconds": 300}
```

#### `POST /api/ramp`

Starts a ramp on `channel`, replacing any ramp already running there
(starting a new one always wins — there's no separate "already ramping"
conflict response). `400` if any of the profile's own numbers are invalid
(`overSeconds`/`stepSeconds`/`durationSeconds` must be at least 1;
`variance` must not be negative), or if — `linear`/`hold` only — the
profile's peak value (`max(from, to)` for `linear`, `value` for `hold`)
would exceed the channel's configured [upper limit](#upper-limit-1).
`random-walk` isn't checked upfront: instead, each step is individually
clamped into the limit by the runner, since a wandering value legitimately
wants to occasionally reach for a range's edge, unlike a fixed target.

#### `POST /api/ramp/stop`

```json
{"channel": "A" | "B"}
```

Stops that channel's active ramp. Always `200 OK`, including when there
wasn't one.

#### Override

`POST /api/strength` cancels whatever ramp is currently active on that
command's channel before sending — a silent no-op when there wasn't one,
so a routine manual +/- click doesn't fire an extra `/events` push on
every single press.

#### `rampA`/`rampB` in `/events`

`null` when no ramp is active on that channel:

```json
{
  "profile": "linear",
  "current": 23,
  "target": 40,
  "remainingSeconds": 340,
  "from": 10, "to": 40, "overSeconds": 600
}
```

`current`/`target` deliberately coincide for `hold`/`random-walk` (both
settle immediately, or on each re-roll) — they diverge meaningfully only
for `linear`, which is the profile they were named for. The profile's own
request fields (`from`/`to`/`overSeconds` for `linear`; `base`/`variance`/
`stepSeconds`/`durationSeconds` for `random-walk`; `value`/`durationSeconds`
for `hold`) are merged in on top of the four fields above so the UI can
redraw its config without having cached the original `POST /api/ramp`
body — same "merge whatever's relevant for this event" shape
[webhook payloads](#webhook-payloads) already use, rather than a fixed set
of always-present-but-often-null fields.

### Session timer

A single, panel-wide countdown timer (not per-channel, unlike playlists
and ramps — there's only ever one) that fires a webhook/log event at
configurable checkpoints — `src/panel/session.rs`/
`src/panel/session_runner.rs`. Every checkpoint (recurring check-ins, one-off
labeled phase gates, a fixed 5-minute-before-the-end warning, and the final
end) is known in full the instant the timer starts, so the whole run is
flattened into one sorted schedule up front and the runner sleeps exactly to
each one in turn — the same "sleep the exact needed duration" discipline
`playlist_runner`/`ramp_runner` already use, rather than polling every
second. Session config is session-scoped like playlists/ramps — it doesn't
survive a panel restart.

#### `POST /api/session/timer`

```json
{
  "durationSeconds": 3600,
  "checkInEverySeconds": 900,
  "phaseGates": [
    {"atSeconds": 300, "label": "warmup-complete"},
    {"atSeconds": 1200, "label": "midpoint"}
  ],
  "autoStopPlaylistsAtEnd": true
}
```

Starts a fresh timer, replacing any existing one outright (same "starting a
new one always wins" rule as `POST /api/ramp`). `checkInEverySeconds: 0` (or
omitted) disables recurring check-ins — a session with only phase gates and
no periodic check-in is a legitimate configuration. `phaseGates` may be
empty/omitted. `400` if `durationSeconds` is `0`, or any gate's `label` is
empty or `atSeconds` is at or past `durationSeconds`.

Fires `session.started` immediately on success (`elapsedSeconds: 0`,
`remainingSeconds: durationSeconds`, `label: null`), then one event per
checkpoint as the timer runs:

| Event | Fires |
| --- | --- |
| `session.check_in` | Every `checkInEverySeconds`, if nonzero |
| `session.phase_gate` | At each configured gate's `atSeconds` (`label` carries the gate's own label) |
| `session.ending` | 5 minutes before the end — only scheduled if `durationSeconds > 300` |
| `session.ended` | At the end (natural or early — see `POST /api/session/end`) |

Every event's payload is `{"event": "...", "elapsedSeconds": <number>,
"remainingSeconds": <number>, "label": "<string>" | null}`, delivered
through the same webhook/log path every other panel event already uses.

#### `POST /api/session/timer/pause` / `POST /api/session/timer/play`

No body. `pause` captures elapsed time and stops the runner; `play` resumes
counting from exactly where it left off (the schedule itself never changes
— a phase gate's `atSeconds` is fixed from when the timer started, so
resuming just continues walking the same precomputed list). Both always
`200 OK`, including when there's nothing to pause/resume.

#### `POST /api/session/end`

No body. Ends the session early: fires `session.ended` with the actual
elapsed/remaining at the moment of stopping (not `0` remaining, unlike a
natural end), and honors `autoStopPlaylistsAtEnd` the same way a natural
end does. Always `200 OK`, including when nothing was running.

#### `sessionTimer` in `/events`

`null` when no session is active:

```json
{
  "state": "running" | "paused",
  "elapsed": 845,
  "remaining": 2755,
  "durationSeconds": 3600,
  "checkInEverySeconds": 900,
  "nextGateLabel": "midpoint",
  "nextGateAt": 1200,
  "phaseGates": [
    {"atSeconds": 300, "label": "warmup-complete"},
    {"atSeconds": 1200, "label": "midpoint"}
  ],
  "autoStopPlaylistsAtEnd": true
}
```

`phaseGates` always carries *every* configured gate (not just the next
one) so the UI can draw the full checkpoint strip; `nextGateLabel`/
`nextGateAt` (both `null` once every gate has passed) tell it which one to
highlight as next. `state` is never `"stopped"` here — a stopped session is
simply `sessionTimer: null`, same as an inactive ramp.

### Subjective check-in

`POST /api/session/checkin` — logs a subjective check-in (`src/panel/state.rs`'s
`CheckIn`), for correlating how a session felt against what the device was
actually doing at the time. Purely additive: it goes through the same
`log_with` every other event does (webhook + file event log), plus keeps
the most recent one for `/events`' `lastCheckIn`.

```json
{
  "color": "green",
  "arousal": 6,
  "discomfort": "none",
  "notes": "feeling good"
}
```

`color` (required) must be exactly `"green"`, `"yellow"`, or `"red"`.
`arousal` (required) must be an integer 1–10. `discomfort` defaults to
`"none"` if omitted, and otherwise must be exactly `"none"`, `"mild"`,
`"moderate"`, or `"severe"`. `notes` is free-form and fully optional.
`400` if `color`/`arousal`/`discomfort` fails validation. The logged
line's `extra.event` is `"subjective.check_in"`, following the existing
dotted event-name taxonomy (`session.started`, `button_feedback`, …).

#### `lastCheckIn` in `/events`

`null` until the first check-in of the process's lifetime:

```json
{"timestamp": "2026-08-24T00:03:12.500Z", "color": "green", "arousal": 6, "discomfort": "none", "notes": "feeling good"}
```

Session-scoped like everything else the panel tracks live — not persisted
to disk (only the file event log, if enabled, keeps a durable record).

### Electrode contact quality alert

Automatic, not an endpoint: the panel already receives `channelAStatus`/
`channelBStatus` from V4 device reports (surfaced as `channelAStatus`/
`channelAStatusLabel`/`channelBStatus`/`channelBStatusLabel` in `/events`,
see [`GET /events`](#get-events-sse) above), and watches both channels
for a transition to/from `"normal"` (status code `2`),
firing a `contact_issue` event through the usual `log_with` pipeline
(webhook + file event log) — same mechanism as every other panel event,
no new endpoint or SSE field.

```json
{"message": "Electrode contact entered on channel A: open_circuit", "timestamp": "...", "event": "contact_issue", "channel": "A", "status": "entered", "issue": "open_circuit"}
```

`status` is `"entered"` (just became an issue) or `"resolved"` (issue
cleared) — both directions fire. `issue` is one of `"loose"`,
`"open_circuit"`, `"damaged"`, or `"unknown"` on `"entered"`, and always
`"normal"` on `"resolved"`. Debounced to at most one alert per channel
per 500ms — a status report that arrives within that window of the last
alert still updates the live `channelAStatus`/`channelBStatus` fields as
always, it just doesn't fire a second alert.

**V4-only.** `channelAStatus`/`channelBStatus` are V4-specific fields —
V3 reports no contact-quality signal on the wire at all, so this can
never fire over a V3 connection.

**Status-code-to-`issue` mapping is a judgment call**, not something the
original request's four-value `issue` enum lines up with cleanly against
the five documented status codes: code `0` ("no output") maps to
`"loose"` as the closest fit (no signal path, e.g. a detached pad); code
`4` ("masked", Coyote-only) falls back to `"unknown"`, alongside any
undocumented code.

### File-based event log

Appends every event `PanelState::log_with` sees — the same superset the
webhook fires for — to a local JSONL file, one JSON object per line
(`src/panel/event_log.rs`). Runs as an independent background task with its
own file handle and rotation state; there's no GET/status endpoint (matches
this feature's own scope: config-only, driven from the Python client, no
panel UI).

**"Session" here means a recording session (one physical file), not the
[session timer](#session-timer) above** — the two share the word by
coincidence in the original request, not by design, and don't interact.

#### `POST /api/session/log-config`

```json
{
  "enabled": true,
  "directory": "/home/user/dg-lab-logs",
  "filenameFormat": "dg-lab-{YYYY-MM-DD}_{HH-mm-ss}.jsonl",
  "maxFileSizeMb": 50,
  "retentionDays": 30
}
```

All five fields are required. `{YYYY-MM-DD}`/`{HH-mm-ss}` in
`filenameFormat` are substituted against the moment a file is opened (any
other text, including no placeholder at all, passes through unchanged —
naming every session the same file is a legal, if unusual, choice).
`maxFileSizeMb: 0` disables size-based rotation (a session's file just
keeps growing); `retentionDays: 0` disables the retention sweep entirely
(nothing is ever auto-deleted). `400` if `directory` or `filenameFormat` is
empty. Setting `enabled: false` stops writing and closes whatever file is
currently open; the next session (auto or explicit) after re-enabling opens
a fresh one.

#### `POST /api/session/start`

No body. Forces a fresh log file — whether or not one is already open —
and sweeps retention. A session's file also opens automatically the first
time *any* event is logged while enabled with no file currently open,
deliberately more general than the original request's literal
"auto-started at first playlist play" (which would miss any
pairing/strength/etc. activity that happens before the first playlist
starts). There's no explicit "end session" endpoint — a session's file
keeps being appended to (rolling to a fresh one only on
`maxFileSizeMb`) until the next explicit `POST /api/session/start`, or
the process restarts.

#### File format

One JSON object per line, the exact same shape [webhook payloads](#webhook-payloads)
already use — `{message, timestamp, ...extra}` — so anything the webhook
would have received, the file also has a line for:

```json
{"message":"Session check-in","timestamp":"2026-08-23T23:01:00.000Z","event":"session.check_in","elapsedSeconds":900,"remainingSeconds":2700,"label":null}
```

Retention is checked by filesystem modified time on any `.jsonl` file in
the configured directory, not by parsing `filenameFormat` back into a date
— a hand-picked format can't always be parsed unambiguously, and mtime is
simpler and always correct.

### Button mapping

Assigns a server-side action to a physical button-shape press (`"A-circle"`,
`"B-hexagon"`, …), dispatched the instant `relay_client.rs`/`v4_client.rs`
decode one (`src/panel/button_map.rs`). Persisted like templates — survives
a panel restart (`button-map.json` under `PANEL_DATA_DIR`).

**Scope: `pattern` (per-button) mapping only** — the original request also
proposed `shortPress`/`doublePress`/`longPress`, but neither protocol
reports press duration or click count anywhere on the wire (see
["Device feedback"](#device-feedback-device--controller): one message per
physical tap, full stop) — there's nothing to detect them from, and the
request doesn't specify which of the 10 button codes a "double press"
would even apply to. `pattern` alone already covers the request's own
primary use case ("tap to pause A's playlist without reaching for the
phone").

#### `GET /api/button-map` / `POST /api/button-map`

`GET` returns the current mapping; `POST` replaces it wholesale (not a
partial patch):

```json
{
  "pattern": {
    "A-circle": {"action": "playlist_toggle", "target": "A"},
    "A-triangle": {"action": "strength_inc", "channel": "A", "amount": 1},
    "A-square": {"action": "strength_dec", "channel": "A", "amount": 1},
    "B-circle": {"action": "playlist_toggle", "target": "B"},
    "B-hexagon": {"action": "ramp_cancel", "channel": "both"}
  }
}
```

Keys are `"{channel}-{shape}"`; see the [button-shape table](#device-feedback-device--controller)
for the 10 valid combinations (`A`/`B` × `circle`/`triangle`/`square`/`star`/`hexagon`).
An unmapped key is a silent no-op — the `button_feedback` webhook/log event
still fires as always, just nothing additional happens.

`target`/`channel` accept `"A"`, `"B"`, or `"both"` **on every action**,
not just the ones the original request's own examples show it on
(`strength_delta`/`ramp_cancel`) — there's no reason `playlist_play` or
`strength_set` couldn't apply to both channels at once too, so it isn't
modeled as two different target types.

| `action` | Fields | Behavior |
| --- | --- | --- |
| `playlist_play` | `target` | Starts/resumes that channel's playlist |
| `playlist_pause` | `target` | Pauses it |
| `playlist_stop` | `target` | Stops and resets it |
| `playlist_toggle` | `target` | Pauses if playing, otherwise plays |
| `strength_inc` | `channel`, `amount` | Adds `amount` to current strength |
| `strength_dec` | `channel`, `amount` | Subtracts `amount` |
| `strength_set` | `channel`, `value` | Sets to exactly `value` |
| `strength_delta` | `channel`, `delta` | Adds a signed `delta` (same operation as `strength_inc`/`_dec`, kept as a separate action for fidelity to the request's own table) |
| `emergency_clear` | — | Clears both channels, stops both playlists, cancels both ramps |
| `ramp_cancel` | `channel` | Cancels that channel's active ramp |
| `webhook_only` | — | Explicitly inert — the button-press event already fires regardless |
| `none` | — | Same as `webhook_only`; both exist for fidelity to the request |

`strength_inc`/`_dec`/`_delta`/`_set` all resolve to a `Set` command built
the exact same way `POST /api/strength`'s `op: "set"` is (respecting the
configured [upper limit](#upper-limit-1); on V4, rejected — silently
logged, not surfaced anywhere else, since a button press has no HTTP
response to carry an error back on — if no baseline strength is known
yet). `strength_inc`/`_dec`/`_delta` additionally need a known *current*
strength to compute their target from, since there's nothing wire-level
these can fall back to the way a raw Inc/Dec click can; skipped (logged) if
unknown.

### `POST /api/webhook`

```json
{"url": "https://..." | null}
```
`url: null` (or omitted, or empty after trimming) clears the webhook.
`400` if a non-empty `url` doesn't start with `http://` or `https://`.
See [Webhook payloads](#webhook-payloads) below for what gets posted where.

### `POST /api/reconnect`

No body. Drops the panel's current V3 **and** V4 connections immediately
(instead of waiting out the normal 2s reconnect backoff on each) and
establishes fresh ones, yielding new controller ids and thus new pairing
QRs on both. Always `200 OK`.

### Playlists

Each channel has its own independent queue of pulse/gap entries that plays
through in order (or shuffled), optionally looping — an alternative to
manually triggering one waveform at a time via `POST /api/pulse`. `{channel}`
in every path below accepts the same spellings as elsewhere (`A`/`a`/`1`,
`B`/`b`/`2`). All state lives in the panel process itself (`src/panel/playlist.rs`),
independent of which protocol/device is currently active — a playlist can be
built and even started before any device is paired; the runner (`src/panel/playlist_runner.rs`)
sends each step's frame through the same active-target routing `POST
/api/pulse` uses, and on failure (no device paired, relay not ready, or a
custom waveform that isn't valid V4 frame-array format) just logs it and
skips sending — the entry's resolved duration still elapses and playback
advances to the next entry on its own clock either way, rather than getting
stuck retrying.

Playlist state is not fetched via any of these endpoints — it's only ever
read from `/events`' `playlistA`/`playlistB` fields, each shaped:

```json
{
  "entries": [
    {
      "id": "<uuid>",
      "kind": "pulse" | "gap",
      "label": "<preset label, or a truncated preview of custom waveform text>",
      "waveform": "<preset id or raw custom string>" | null,
      "waveformResolved": "<actual frame data, for drawing a preview>" | null,
      "duration": {"mode": "fixed", "seconds": <number>} | {"mode": "random", "min": <number>, "max": <number>}
    }
  ],
  "shuffle": <bool>,
  "loopPlayback": <bool>,
  "phase": "stopped" | "playing" | "paused",
  "currentId": "<uuid of the entry currently running>" | null,
  "remainingMs": <number> | null,
  "currentDurationMs": <number> | null,
  "totalEntries": <number>
}
```

`waveform`/`waveformResolved` are `null` for a `"gap"` entry. `remainingMs`
counts down within `currentDurationMs` (the concrete duration rolled for this
run — the denominator a progress bar needs; distinct from `duration`, which
for `"random"` is the configured range, not a resolved value). A silent gap
is `phase: "playing"` with `currentId` pointing at the gap entry, not a
separate phase — check `entries[].kind` for the currently-playing entry to
tell the two apart.

#### `POST /api/playlist/{channel}/items`

```json
// pulse entry
{"kind": "pulse", "waveform": "<preset id or custom frame data>", "duration": {"mode": "fixed", "seconds": <number>} | {"mode": "random", "min": <number>, "max": <number>}}

// gap entry
{"kind": "gap", "duration": {"mode": "fixed", "seconds": <number>} | {"mode": "random", "min": <number>, "max": <number>}}
```

Appends to the end of that channel's queue — there's no separate insert-at
endpoint; use `POST .../reorder` afterwards to move it. `200` with
`{"id": "<uuid>"}` on success. `400` for an empty `waveform`, a `duration`
below 1 second, or (`random`) `min > max`.

#### `DELETE /api/playlist/{channel}/items/{id}`

No body. `200` on success, `404` if no entry with that id exists on that
channel's queue (including one already removed).

#### `POST /api/playlist/{channel}/reorder`

```json
{"order": ["<uuid>", "<uuid>", "..."]}
```

`order` must name every entry currently in that channel's queue exactly
once — anything else (a missing id, a duplicate, an id from the other
channel) is rejected with `400` and the queue is left unchanged.

#### `POST /api/playlist/{channel}/settings`

```json
{"shuffle": <bool>, "loopPlayback": <bool>}
```

Both fields are required (this replaces the whole settings pair, not a
partial patch). `shuffle` randomizes play order each time playback starts
from `Stopped` or wraps around; `loopPlayback` controls whether reaching the
end of the queue wraps back to the start or stops. Always `200 OK`.

#### `POST /api/playlist/{channel}/play`

No body. Starts playback from the current position, or resumes a paused
queue. `200` on success (including the idempotent case of calling it again
while already playing — a no-op, not an error). `409` if the queue is empty.

#### `POST /api/playlist/{channel}/pause`

No body. Pauses in place — `remainingMs` on the current entry is preserved,
not reset — so a subsequent `play` continues from where it left off, not
from the top. Always `200 OK`, including if already paused or stopped.

#### `POST /api/playlist/{channel}/stop`

No body. Stops playback and resets position to the start of the queue (the
next `play` starts from the first entry, unlike `pause`). Always `200 OK`.

### Templates

Named, reusable playlist definitions (`src/panel/templates.rs`) — build a
channel's queue once, save it under a name, then load it into either
channel later without rebuilding it by hand. Unlike every other piece of
panel state, templates survive a process restart: they're stored as one
JSON file (`templates.json` under `PANEL_DATA_DIR`, default `panel-data`)
rather than only in memory.

A template's `items` deliberately don't carry entry ids — an id is
queue-local identity assigned fresh by whichever queue loads the template,
not portable data, so loading the same template twice (or into both
channels) never produces colliding ids:

```json
{
  "name": "edge-test",
  "items": [
    {"kind": "pulse", "waveform": "coyote-extrusion", "duration": {"mode": "fixed", "seconds": 20}},
    {"kind": "gap", "duration": {"mode": "random", "min": 8, "max": 15}}
  ],
  "settings": {"shuffle": false, "loopPlayback": true}
}
```

This is exactly the shape `GET /api/templates/{name}` returns, and what
`POST /api/templates/{name}` builds internally from a channel's live
queue — `items[].duration` uses the same `{"mode":"fixed",...}` /
`{"mode":"random",...}` shapes as `POST /api/playlist/{channel}/items`,
validated the same way (`400` for a `0`-second duration or `min > max`) at
load time, since a hand-edited `templates.json` can name an invalid one
even though anything saved through the API can't.

#### `GET /api/templates`

```json
["edge-test", "warmup-steady"]
```

Template names only, sorted alphabetically — fetch `GET
/api/templates/{name}` for one template's full contents.

#### `GET /api/templates/{name}`

Returns the template shape above. `404` if no template with that name
exists.

#### `POST /api/templates/{name}`

```json
{"sourceChannel": "A" | "B"}
```

Saves that channel's *current* queue (contents plus its shuffle/loop
settings) under `{name}`, overwriting any existing template with the same
name — there's no separate rename/update endpoint, saving under an
existing name is how you update it. `200` with `{"name": "..."}` on
success; `400` for an empty name or an invalid `sourceChannel`.

#### `DELETE /api/templates/{name}`

No body. `200` on success, `404` if no template with that name exists.

#### `POST /api/playlist/{channel}/load-template`

```json
{"name": "edge-test", "shuffle": false}
```

Replaces `{channel}`'s entire queue with the named template's items,
stopping any playback in progress first (loading is a destructive
replace, not a merge or an append — a live runner task can't be left
holding an entry id from a queue that no longer exists). `loopPlayback`
comes from the template itself; `shuffle` is the one setting the load
request overrides independently of what the template was saved with,
since there's a real use case for wanting a different shuffle setting on
a given night without needing a second copy of the same template just to
flip it (`loopPlayback` has no equivalent per-load override — see
`docs/dg-lab-panel-feature-requests.md`'s Templates section for why).
`404` if `name` doesn't match a saved template; `400` if the template's
stored data is invalid (see above).

### Session presets / recipes

Pure composition (`src/panel/recipe.rs`): a named bundle of a [session
timer](#session-timer) config, per-channel [ramp](#ramps) profiles, and
per-channel playlist references (by [template](#templates) name) that a
single `POST .../start` resolves and starts together. A recipe carries no
device-control logic of its own — starting one just calls the same
`ramp_start`/`session_start`/`playlist_load`+`playlist_play` machinery
`POST /api/ramp`, `POST /api/session/timer`, and `POST
/api/playlist/{channel}/load-template` already use. Persisted like
templates/the button map — survives a panel restart (`recipes.json` under
`PANEL_DATA_DIR`).

```json
{
  "name": "evening-warmup",
  "timer": {"durationSeconds": 1800, "checkInEverySeconds": 900, "autoStopPlaylistsAtEnd": true},
  "playlistA": {"template": "warmup-steady", "shuffle": false},
  "rampA": {"profile": "linear", "from": 10, "to": 30, "overSeconds": 600},
  "rampB": {"profile": "hold", "value": 15, "durationSeconds": 1800}
}
```

`timer`/`playlistA`/`playlistB`/`rampA`/`rampB` are all optional — a recipe
that only sets `rampA`, say, is legal, and starting it just starts that one
ramp. `timer` is exactly a [`POST /api/session/timer`](#post-apisessiontimer)
body; `rampA`/`rampB` are exactly a [`POST /api/ramp`](#post-apiramp) body's
profile fields (no `channel`, since which channel is implied by the field
name); `playlistA`/`playlistB` are `{"template": "...", "shuffle": bool}`,
the same shape [`POST /api/playlist/{channel}/load-template`](#post-apiplaylistchannelload-template)
takes.

**Two deliberate deviations from the original request:**

- **No `buttonMap` field.** The request's example recipe references a
  button map *by name* (`"buttonMap": "default-warmup"`), but [button
  mapping](#button-mapping) as built has exactly one active map, not a
  named collection to pick between — there's nothing for a name reference
  to resolve against. Out of scope here; would need "templates, but for
  button maps" as its own feature first.
- **Saving takes an explicit body, not a live-state snapshot.** The
  request frames `POST /api/session/recipes/{name}` as "save current
  session config as recipe," mirroring how `POST /api/templates/{name}`
  captures a channel's *actual* live queue. But `playlistA`/`playlistB`
  reference a template *by name*, and a live playlist queue has no such
  name once loaded — there's nothing to snapshot back into a name
  reference. The body is instead authored explicitly, the same way `POST
  /api/button-map` and `POST /api/session/timer` already work.

#### `GET /api/session/recipes`

```json
["evening-warmup", "quick-tease"]
```

Recipe names only, sorted alphabetically.

#### `GET /api/session/recipes/{name}`

Returns the recipe shape above. `404` if no recipe with that name exists.

#### `POST /api/session/recipes/{name}`

Body: the recipe shape above (`name` in the body is ignored — the path
segment wins, same as templates). Upsert — overwrites any existing recipe
with the same name. `400` if the name is empty, or if `timer`/`rampA`/
`rampB`'s own numbers don't validate (the same rules `POST
/api/session/timer`/`POST /api/ramp` already enforce) — **not** checked at
save time: whether `playlistA`/`playlistB`'s named templates actually
exist. `200` with `{"name": "..."}` on success.

#### `DELETE /api/session/recipes/{name}`

No body. `200` on success, `404` if no recipe with that name exists.

#### `POST /api/session/recipes/{name}/start`

No body. Starts every piece the recipe defines, in one call: loads and
plays `playlistA`/`playlistB` (each stopping and replacing whatever that
channel's queue currently holds, same as `load-template`), starts
`rampA`/`rampB`, starts `timer`. Everything is validated up front —
including that every referenced template actually exists — before any
state changes, so a bad recipe fails cleanly with `400`/`404` rather than
partially applying (e.g. channel A already swapped over by the time a
missing channel B template is discovered). `404` if the recipe itself
doesn't exist.

#### `POST /api/session/stop`

No body. The emergency "stop everything" companion to starting a recipe:
stops both channels' playlists, cancels both channels' ramps, sends a
clear frame to each channel (best-effort — silently skipped if no device
is currently paired), and ends the session timer if one is running.
Always `200 OK` — nothing here reports partial failure, since every step
is unconditional.

---

## Webhook payloads

Configured via `PANEL_WEBHOOK_URL` or `POST /api/webhook`. Every line the
panel logs internally — from either protocol — also fires a `POST` to this
URL — delivery is fire-and-forget with a 5s timeout; failures are only
logged to the server's own stdout, never fed back into the panel's own log
or another webhook call (so a broken endpoint can't create a notification
loop).

Every payload has at minimum:

```json
{"message": "<human-readable log line>", "timestamp": "<RFC3339 UTC>"}
```

Events the panel can classify add an `event` field, a `protocol` field
(`"v3"` or `"v4"` — which leg the event came from), and event-specific
fields, merged into the same object:

| `event` | Extra fields | Fires when |
| --- | --- | --- |
| `controller_connected` | `controllerId` | The panel (re)connects to V3 or V4 and gets a new controller id |
| `paired` | `deviceId` | A device pairs on V3, or an APP attaches on V4 |
| `bind_failed` | `code` (`"400"` \| `"401"`) | V3 rejects the panel's own bind attempt (V3 only) |
| `device_disconnected` | — | The paired V3 device, or the attached V4 APP, disconnects |
| `error` | `code` | The relay sends the panel a protocol `error` frame |
| `button_feedback` | `code` (0-9), `channel` (`"A"`\|`"B"`), `shape` | A shape button is tapped — V3's `feedback-*` or V4's `custom.action`, same mapping either way, see the [table above](#device-feedback-device--controller) |
| `device_status` | `strengthA`, `strengthB`, `softLimitA`, `softLimitB` (V3 only) | The device reports its current state |
| `relay_error` | `error` | The panel's own connection to that leg's local relay fails |
| `relay_disconnected` | — | The panel's connection to that leg's local relay drops, before it reconnects |

Anything else the panel logs (commands it sent, manual limit/webhook
changes, unclassified `notify`/`feedback` text) still fires the webhook with
just `message`/`timestamp` — no `event` field. See
[sequence-diagrams.md #4](sequence-diagrams.md#4-physical-button-press--webhook)
for the full path from device tap to delivered POST.
