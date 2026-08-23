//! Embeds the panel's static frontend (`index.html`/`style.css`/`app.js`)
//! into the binary via `rust-embed`, so the served binary is still a
//! single self-contained file with no separate assets directory to ship
//! alongside it. In debug builds these are read live from disk on every
//! request (the `debug-embed` feature is deliberately not enabled), so
//! editing them during development doesn't require a rebuild; release
//! builds truly embed the bytes at compile time.

use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "src/panel/assets/"]
struct Assets;

/// Serves one embedded file by path (e.g. `"index.html"`, `"style.css"`),
/// with its content type set from the file extension. `404` if the path
/// isn't one of the embedded files.
pub fn serve(path: &str) -> Response {
    match Assets::get(path) {
        Some(file) => (
            [(header::CONTENT_TYPE, file.metadata.mimetype())],
            file.data,
        )
            .into_response(),
        None => (StatusCode::NOT_FOUND, "404 Not Found").into_response(),
    }
}
