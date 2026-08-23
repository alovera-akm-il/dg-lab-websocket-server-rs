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
