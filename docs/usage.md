# Usage guide

How to actually run this and drive a device with it — as an operator using
the control panel, or as a developer integrating your own controller against
V3/V4 directly. For wire-level details see [api.md](api.md); for how the
pieces fit together see [architecture.md](architecture.md).

## Requirements

- Rust 1.97+ (`edition = "2024"`)
- A DG-LAB device and the DG-LAB APP on a phone, on the same local network as
  the machine running this server (needed for the control panel path; a
  direct V3/V4 integration just needs whatever client you're writing)

## 1. Run it

```bash
cargo run
```

or, for a release build:

```bash
cargo build --release
./target/release/dg-lab-websocket-server-rs
```

All three servers start together and the process exits with an error if any
one of them fails to bind (most commonly: a port already in use):

```text
ws://0.0.0.0:10001    # V4 relay
ws://0.0.0.0:10002    # V3 relay
http://0.0.0.0:40000  # control panel
```

Set `VERBOSE=true` for debug-level logging of every frame in and out —
useful while integrating a new client or diagnosing a pairing issue:

```bash
VERBOSE=true cargo run
```

## 2. Operating a device through the control panel (the common path)

This is the path that needs no protocol knowledge at all.

1. Start the server (step 1 above) on a machine on the same Wi-Fi network as
   the phone.
2. From any device on that network (a laptop, or the phone itself), open
   `http://<that machine's LAN IP>:40000/`.
   - If you open it as `http://localhost:40000` on the same machine the
     server runs on, the panel automatically falls back to its
     auto-detected LAN IP for the QR code (a QR encoding `localhost` would be
     useless to a phone on Wi-Fi) — logged at startup as `detected LAN IP ...`.
     Override with `PANEL_PUBLIC_WS_BASE=ws://<the-right-ip>` if that
     autodetection ever picks the wrong network interface (e.g. VPN, multiple
     NICs).
3. The page shows two QR codes as soon as the panel finishes connecting to
   its own local V3 and V4 relays (near-instant, both at once) — **V4**
   (recommended) and **V3** (legacy). Open the DG-LAB APP and scan whichever
   matches your APP version — or, if scanning isn't convenient, tap the
   plain link shown under it, the same DG-LAB APP deep link the QR encodes.
   You don't need to know in advance which protocol your APP speaks; both
   are always available.
4. Once the APP connects on either protocol, that leg's status flips to
   **paired**, a badge marks it **active**, and the strength/pulse controls
   become live, driving that device.
5. Use the **Strength** card to increase/decrease/set an exact value per
   channel, optionally with a per-channel **upper limit** — an
   application-level safety cap the panel enforces on its own commands
   (neither protocol has a remote command to change the device's own
   configured limit, so this is not the same as the device's own limit,
   shown read-only next to it on V3). On V4, "Set" is emulated as a
   relative adjustment from the last known strength (V4's wire protocol has
   no "set to an exact value" command) — it's rejected if no baseline is
   known yet.
6. Use the **Pulse waveform** card to trigger a waveform: pick one of the 44
   bundled presets (sourced and verified against `dglab-kit`'s official
   waveform library — see [`src/panel/presets.rs`](../src/panel/presets.rs)
   for provenance) or paste your own frame data into the custom field. On
   V4, custom/preset text must be in the `"<prefix>:[...]"` frame-array
   format (no raw-legacy-string fallback, unlike V3).
7. The **Log** card shows a live, scrolling record of every pairing/command/
   feedback event on both protocols — the same events the
   [webhook](#5-getting-notified-of-events-webhook) fires on.
8. If a phone loses connection or you want to re-pair (e.g. a new session),
   hit **Reconnect both** to force fresh controller ids and QRs on both
   protocols without restarting the process.

If the currently-active device disconnects, the panel automatically hands
control to the other protocol's device if one is still paired there;
otherwise the controls disable and that protocol's own status drops back to
"waiting for device", then shortly gets a brand-new controller id (and thus
a new QR) once its relay connection is re-established. This is expected —
see [sequence-diagrams.md #5](sequence-diagrams.md#5-relay-disconnect-and-reconnect)
(V3) and [#7](sequence-diagrams.md#7-panel-startup-and-pairing-v4-via-the-control-panel)
(V4), and `src/panel/state.rs`'s module docs for exactly how the two
protocols are reconciled into one set of controls.

## 3. Integrating your own controller against V3 directly

Use this if you're building your own app/script that should drive a device,
instead of using the bundled panel. Full message reference:
[api.md — V3 relay](api.md#v3-relay-websocket).

Minimal flow:

1. Open a WebSocket to `ws://<host>:10002/` (no target — you become the
   "web"/controller side). You'll immediately receive
   `{"type":"bind","clientId":"<your-id>","targetId":"","message":"targetId"}`.
2. Have the DG-LAB APP connect to `ws://<host>:10002/<your-id>` (path tail
   form — this is exactly what the panel's QR encodes) or send an explicit
   `bind` frame naming both ids yourself.
3. On successful pairing, both sides get
   `{"type":"bind","clientId":"<your-id>","targetId":"<device-id>","message":"200"}`.
4. Send strength/clear/pulse commands (`type` 1–4, `clientMsg`) addressed
   `clientId: <your-id>`, `targetId: <device-id>`.
5. Watch for `feedback-<n>` (button presses) and `strength-<a>+<b>+<softA>+<softB>`
   (status reports) forwarded from the device, and `break`/`error` frames.

`tests/v3_integration.rs` and `tests/panel_webhook_integration.rs` are
working, runnable examples of this exact flow driven by a real
`tokio-tungstenite` client — read those if you want copy-pasteable code
rather than a written description.

## 4. Integrating your own controller against V4 directly

V4 is the newer, simpler, N-device protocol, and the one the control panel
itself uses to drive a real DG-LAB 4 APP. Two ways to use it:

**Bare relay protocol** — payloads under `data` are opaque to the relay, so
this is a good fit if you're relaying between your *own* controller and
device implementations, where you control both ends' payload format. Full
reference: [api.md — V4 relay](api.md#v4-relay-websocket).

1. Open a WebSocket to `ws://<host>:10001/` (or under `PREFIX` if set) with
   no query params → you're a controller, and receive
   `{"type":"hello","clientId":"<id>"}`.
2. Have a device connect to `ws://<host>:10001/?targetId=<controller-id>`.
3. Controller → device: `{"type":"message","clientId":"<device-id>","data":{...}}`.
   Device → controller: `{"type":"message","data":{...}}` (arrives at the
   controller tagged with the sending device's id).

`tests/v4_integration.rs` is a working example of this bare protocol.

**Driving a real DG-LAB 4 APP** — the APP speaks a specific RPC schema
inside `data`, documented by `dglab-kit` (the official SDK) and implemented
by the panel itself in `src/panel/v4_commands.rs`/`v4_client.rs`. Full
reference: [api.md — the `data` schema real DG-LAB 4 APPs use](api.md#the-data-schema-real-dg-lab-4-apps-use).
In short: `device.op` RPC requests to control strength/waveforms by
`slotId`, `devices.snapshot`/`devices.patch`/`slots.patch` events to learn
what devices are attached, `custom.action` for button presses.
`tests/panel_v4_integration.rs` is a complete working example of this exact
flow — a real V4 relay, a real panel, and a simulated APP exchanging the
full handshake and command set, worth reading directly if you want
copy-pasteable code.

## 5. Getting notified of events (webhook)

If you want to react to panel events from another system (a notification
bot, a logging pipeline, a scene-trigger service) without polling `/events`,
point the panel at an HTTP endpoint:

```bash
PANEL_WEBHOOK_URL=https://example.com/dg-lab-hook cargo run
```

or set/change/clear it live from the panel's **Webhook** card, or via
`POST /api/webhook {"url": "https://..."}`. It fires on events from both
protocols. Every payload always has `message`/`timestamp`; classifiable
events (pairing, button feedback, device status, errors, ...) add a
`protocol` field (`"v3"`/`"v4"`) plus event-specific fields on top — full
table and payload shapes in [api.md — Webhook payloads](api.md#webhook-payloads).
Delivery is fire-and-forget with a 5s timeout and failures only reach the
server's own stdout, so a flaky or offline receiver never blocks or breaks
anything else the panel is doing.

## 6. Configuration reference

Every environment variable, with defaults — see the README's
[Configuration](../README.md#configuration) section for the authoritative,
up-to-date table (kept there rather than duplicated here, since it's the
first thing read for day-to-day operation).

## 7. Testing your changes

```bash
cargo build
cargo test                   # unit + integration tests
cargo clippy --all-targets
cargo doc --no-deps --open   # browse rustdoc for every module
```

See the README's [Testing](../README.md#testing) section for what each test
suite actually covers.
