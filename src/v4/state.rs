//! Shared V4 connection/controller/device state, mirroring `sockets`,
//! `controllersById`, `controlledClients`, `clientToController`,
//! `idleTimers` and `missedWsPongs` on `RelayServer`.
//!
//! As in `v3::state`, everything is keyed by `String` clientId (not a
//! `ws` reference) and all maps live behind one `std::sync::Mutex` so
//! multi-map operations stay atomic. `CancellationToken`s are one-shot,
//! so a controller's idle timer is *replaced* (fresh token, freshly
//! spawned sleep task) every time its device count returns to zero,
//! rather than reused.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use axum::extract::ws::Message;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

pub struct ConnectionEntry {
    pub tx: mpsc::UnboundedSender<Message>,
    /// Cancelled to end this connection: either a clean close (a
    /// `Message::Close` is sent through `tx` first) or an abrupt
    /// terminate (missed-pong threshold -- no close frame sent).
    pub shutdown_token: CancellationToken,
    pub missed_pongs: u32,
}

struct ControllerEntry {
    devices: HashSet<String>,
    idle_token: CancellationToken,
}

#[derive(Default)]
struct HubInner {
    /// All live connections, controllers and devices alike.
    connections: HashMap<String, ConnectionEntry>,
    controllers: HashMap<String, ControllerEntry>,
    /// device clientId -> controller clientId
    client_to_controller: HashMap<String, String>,
}

#[derive(Default)]
pub struct Hub {
    inner: Mutex<HubInner>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PingAction {
    SendPing,
    Terminate,
}

#[allow(clippy::enum_variant_names)] // "outcome was: ..." reads better than dropping the shared prefix
pub enum CloseOutcome {
    WasController {
        devices: Vec<(String, mpsc::UnboundedSender<Message>, CancellationToken)>,
    },
    WasDevice {
        controller: Option<(String, mpsc::UnboundedSender<Message>)>,
        /// `Some(fresh_token)` when the detach just brought the
        /// controller's device count to zero -- the caller must spawn a
        /// new idle-timeout task driven by this token.
        restart_idle: Option<CancellationToken>,
    },
    /// Neither a tracked controller nor a tracked device (already
    /// removed, or was never registered) -- nothing to cascade.
    WasUnknown,
}

impl Hub {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a fresh socket in the all-connections table. Called for
    /// every connection, controller or device alike.
    pub fn register_connection(
        &self,
        client_id: String,
        tx: mpsc::UnboundedSender<Message>,
    ) -> CancellationToken {
        let shutdown_token = CancellationToken::new();
        self.inner.lock().unwrap().connections.insert(
            client_id,
            ConnectionEntry {
                tx,
                shutdown_token: shutdown_token.clone(),
                missed_pongs: 0,
            },
        );
        shutdown_token
    }

    /// Registers a controller (no `tid` on connect), starting it with
    /// zero attached devices. Returns the idle-timeout token the caller
    /// should spawn its first idle-timer task against.
    pub fn register_controller(&self, client_id: String) -> CancellationToken {
        let idle_token = CancellationToken::new();
        self.inner.lock().unwrap().controllers.insert(
            client_id,
            ControllerEntry {
                devices: HashSet::new(),
                idle_token: idle_token.clone(),
            },
        );
        idle_token
    }

    pub fn is_controller(&self, client_id: &str) -> bool {
        self.inner
            .lock()
            .unwrap()
            .controllers
            .contains_key(client_id)
    }

    /// Whether `id` is currently in use by any live connection (controller
    /// or device) -- mirrors `createClientId`'s collision check against
    /// `wsToClientId`'s values.
    pub fn id_in_use(&self, id: &str) -> bool {
        self.inner.lock().unwrap().connections.contains_key(id)
    }

    /// Looks up any live connection's sender by id, controller or device
    /// alike.
    pub fn sender_of(&self, client_id: &str) -> Option<mpsc::UnboundedSender<Message>> {
        self.inner
            .lock()
            .unwrap()
            .connections
            .get(client_id)
            .map(|e| e.tx.clone())
    }

    pub fn shutdown_token_of(&self, client_id: &str) -> Option<CancellationToken> {
        self.inner
            .lock()
            .unwrap()
            .connections
            .get(client_id)
            .map(|e| e.shutdown_token.clone())
    }

    pub fn device_sender_under(
        &self,
        controller_id: &str,
        device_id: &str,
    ) -> Option<mpsc::UnboundedSender<Message>> {
        let inner = self.inner.lock().unwrap();
        let controller = inner.controllers.get(controller_id)?;
        if !controller.devices.contains(device_id) {
            return None;
        }
        inner.connections.get(device_id).map(|e| e.tx.clone())
    }

    pub fn controller_of(&self, device_id: &str) -> Option<String> {
        self.inner
            .lock()
            .unwrap()
            .client_to_controller
            .get(device_id)
            .cloned()
    }

    /// Attaches a device to an existing controller (`attachClient`'s
    /// success path). Returns `false` if `controller_id` isn't a
    /// currently registered controller.
    pub fn attach_device(&self, controller_id: &str, device_id: &str) -> bool {
        let mut inner = self.inner.lock().unwrap();
        let Some(controller) = inner.controllers.get_mut(controller_id) else {
            return false;
        };
        controller.devices.insert(device_id.to_string());
        controller.idle_token.cancel();
        inner
            .client_to_controller
            .insert(device_id.to_string(), controller_id.to_string());
        true
    }

    /// Full onClose teardown for one connection: removes it from the
    /// all-connections table and, depending on whether it was a
    /// controller or an attached device, cascades the appropriate
    /// cleanup. Notification/force-close of any affected peers is done
    /// by the caller outside the lock.
    pub fn remove_connection(&self, client_id: &str) -> CloseOutcome {
        let mut inner = self.inner.lock().unwrap();
        if inner.connections.remove(client_id).is_none() {
            return CloseOutcome::WasUnknown;
        }

        if let Some(controller) = inner.controllers.remove(client_id) {
            let mut devices = Vec::new();
            for device_id in controller.devices {
                inner.client_to_controller.remove(&device_id);
                if let Some(entry) = inner.connections.get(&device_id) {
                    devices.push((
                        device_id.clone(),
                        entry.tx.clone(),
                        entry.shutdown_token.clone(),
                    ));
                }
            }
            return CloseOutcome::WasController { devices };
        }

        if let Some(controller_id) = inner.client_to_controller.remove(client_id) {
            let mut restart_idle = None;
            let controller_tx = inner.connections.get(&controller_id).map(|e| e.tx.clone());
            if let Some(controller) = inner.controllers.get_mut(&controller_id) {
                controller.devices.remove(client_id);
                if controller.devices.is_empty() {
                    let fresh = CancellationToken::new();
                    controller.idle_token = fresh.clone();
                    restart_idle = Some(fresh);
                }
            }
            return CloseOutcome::WasDevice {
                controller: controller_tx.map(|tx| (controller_id, tx)),
                restart_idle,
            };
        }

        CloseOutcome::WasDevice {
            controller: None,
            restart_idle: None,
        }
    }

    pub fn reset_missed_pongs(&self, client_id: &str) {
        if let Some(entry) = self.inner.lock().unwrap().connections.get_mut(client_id) {
            entry.missed_pongs = 0;
        }
    }

    /// Snapshots every live connection's ping/terminate decision for one
    /// `pingConnections` tick, incrementing the counter for anything
    /// that's pinged. The caller acts on the results outside the lock.
    pub fn tick_pings(
        &self,
        max_missed: u32,
    ) -> Vec<(
        String,
        mpsc::UnboundedSender<Message>,
        CancellationToken,
        PingAction,
    )> {
        let mut inner = self.inner.lock().unwrap();
        inner
            .connections
            .iter_mut()
            .map(|(id, entry)| {
                let action = if entry.missed_pongs >= max_missed {
                    PingAction::Terminate
                } else {
                    entry.missed_pongs += 1;
                    PingAction::SendPing
                };
                (
                    id.clone(),
                    entry.tx.clone(),
                    entry.shutdown_token.clone(),
                    action,
                )
            })
            .collect()
    }

    /// Snapshots every live connection's sender for the bare
    /// `{type:'heartbeat'}` broadcast.
    pub fn all_senders(&self) -> Vec<mpsc::UnboundedSender<Message>> {
        self.inner
            .lock()
            .unwrap()
            .connections
            .values()
            .map(|e| e.tx.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_tx() -> mpsc::UnboundedSender<Message> {
        mpsc::unbounded_channel().0
    }

    #[test]
    fn attach_device_fails_for_unknown_controller() {
        let hub = Hub::new();
        assert!(!hub.attach_device("ghost", "dev1"));
    }

    #[test]
    fn attach_device_cancels_controller_idle_and_registers_lookup() {
        let hub = Hub::new();
        hub.register_connection("ctrl".into(), dummy_tx());
        let idle_token = hub.register_controller("ctrl".into());
        hub.register_connection("dev1".into(), dummy_tx());

        assert!(!idle_token.is_cancelled());
        assert!(hub.attach_device("ctrl", "dev1"));
        assert!(idle_token.is_cancelled());
        assert_eq!(hub.controller_of("dev1"), Some("ctrl".to_string()));
        assert!(hub.device_sender_under("ctrl", "dev1").is_some());
    }

    #[test]
    fn detach_restarts_idle_only_when_count_hits_zero() {
        let hub = Hub::new();
        hub.register_connection("ctrl".into(), dummy_tx());
        hub.register_controller("ctrl".into());
        hub.register_connection("dev1".into(), dummy_tx());
        hub.register_connection("dev2".into(), dummy_tx());
        assert!(hub.attach_device("ctrl", "dev1"));
        assert!(hub.attach_device("ctrl", "dev2"));

        // Removing dev1 still leaves dev2 attached -- no idle restart yet.
        let outcome = hub.remove_connection("dev1");
        match outcome {
            CloseOutcome::WasDevice {
                controller,
                restart_idle,
            } => {
                assert_eq!(controller.unwrap().0, "ctrl");
                assert!(restart_idle.is_none());
            }
            _ => panic!("expected WasDevice"),
        }

        // Removing dev2 drops the count to zero -- idle timer restarts.
        let outcome = hub.remove_connection("dev2");
        match outcome {
            CloseOutcome::WasDevice { restart_idle, .. } => {
                let token = restart_idle.expect("idle timer should restart at zero devices");
                assert!(!token.is_cancelled());
            }
            _ => panic!("expected WasDevice"),
        }
    }

    #[test]
    fn controller_disconnect_cascades_to_all_devices() {
        let hub = Hub::new();
        hub.register_connection("ctrl".into(), dummy_tx());
        hub.register_controller("ctrl".into());
        hub.register_connection("dev1".into(), dummy_tx());
        hub.register_connection("dev2".into(), dummy_tx());
        assert!(hub.attach_device("ctrl", "dev1"));
        assert!(hub.attach_device("ctrl", "dev2"));

        let outcome = hub.remove_connection("ctrl");
        match outcome {
            CloseOutcome::WasController { devices } => {
                let ids: HashSet<_> = devices.iter().map(|(id, _, _)| id.clone()).collect();
                assert_eq!(ids, HashSet::from(["dev1".to_string(), "dev2".to_string()]));
            }
            _ => panic!("expected WasController"),
        }

        // Devices' own connection entries are untouched -- cleaned up by
        // their own subsequent onClose, not by the controller's cascade.
        assert!(hub.controller_of("dev1").is_none());
    }

    #[test]
    fn removing_unknown_client_reports_unknown() {
        let hub = Hub::new();
        match hub.remove_connection("ghost") {
            CloseOutcome::WasUnknown => {}
            _ => panic!("expected WasUnknown"),
        }
    }

    #[test]
    fn tick_pings_terminates_at_threshold_and_increments_otherwise() {
        let hub = Hub::new();
        hub.register_connection("a".into(), dummy_tx());

        // First two ticks under the threshold of 2 increment and ping.
        let results = hub.tick_pings(2);
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].3, PingAction::SendPing);

        let results = hub.tick_pings(2);
        assert_eq!(results[0].3, PingAction::SendPing);

        // Third tick: missed_pongs is now 2, >= max_missed -> terminate.
        let results = hub.tick_pings(2);
        assert_eq!(results[0].3, PingAction::Terminate);
    }

    #[test]
    fn reset_missed_pongs_clears_counter() {
        let hub = Hub::new();
        hub.register_connection("a".into(), dummy_tx());
        hub.tick_pings(5);
        hub.tick_pings(5);
        hub.reset_missed_pongs("a");
        let results = hub.tick_pings(5);
        assert_eq!(results[0].3, PingAction::SendPing);
    }
}
