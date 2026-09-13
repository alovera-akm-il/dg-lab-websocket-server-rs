//! Runs the V3 and V4 relay servers, plus the control panel webserver,
//! concurrently in one process. See the crate root docs (`src/lib.rs`)
//! for the module overview and the README for configuration/usage.

use std::sync::Arc;

use dg_lab_websocket_server_rs::{panel, v3, v4};

#[tokio::main]
async fn main() {
    dg_lab_websocket_server_rs::logging::init();

    let v3_config = Arc::new(v3::config::Config::from_env());
    let v3_port = v3_config.port;
    let v4_config = Arc::new(v4::config::Config::from_env());
    let v4_port = v4_config.port;
    let v4_prefix = v4_config.prefix.clone();

    // V3 and V4 both read the same `PORT` env var (only their *defaults*
    // differ -- 10002 vs 10001, see each config's own docs) -- so setting
    // `PORT` explicitly collides them onto one port. Without this check,
    // that surfaces as one of the two failing to bind, which `try_join!`
    // below turns into the *entire* process exiting -- taking the server
    // that bound fine down with it too, with nothing indicating why. Fail
    // loud here instead, before either server has started, rather than
    // days later as "the DG-LAB APP just won't connect."
    if v3_port == v4_port {
        eprintln!(
            "V3 and V4 would both listen on port {v3_port} -- refusing to start.\n\
             Both protocols read the same `PORT` env var (V3 defaults to 10002, V4 to \
             10001 when it's unset), so setting `PORT` explicitly points them at the \
             same port and only one of them can actually bind it. See the `PORT` row \
             in README.md's configuration table for how to give them independent ports."
        );
        std::process::exit(1);
    }

    let v3 = tokio::spawn(v3::serve_with(v3_config));
    let v4 = tokio::spawn(v4::serve_with(v4_config));
    let panel = tokio::spawn(panel::serve(v3_port, v4_port, v4_prefix));

    // try_join! short-circuits as soon as any server fails (e.g. its
    // port is already taken), instead of silently waiting forever on the
    // others -- all three must come up for the process to be useful.
    if let Err(err) = tokio::try_join!(flatten(v3), flatten(v4), flatten(panel)) {
        eprintln!("server failed: {err}");
        std::process::exit(1);
    }
}

async fn flatten(handle: tokio::task::JoinHandle<std::io::Result<()>>) -> std::io::Result<()> {
    match handle.await {
        Ok(result) => result,
        Err(join_err) => Err(std::io::Error::other(join_err)),
    }
}
