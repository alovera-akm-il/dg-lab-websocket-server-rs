//! Shared V3 connection/pairing/pulse-timer state, mirroring the
//! `connections`/`webToApp`/`appToWeb`/`pulseTimers` maps and the
//! `pair`/`unpair`/`isPaired`/`isBound`/`isAvailableTarget` helpers on
//! `V3SocketServer`.
//!
//! Everything is keyed by the connection's `String` clientId rather than
//! by a `ws` reference (unlike the TS maps) -- each connection task always
//! has its own clientId in scope once registered, so no ws-identity
//! reverse-map is needed. All the maps live behind one `std::sync::Mutex`
//! so multi-map operations like `pair()` stay atomic, the same guarantee
//! the TS code gets for free from JS's single-threaded event loop. Lock
//! scope is always kept tiny: no `.await` is ever held across a lock.

use std::collections::HashMap;
use std::sync::Mutex;

use axum::extract::ws::Message;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::protocol::Channel;

pub struct ClientEntry {
    pub tx: mpsc::UnboundedSender<Message>,
    /// Cancelled when this connection is no longer "unpaired" (i.e. on a
    /// successful pair) so its idle-timeout sleep can be aborted early.
    pub idle_token: CancellationToken,
    /// Cancelled to force this connection's reader loop to end (idle
    /// timeout firing, or a partner disconnecting and closing this side).
    pub shutdown_token: CancellationToken,
}

#[derive(Default)]
struct HubInner {
    connections: HashMap<String, ClientEntry>,
    web_to_app: HashMap<String, String>,
    app_to_web: HashMap<String, String>,
    pulse_tokens: HashMap<(String, Channel), CancellationToken>,
}

#[derive(Default)]
pub struct Hub {
    inner: Mutex<HubInner>,
}

pub struct PairResult {
    pub ok: bool,
    pub code: &'static str,
}

/// What `remove()` learned about the closing connection's pairing, for the
/// caller to build and send the `break` notification to any live partner.
pub struct RemovalOutcome {
    pub paired_id: Option<String>,
    pub web_id: Option<String>,
    pub app_id: Option<String>,
    pub partner: Option<(String, mpsc::UnboundedSender<Message>, CancellationToken)>,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(
        &self,
        client_id: String,
        tx: mpsc::UnboundedSender<Message>,
    ) -> (CancellationToken, CancellationToken) {
        let idle_token = CancellationToken::new();
        let shutdown_token = CancellationToken::new();
        let entry = ClientEntry {
            tx,
            idle_token: idle_token.clone(),
            shutdown_token: shutdown_token.clone(),
        };
        self.inner.lock().unwrap().connections.insert(client_id, entry);
        (idle_token, shutdown_token)
    }

    /// Drops a not-yet-fully-registered connection (used when an
    /// on-open pairing attempt fails and the just-inserted entry must be
    /// torn down again before closing the socket).
    pub fn unregister(&self, client_id: &str) {
        self.inner.lock().unwrap().connections.remove(client_id);
    }

    pub fn sender(&self, client_id: &str) -> Option<mpsc::UnboundedSender<Message>> {
        self.inner
            .lock()
            .unwrap()
            .connections
            .get(client_id)
            .map(|e| e.tx.clone())
    }

    pub fn is_connected(&self, client_id: &str) -> bool {
        self.inner.lock().unwrap().connections.contains_key(client_id)
    }

    pub fn is_bound(&self, client_id: &str) -> bool {
        is_bound_locked(&self.inner.lock().unwrap(), client_id)
    }

    pub fn is_paired(&self, client_id: &str, target_id: &str) -> bool {
        is_paired_locked(&self.inner.lock().unwrap(), client_id, target_id)
    }

    pub fn is_available_target(&self, client_id: &str) -> bool {
        let inner = self.inner.lock().unwrap();
        inner.connections.contains_key(client_id) && !is_bound_locked(&inner, client_id)
    }

    /// Whether `client_id` is currently the "app" (device) side of a
    /// pairing -- used by `forwardMessage`'s id-swap rule, which must be
    /// computed from live pairing state, never from a frame's own fields.
    pub fn is_app(&self, client_id: &str) -> bool {
        self.inner.lock().unwrap().app_to_web.contains_key(client_id)
    }

    pub fn pair(&self, web_id: &str, app_id: &str) -> PairResult {
        pair_locked(&mut self.inner.lock().unwrap(), web_id, app_id)
    }

    /// Snapshot of every live connection's id, sender and current partner
    /// id (if any), for the heartbeat broadcast.
    pub fn all_connections(&self) -> Vec<(String, mpsc::UnboundedSender<Message>, Option<String>)> {
        let inner = self.inner.lock().unwrap();
        inner
            .connections
            .iter()
            .map(|(id, entry)| (id.clone(), entry.tx.clone(), partner_id_locked(&inner, id)))
            .collect()
    }

    /// Full onClose teardown: removes the connection, clears its pulse
    /// timers if it was the "web" side, and unpairs it. Returns what's
    /// needed to notify/force-close a live partner -- the caller does
    /// that outside the lock.
    pub fn remove(&self, client_id: &str) -> RemovalOutcome {
        let mut inner = self.inner.lock().unwrap();
        inner.connections.remove(client_id);

        let paired_id = partner_id_locked(&inner, client_id);
        let web_id = web_id_for_locked(&inner, client_id);
        let app_id = web_id
            .as_ref()
            .and_then(|w| inner.web_to_app.get(w).cloned());

        if let Some(web_id) = &web_id {
            clear_client_pulse_tokens_locked(&mut inner, web_id);
        }

        unpair_locked(&mut inner, client_id);

        let partner = paired_id.as_ref().and_then(|id| {
            inner
                .connections
                .get(id)
                .map(|e| (id.clone(), e.tx.clone(), e.shutdown_token.clone()))
        });

        RemovalOutcome {
            paired_id,
            web_id,
            app_id,
            partner,
        }
    }

    /// Starts (or atomically replaces) the pulse-timer slot for
    /// (client_id, channel). Returns the fresh token the caller's pulse
    /// task must select against, plus whether a prior slot existed (and
    /// was just cancelled) -- mirroring `queuePulse`'s "clear existing
    /// timer, then replace" branch.
    pub fn start_pulse_slot(&self, client_id: &str, channel: Channel) -> (CancellationToken, bool) {
        let key = (client_id.to_string(), channel);
        let mut inner = self.inner.lock().unwrap();
        let had_existing = if let Some(old) = inner.pulse_tokens.remove(&key) {
            old.cancel();
            true
        } else {
            false
        };
        let token = CancellationToken::new();
        inner.pulse_tokens.insert(key, token.clone());
        (token, had_existing)
    }

    /// Explicit channel clear (type-4 `clear` command): cancels and drops
    /// the slot without starting a new one.
    pub fn clear_pulse_slot(&self, client_id: &str, channel: Channel) {
        let key = (client_id.to_string(), channel);
        if let Some(old) = self.inner.lock().unwrap().pulse_tokens.remove(&key) {
            old.cancel();
        }
    }

    /// Drops the slot once a sequence has run to natural completion (or
    /// found its target gone mid-stream), so a later new sequence on the
    /// same (client_id, channel) starts fresh rather than seeing a stale
    /// "already sending" state. A pragmatic simplification vs. the TS
    /// map, which drops the entry unconditionally too; see module docs.
    pub fn finish_pulse_slot(&self, client_id: &str, channel: Channel) {
        let key = (client_id.to_string(), channel);
        self.inner.lock().unwrap().pulse_tokens.remove(&key);
    }
}

fn is_bound_locked(inner: &HubInner, client_id: &str) -> bool {
    inner.web_to_app.contains_key(client_id) || inner.app_to_web.contains_key(client_id)
}

fn is_paired_locked(inner: &HubInner, client_id: &str, target_id: &str) -> bool {
    inner.web_to_app.get(client_id).map(String::as_str) == Some(target_id)
        || inner.app_to_web.get(client_id).map(String::as_str) == Some(target_id)
}

fn partner_id_locked(inner: &HubInner, client_id: &str) -> Option<String> {
    inner
        .web_to_app
        .get(client_id)
        .or_else(|| inner.app_to_web.get(client_id))
        .cloned()
}

fn web_id_for_locked(inner: &HubInner, client_id: &str) -> Option<String> {
    if inner.web_to_app.contains_key(client_id) {
        Some(client_id.to_string())
    } else {
        inner.app_to_web.get(client_id).cloned()
    }
}

fn unpair_locked(inner: &mut HubInner, client_id: &str) {
    if let Some(app_id) = inner.web_to_app.remove(client_id) {
        inner.app_to_web.remove(&app_id);
        return;
    }
    if let Some(web_id) = inner.app_to_web.remove(client_id) {
        inner.web_to_app.remove(&web_id);
    }
}

fn clear_client_pulse_tokens_locked(inner: &mut HubInner, client_id: &str) {
    inner.pulse_tokens.retain(|(id, _), token| {
        if id == client_id {
            token.cancel();
            false
        } else {
            true
        }
    });
}

fn pair_locked(inner: &mut HubInner, web_id: &str, app_id: &str) -> PairResult {
    if web_id == app_id {
        return PairResult { ok: false, code: "401" };
    }
    if !inner.connections.contains_key(web_id) || !inner.connections.contains_key(app_id) {
        return PairResult { ok: false, code: "401" };
    }
    if is_paired_locked(inner, web_id, app_id) {
        return PairResult { ok: true, code: "200" };
    }
    if is_bound_locked(inner, web_id) || is_bound_locked(inner, app_id) {
        return PairResult { ok: false, code: "400" };
    }

    inner.web_to_app.insert(web_id.to_string(), app_id.to_string());
    inner.app_to_web.insert(app_id.to_string(), web_id.to_string());
    if let Some(entry) = inner.connections.get(web_id) {
        entry.idle_token.cancel();
    }
    if let Some(entry) = inner.connections.get(app_id) {
        entry.idle_token.cancel();
    }
    PairResult { ok: true, code: "200" }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_tx() -> mpsc::UnboundedSender<Message> {
        mpsc::unbounded_channel().0
    }

    #[test]
    fn pair_rejects_self_pairing() {
        let hub = Hub::new();
        hub.register("a".into(), dummy_tx());
        let result = hub.pair("a", "a");
        assert!(!result.ok);
        assert_eq!(result.code, "401");
    }

    #[test]
    fn pair_rejects_unknown_ids() {
        let hub = Hub::new();
        hub.register("a".into(), dummy_tx());
        let result = hub.pair("a", "ghost");
        assert!(!result.ok);
        assert_eq!(result.code, "401");
    }

    #[test]
    fn pair_succeeds_and_is_idempotent() {
        let hub = Hub::new();
        hub.register("web".into(), dummy_tx());
        hub.register("app".into(), dummy_tx());

        let first = hub.pair("web", "app");
        assert!(first.ok);
        assert_eq!(first.code, "200");
        assert!(hub.is_paired("web", "app"));
        assert!(hub.is_bound("web"));
        assert!(hub.is_bound("app"));

        // Re-binding the exact same pair is idempotent success.
        let again = hub.pair("web", "app");
        assert!(again.ok);
        assert_eq!(again.code, "200");
    }

    #[test]
    fn pair_rejects_already_bound_sides() {
        let hub = Hub::new();
        hub.register("web".into(), dummy_tx());
        hub.register("app1".into(), dummy_tx());
        hub.register("app2".into(), dummy_tx());
        assert!(hub.pair("web", "app1").ok);

        let result = hub.pair("web", "app2");
        assert!(!result.ok);
        assert_eq!(result.code, "400");
    }

    #[test]
    fn pair_success_cancels_both_idle_tokens() {
        let hub = Hub::new();
        let (web_idle, _web_shutdown) = hub.register("web".into(), dummy_tx());
        let (app_idle, _app_shutdown) = hub.register("app".into(), dummy_tx());
        assert!(!web_idle.is_cancelled());
        assert!(!app_idle.is_cancelled());

        assert!(hub.pair("web", "app").ok);
        assert!(web_idle.is_cancelled());
        assert!(app_idle.is_cancelled());
    }

    #[test]
    fn is_available_target_requires_connected_and_unbound() {
        let hub = Hub::new();
        assert!(!hub.is_available_target("nope"));

        hub.register("web".into(), dummy_tx());
        assert!(hub.is_available_target("web"));

        hub.register("app".into(), dummy_tx());
        hub.pair("web", "app");
        assert!(!hub.is_available_target("web"));
        assert!(!hub.is_available_target("app"));
    }

    #[test]
    fn remove_resolves_web_id_from_either_side_of_the_pairing() {
        // web_id in RemovalOutcome always names the "web"/controller side
        // of the pairing, regardless of which side is the one closing.
        let hub_app_closes = Hub::new();
        hub_app_closes.register("web".into(), dummy_tx());
        hub_app_closes.register("app".into(), dummy_tx());
        hub_app_closes.pair("web", "app");
        assert_eq!(hub_app_closes.remove("app").web_id, Some("web".to_string()));

        let hub_web_closes = Hub::new();
        hub_web_closes.register("web".into(), dummy_tx());
        hub_web_closes.register("app".into(), dummy_tx());
        hub_web_closes.pair("web", "app");
        assert_eq!(hub_web_closes.remove("web").web_id, Some("web".to_string()));
    }

    #[test]
    fn remove_reports_partner_and_unpairs() {
        let hub = Hub::new();
        hub.register("web".into(), dummy_tx());
        hub.register("app".into(), dummy_tx());
        hub.pair("web", "app");

        let outcome = hub.remove("web");
        assert_eq!(outcome.paired_id, Some("app".to_string()));
        assert_eq!(outcome.web_id, Some("web".to_string()));
        assert_eq!(outcome.app_id, Some("app".to_string()));
        assert!(outcome.partner.is_some());
        assert_eq!(outcome.partner.unwrap().0, "app");

        assert!(!hub.is_paired("web", "app"));
        assert!(!hub.is_bound("app"));
        assert!(!hub.is_connected("web"));
    }

    #[test]
    fn remove_of_unpaired_connection_reports_no_partner() {
        let hub = Hub::new();
        hub.register("solo".into(), dummy_tx());
        let outcome = hub.remove("solo");
        assert_eq!(outcome.paired_id, None);
        assert!(outcome.partner.is_none());
    }

    #[test]
    fn start_pulse_slot_cancels_prior_slot_and_reports_replacement() {
        let hub = Hub::new();
        let (first_token, had_existing) = hub.start_pulse_slot("web", Channel::A);
        assert!(!had_existing);
        assert!(!first_token.is_cancelled());

        let (second_token, had_existing) = hub.start_pulse_slot("web", Channel::A);
        assert!(had_existing);
        assert!(first_token.is_cancelled());
        assert!(!second_token.is_cancelled());
    }

    #[test]
    fn start_pulse_slot_is_independent_per_channel() {
        let hub = Hub::new();
        let (a_token, _) = hub.start_pulse_slot("web", Channel::A);
        let (_, had_existing) = hub.start_pulse_slot("web", Channel::B);
        assert!(!had_existing);
        assert!(!a_token.is_cancelled());
    }

    #[test]
    fn clear_pulse_slot_cancels_without_replacement() {
        let hub = Hub::new();
        let (token, _) = hub.start_pulse_slot("web", Channel::A);
        hub.clear_pulse_slot("web", Channel::A);
        assert!(token.is_cancelled());

        let (_, had_existing) = hub.start_pulse_slot("web", Channel::A);
        assert!(!had_existing);
    }

    #[test]
    fn removing_web_side_clears_its_pulse_tokens() {
        let hub = Hub::new();
        hub.register("web".into(), dummy_tx());
        hub.register("app".into(), dummy_tx());
        hub.pair("web", "app");
        let (token, _) = hub.start_pulse_slot("web", Channel::A);

        hub.remove("web");
        assert!(token.is_cancelled());
    }
}
