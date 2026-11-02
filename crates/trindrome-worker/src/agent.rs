//! `trindrome-agent`: the background worker with a tray icon, without a console.
//!
//! Started at login (see `trindrome startup enable`), it runs in the user's
//! session so idle detection works, computes only while the resource policy
//! allows, and logs to `<data dir>\logs`. The worker runs on a background
//! thread; the main thread owns the tray icon and its message loop. Control and
//! status go through the database exactly as for the CLI, so `trindrome
//! status`, `pause`, `resume` and `stop` work alongside the tray menu.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;

use anyhow::Result;
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
    info!(version = env!("CARGO_PKG_VERSION"), "trindrome-agent starting");

    let config = match Config::load(explicit.as_deref()) {
        Ok(c) => c,
        Err(e) => {
            error!("cannot start: {e:#}");
            notify(&format!("Trindrome cannot start:\n\n{e:#}"));
            std::process::exit(1);
        }
    };
    if scheduler::worker_is_running(&config.storage.database_path) {
        info!("another worker already uses this database; exiting");
        notify("Trindrome is already running.");
        return;
    }

    let worker = Worker::spawn(explicit.clone());
    #[cfg(windows)]
    tray::run(config, explicit, worker);
    #[cfg(not(windows))]
    {
        let mut worker = worker;
        let _ = config;
        worker.join();
    }
}

/// The search running on a background thread.
pub struct Worker {
    interrupted: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<String>>>,
    outcome: Option<String>,
}

impl Worker {
    fn spawn(explicit: Option<PathBuf>) -> Self {
        let interrupted = Arc::new(AtomicBool::new(false));
        let flag = Arc::clone(&interrupted);
        let handle = std::thread::Builder::new()
            .name("worker".into())
            .spawn(move || {
                // Reload so a restart picks up the current configuration file.
                let config = Config::load(explicit.as_deref())?;
                let engines = engines_for(&config.compute, false, None)?;
                let options = RunOptions {
                    immediate: false,
                    config_file: Some(config_path(explicit.as_deref())),
                };
                scheduler::run(&config, engines, &options, &flag)
            })
            .expect("can spawn the worker thread");
        Self {
            interrupted,
            handle: Some(handle),
            outcome: None,
        }
    }

    /// The worker's final message once it has stopped (`None` while running).
    fn outcome(&mut self) -> Option<&str> {
        if self.handle.as_ref().is_some_and(|h| h.is_finished()) {
            self.join();
        }
        self.outcome.as_deref()
    }

    fn request_stop(&self) {
        self.interrupted.store(true, Ordering::SeqCst);
    }

    fn join(&mut self) {
        if let Some(handle) = self.handle.take() {
            let outcome = match handle.join() {
                Ok(Ok(reason)) => reason,
                Ok(Err(e)) => format!("error: {e:#}"),
                Err(_) => "error: the worker panicked".into(),
            };
            info!(%outcome, "worker finished");
            if outcome.starts_with("error") {
                error!("{outcome}");
            }
            self.outcome = Some(outcome);
        }
    }
}

#[cfg(windows)]
fn notify(text: &str) {
    use windows::Win32::UI::WindowsAndMessaging::{MB_ICONINFORMATION, MB_OK, MessageBoxW};
    use windows::core::HSTRING;
    // SAFETY: plain modal message box with owned strings.
    unsafe {
        MessageBoxW(
            None,
            &HSTRING::from(text),
            &HSTRING::from("Trindrome"),
            MB_OK | MB_ICONINFORMATION,
        )
    };
}

#[cfg(not(windows))]
fn notify(text: &str) {
    eprintln!("{text}");
}

#[cfg(windows)]
mod tray {
    use std::path::PathBuf;
    use std::time::{Duration, Instant};

    use tracing::{info, warn};
    use tray_icon::menu::{Menu, MenuEvent, MenuItem, PredefinedMenuItem};
    use tray_icon::{Icon, TrayIcon, TrayIconBuilder};
    use trindrome_worker::configuration::{Config, data_dir};
    use trindrome_worker::persistence::{Requested, Store};

    use super::{Worker, notify};

    const REFRESH: Duration = Duration::from_secs(1);

    /// Icon colour per worker state.
    fn color(state: &str) -> [u8; 3] {
        match state {
            "running" => [0x2E, 0x9E, 0x44],
            "waiting" => [0x3B, 0x82, 0xF6],
            "error" | "failed" => [0xDC, 0x26, 0x26],
            _ => [0x9C, 0xA3, 0xAF],
        }
    }

    /// A smooth filled circle with a darker rim.
    fn icon(rgb: [u8; 3]) -> Icon {
        const SIZE: u32 = 32;
        let mut rgba = Vec::with_capacity((SIZE * SIZE * 4) as usize);
        let center = (SIZE as f32 - 1.0) / 2.0;
        for y in 0..SIZE {
            for x in 0..SIZE {
                let d = ((x as f32 - center).powi(2) + (y as f32 - center).powi(2)).sqrt();
                let alpha = (14.5 - d).clamp(0.0, 1.0);
                let rim = if d > 12.0 { 0.65 } else { 1.0 };
                rgba.extend(rgb.iter().map(|&c| (c as f32 * rim) as u8));
                rgba.push((alpha * 255.0) as u8);
            }
        }
        Icon::from_rgba(rgba, SIZE, SIZE).expect("valid icon")
    }

    fn human(v: f64) -> String {
        match v {
            v if v >= 1e12 => format!("{:.2}T", v / 1e12),
            v if v >= 1e9 => format!("{:.2}B", v / 1e9),
            v if v >= 1e6 => format!("{:.2}M", v / 1e6),
            v => format!("{v:.0}"),
        }
    }

    struct Items {
        status: MenuItem,
        pause: MenuItem,
        settings: MenuItem,
        folder: MenuItem,
        restart: MenuItem,
        quit: MenuItem,
    }

    pub fn run(config: Config, explicit: Option<PathBuf>, mut worker: Worker) {
        let items = Items {
            status: MenuItem::new("Trindrome — starting", false, None),
            pause: MenuItem::new("Pause", true, None),
            settings: MenuItem::new("Settings…", true, None),
            folder: MenuItem::new("Open data folder", true, None),
            restart: MenuItem::new("Restart worker", false, None),
            quit: MenuItem::new("Quit Trindrome", true, None),
        };
        let menu = Menu::new();
        let separator = PredefinedMenuItem::separator();
        let _ = menu.append_items(&[
            &items.status,
            &separator,
            &items.pause,
            &items.settings,
            &items.folder,
            &items.restart,
            &PredefinedMenuItem::separator(),
            &items.quit,
        ]);
        let tray = match TrayIconBuilder::new()
            .with_menu(Box::new(menu))
            .with_tooltip("Trindrome")
            .with_icon(icon(color("starting")))
            .build()
        {
            Ok(t) => t,
            Err(e) => {
                // Without a tray the agent still works; it just has no icon.
                warn!("cannot create the tray icon: {e}; continuing without it");
                worker.join();
                return;
            }
        };
        info!("tray icon ready");

        let store = Store::open(&config.storage.database_path).ok();
        let mut last_refresh = Instant::now() - REFRESH;
        let mut shown_state = String::new();
        let mut quitting = false;
        loop {
            pump_messages();

            while let Ok(event) = MenuEvent::receiver().try_recv() {
                let id = event.id;
                if id == *items.quit.id() {
                    info!("quit requested from the tray");
                    worker.request_stop();
                    items.status.set_text("Trindrome — stopping…");
                    quitting = true;
                } else if id == *items.pause.id() {
                    if let Some(store) = &store {
                        let paused = store.worker().map(|w| w.requested == "pause").unwrap_or(false);
                        let _ = store.set_requested(if paused { Requested::Run } else { Requested::Pause });
                        last_refresh = Instant::now() - REFRESH;
                    }
                } else if id == *items.settings.id() {
                    open_settings(explicit.as_ref());
                } else if id == *items.folder.id() {
                    let _ = std::process::Command::new("explorer").arg(data_dir()).spawn();
                } else if id == *items.restart.id() && worker.outcome().is_some() {
                    info!("restarting the worker from the tray");
                    worker = Worker::spawn(explicit.clone());
                    last_refresh = Instant::now() - REFRESH;
                }
            }

            // Quit, or `trindrome stop`, closes the agent; an error or an exhausted
            // plan keeps the tray up so the user sees it and can restart.
            if let Some(outcome) = worker.outcome()
                && (quitting || outcome == "stop requested")
            {
                break;
            }
            if last_refresh.elapsed() >= REFRESH {
                last_refresh = Instant::now();
                refresh(&tray, &items, store.as_ref(), &mut worker, &mut shown_state);
            }
            std::thread::sleep(Duration::from_millis(50));
        }
        info!("trindrome-agent exiting");
    }

    fn refresh(tray: &TrayIcon, items: &Items, store: Option<&Store>, worker: &mut Worker, shown: &mut String) {
        let (state, line, paused) = match (
            worker.outcome().map(str::to_string),
            store.and_then(|s| s.worker().ok()),
        ) {
            (Some(outcome), _) => {
                let failed = outcome.starts_with("error");
                (
                    if failed { "failed" } else { "stopped" }.to_string(),
                    format!("stopped: {outcome}"),
                    false,
                )
            }
            (None, Some(w)) => {
                let reason = w.reason.clone().unwrap_or_default();
                let line = if w.state == "running" {
                    "computing".to_string()
                } else {
                    reason
                };
                (w.state.clone(), line, w.requested == "pause")
            }
            (None, None) => ("starting".into(), "starting".into(), false),
        };
        let progress = store
            .and_then(|s| s.totals().ok())
            .map(|t| format!("{} searched", human(t.candidates as f64)))
            .unwrap_or_default();

        let header = format!("Trindrome — {line}");
        items.status.set_text(truncate(&header, 90));
        items.pause.set_text(if paused { "Resume" } else { "Pause" });
        items.pause.set_enabled(worker.outcome().is_none());
        items.restart.set_enabled(worker.outcome().is_some());
        // Windows limits tray tooltips to 127 characters.
        let _ = tray.set_tooltip(Some(truncate(&format!("Trindrome: {line}\n{progress}"), 127)));
        if state != *shown {
            let _ = tray.set_icon(Some(icon(color(&state))));
            *shown = state;
        }
    }

    fn truncate(s: &str, max: usize) -> String {
        if s.chars().count() <= max {
            s.to_string()
        } else {
            let mut t: String = s.chars().take(max - 1).collect();
            t.push('…');
            t
        }
    }

    fn open_settings(explicit: Option<&PathBuf>) {
        let Ok(exe) = std::env::current_exe().map(|p| p.with_file_name("trindrome-settings.exe")) else {
            return;
        };
        if !exe.exists() {
            notify(&format!("The settings window ({}) is missing.", exe.display()));
            return;
        }
        let mut cmd = std::process::Command::new(exe);
        if let Some(path) = explicit {
            cmd.arg("--config").arg(path);
        }
        if let Err(e) = cmd.spawn() {
            notify(&format!("Cannot open settings: {e}"));
        }
    }

    /// Dispatches pending window messages; the tray icon's window needs them.
    fn pump_messages() {
        use windows::Win32::UI::WindowsAndMessaging::{
            DispatchMessageW, MSG, PM_REMOVE, PeekMessageW, TranslateMessage,
        };
        let mut msg = MSG::default();
        // SAFETY: standard message pump on the thread that owns the tray window.
        unsafe {
            while PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE).as_bool() {
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
        }
    }
}
