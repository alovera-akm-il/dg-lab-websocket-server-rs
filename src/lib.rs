//! Rust port of the [DG-LAB WebSocket Server](https://github.com/dungeonlab-open/dglab-kit/)
//! TypeScript/Bun relay (`v3-server.ts` / `v4-server.ts`), preserving both
//! protocols' externally-visible behavior: message shapes, error/close
//! codes, log line formats, and env var names/defaults.
//!
//! The two protocols are independent and share no state.
//!
//! [`v3`] is the older 1-controller:1-device pairing relay (strength
//! adjust, clear, pulse-waveform commands). See [`v3::handler`] for the
//! connection lifecycle and message routing, [`v3::state`] for the shared
//! pairing/pulse-timer state, and [`v3::pulse`] for waveform
//! packetization.
//!
//! [`v4`] is the newer 1-controller:N-device relay with a simpler
//! passthrough `message` envelope. See [`v4::handler`] and [`v4::state`].
//!
//! [`v3::serve`] and [`v4::serve`] each build a [`tokio::net::TcpListener`]
//! and an [`axum::Router`] from env-driven config and run until the
//! process exits; `src/main.rs` runs both concurrently in one binary.
//! Tests (and any embedder wanting a fresh instance on an ephemeral port)
//! should use [`v3::build`] / [`v4::build`] instead, which construct the
//! `Hub` and `Router` without binding a socket or starting background
//! tasks.
//!
//! [`panel`] is a third, independent webserver: a control panel that
//! shows a pairing QR code and drives a paired device through the V3
//! relay, connecting to it as an ordinary loopback client rather than
//! sharing state in-process.

pub mod env;
pub mod logging;
pub mod panel;
pub mod v3;
pub mod v4;
