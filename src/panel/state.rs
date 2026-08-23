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

use std::collections::VecDeque;
use std::sync::Mutex;

use serde_json::Value;
use tokio::sync::{broadcast, mpsc};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_util::sync::CancellationToken;

use crate::v3::protocol::Channel;

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
    pub limit_a: Option<i64>,
    pub limit_b: Option<i64>,
    pub webhook_url: Option<String>,
    pub log: Vec<String>,
}

pub struct PanelState {
    inner: Mutex<Inner>,
    /// Fires (with no payload -- subscribers just re-read a fresh
    /// `snapshot()`) whenever status, ids, feedback, or the log change.
    changed: broadcast::Sender<()>,
    reconnect: Mutex<CancellationToken>,
    v4_reconnect: Mutex<CancellationToken>,
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
                limit_a: None,
                limit_b: None,
                webhook_url: None,
                log: VecDeque::new(),
            }),
            changed,
            reconnect: Mutex::new(CancellationToken::new()),
            v4_reconnect: Mutex::new(CancellationToken::new()),
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
            limit_a: inner.limit_a,
            limit_b: inner.limit_b,
            webhook_url: inner.webhook_url.clone(),
            log: inner.log.iter().cloned().collect(),
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
        webhook::notify(webhook_url.as_deref(), &line, extra);
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
    /// tracks the first device it sees per APP.
    pub fn v4_set_device(
        &self,
        slot_id: String,
        name: String,
        strength_a: Option<i64>,
        strength_b: Option<i64>,
    ) {
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
            }
        }
        self.notify_changed();
    }

    /// The tracked device's props changed (`slots.patch`) -- only applied
    /// if V4 is the active leg and the patch is for the device we track.
    pub fn v4_update_device(
        &self,
        slot_id: &str,
        strength_a: Option<i64>,
        strength_b: Option<i64>,
    ) {
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
            }
        }
        self.notify_changed();
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
        state.v4_set_device("slot1".into(), "Coyote".into(), Some(10), Some(20));

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
        state.v4_set_device("slot4".into(), "Coyote".into(), Some(5), Some(5));
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
        state.v4_set_device("slot4".into(), "Coyote".into(), None, None);
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
        state.v4_set_device("slot4".into(), "Coyote".into(), Some(1), Some(1));
        assert_eq!(state.snapshot().active_protocol, Some(Protocol::V4));

        // A stray V3 report arrives while V4 is active -- ignored.
        state.set_device_strength(99, 99, 50, 50);
        assert_eq!(state.snapshot().strength_a, Some(1));
    }
}
