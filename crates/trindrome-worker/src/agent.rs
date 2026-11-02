//! `trindrome-agent`: the background worker, without a console window.
//!
//! Started at login (see `trindrome startup enable`), it runs in the user's
//! session so idle detection works, computes only while the resource policy
//! allows, and logs to `<data dir>\logs`. Control and status go through the
//! database exactly as for the CLI (`trindrome status`, `pause`, `resume`,
//! `stop`). This process will also host the tray icon.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::atomic::AtomicBool;

use anyhow::{Result, bail};
use tracing::{error, info};
use trindrome_worker::configuration::{Config, config_path, data_dir};
use trindrome_worker::engine::engines_for;
use trindrome_worker::scheduler::{self, RunOptions};

fn main() {
    // Usage: trindrome-agent [--config <path>]
    let mut args = std::env::args().skip(1);
    let explicit = match (args.next().as_deref(), args.next()) {
        (Some("--config"), Some(path)) => Some(PathBuf::from(path)),
        _ => None,
    };

    let logs = data_dir().join("logs");
    let _ = std::fs::create_dir_all(&logs);
    let (writer, _guard) = tracing_appender::non_blocking(tracing_appender::rolling::daily(&logs, "agent.log"));
    let level = Config::load(explicit.as_deref())
        .ok()
        .and_then(|c| c.logging.level.parse().ok())
        .unwrap_or(tracing::Level::INFO);
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_ansi(false)
        .with_writer(writer)
        .init();

    if let Err(e) = run(explicit) {
        error!("agent stopped: {e:#}");
        std::process::exit(1);
    }
}

fn run(explicit: Option<PathBuf>) -> Result<()> {
    let config = Config::load(explicit.as_deref())?;
    if scheduler::worker_is_running(&config.storage.database_path) {
        bail!(
            "another worker is already running on {}",
            config.storage.database_path.display()
        );
    }
    let engines = engines_for(&config.compute, false, None)?;
    let options = RunOptions {
        immediate: false,
        config_file: Some(config_path(explicit.as_deref())),
    };
    info!(version = env!("CARGO_PKG_VERSION"), "trindrome-agent starting");
    let reason = scheduler::run(&config, engines, &options, &AtomicBool::new(false))?;
    info!(%reason, "trindrome-agent finished");
    Ok(())
}
