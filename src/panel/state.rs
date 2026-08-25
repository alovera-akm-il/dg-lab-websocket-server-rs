//! Shared control-panel state: connection status/ids for both the V3 and
//! V4 relay connections the panel maintains, which of the two currently
//! has a controllable device attached (see [`Protocol`]), the live
//! outbound sender for each, and a capped event log -- plus a broadcast
//! channel that fires whenever any of it changes, driving the `/events`
//! SSE stream.
//!
//! **Design note on running both protocols at once:** the panel connects
//! to V3 and V4 independently and simultaneously (see [`super::relay_client`]
//! and [`super::v4_client`]), so a phone could in principle pair via
//! either QR at any time. Only one device drives the shared strength/
//! pulse controls at a time -- `active_protocol` (see [`Protocol`]) -- and it's
//! simply "whichever leg most recently got a controllable device", not a
//! merge of both. If a leg that *wasn't* active loses its device, nothing
//! changes; if the *active* leg loses its device, the other leg takes
//! over if it still has one (its strength/soft-limit will show as
//! unknown until that leg's next status report re-populates them, since
//! only the active leg's values are kept in the shared fields below).
//! This is a deliberate simplification -- real usage pairs one device at
//! a time -- rather than tracking two full parallel strength/soft-limit
//! snapshots for a case that in practice won't happen.

use std::collections::{HashMap, VecDeque};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::v3::protocol::Channel;

use super::button_map::{self, ButtonAction, ButtonMap};
use super::event_log::{EventLogConfig, EventLogMsg};
use super::playlist::{self, PlaylistEntry, PlaylistQueue, PlaylistSnapshot};
use super::ramp::{RampProfile, RampSnapshot};
use super::recipe::{self, Recipe};
use super::session::{self, SessionConfig, SessionSnapshot, SessionTimer};
use super::templates::{self, Template};
use super::webhook;

const LOG_CAPACITY: usize = 200;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Connecting,
    WaitingForDevice,
    Paired,
    Disconnected,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Connecting => "connecting",
            Status::WaitingForDevice => "waiting_for_device",
            Status::Paired => "paired",
            Status::Disconnected => "disconnected",
        }
    }
}

/// Which relay leg is currently driving the shared strength/pulse
/// controls. See the module docs for how this is decided.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Protocol {
    V3,
    V4,
}

impl Protocol {
    pub fn as_str(self) -> &'static str {
        match self {
            Protocol::V3 => "v3",
            Protocol::V4 => "v4",
        }
    }
}

/// Extra Coyote-specific telemetry from V4's `props`/`slotState`, beyond
/// the channel intensities every device type reports -- see
/// `v4_client::extract_health`. `channel_*_status`/`channel_*_overheat*`
/// are documented by `dglab-kit` as Coyote-only (`COYOTE_020`/`COYOTE_030`),
/// so they stay `None` for other device types, same as any field a given
/// `props`/`slotState` payload simply didn't include this tick.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeviceHealth {
    /// `props.power`, 0-100.
    pub battery: Option<i64>,
    /// `props.channelAStatus`/`channelBStatus`, 0-4 (no output / open
    /// circuit / normal / damaged / masked) -- see
    /// `v4_client::channel_status_label` for the decoded meaning.
    pub channel_a_status: Option<i64>,
    pub channel_b_status: Option<i64>,
    /// `slotState.channelA/B.comfortLimit.overheat`/`overheatPercent`.
    pub channel_a_overheat: Option<bool>,
    pub channel_b_overheat: Option<bool>,
    pub channel_a_overheat_pct: Option<i64>,
    pub channel_b_overheat_pct: Option<i64>,
}

/// A subjective check-in logged via `POST /api/session/checkin`
/// (Feature 7) -- the most recent one is kept for the `/events`
/// snapshot's `lastCheckIn`; every one is also logged/webhooked via
/// `PanelState::log_with`, same as any other event.
#[derive(Debug, Clone)]
pub struct CheckIn {
    pub timestamp: String,
    pub color: String,
    pub arousal: i64,
    pub discomfort: String,
    pub notes: Option<String>,
}

/// A `normal` <-> issue transition on one channel's electrode contact
/// status (Feature 8), detected by comparing consecutive V4 health
/// reports -- see `detect_contact_transition`. `status` is `"entered"`
/// (just became an issue) or `"resolved"` (issue cleared); `issue` is
/// `"normal"` for a `"resolved"` transition, or the specific problem
/// (`contact_issue_str`) for an `"entered"` one.
pub struct ContactTransition {
    pub channel: Channel,
    pub status: &'static str,
    pub issue: &'static str,
}

struct Inner {
    // -- V3 leg --
    status: Status,
    controller_id: Option<String>,
    device_id: Option<String>,
    outbound: Option<mpsc::UnboundedSender<WsMessage>>,

    // -- V4 leg --
    v4_status: Status,
    v4_controller_id: Option<String>,
    /// The attached APP's own V4 connection id (`clientId`) -- needed to
    /// address the wire envelope (`{"type":"message","clientId":...}`).
    v4_device_id: Option<String>,
    /// The specific device's `slotId` inside that APP -- needed to
    /// address `device.op`'s `s` field. Distinct from `v4_device_id`:
    /// one APP connection can (in principle) expose multiple devices,
    /// though the panel only ever tracks the first one it sees.
    v4_device_slot_id: Option<String>,
    v4_device_name: Option<String>,
    v4_outbound: Option<mpsc::UnboundedSender<WsMessage>>,

    /// See the module docs.
    active_protocol: Option<Protocol>,

    /// Parsed from the active leg's device status reports (V3:
    /// `strength-<a>+<b>+<softLimitA>+<softLimitB>`; V4: `props.intensityA`/
    /// `intensityB` from `devices.snapshot`/`devices.patch`/`slots.patch` --
    /// V4 has no single documented soft-limit field, so `soft_limit_a/b`
    /// stay `None` on that leg).
    strength_a: Option<i64>,
    strength_b: Option<i64>,
    soft_limit_a: Option<i64>,
    soft_limit_b: Option<i64>,
    /// The most recent physical/on-screen button press from the active
    /// leg (V3: `feedback-<n>`; V4: `custom.action`, `0`-`9`, documented
    /// by dglab-kit as the same concept as V3's `feedback-*`).
    last_button_action: Option<i64>,

    /// See [`DeviceHealth`] -- V4-only, mirroring how `soft_limit_a/b`
    /// above are V3-only (each protocol reports what the other doesn't).
    battery: Option<i64>,
    channel_a_status: Option<i64>,
    channel_b_status: Option<i64>,
    channel_a_overheat: Option<bool>,
    channel_b_overheat: Option<bool>,
    channel_a_overheat_pct: Option<i64>,
    channel_b_overheat_pct: Option<i64>,
    /// Debounce timestamps for Feature 8's `contact_issue` alerts -- see
    /// `detect_contact_transition`. Reset alongside the status fields
    /// above whenever `clear_health` runs, so a fresh device attachment
    /// starts with a clean debounce window.
    contact_alert_at_a: Option<Instant>,
    contact_alert_at_b: Option<Instant>,

    /// Operator-configured safety cap: the panel refuses to send any
    /// Inc/Set command that would push a channel's strength above this.
    /// `None` means no cap. This is a *panel-side* application-level
    /// guard, protocol-agnostic, and survives reconnects.
    limit_a: Option<i64>,
    limit_b: Option<i64>,
    /// URL to POST an event notification to on every log-worthy event
    /// (see [`webhook`]). Operator configuration, like the limits above
    /// -- survives reconnects, seeded at startup from `PANEL_WEBHOOK_URL`.
    webhook_url: Option<String>,

    log: VecDeque<String>,

    /// Per-channel pulse playlists -- queue contents/settings and
    /// playback position, guarded by this same lock like everything else
    /// above. See [`playlist`] for the state machine itself; playback's
    /// cancellation tokens live outside `Inner` (see `playlist_token_a`/
    /// `_b` below), mirroring `reconnect`/`v4_reconnect`.
    playlist_a: PlaylistQueue,
    playlist_b: PlaylistQueue,

    /// Per-channel active strength ramp, if any -- `None` means no ramp
    /// is running on that channel. Like playlists, this is a read-only
    /// snapshot updated in place by the runner task (see
    /// `super::ramp_runner`); there's no pause/resume, only start/stop
    /// (see `ramp_token_a`/`_b` below), so unlike playlists there's no
    /// captured "remaining" state to restore on resume.
    ramp_a: Option<RampSnapshot>,
    ramp_b: Option<RampSnapshot>,

    /// Panel-wide session timer -- unlike playlists/ramps, there's only
    /// ever one, not one per channel. See [`super::session`] for the
    /// state machine itself; playback's cancellation token lives
    /// outside `Inner` (see `session_token` below), mirroring
    /// `playlist_token_a`/`_b`.
    session: SessionTimer,

    /// Named, reusable playlist definitions -- unlike everything else in
    /// `Inner`, this one survives a process restart (see
    /// [`super::templates`]/[`super::persistence`]); every mutating
    /// method below writes the whole map back to disk right after
    /// updating it in memory.
    templates: HashMap<String, Template>,

    /// Configurable button mapping -- like templates, survives a
    /// process restart (see [`super::button_map`]/[`super::persistence`]).
    button_map: ButtonMap,

    /// Named session presets -- like templates, survives a process
    /// restart (see [`super::recipe`]/[`super::persistence`]).
    recipes: HashMap<String, Recipe>,

    /// Most recent subjective check-in (Feature 7) -- session-scoped,
    /// like the log/webhook events it's logged alongside, not persisted
    /// to disk.
    last_check_in: Option<CheckIn>,
}

pub struct Snapshot {
    pub status: Status,
    pub controller_id: Option<String>,
    pub device_id: Option<String>,
    pub v4_status: Status,
    pub v4_controller_id: Option<String>,
    pub v4_device_id: Option<String>,
    pub v4_device_slot_id: Option<String>,
    pub v4_device_name: Option<String>,
    pub active_protocol: Option<Protocol>,
    pub strength_a: Option<i64>,
    pub strength_b: Option<i64>,
    pub soft_limit_a: Option<i64>,
    pub soft_limit_b: Option<i64>,
    pub last_button_action: Option<i64>,
    pub battery: Option<i64>,
    pub channel_a_status: Option<i64>,
    pub channel_b_status: Option<i64>,
    pub channel_a_overheat: Option<bool>,
    pub channel_b_overheat: Option<bool>,
    pub channel_a_overheat_pct: Option<i64>,
    pub channel_b_overheat_pct: Option<i64>,
    pub limit_a: Option<i64>,
    pub limit_b: Option<i64>,
    pub webhook_url: Option<String>,
    pub log: Vec<String>,
    pub playlist_a: PlaylistSnapshot,
    pub playlist_b: PlaylistSnapshot,
    pub ramp_a: Option<RampSnapshot>,
    pub ramp_b: Option<RampSnapshot>,
    pub session_timer: Option<SessionSnapshot>,
    pub last_check_in: Option<CheckIn>,
}

pub struct PanelState {
    inner: Mutex<Inner>,
    /// Fires (with no payload -- subscribers just re-read a fresh
    /// `snapshot()`) whenever status, ids, feedback, or the log change.
    changed: broadcast::Sender<()>,
    reconnect: Mutex<CancellationToken>,
    v4_reconnect: Mutex<CancellationToken>,
    /// Cancelling one of these interrupts that channel's playlist runner
    /// task, if one is currently alive (which is exactly when that
    /// channel's playlist is `Playing` -- see the `playlist_*` methods
    /// below). A fresh token is issued every time playback (re)starts,
    /// same pattern as `reconnect`/`v4_reconnect`.
    playlist_token_a: Mutex<CancellationToken>,
    playlist_token_b: Mutex<CancellationToken>,
    /// Same pattern as `playlist_token_a`/`_b`, for the ramp runner --
    /// see `Inner::ramp_a`/`_b`'s docs.
    ramp_token_a: Mutex<CancellationToken>,
    ramp_token_b: Mutex<CancellationToken>,
    /// Same pattern, for the panel-wide session timer -- see
    /// `Inner::session`'s docs. Singular, not per-channel.
    session_token: Mutex<CancellationToken>,
    /// Channel to the event-log writer task (`event_log::run`), if one
    /// has been wired up -- `None` for a `PanelState` constructed
    /// directly by a test that never calls
    /// `install_event_log_sender`, in which case `log_with` simply
    /// skips the file-log sink (there's no file-log behavior under test
    /// at that level anyway). Set once, at startup, by `panel::build()`.
    event_log_tx: Mutex<Option<mpsc::UnboundedSender<EventLogMsg>>>,
}

impl PanelState {
    pub fn new() -> Self {
        let (changed, _) = broadcast::channel(256);
        PanelState {
            inner: Mutex::new(Inner {
                status: Status::Connecting,
                controller_id: None,
                device_id: None,
                outbound: None,
                v4_status: Status::Connecting,
                v4_controller_id: None,
                v4_device_id: None,
                v4_device_slot_id: None,
                v4_device_name: None,
                v4_outbound: None,
                active_protocol: None,
                strength_a: None,
                strength_b: None,
                soft_limit_a: None,
                soft_limit_b: None,
                last_button_action: None,
                battery: None,
                channel_a_status: None,
                channel_b_status: None,
                channel_a_overheat: None,
                channel_b_overheat: None,
                channel_a_overheat_pct: None,
                channel_b_overheat_pct: None,
                contact_alert_at_a: None,
                contact_alert_at_b: None,
                limit_a: None,
                limit_b: None,
                webhook_url: None,
                log: VecDeque::new(),
                playlist_a: PlaylistQueue::new(),
                playlist_b: PlaylistQueue::new(),
                ramp_a: None,
                ramp_b: None,
                session: SessionTimer::new(),
                templates: templates::load_all(),
                button_map: button_map::load(),
                recipes: recipe::load_all(),
                last_check_in: None,
            }),
            changed,
            reconnect: Mutex::new(CancellationToken::new()),
            v4_reconnect: Mutex::new(CancellationToken::new()),
            playlist_token_a: Mutex::new(CancellationToken::new()),
            playlist_token_b: Mutex::new(CancellationToken::new()),
            ramp_token_a: Mutex::new(CancellationToken::new()),
            ramp_token_b: Mutex::new(CancellationToken::new()),
            session_token: Mutex::new(CancellationToken::new()),
            event_log_tx: Mutex::new(None),
        }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<()> {
        self.changed.subscribe()
    }

    pub fn snapshot(&self) -> Snapshot {
        let inner = self.inner.lock().unwrap();
        Snapshot {
            status: inner.status,
            controller_id: inner.controller_id.clone(),
            device_id: inner.device_id.clone(),
            v4_status: inner.v4_status,
            v4_controller_id: inner.v4_controller_id.clone(),
            v4_device_id: inner.v4_device_id.clone(),
            v4_device_slot_id: inner.v4_device_slot_id.clone(),
            v4_device_name: inner.v4_device_name.clone(),
            active_protocol: inner.active_protocol,
            strength_a: inner.strength_a,
            strength_b: inner.strength_b,
            soft_limit_a: inner.soft_limit_a,
            soft_limit_b: inner.soft_limit_b,
            last_button_action: inner.last_button_action,
            battery: inner.battery,
            channel_a_status: inner.channel_a_status,
            channel_b_status: inner.channel_b_status,
            channel_a_overheat: inner.channel_a_overheat,
            channel_b_overheat: inner.channel_b_overheat,
            channel_a_overheat_pct: inner.channel_a_overheat_pct,
            channel_b_overheat_pct: inner.channel_b_overheat_pct,
            limit_a: inner.limit_a,
            limit_b: inner.limit_b,
            webhook_url: inner.webhook_url.clone(),
            log: inner.log.iter().cloned().collect(),
            playlist_a: inner.playlist_a.snapshot(),
            playlist_b: inner.playlist_b.snapshot(),
            ramp_a: inner.ramp_a,
            ramp_b: inner.ramp_b,
            session_timer: inner.session.snapshot(),
            last_check_in: inner.last_check_in.clone(),
        }
    }

    pub fn outbound(&self) -> Option<mpsc::UnboundedSender<WsMessage>> {
        self.inner.lock().unwrap().outbound.clone()
    }

    pub fn v4_outbound(&self) -> Option<mpsc::UnboundedSender<WsMessage>> {
        self.inner.lock().unwrap().v4_outbound.clone()
    }

    /// The command endpoints need to know which leg is active, and its
    /// addressing ids, in one shot.
    pub fn active_target(&self) -> Option<ActiveTarget> {
        let inner = self.inner.lock().unwrap();
        match inner.active_protocol? {
            Protocol::V3 => Some(ActiveTarget::V3 {
                controller_id: inner.controller_id.clone()?,
                device_id: inner.device_id.clone()?,
            }),
            Protocol::V4 => Some(ActiveTarget::V4 {
                device_id: inner.v4_device_id.clone()?,
                slot_id: inner.v4_device_slot_id.clone()?,
            }),
        }
    }

    fn notify_changed(&self) {
        let _ = self.changed.send(());
    }

    /// Appends a log line, notifies SSE subscribers, and -- if a webhook
    /// URL is configured -- fires a notification for it. Equivalent to
    /// `log_with(line, Value::Null)`; every log call in the panel goes
    /// through one of these two, which is what makes the webhook fire on
    /// "everything the panel already logs" without a second parallel
    /// classification pass.
    pub fn log(&self, line: impl Into<String>) {
        self.log_with(line, Value::Null);
    }

    /// Like [`Self::log`], but merges `extra` into the webhook payload
    /// (ignored if `Value::Null`) for event types the caller can already
    /// classify -- e.g. `{"event":"button_feedback","channel":"A",...}`.
    pub fn log_with(&self, line: impl Into<String>, extra: Value) {
        let line = line.into();
        let webhook_url = {
            let mut inner = self.inner.lock().unwrap();
            if inner.log.len() >= LOG_CAPACITY {
                inner.log.pop_front();
            }
            inner.log.push_back(line.clone());
            inner.webhook_url.clone()
        };
        self.notify_changed();
        webhook::notify(webhook_url.as_deref(), &line, extra.clone());
        self.event_log_append(line, extra);
    }

    pub fn set_webhook_url(&self, url: Option<String>) {
        {
            self.inner.lock().unwrap().webhook_url = url;
        }
        self.notify_changed();
    }

    // ---- V3 leg -----------------------------------------------------

    /// Called at the start of each V3 relay connection attempt.
    pub fn begin_connecting(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.status = Status::Connecting;
            inner.controller_id = None;
            inner.device_id = None;
            inner.outbound = None;
            deactivate_if_active(&mut inner, Protocol::V3);
        }
        self.notify_changed();
    }

    /// Our own `bind`/`targetId` frame arrived -- we now have a
    /// controller id and a live outbound sender.
    pub fn set_connected(&self, controller_id: String, outbound: mpsc::UnboundedSender<WsMessage>) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.controller_id = Some(controller_id);
            inner.outbound = Some(outbound);
            inner.status = Status::WaitingForDevice;
        }
        self.notify_changed();
    }

    /// A device paired with us (`bind`/`200`).
    pub fn set_paired(&self, device_id: String) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.device_id = Some(device_id);
            inner.status = Status::Paired;
            activate(&mut inner, Protocol::V3);
        }
        self.notify_changed();
    }

    /// The paired device disconnected (`break`), or our relay connection
    /// itself dropped.
    pub fn clear_device(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.device_id = None;
            if inner.status == Status::Paired {
                inner.status = Status::WaitingForDevice;
            }
            deactivate_if_active(&mut inner, Protocol::V3);
        }
        self.notify_changed();
    }

    /// Drops the stored outbound sender clone without touching status or
    /// ids. Must be called before awaiting the relay connection's writer
    /// task to drain: that task only exits once every clone of its
    /// sender (including this one) is dropped, and this is the clone the
    /// rest of the app reaches for -- without clearing it first, the
    /// task that's supposed to observe the drain would be waiting on
    /// itself.
    pub fn clear_outbound(&self) {
        self.inner.lock().unwrap().outbound = None;
    }

    pub fn set_disconnected(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.status = Status::Disconnected;
            inner.controller_id = None;
            inner.device_id = None;
            inner.outbound = None;
            deactivate_if_active(&mut inner, Protocol::V3);
        }
        self.notify_changed();
    }

    /// Token the active V3 relay connection should select against to know
    /// when it's been asked to drop and reconnect immediately.
    pub fn reconnect_token(&self) -> CancellationToken {
        self.reconnect.lock().unwrap().clone()
    }

    /// Cancels the current V3 reconnect token (waking any active
    /// connection waiting on it) and installs a fresh one for the next
    /// attempt.
    pub fn request_reconnect(&self) {
        let mut guard = self.reconnect.lock().unwrap();
        guard.cancel();
        *guard = CancellationToken::new();
    }

    // ---- V4 leg -------------------------------------------------------

    /// Called at the start of each V4 relay connection attempt.
    pub fn v4_begin_connecting(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.v4_status = Status::Connecting;
            inner.v4_controller_id = None;
            inner.v4_device_id = None;
            inner.v4_device_slot_id = None;
            inner.v4_device_name = None;
            inner.v4_outbound = None;
            deactivate_if_active(&mut inner, Protocol::V4);
        }
        self.notify_changed();
    }

    /// Our own `hello` frame arrived -- we now have a controller id and a
    /// live outbound sender.
    pub fn v4_set_connected(
        &self,
        controller_id: String,
        outbound: mpsc::UnboundedSender<WsMessage>,
    ) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.v4_controller_id = Some(controller_id);
            inner.v4_outbound = Some(outbound);
            inner.v4_status = Status::WaitingForDevice;
        }
        self.notify_changed();
    }

    /// An APP attached (`client_attached`) -- tracked, but not yet
    /// "paired" for UI/active-leg purposes until it exposes a
    /// controllable device (see [`Self::v4_set_device`]).
    pub fn v4_set_app_attached(&self, device_id: String) {
        {
            self.inner.lock().unwrap().v4_device_id = Some(device_id);
        }
        self.notify_changed();
    }

    /// The attached APP reported a device we can control (from
    /// `devices.snapshot`/`devices.patch.added`) -- the panel only ever
    /// tracks the first device it sees per APP. Returns any Feature 8
    /// `contact_issue` transitions this health report triggered, for the
    /// caller (`v4_client.rs`) to log after the lock is released.
    pub fn v4_set_device(
        &self,
        slot_id: String,
        name: String,
        strength_a: Option<i64>,
        strength_b: Option<i64>,
        health: DeviceHealth,
    ) -> Vec<ContactTransition> {
        let mut transitions = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            inner.v4_device_slot_id = Some(slot_id);
            inner.v4_device_name = Some(name);
            inner.v4_status = Status::Paired;
            activate(&mut inner, Protocol::V4);
            if inner.active_protocol == Some(Protocol::V4) {
                if let Some(a) = strength_a {
                    inner.strength_a = Some(a);
                }
                if let Some(b) = strength_b {
                    inner.strength_b = Some(b);
                }
                transitions = detect_contact_transitions(&mut inner, &health);
                apply_health(&mut inner, health);
            }
        }
        self.notify_changed();
        transitions
    }

    /// The tracked device's props changed (`slots.patch`) -- only applied
    /// if V4 is the active leg and the patch is for the device we track.
    /// Same `ContactTransition` return contract as [`Self::v4_set_device`].
    pub fn v4_update_device(
        &self,
        slot_id: &str,
        strength_a: Option<i64>,
        strength_b: Option<i64>,
        health: DeviceHealth,
    ) -> Vec<ContactTransition> {
        let mut transitions = Vec::new();
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.active_protocol == Some(Protocol::V4)
                && inner.v4_device_slot_id.as_deref() == Some(slot_id)
            {
                if let Some(a) = strength_a {
                    inner.strength_a = Some(a);
                }
                if let Some(b) = strength_b {
                    inner.strength_b = Some(b);
                }
                transitions = detect_contact_transitions(&mut inner, &health);
                apply_health(&mut inner, health);
            }
        }
        self.notify_changed();
        transitions
    }

    /// The attached APP (and whatever device it exposed) disconnected
    /// (`client_disconnected`), or our relay connection itself dropped.
    pub fn v4_clear_device(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.v4_device_id = None;
            inner.v4_device_slot_id = None;
            inner.v4_device_name = None;
            if inner.v4_status == Status::Paired {
                inner.v4_status = Status::WaitingForDevice;
            }
            deactivate_if_active(&mut inner, Protocol::V4);
        }
        self.notify_changed();
    }

    pub fn v4_clear_outbound(&self) {
        self.inner.lock().unwrap().v4_outbound = None;
    }

    pub fn v4_set_disconnected(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.v4_status = Status::Disconnected;
            inner.v4_controller_id = None;
            inner.v4_device_id = None;
            inner.v4_device_slot_id = None;
            inner.v4_device_name = None;
            inner.v4_outbound = None;
            deactivate_if_active(&mut inner, Protocol::V4);
        }
        self.notify_changed();
    }

    /// Records the most recent physical/on-screen button press reported
    /// by the active leg's device (V3 `feedback-<n>` / V4 `custom.action`).
    /// Ignored if reported by a leg that isn't currently active, so a
    /// stray report from an inactive secondary connection can't overwrite
    /// what the UI is showing for the device actually being driven.
    pub fn set_button_action(&self, protocol: Protocol, action: i64) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.active_protocol == Some(protocol) {
                inner.last_button_action = Some(action);
            }
        }
        self.notify_changed();
    }

    pub fn v4_reconnect_token(&self) -> CancellationToken {
        self.v4_reconnect.lock().unwrap().clone()
    }

    pub fn v4_request_reconnect(&self) {
        let mut guard = self.v4_reconnect.lock().unwrap();
        guard.cancel();
        *guard = CancellationToken::new();
    }

    // ---- shared (protocol-agnostic) ------------------------------------

    /// Updates the parsed per-channel strength/soft-limit values from the
    /// V3 device's status report (`strength-<a>+<b>+<softLimitA>+<softLimitB>`).
    /// Ignored if V3 isn't currently the active leg (see
    /// [`Self::set_button_action`]'s doc for why).
    pub fn set_device_strength(
        &self,
        strength_a: i64,
        strength_b: i64,
        soft_limit_a: i64,
        soft_limit_b: i64,
    ) {
        {
            let mut inner = self.inner.lock().unwrap();
            if inner.active_protocol == Some(Protocol::V3) {
                inner.strength_a = Some(strength_a);
                inner.strength_b = Some(strength_b);
                inner.soft_limit_a = Some(soft_limit_a);
                inner.soft_limit_b = Some(soft_limit_b);
            }
        }
        self.notify_changed();
    }

    /// Sets (or, with `None`, clears) the operator-configured safety cap
    /// for a channel. Persists across relay reconnects -- see the field
    /// docs on `Inner::limit_a`/`limit_b`.
    pub fn set_limit(&self, channel: Channel, value: Option<i64>) {
        {
            let mut inner = self.inner.lock().unwrap();
            match channel {
                Channel::A => inner.limit_a = value,
                Channel::B => inner.limit_b = value,
            }
        }
        self.notify_changed();
    }

    /// The best current guess at a channel's strength (from whichever leg
    /// is active) and the configured safety cap, if any -- what the
    /// strength-command handler needs to decide whether a requested
    /// Inc/Set would exceed it.
    pub fn strength_and_limit(&self, channel: Channel) -> (Option<i64>, Option<i64>) {
        let inner = self.inner.lock().unwrap();
        match channel {
            Channel::A => (inner.strength_a, inner.limit_a),
            Channel::B => (inner.strength_b, inner.limit_b),
        }
    }

    /// Optimistically updates the panel's own best-guess of a channel's
    /// current strength right after successfully sending a command that
    /// changes it -- keeps the upper-limit check useful across a rapid
    /// sequence of Inc/Set clicks without waiting for the device's own
    /// (possibly infrequent, possibly racy) status echo. A real status
    /// report always overwrites this when one arrives.
    pub fn apply_optimistic_strength(&self, channel: Channel, value: i64) {
        {
            let mut inner = self.inner.lock().unwrap();
            match channel {
                Channel::A => inner.strength_a = Some(value),
                Channel::B => inner.strength_b = Some(value),
            }
        }
        self.notify_changed();
    }

    /// Resolves which leg is currently active and its live outbound
    /// sender together, since every command endpoint (and the playlist
    /// runner, which isn't itself an HTTP handler and so has only this
    /// `PanelState` to work with) needs both -- `409` if no device is
    /// paired on either leg, `503` if that leg's relay connection isn't
    /// currently ready to send. Kept as a small `(status, message)` error
    /// rather than a built `Response` so this `Result` stays cheap to
    /// pass around (clippy's `result_large_err`), and so this module
    /// doesn't need to depend on `axum::response`.
    pub fn active_target_and_outbound(
        &self,
    ) -> Result<(ActiveTarget, mpsc::UnboundedSender<WsMessage>), (StatusCode, &'static str)> {
        let Some(target) = self.active_target() else {
            return Err((StatusCode::CONFLICT, "no device paired"));
        };
        let tx = match &target {
            ActiveTarget::V3 { .. } => self.outbound(),
            ActiveTarget::V4 { .. } => self.v4_outbound(),
        };
        match tx {
            Some(tx) => Ok((target, tx)),
            None => Err((
                StatusCode::SERVICE_UNAVAILABLE,
                "relay connection not ready",
            )),
        }
    }

    // ---- playlists ------------------------------------------------------

    fn playlist_token_mutex(&self, channel: Channel) -> &Mutex<CancellationToken> {
        match channel {
            Channel::A => &self.playlist_token_a,
            Channel::B => &self.playlist_token_b,
        }
    }

    /// Cancels and reissues `channel`'s playlist token, returning the
    /// fresh one for the caller to spawn a new runner task with. Safe to
    /// call even if no task is currently alive (matches
    /// `request_reconnect`'s pattern).
    fn reset_playlist_token(&self, channel: Channel) -> CancellationToken {
        let mut guard = self.playlist_token_mutex(channel).lock().unwrap();
        guard.cancel();
        *guard = CancellationToken::new();
        guard.clone()
    }

    /// Wakes (and lets exit) whatever runner task is currently alive for
    /// `channel`, if any -- harmless to call when none is.
    fn cancel_playlist_token(&self, channel: Channel) {
        self.playlist_token_mutex(channel).lock().unwrap().cancel();
    }

    pub fn playlist_add(
        &self,
        channel: Channel,
        kind: playlist::EntryKind,
        duration: playlist::DurationSpec,
    ) -> Uuid {
        let id = {
            let mut inner = self.inner.lock().unwrap();
            playlist_mut(&mut inner, channel).add(kind, duration)
        };
        self.notify_changed();
        id
    }

    pub fn playlist_remove(&self, channel: Channel, id: Uuid) -> bool {
        let removed = {
            let mut inner = self.inner.lock().unwrap();
            playlist_mut(&mut inner, channel).remove(id)
        };
        self.notify_changed();
        removed
    }

    pub fn playlist_reorder(&self, channel: Channel, order: &[Uuid]) -> bool {
        let ok = {
            let mut inner = self.inner.lock().unwrap();
            playlist_mut(&mut inner, channel).reorder(order)
        };
        self.notify_changed();
        ok
    }

    pub fn playlist_set_settings(&self, channel: Channel, shuffle: bool, loop_playback: bool) {
        {
            let mut inner = self.inner.lock().unwrap();
            playlist_mut(&mut inner, channel).set_settings(shuffle, loop_playback);
        }
        self.notify_changed();
    }

    /// Starts or resumes playback of `channel`'s playlist, and -- only
    /// when that succeeds -- issues a fresh cancellation token for the
    /// caller to spawn the runner task with (see
    /// `playlist_runner::run`). The caller is responsible for actually
    /// spawning that task; this method only decides *whether* one should
    /// run and what its first step is.
    pub fn playlist_play(
        &self,
        channel: Channel,
    ) -> Result<(playlist::Step, CancellationToken), playlist::PlayError> {
        let step = {
            let mut inner = self.inner.lock().unwrap();
            playlist_mut(&mut inner, channel).play()
        }?;
        let token = self.reset_playlist_token(channel);
        self.notify_changed();
        Ok((step, token))
    }

    /// Called by the runner once its current entry's resolved duration
    /// elapses naturally (never as a result of pause/stop cancelling the
    /// token -- those mutate state directly, see below).
    pub fn playlist_advance(&self, channel: Channel) -> playlist::Step {
        let step = {
            let mut inner = self.inner.lock().unwrap();
            playlist_mut(&mut inner, channel).advance()
        };
        self.notify_changed();
        step
    }

    /// Pauses `channel`'s playlist (capturing time left on the current
    /// entry) and interrupts its runner task. No-op if not currently
    /// playing. Returns whether it actually did anything.
    pub fn playlist_pause(&self, channel: Channel) -> bool {
        let paused = {
            let mut inner = self.inner.lock().unwrap();
            playlist_mut(&mut inner, channel).pause()
        };
        if paused {
            self.cancel_playlist_token(channel);
        }
        self.notify_changed();
        paused
    }

    /// Stops `channel`'s playlist (resetting to the top of the queue) and
    /// interrupts its runner task. Always succeeds, including when
    /// already stopped.
    pub fn playlist_stop(&self, channel: Channel) {
        {
            let mut inner = self.inner.lock().unwrap();
            playlist_mut(&mut inner, channel).stop();
        }
        self.cancel_playlist_token(channel);
        self.notify_changed();
    }

    /// `channel`'s current queue contents plus its shuffle/loop
    /// settings -- exactly what saving it as a template needs (see
    /// `handler::post_template`). Read-only, doesn't touch playback
    /// state.
    pub fn playlist_entries_snapshot(&self, channel: Channel) -> (Vec<PlaylistEntry>, bool, bool) {
        let inner = self.inner.lock().unwrap();
        let queue = match channel {
            Channel::A => &inner.playlist_a,
            Channel::B => &inner.playlist_b,
        };
        (
            queue.entries().to_vec(),
            queue.shuffle_enabled(),
            queue.loop_playback(),
        )
    }

    /// Replaces `channel`'s queue wholesale (e.g. loading a saved
    /// template) -- see [`PlaylistQueue::load`]. Interrupts any
    /// currently-running playback first, same as [`Self::playlist_stop`],
    /// since swapping the underlying entries out from under a live
    /// runner task would leave it holding an entry id that no longer
    /// exists in a completely different queue.
    pub fn playlist_load(
        &self,
        channel: Channel,
        items: Vec<(playlist::EntryKind, playlist::DurationSpec)>,
        shuffle: bool,
        loop_playback: bool,
    ) {
        {
            let mut inner = self.inner.lock().unwrap();
            playlist_mut(&mut inner, channel).load(items, shuffle, loop_playback);
        }
        self.cancel_playlist_token(channel);
        self.notify_changed();
    }

    // ---- ramps ------------------------------------------------------------

    fn ramp_token_mutex(&self, channel: Channel) -> &Mutex<CancellationToken> {
        match channel {
            Channel::A => &self.ramp_token_a,
            Channel::B => &self.ramp_token_b,
        }
    }

    /// Replaces any existing ramp on `channel` with a fresh one and
    /// returns the token to spawn its runner with -- validation
    /// (`profile.validate()`, the upper-limit check) is the caller's
    /// job (`handler::post_ramp`), same as `post_strength` validates
    /// before ever touching `PanelState`.
    pub fn ramp_start(&self, channel: Channel, profile: RampProfile) -> CancellationToken {
        {
            let mut inner = self.inner.lock().unwrap();
            *ramp_mut(&mut inner, channel) = Some(RampSnapshot {
                profile,
                current: profile.initial_value(),
                target: profile.target(None),
                remaining_secs: profile.total_seconds(),
            });
        }
        let mut guard = self.ramp_token_mutex(channel).lock().unwrap();
        guard.cancel();
        *guard = CancellationToken::new();
        let token = guard.clone();
        drop(guard);
        self.notify_changed();
        token
    }

    /// Called by the runner once per tick to report progress -- a no-op
    /// if the ramp has already been cleared (e.g. cancelled the instant
    /// before this call landed).
    pub fn ramp_tick(
        &self,
        channel: Channel,
        current: i64,
        target: Option<i64>,
        remaining_secs: u32,
    ) {
        {
            let mut inner = self.inner.lock().unwrap();
            if let Some(snap) = ramp_mut(&mut inner, channel) {
                snap.current = current;
                snap.target = target;
                snap.remaining_secs = remaining_secs;
            }
        }
        self.notify_changed();
    }

    /// Clears `channel`'s ramp state without touching its token --
    /// called by the runner itself on a *natural* end (duration
    /// elapsed), where nothing external needs to be interrupted. See
    /// [`Self::ramp_cancel`] for the operator-initiated stop, which
    /// also cancels the token.
    pub fn ramp_clear(&self, channel: Channel) {
        {
            let mut inner = self.inner.lock().unwrap();
            *ramp_mut(&mut inner, channel) = None;
        }
        self.notify_changed();
    }

    /// Stops `channel`'s ramp, if one is active: clears its state and
    /// cancels its runner task. Used by `POST /api/ramp/stop` and by
    /// `post_strength`'s override (any manual strength command cancels
    /// the channel's active ramp) -- a no-op, not just idempotent but
    /// silent (no log line, no SSE push), when there wasn't one, so a
    /// manual +/- click doesn't cause a redundant broadcast on every
    /// single press.
    pub fn ramp_cancel(&self, channel: Channel) {
        let had_ramp = {
            let mut inner = self.inner.lock().unwrap();
            let slot = ramp_mut(&mut inner, channel);
            let had = slot.is_some();
            *slot = None;
            had
        };
        if had_ramp {
            self.ramp_token_mutex(channel).lock().unwrap().cancel();
            self.notify_changed();
        }
    }

    // ---- session timer ------------------------------------------------

    fn reset_session_token(&self) -> CancellationToken {
        let mut guard = self.session_token.lock().unwrap();
        guard.cancel();
        *guard = CancellationToken::new();
        guard.clone()
    }

    fn cancel_session_token(&self) {
        self.session_token.lock().unwrap().cancel();
    }

    /// Starts a fresh session (replacing any existing one outright, same
    /// "starting a new one always wins" rule as `ramp_start`). Returns
    /// the full schedule, its total duration, and the token to spawn a
    /// runner with. Validation (`config.validate()`) is the caller's
    /// job (`handler::post_session_timer`), same as `post_ramp`
    /// validates before ever touching `PanelState`.
    pub fn session_start(
        &self,
        config: SessionConfig,
    ) -> (Vec<session::Checkpoint>, u32, CancellationToken) {
        let (schedule, total) = {
            let mut inner = self.inner.lock().unwrap();
            let total = config.duration_seconds;
            (inner.session.start(config), total)
        };
        let token = self.reset_session_token();
        self.notify_changed();
        (schedule, total, token)
    }

    /// Pauses the session, if one is running. Returns whether it
    /// actually did anything.
    pub fn session_pause(&self) -> bool {
        let paused = {
            let mut inner = self.inner.lock().unwrap();
            inner.session.pause()
        };
        if paused {
            self.cancel_session_token();
            self.notify_changed();
        }
        paused
    }

    /// Resumes a paused session. Returns the remaining schedule, the
    /// elapsed seconds to resume from, the total duration, and a fresh
    /// token to spawn a new runner with -- `None` if not paused.
    pub fn session_resume(
        &self,
    ) -> Option<(Vec<session::Checkpoint>, u32, u32, CancellationToken)> {
        let (schedule, elapsed, total) = {
            let mut inner = self.inner.lock().unwrap();
            inner.session.resume()
        }?;
        let token = self.reset_session_token();
        self.notify_changed();
        Some((schedule, elapsed, total, token))
    }

    /// Called by the runner after firing the checkpoint at the current
    /// cursor, to move past it.
    pub fn session_advance(&self) {
        {
            let mut inner = self.inner.lock().unwrap();
            inner.session.advance();
        }
        self.notify_changed();
    }

    /// Stops the session (explicit early end via `POST /api/session/end`,
    /// or the runner reaching its final checkpoint) and returns the
    /// config that was active plus the elapsed seconds at the moment of
    /// stopping, so the caller can log an accurate `session.ended` event
    /// and honor `autoStopPlaylistsAtEnd`. `None` (and silently a
    /// no-op -- no log line, no SSE push) if nothing was running, so
    /// calling `/api/session/end` when there's no session doesn't cause
    /// a spurious broadcast.
    pub fn session_stop(&self) -> Option<(SessionConfig, u32)> {
        let result = {
            let mut inner = self.inner.lock().unwrap();
            inner.session.stop()
        };
        if result.is_some() {
            self.cancel_session_token();
            self.notify_changed();
        }
        result
    }

    // ---- templates ------------------------------------------------------

    pub fn template_names(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        let mut names: Vec<String> = inner.templates.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn template_get(&self, name: &str) -> Option<Template> {
        self.inner.lock().unwrap().templates.get(name).cloned()
    }

    /// Inserts or overwrites (upsert -- "save current queue as X"
    /// naturally replaces an existing X) and persists the whole store to
    /// disk. The file write happens outside the lock so it can't hold up
    /// every other request for its duration.
    pub fn template_save(&self, template: Template) {
        let snapshot = {
            let mut inner = self.inner.lock().unwrap();
            inner.templates.insert(template.name.clone(), template);
            inner.templates.clone()
        };
        templates::save_all(&snapshot);
    }

    /// Returns whether a template with that name existed. Persists the
    /// whole store to disk (outside the lock) if it did.
    pub fn template_delete(&self, name: &str) -> bool {
        let (removed, snapshot) = {
            let mut inner = self.inner.lock().unwrap();
            let removed = inner.templates.remove(name).is_some();
            (removed, inner.templates.clone())
        };
        if removed {
            templates::save_all(&snapshot);
        }
        removed
    }

    // ---- recipes ----------------------------------------------------------

    pub fn recipe_names(&self) -> Vec<String> {
        let inner = self.inner.lock().unwrap();
        let mut names: Vec<String> = inner.recipes.keys().cloned().collect();
        names.sort();
        names
    }

    pub fn recipe_get(&self, name: &str) -> Option<Recipe> {
        self.inner.lock().unwrap().recipes.get(name).cloned()
    }

    /// Inserts or overwrites (upsert, same as [`Self::template_save`])
    /// and persists the whole store to disk outside the lock.
    pub fn recipe_save(&self, recipe: Recipe) {
        let snapshot = {
            let mut inner = self.inner.lock().unwrap();
            inner.recipes.insert(recipe.name.clone(), recipe);
            inner.recipes.clone()
        };
        super::recipe::save_all(&snapshot);
    }

    /// Returns whether a recipe with that name existed. Persists the
    /// whole store to disk (outside the lock) if it did.
    pub fn recipe_delete(&self, name: &str) -> bool {
        let (removed, snapshot) = {
            let mut inner = self.inner.lock().unwrap();
            let removed = inner.recipes.remove(name).is_some();
            (removed, inner.recipes.clone())
        };
        if removed {
            super::recipe::save_all(&snapshot);
        }
        removed
    }

    // ---- check-ins (Feature 7) --------------------------------------------

    /// Records a subjective check-in for `/events`' `lastCheckIn` --
    /// logging it (webhook + event log) is the caller's job
    /// (`handler::post_session_checkin`), via the usual `log_with`.
    pub fn record_check_in(&self, check_in: CheckIn) {
        {
            self.inner.lock().unwrap().last_check_in = Some(check_in);
        }
        self.notify_changed();
    }

    // ---- event log ------------------------------------------------------

    /// Wires up the event-log writer task's channel -- called once by
    /// `panel::build()` right after spawning `event_log::run`. See the
    /// `event_log_tx` field docs for why this is a separate step
    /// instead of a `new()` constructor argument.
    pub fn install_event_log_sender(&self, tx: mpsc::UnboundedSender<EventLogMsg>) {
        *self.event_log_tx.lock().unwrap() = Some(tx);
    }

    fn event_log_send(&self, msg: EventLogMsg) {
        if let Some(tx) = self.event_log_tx.lock().unwrap().as_ref() {
            let _ = tx.send(msg);
        }
    }

    pub fn event_log_configure(&self, config: EventLogConfig) {
        self.event_log_send(EventLogMsg::Configure(config));
    }

    /// Forces a fresh log file, whether or not one is already open --
    /// see `event_log`'s module docs on what "session" means here.
    pub fn event_log_start_session(&self) {
        self.event_log_send(EventLogMsg::StartSession);
    }

    fn event_log_append(&self, message: String, extra: Value) {
        self.event_log_send(EventLogMsg::Append { message, extra });
    }

    // ---- button mapping ---------------------------------------------------

    pub fn button_map_get(&self) -> ButtonMap {
        self.inner.lock().unwrap().button_map.clone()
    }

    pub fn button_map_set(&self, map: ButtonMap) {
        {
            self.inner.lock().unwrap().button_map = map.clone();
        }
        button_map::save(&map);
    }

    pub fn button_map_action_for(&self, key: &str) -> Option<ButtonAction> {
        self.inner
            .lock()
            .unwrap()
            .button_map
            .pattern
            .get(key)
            .cloned()
    }

    /// Whether `channel`'s playlist is currently playing -- used by
    /// `button_map::dispatch`'s `playlist_toggle` action. A thin,
    /// direct accessor rather than reading the whole `snapshot()` just
    /// to check one field.
    pub fn playlist_is_playing(&self, channel: Channel) -> bool {
        let inner = self.inner.lock().unwrap();
        let queue = match channel {
            Channel::A => &inner.playlist_a,
            Channel::B => &inner.playlist_b,
        };
        queue.phase() == playlist::Phase::Playing
    }
}

/// Picks the `Inner` field for `channel`'s playlist -- a free function
/// (rather than a method) so it can be called while `inner` is already
/// locked, same reasoning as `activate`/`deactivate_if_active` below.
fn playlist_mut(inner: &mut Inner, channel: Channel) -> &mut PlaylistQueue {
    match channel {
        Channel::A => &mut inner.playlist_a,
        Channel::B => &mut inner.playlist_b,
    }
}

fn ramp_mut(inner: &mut Inner, channel: Channel) -> &mut Option<RampSnapshot> {
    match channel {
        Channel::A => &mut inner.ramp_a,
        Channel::B => &mut inner.ramp_b,
    }
}

/// What the command endpoints need to address the currently active
/// device, without the caller needing to know which leg's ids to reach
/// for.
pub enum ActiveTarget {
    V3 {
        controller_id: String,
        device_id: String,
    },
    V4 {
        device_id: String,
        slot_id: String,
    },
}

/// Makes `protocol` the active leg, always overriding whatever was active
/// before -- "most recently attached device wins" (see the module docs).
fn activate(inner: &mut Inner, protocol: Protocol) {
    if inner.active_protocol != Some(protocol) {
        inner.strength_a = None;
        inner.strength_b = None;
        inner.soft_limit_a = None;
        inner.soft_limit_b = None;
        inner.last_button_action = None;
        clear_health(inner);
    }
    inner.active_protocol = Some(protocol);
}

/// If `protocol` is currently active, clears the shared display fields
/// and either hands activity to the other leg (if it still has a device)
/// or clears `active_protocol` entirely.
fn deactivate_if_active(inner: &mut Inner, protocol: Protocol) {
    if inner.active_protocol != Some(protocol) {
        return;
    }
    inner.strength_a = None;
    inner.strength_b = None;
    inner.soft_limit_a = None;
    inner.soft_limit_b = None;
    inner.last_button_action = None;
    clear_health(inner);

    let other_still_has_device = match protocol {
        Protocol::V3 => inner.v4_device_slot_id.is_some(),
        Protocol::V4 => inner.device_id.is_some(),
    };
    inner.active_protocol = if other_still_has_device {
        Some(match protocol {
            Protocol::V3 => Protocol::V4,
            Protocol::V4 => Protocol::V3,
        })
    } else {
        None
    };
}

fn clear_health(inner: &mut Inner) {
    inner.battery = None;
    inner.channel_a_status = None;
    inner.channel_b_status = None;
    inner.channel_a_overheat = None;
    inner.channel_b_overheat = None;
    inner.channel_a_overheat_pct = None;
    inner.channel_b_overheat_pct = None;
    inner.contact_alert_at_a = None;
    inner.contact_alert_at_b = None;
}

/// `channel_a_status`/`channel_b_status`'s documented "normal" code (see
/// [`DeviceHealth`]'s field docs) -- Feature 8's alerts fire on crossing
/// to/from this value, not on every status change.
const CHANNEL_STATUS_NORMAL: i64 = 2;

/// Maps a non-normal `channel_*_status` code to one of Feature 8's four
/// `issue` values. The original request's `issue` enum
/// (`"loose"`/`"damaged"`/`"open_circuit"`/`"unknown"`) doesn't have a
/// one-to-one match for all five documented status codes -- this is a
/// judgment call, not something Mara's clarification round covered:
/// `0` ("no output") is mapped to `"loose"` as the closest fit (no
/// signal path, e.g. a detached pad), and `4` ("masked", Coyote-only)
/// falls back to `"unknown"` alongside any undocumented code, same as
/// `v4_client::channel_status_label`'s own catch-all.
fn contact_issue_str(code: i64) -> &'static str {
    match code {
        0 => "loose",
        1 => "open_circuit",
        3 => "damaged",
        _ => "unknown",
    }
}

/// Minimum time between `contact_issue` alerts on the same channel, per
/// Feature 8's clarification -- a flapping status still updates
/// `channel_*_status` every tick, but only alerts at most once per
/// window.
const CONTACT_ALERT_DEBOUNCE: Duration = Duration::from_millis(500);

/// Checks one channel's health report against its previous status for a
/// `normal` <-> issue transition, honoring the debounce window. Must run
/// *before* `apply_health` overwrites `channel_*_status`, since it needs
/// both the old and new value.
fn detect_contact_transition(
    previous: Option<i64>,
    new_status: Option<i64>,
    last_alert_at: &mut Option<Instant>,
    channel: Channel,
) -> Option<ContactTransition> {
    let new_code = new_status?;
    let was_issue = previous.is_some_and(|c| c != CHANNEL_STATUS_NORMAL);
    let is_issue = new_code != CHANNEL_STATUS_NORMAL;
    if was_issue == is_issue {
        return None;
    }
    let now = Instant::now();
    if let Some(at) = last_alert_at
        && now.duration_since(*at) < CONTACT_ALERT_DEBOUNCE
    {
        return None;
    }
    *last_alert_at = Some(now);
    Some(ContactTransition {
        channel,
        status: if is_issue { "entered" } else { "resolved" },
        issue: if is_issue {
            contact_issue_str(new_code)
        } else {
            "normal"
        },
    })
}

/// Runs [`detect_contact_transition`] for both channels against this
/// tick's health report.
fn detect_contact_transitions(inner: &mut Inner, health: &DeviceHealth) -> Vec<ContactTransition> {
    let mut transitions = Vec::new();
    if let Some(t) = detect_contact_transition(
        inner.channel_a_status,
        health.channel_a_status,
        &mut inner.contact_alert_at_a,
        Channel::A,
    ) {
        transitions.push(t);
    }
    if let Some(t) = detect_contact_transition(
        inner.channel_b_status,
        health.channel_b_status,
        &mut inner.contact_alert_at_b,
        Channel::B,
    ) {
        transitions.push(t);
    }
    transitions
}

/// Overwrites only the fields `health` actually carries a value for,
/// leaving the rest at whatever was last known -- same "partial update"
/// treatment `strength_a`/`strength_b` already get from `v4_set_device`/
/// `v4_update_device`, appropriate for `slots.patch`'s incremental
/// payloads (which only include what changed this tick).
fn apply_health(inner: &mut Inner, health: DeviceHealth) {
    if let Some(v) = health.battery {
        inner.battery = Some(v);
    }
    if let Some(v) = health.channel_a_status {
        inner.channel_a_status = Some(v);
    }
    if let Some(v) = health.channel_b_status {
        inner.channel_b_status = Some(v);
    }
    if let Some(v) = health.channel_a_overheat {
        inner.channel_a_overheat = Some(v);
    }
    if let Some(v) = health.channel_b_overheat {
        inner.channel_b_overheat = Some(v);
    }
    if let Some(v) = health.channel_a_overheat_pct {
        inner.channel_a_overheat_pct = Some(v);
    }
    if let Some(v) = health.channel_b_overheat_pct {
        inner.channel_b_overheat_pct = Some(v);
    }
}

impl Default for PanelState {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_limit_is_per_channel_and_defaults_to_none() {
        let state = PanelState::new();
        assert_eq!(state.strength_and_limit(Channel::A), (None, None));

        state.set_limit(Channel::A, Some(50));
        assert_eq!(state.strength_and_limit(Channel::A), (None, Some(50)));
        assert_eq!(state.strength_and_limit(Channel::B), (None, None));

        state.set_limit(Channel::A, None);
        assert_eq!(state.strength_and_limit(Channel::A), (None, None));
    }

    #[test]
    fn limit_survives_reconnect_but_strength_does_not() {
        let state = PanelState::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.set_connected("c".into(), tx);
        state.set_paired("d".into());

        state.set_limit(Channel::A, Some(40));
        state.apply_optimistic_strength(Channel::A, 20);
        assert_eq!(state.strength_and_limit(Channel::A), (Some(20), Some(40)));

        state.begin_connecting();
        assert_eq!(state.strength_and_limit(Channel::A), (None, Some(40)));

        state.set_disconnected();
        assert_eq!(state.strength_and_limit(Channel::A), (None, Some(40)));
    }

    #[test]
    fn apply_optimistic_strength_is_overwritten_by_a_real_device_report() {
        let state = PanelState::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.set_connected("c".into(), tx);
        state.set_paired("d".into());

        state.apply_optimistic_strength(Channel::A, 15);
        assert_eq!(state.strength_and_limit(Channel::A).0, Some(15));

        // A real report is ground truth and wins even if it disagrees
        // with our own optimistic guess.
        state.set_device_strength(12, 0, 100, 100);
        assert_eq!(state.strength_and_limit(Channel::A).0, Some(12));
    }

    #[test]
    fn log_caps_at_capacity_dropping_oldest() {
        let state = PanelState::new();
        for i in 0..(LOG_CAPACITY + 10) {
            state.log(format!("line-{i}"));
        }
        let snapshot = state.snapshot();
        assert_eq!(snapshot.log.len(), LOG_CAPACITY);
        assert_eq!(snapshot.log.first().unwrap(), "line-10");
        assert_eq!(
            snapshot.log.last().unwrap(),
            &format!("line-{}", LOG_CAPACITY + 9)
        );
    }

    #[test]
    fn connect_pair_break_transitions() {
        let state = PanelState::new();
        assert_eq!(state.snapshot().status, Status::Connecting);

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.set_connected("controller-1".into(), tx);
        let snap = state.snapshot();
        assert_eq!(snap.status, Status::WaitingForDevice);
        assert_eq!(snap.controller_id.as_deref(), Some("controller-1"));
        assert!(state.outbound().is_some());

        state.set_paired("device-1".into());
        let snap = state.snapshot();
        assert_eq!(snap.status, Status::Paired);
        assert_eq!(snap.device_id.as_deref(), Some("device-1"));
        assert_eq!(snap.active_protocol, Some(Protocol::V3));

        state.clear_device();
        let snap = state.snapshot();
        assert_eq!(snap.status, Status::WaitingForDevice);
        assert_eq!(snap.device_id, None);
        // controller id / outbound survive a device disconnect -- only
        // the relay connection itself dropping should clear those.
        assert_eq!(snap.controller_id.as_deref(), Some("controller-1"));
        assert!(state.outbound().is_some());
    }

    #[test]
    fn set_disconnected_clears_everything() {
        let state = PanelState::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.set_connected("controller-1".into(), tx);
        state.set_paired("device-1".into());
        state.set_device_strength(10, 20, 50, 60);
        state.set_button_action(Protocol::V3, 3);

        state.set_disconnected();
        let snap = state.snapshot();
        assert_eq!(snap.status, Status::Disconnected);
        assert_eq!(snap.controller_id, None);
        assert_eq!(snap.device_id, None);
        assert_eq!(snap.strength_a, None);
        assert_eq!(snap.strength_b, None);
        assert_eq!(snap.soft_limit_a, None);
        assert_eq!(snap.soft_limit_b, None);
        assert_eq!(snap.last_button_action, None);
        assert_eq!(snap.active_protocol, None);
        assert!(state.outbound().is_none());
    }

    #[test]
    fn device_status_report_updates_strength_and_soft_limits() {
        let state = PanelState::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.set_connected("controller-1".into(), tx);
        state.set_paired("device-1".into());

        state.set_device_strength(10, 20, 50, 60);
        let snap = state.snapshot();
        assert_eq!(snap.strength_a, Some(10));
        assert_eq!(snap.strength_b, Some(20));
        assert_eq!(snap.soft_limit_a, Some(50));
        assert_eq!(snap.soft_limit_b, Some(60));

        // A device disconnect (break) clears the now-stale readings, but
        // a fresh reconnect (begin_connecting) is what clears the
        // separately-tracked button action -- see the next test.
        state.clear_device();
        let snap = state.snapshot();
        assert_eq!(snap.strength_a, None);
        assert_eq!(snap.strength_b, None);
        assert_eq!(snap.soft_limit_a, None);
        assert_eq!(snap.soft_limit_b, None);
    }

    #[test]
    fn begin_connecting_resets_button_action_from_a_prior_session() {
        let state = PanelState::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.set_connected("c".into(), tx);
        state.set_paired("d".into());
        state.set_button_action(Protocol::V3, 7);
        assert_eq!(state.snapshot().last_button_action, Some(7));

        state.begin_connecting();
        assert_eq!(state.snapshot().last_button_action, None);
    }

    #[test]
    fn request_reconnect_cancels_the_current_token_and_issues_a_fresh_one() {
        let state = PanelState::new();
        let first = state.reconnect_token();
        assert!(!first.is_cancelled());

        state.request_reconnect();
        assert!(first.is_cancelled());

        let second = state.reconnect_token();
        assert!(!second.is_cancelled());
    }

    #[tokio::test]
    async fn subscribers_are_notified_on_change() {
        let state = PanelState::new();
        let mut rx = state.subscribe();
        state.log("hello");
        tokio::time::timeout(std::time::Duration::from_secs(1), rx.recv())
            .await
            .expect("timed out")
            .expect("channel closed");
    }

    #[test]
    fn v4_device_attach_becomes_active_and_populates_strength() {
        let state = PanelState::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.v4_set_connected("vc1".into(), tx);
        state.v4_set_app_attached("app1".into());
        state.v4_set_device(
            "slot1".into(),
            "Coyote".into(),
            Some(10),
            Some(20),
            DeviceHealth::default(),
        );

        let snap = state.snapshot();
        assert_eq!(snap.v4_status, Status::Paired);
        assert_eq!(snap.active_protocol, Some(Protocol::V4));
        assert_eq!(snap.strength_a, Some(10));
        assert_eq!(snap.strength_b, Some(20));
        assert!(matches!(
            state.active_target(),
            Some(ActiveTarget::V4 { .. })
        ));
    }

    #[test]
    fn most_recently_attached_device_wins_active_status() {
        let state = PanelState::new();
        let (tx3, _rx3) = tokio::sync::mpsc::unbounded_channel();
        state.set_connected("c3".into(), tx3);
        state.set_paired("d3".into());
        assert_eq!(state.snapshot().active_protocol, Some(Protocol::V3));

        let (tx4, _rx4) = tokio::sync::mpsc::unbounded_channel();
        state.v4_set_connected("c4".into(), tx4);
        state.v4_set_app_attached("app4".into());
        state.v4_set_device(
            "slot4".into(),
            "Coyote".into(),
            Some(5),
            Some(5),
            DeviceHealth::default(),
        );
        assert_eq!(state.snapshot().active_protocol, Some(Protocol::V4));
    }

    #[test]
    fn losing_the_active_device_falls_back_to_the_other_leg_if_still_present() {
        let state = PanelState::new();
        let (tx3, _rx3) = tokio::sync::mpsc::unbounded_channel();
        state.set_connected("c3".into(), tx3);
        state.set_paired("d3".into());

        let (tx4, _rx4) = tokio::sync::mpsc::unbounded_channel();
        state.v4_set_connected("c4".into(), tx4);
        state.v4_set_app_attached("app4".into());
        state.v4_set_device(
            "slot4".into(),
            "Coyote".into(),
            None,
            None,
            DeviceHealth::default(),
        );
        assert_eq!(state.snapshot().active_protocol, Some(Protocol::V4));

        // V4 device drops -- V3 is still paired, so it becomes active again.
        state.v4_clear_device();
        assert_eq!(state.snapshot().active_protocol, Some(Protocol::V3));
    }

    #[test]
    fn losing_the_only_active_device_clears_active_protocol() {
        let state = PanelState::new();
        let (tx3, _rx3) = tokio::sync::mpsc::unbounded_channel();
        state.set_connected("c3".into(), tx3);
        state.set_paired("d3".into());

        state.clear_device();
        assert_eq!(state.snapshot().active_protocol, None);
    }

    #[test]
    fn inactive_leg_reports_are_ignored_for_shared_display_fields() {
        let state = PanelState::new();
        let (tx3, _rx3) = tokio::sync::mpsc::unbounded_channel();
        state.set_connected("c3".into(), tx3);
        state.set_paired("d3".into());
        state.set_device_strength(10, 10, 50, 50);

        let (tx4, _rx4) = tokio::sync::mpsc::unbounded_channel();
        state.v4_set_connected("c4".into(), tx4);
        state.v4_set_app_attached("app4".into());
        // V4 isn't active (V3 already is) -- v4_set_device only activates
        // if it wasn't already the active leg, so attaching V4 here makes
        // it active per "most recent wins". To test the ignore-if-inactive
        // path, update the tracked V4 device *after* V3 re-takes activity.
        state.v4_set_device(
            "slot4".into(),
            "Coyote".into(),
            Some(1),
            Some(1),
            DeviceHealth::default(),
        );
        assert_eq!(state.snapshot().active_protocol, Some(Protocol::V4));

        // A stray V3 report arrives while V4 is active -- ignored.
        state.set_device_strength(99, 99, 50, 50);
        assert_eq!(state.snapshot().strength_a, Some(1));
    }

    #[test]
    fn record_check_in_populates_the_snapshot() {
        let state = PanelState::new();
        assert!(state.snapshot().last_check_in.is_none());

        state.record_check_in(CheckIn {
            timestamp: "2026-08-24T00:00:00.000Z".into(),
            color: "green".into(),
            arousal: 6,
            discomfort: "none".into(),
            notes: Some("feeling good".into()),
        });
        let snap = state.snapshot();
        let check_in = snap.last_check_in.expect("check-in should be recorded");
        assert_eq!(check_in.color, "green");
        assert_eq!(check_in.arousal, 6);
        assert_eq!(check_in.notes.as_deref(), Some("feeling good"));
    }

    #[test]
    fn contact_issue_str_maps_documented_codes() {
        assert_eq!(contact_issue_str(0), "loose");
        assert_eq!(contact_issue_str(1), "open_circuit");
        assert_eq!(contact_issue_str(3), "damaged");
        assert_eq!(contact_issue_str(4), "unknown");
        assert_eq!(contact_issue_str(99), "unknown");
    }

    #[test]
    fn detect_contact_transition_fires_on_entering_and_ignores_no_report() {
        let mut last_alert = None;
        // First-ever report, already normal -- no transition.
        assert!(
            detect_contact_transition(
                None,
                Some(CHANNEL_STATUS_NORMAL),
                &mut last_alert,
                Channel::A
            )
            .is_none()
        );
        assert!(last_alert.is_none());

        // normal -> open circuit: entering.
        let t = detect_contact_transition(
            Some(CHANNEL_STATUS_NORMAL),
            Some(1),
            &mut last_alert,
            Channel::A,
        )
        .expect("should fire");
        assert_eq!(t.status, "entered");
        assert_eq!(t.issue, "open_circuit");
        assert!(last_alert.is_some());

        // No fresh report this tick -- no transition, regardless of prior state.
        assert!(detect_contact_transition(Some(1), None, &mut None, Channel::A).is_none());
    }

    #[test]
    fn detect_contact_transition_debounces_rapid_flapping_but_allows_after_the_window() {
        let mut last_alert = None;
        let entered = detect_contact_transition(
            Some(CHANNEL_STATUS_NORMAL),
            Some(1),
            &mut last_alert,
            Channel::A,
        )
        .expect("first transition should fire");
        assert_eq!(entered.status, "entered");

        // Immediately flapping back to normal -- within the debounce window, suppressed.
        assert!(
            detect_contact_transition(
                Some(1),
                Some(CHANNEL_STATUS_NORMAL),
                &mut last_alert,
                Channel::A
            )
            .is_none()
        );

        // Simulate the debounce window having already elapsed.
        last_alert = Some(Instant::now() - CONTACT_ALERT_DEBOUNCE - Duration::from_millis(10));
        let resolved = detect_contact_transition(
            Some(1),
            Some(CHANNEL_STATUS_NORMAL),
            &mut last_alert,
            Channel::A,
        )
        .expect("should fire once the debounce window passes");
        assert_eq!(resolved.status, "resolved");
        assert_eq!(resolved.issue, "normal");
    }

    #[test]
    fn v4_update_device_surfaces_a_contact_transition_end_to_end() {
        let state = PanelState::new();
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.v4_set_connected("c4".into(), tx);
        state.v4_set_app_attached("app4".into());
        let transitions = state.v4_set_device(
            "slot4".into(),
            "Coyote".into(),
            Some(10),
            Some(10),
            DeviceHealth {
                channel_a_status: Some(CHANNEL_STATUS_NORMAL),
                ..Default::default()
            },
        );
        assert!(
            transitions.is_empty(),
            "starting out normal shouldn't alert"
        );

        let transitions = state.v4_update_device(
            "slot4",
            None,
            None,
            DeviceHealth {
                channel_a_status: Some(1),
                ..Default::default()
            },
        );
        assert_eq!(transitions.len(), 1);
        assert_eq!(transitions[0].channel, Channel::A);
        assert_eq!(transitions[0].status, "entered");
        assert_eq!(transitions[0].issue, "open_circuit");
    }
}
