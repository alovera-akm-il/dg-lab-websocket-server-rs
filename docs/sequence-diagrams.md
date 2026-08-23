# Sequence diagrams

These trace the exact message shapes exchanged on the wire (field names/types
verbatim from `src/v3/handler.rs`, `src/v3/protocol.rs`, `src/v4/handler.rs`),
not simplified summaries.

## 1. Panel startup and pairing (V3, via the control panel)

The panel is architecturally just another controller, on both protocols at
once — see
[architecture.md](architecture.md#why-the-panel-doesnt-share-the-v3-and-v4-hubs-in-process-state).
This trace covers its V3 leg; see [diagram 7](#7-panel-startup-and-pairing-v4-via-the-control-panel)
for the equivalent V4 leg, which runs independently and simultaneously.

```mermaid
sequenceDiagram
    participant Panel as Control panel<br/>(relay_client.rs)
    participant V3 as V3 relay :10002
    participant Browser as Operator's browser
    participant Phone as DG-LAB APP

    Note over Panel,V3: Panel connects like any V3 controller, no targetId
    Panel->>V3: WS connect ws://127.0.0.1:10002
    V3-->>Panel: {"type":"bind","clientId":"<uuid>","targetId":"","message":"targetId"}
    Panel->>Panel: PanelState.set_connected(clientId)<br/>status = waiting_for_device

    Browser->>Panel: GET / (loads assets/index.html)
    Browser->>Panel: GET /events (SSE)
    Panel-->>Browser: event: {status, controllerId, qrSvg, pairUrl, ...}

    Note over Browser,Phone: QR encodes ws://<lan-ip>:10002/<controllerId>,<br/>wrapped in the DG-LAB deep link
    Phone->>V3: WS connect ws://<lan-ip>:10002/<controllerId><br/>(path tail = targetId)
    V3->>V3: hub.pair(targetId=controllerId, clientId=phone)
    V3-->>Phone: {"type":"bind","clientId":"<uuid>",<br/>"targetId":"","message":"targetId"}
    V3-->>Panel: {"type":"bind","clientId":"<controllerId>",<br/>"targetId":"<phoneId>","message":"200"}
    V3-->>Phone: {"type":"bind","clientId":"<controllerId>",<br/>"targetId":"<phoneId>","message":"200"}
    Panel->>Panel: PanelState.set_paired(phoneId)<br/>status = paired
    Panel-->>Browser: SSE update (status=paired, deviceId set)
```

## 2. Strength command (increase / decrease / set)

```mermaid
sequenceDiagram
    participant Browser
    participant Panel as Panel handler.rs
    participant V3 as V3 relay
    participant Phone as DG-LAB APP

    Browser->>Panel: POST /api/strength<br/>{"channel":"A","op":"inc"}
    Panel->>Panel: strength_and_limit(A): check predicted<br/>value against configured upper limit
    alt would exceed configured limit
        Panel-->>Browser: 400 {"error":"channel A would reach..."}
    else within limit (or no baseline/limit yet)
        Panel->>V3: {"type":1,"clientId":"<controllerId>",<br/>"targetId":"<phoneId>","channel":"A","message":"set channel"}
        V3->>V3: hub.is_paired(...) check, normalize_channel
        V3->>Phone: {"type":"msg","clientId":"<controllerId>",<br/>"targetId":"<phoneId>","message":"strength-1+0+1"}
        Panel->>Panel: apply_optimistic_strength(A, current+1)
        Panel-->>Browser: 200 OK
    end

    Note over Phone,V3: Device periodically reports its real state
    Phone->>V3: {"type":"msg","clientId":"<phoneId>",<br/>"targetId":"<controllerId>","message":"strength-12+34+56+78"}
    V3->>Panel: forwarded verbatim (is_app_report_message)
    Panel->>Panel: parse_device_message -> set_device_strength<br/>(overwrites the optimistic guess with ground truth)
    Panel-->>Browser: SSE update (strengthA/B, softLimitA/B)
```

`type` 1/2/3 = increase/decrease/set (message `"strength-<channel>+<sendType>+<value>"`,
`sendType = type - 1`); `type` 4 with `message` containing `"clear"` clears a
channel and the app is notified with a Chinese `notify` frame (kept
byte-for-byte faithful to the reference spec — see
[api.md](api.md#wire-fidelity-note-two-chinese-strings)); a `time`/`strength`
outside those numeric types goes through `type` 3's "custom strength"
handling instead.

## 3. Pulse waveform (with an in-flight replacement)

```mermaid
sequenceDiagram
    participant Browser
    participant Panel
    participant V3
    participant Phone

    Browser->>Panel: POST /api/pulse<br/>{"channel":"A","time":3,"waveform":"A:[\"0A0A...\"]"}
    Panel->>V3: {"type":"clientMsg","clientId":"<controllerId>",<br/>"targetId":"<phoneId>","channel":"A","time":3,"message":"A:[...]"}

    alt no pulse already running on this client+channel
        V3->>Phone: {"type":"msg",...,"message":"pulse-A:[\"...\",\"...\"]"} (packet 1/N)
        loop every interval_ms until sequence exhausted
            V3->>Phone: {"type":"msg",...,"message":"pulse-A:[...]"} (packet k/N)
        end
        V3->>Panel: {"type":"notify","message":"发送完毕"}
        Note right of Panel: translated for display only:<br/>"Waveform sequence complete"
    else a pulse is already running on client+channel A
        V3->>Phone: {"type":"msg",...,"message":"clear-1"}
        V3->>Panel: {"type":"notify","message":"当前通道A有正在发送的消息，覆盖之前的消息"}
        Note right of Panel: translated: "Channel A already has a<br/>waveform in flight -- replacing it"
        V3->>V3: wait PULSE_REPLACE_DELAY_MS (150ms)
        V3->>Phone: new sequence starts (as in the top branch)
    end
```

## 4. Physical button press → webhook

```mermaid
sequenceDiagram
    participant Phone as DG-LAB APP
    participant V3
    participant Panel as relay_client.rs
    participant State as PanelState
    participant Hook as webhook.rs
    participant Endpoint as operator's HTTP endpoint
    participant Browser as Operator's browser

    Phone->>V3: {"type":"msg","clientId":"<phoneId>",<br/>"targetId":"<controllerId>","message":"feedback-7"}
    V3->>Panel: forwarded verbatim (is_app_report_message)
    Panel->>Panel: parse_action_message("feedback-7") -> 7<br/>decode_button_feedback(7) -> (channel="B", shape="square")
    Panel->>State: log_with("Button feedback: action 7",<br/>{"event":"button_feedback","code":7,"channel":"B","shape":"square"})
    State->>State: append to log ring buffer, notify_changed()
    State-->>Browser: SSE update (broadcast, via /events)
    State->>Hook: notify(webhook_url, message, extra)
    Hook->>Hook: tokio::spawn (fire-and-forget, 5s timeout)
    Hook->>Endpoint: POST {url}<br/>{"message":"Button feedback: action 7","timestamp":"...",<br/>"event":"button_feedback","code":7,"channel":"B","shape":"square"}
    Note right of Hook: On failure (timeout, connection refused, non-2xx):<br/>logged to server stdout only -- never re-enters<br/>PanelState.log(), so a broken endpoint can't loop
```

The channel/shape mapping is empirically determined, not from any official
spec — see the doc comment on `decode_button_feedback` in
`src/panel/relay_client.rs` and
[api.md — Device feedback](api.md#device-feedback-device--controller) for the
full table and how it was derived.

## 5. Relay disconnect and reconnect

```mermaid
sequenceDiagram
    participant Panel as relay_client.rs
    participant State as PanelState
    participant V3
    participant Browser as Operator's browser

    Note over Panel,V3: Either the device disconnects (V3 tears down<br/>the pairing) or the panel's own TCP connection drops
    V3--xPanel: WS close / connection error
    Panel->>State: set_disconnected()<br/>(clears controllerId, deviceId, strength, button action)
    State-->>Browser: SSE update (status=disconnected)
    Panel->>Panel: select! { reconnect_token.cancelled(), sleep(2s) }
    Note over Panel: A manual POST /api/reconnect cancels the token<br/>immediately instead of waiting out the 2s backoff
    Panel->>State: begin_connecting()<br/>status = connecting
    Panel->>V3: WS connect ws://127.0.0.1:10002 (fresh clientId)
    V3-->>Panel: {"type":"bind","clientId":"<new-uuid>","targetId":"","message":"targetId"}
    Note over Panel: New controllerId -> new pairing QR<br/>(any previously-paired phone must re-scan)
```

## 6. V4 relay (bare wire protocol — 1 controller : N devices, opaque payloads)

This trace shows the raw relay-level protocol, treating `data` as opaque
(which is all the relay itself ever does). For what a real DG-LAB 4
APP/controller actually puts inside `data` — and how the control panel
uses it — see [diagram 7](#7-panel-startup-and-pairing-v4-via-the-control-panel)
and [api.md's `data` schema section](api.md#the-data-schema-real-dg-lab-4-apps-use).

```mermaid
sequenceDiagram
    participant Controller as V4 controller
    participant V4 as V4 relay :10001
    participant Device as V4 device

    Controller->>V4: WS connect (no targetId)
    V4-->>Controller: {"type":"hello","clientId":"<4-byte hex>"}
    V4->>V4: register_controller, start idle timer<br/>(cancelled once a device attaches)

    Device->>V4: WS connect ?targetId=<controllerId>
    V4-->>Device: {"type":"hello","clientId":"<id>"}
    V4-->>Device: {"type":"controller_attached","clientId":"<controllerId>"}
    V4-->>Controller: {"type":"client_attached","clientId":"<deviceId>"}

    Controller->>V4: {"type":"message","clientId":"<deviceId>","data":{...}}
    V4->>Device: {"type":"message","data":{...}}

    Device->>V4: {"type":"message","data":{...}}
    V4->>Controller: {"type":"message","clientId":"<deviceId>","data":{...}}

    Note over Controller,V4: App-level ping/pong (JSON), independent<br/>of native WS ping/pong used for MAX_MISSED_WS_PONGS
    Controller->>V4: {"type":"ping"}
    V4-->>Controller: {"type":"pong","ts":<unix-ms>}
```

## 7. Panel startup and pairing (V4, via the control panel)

Runs independently and simultaneously with [diagram 1](#1-panel-startup-and-pairing-v3-via-the-control-panel)'s
V3 leg — the panel maintains both connections at once. `data` payloads here
follow the RPC schema `dglab-kit` documents for the real DG-LAB 4 APP (see
[api.md](api.md#the-data-schema-real-dg-lab-4-apps-use)), which is what the
panel implements in `src/panel/v4_commands.rs`/`v4_client.rs` — this is not
the bare opaque-`data` relay trace from diagram 6.

```mermaid
sequenceDiagram
    participant Panel as Control panel<br/>(v4_client.rs)
    participant V4 as V4 relay :10001
    participant App as DG-LAB 4 APP
    participant Browser as Operator's browser

    Panel->>V4: WS connect ws://127.0.0.1:10001&lt;prefix&gt;
    V4-->>Panel: {"type":"hello","clientId":"&lt;8-hex-char id&gt;"}
    Panel->>Panel: PanelState.v4_set_connected(clientId)<br/>v4_status = waiting_for_device

    Note over Panel,App: V4's QR/deep link encodes<br/>ws://&lt;lan-ip&gt;:10001&lt;prefix&gt;/?tid=&lt;v4 controller id&gt;
    App->>V4: WS connect ?tid=&lt;v4 controller id&gt;
    V4-->>App: {"type":"hello","clientId":"&lt;appId&gt;"}
    V4-->>App: {"type":"controller_attached","clientId":"&lt;v4 controller id&gt;"}
    V4-->>Panel: {"type":"client_attached","clientId":"&lt;appId&gt;"}
    Panel->>Panel: v4_set_app_attached(appId)

    App->>V4: {"type":"message","data":{"t":"ev","ev":"devices.snapshot",<br/>"devices":[{"slotId":"slot1","name":"Coyote","type":"COYOTE_030",<br/>"props":{"intensityA":0,"intensityB":0}}]}}
    V4-->>Panel: {"type":"message","clientId":"&lt;appId&gt;","data":{...}}
    Panel->>Panel: v4_set_device(slot1, "Coyote", 0, 0)<br/>v4_status = paired, active_protocol = V4
    Panel-->>Browser: SSE update (v4Status=paired,<br/>activeProtocol="v4", strengthA/B populated)

    Note over Browser,App: Operator clicks "+" on channel A
    Browser->>Panel: POST /api/strength {"channel":"A","op":"inc"}
    Panel->>V4: {"type":"message","clientId":"&lt;appId&gt;","data":{"t":"req","reqId":"1",<br/>"m":"device.op","data":{"s":"slot1","t":3,"c":0,"p":1,"v":1}}}
    V4->>App: {"type":"message","data":{"t":"req","reqId":"1",<br/>"m":"device.op","data":{"s":"slot1","t":3,"c":0,"p":1,"v":1}}}
    Note right of V4: The relay strips the outer clientId when<br/>forwarding controller -> device (a device only<br/>ever has one controller, so it isn't needed)
    Panel->>Panel: apply_optimistic_strength(A, 1)
    Panel-->>Browser: 200 OK

    Note over App,Panel: On-screen button tap
    App->>V4: {"type":"message","data":{"t":"ev","ev":"custom.action","action":2}}
    V4-->>Panel: {"type":"message","clientId":"&lt;appId&gt;","data":{...}}
    Panel->>Panel: decode_button_feedback(2) -> (A, square)<br/>set_button_action(V4, 2)

    Note over App,Panel: APP disconnects
    App--xV4: WS close
    V4-->>Panel: {"type":"client_disconnected","clientId":"&lt;appId&gt;"}
    Panel->>Panel: v4_clear_device()<br/>active_protocol falls back to V3 if still paired, else None
```
