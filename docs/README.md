# Documentation

- [Architecture](architecture.md) — how the three servers relate, module
  layout, state ownership, concurrency model
- [Sequence diagrams](sequence-diagrams.md) — pairing, strength/pulse
  commands, button feedback → webhook, reconnect, and V4 for comparison
- [API reference](api.md) — full V3/V4 WebSocket wire protocol and the
  control panel's HTTP API, including webhook payload shapes
- [Usage guide](usage.md) — running the server, operating the control panel,
  integrating your own controller against V3 or V4, wiring up the webhook
- [Channel playlists proposal](channel-playlists-proposal.md) and
  [implementation plan](channel-playlists-implementation.md) — the original
  design exploration and Rust-side design doc for per-channel pulse
  playlists. Both are archival: the feature they describe has since shipped
  (see [Playlists](api.md#playlists) in the API reference and
  `src/panel/playlist.rs`/`playlist_runner.rs`) and the docs are kept only
  for the design rationale, not as a status indicator

See the top-level [README](../README.md) for installation, the environment
variable reference, and quick-start instructions. See the top-level
[NOTICE.md](../NOTICE.md) for upstream attribution and
[LICENSE](../LICENSE) for licensing (GPLv3).
