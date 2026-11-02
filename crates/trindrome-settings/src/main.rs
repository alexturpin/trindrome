//! `trindrome-settings`: a small window for the settings people actually change,
//! mostly about when Trindrome may use the computer.
//!
//! It edits `trindrome.toml` in place (keeping comments and every other key) and
//! the running agent reloads it within a few seconds. Start-at-login is applied
//! immediately through the registry.

#![cfg_attr(windows, windows_subsystem = "windows")]

use std::path::PathBuf;
use std::time::{Duration, Instant};

use eframe::egui::{self, Color32, RichText};
use trindrome_worker::configuration::{Config, config_path};
use trindrome_worker::idle::running_process_names;
use trindrome_worker::persistence::Store;
use trindrome_worker::scheduler::worker_is_running;
use trindrome_worker::settings_file::{self, UserSettings};
use trindrome_worker::startup;

fn main() -> eframe::Result {
    let mut args = std::env::args().skip(1);
    let explicit = match (args.next().as_deref(), args.next()) {
        (Some("--config"), Some(path)) => Some(PathBuf::from(path)),
        _ => None,
    };
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("Trindrome settings")
            .with_inner_size([540.0, 700.0])
            .with_min_inner_size([420.0, 400.0]),
        ..Default::default()
    };
    eframe::run_native(
        "Trindrome settings",
        options,
        Box::new(move |_cc| Ok(Box::new(SettingsApp::new(explicit)))),
    )
}

struct Status {
    running: bool,
    state: String,
    reason: String,
    searched: u64,
    nontrivial: usize,
}

struct SettingsApp {
    explicit: Option<PathBuf>,
    path: PathBuf,
    database: PathBuf,
    load_error: Option<String>,
    saved: UserSettings,
    edit: UserSettings,
    new_app: String,
    processes: Vec<String>,
    picked: String,
    message: Option<(String, bool)>,
    store: Option<Store>,
    status: Option<Status>,
    last_status: Option<Instant>,
    starts_at_login: bool,
}

impl SettingsApp {
    fn new(explicit: Option<PathBuf>) -> Self {
        let (path, required) = config_path(explicit.as_deref());
        let (config, load_error) = match Config::load_from(&path, required) {
            Ok(c) => (c, None),
            Err(e) => (Config::default(), Some(format!("{e:#}"))),
        };
        let saved = UserSettings::from_config(&config);
        Self {
            explicit,
            path,
            database: config.storage.database_path.clone(),
            load_error,
            edit: saved.clone(),
            saved,
            new_app: String::new(),
            processes: running_process_names(),
            picked: String::new(),
            message: None,
            store: None,
            status: None,
            last_status: None,
            starts_at_login: startup::get(startup::RUN_KEY, startup::VALUE_NAME)
                .ok()
                .flatten()
                .is_some(),
        }
    }

    fn refresh_status(&mut self) {
        if self.last_status.is_some_and(|t| t.elapsed() < Duration::from_secs(1)) {
            return;
        }
        self.last_status = Some(Instant::now());
        if self.store.is_none() && self.database.exists() {
            self.store = Store::open(&self.database).ok();
        }
        let running = worker_is_running(&self.database);
        self.status = self.store.as_ref().and_then(|store| {
            let w = store.worker().ok()?;
            Some(Status {
                running,
                state: if running { w.state } else { "not running".into() },
                reason: if running {
                    w.reason.unwrap_or_default()
                } else {
                    String::new()
                },
                searched: store.totals().map(|t| t.candidates).unwrap_or(0),
                nontrivial: store
                    .discoveries()
                    .map(|d| d.iter().filter(|d| d.nontrivial).count())
                    .unwrap_or(0),
            })
        });
    }

    fn agent_command(&self) -> Option<String> {
        let agent = std::env::current_exe().ok()?.with_file_name("trindrome-agent.exe");
        let mut command = format!("\"{}\"", agent.display());
        if let Some(path) = &self.explicit {
            command.push_str(&format!(" --config \"{}\"", path.display()));
        }
        Some(command)
    }

    fn start_agent(&mut self) {
        let Ok(agent) = std::env::current_exe().map(|p| p.with_file_name("trindrome-agent.exe")) else {
            return;
        };
        let mut cmd = std::process::Command::new(&agent);
        if let Some(path) = &self.explicit {
            cmd.arg("--config").arg(path);
        }
        self.message = Some(match cmd.spawn() {
            Ok(_) => ("Started Trindrome.".into(), false),
            Err(e) => (format!("Cannot start {}: {e}", agent.display()), true),
        });
    }

    fn save(&mut self) {
        self.message = Some(match settings_file::save(&self.path, &self.edit) {
            Ok(()) => {
                self.saved = self.edit.clone();
                (
                    "Saved. The running agent applies changes within a few seconds.".into(),
                    false,
                )
            }
            Err(e) => (format!("Not saved: {e:#}"), true),
        });
    }

    fn status_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Status");
        match &self.status {
            Some(s) if s.running => {
                let colour = match s.state.as_str() {
                    "running" => Color32::from_rgb(0x2E, 0x9E, 0x44),
                    "error" => Color32::from_rgb(0xDC, 0x26, 0x26),
                    _ => Color32::from_rgb(0x3B, 0x82, 0xF6),
                };
                ui.colored_label(colour, RichText::new(&s.state).strong());
                if s.state != "running" {
                    ui.label(&s.reason);
                }
                ui.label(format!("{} candidates searched so far.", group(s.searched)));
                if s.nontrivial > 0 {
                    ui.colored_label(Color32::GOLD, "A verified nontrivial discovery is recorded!");
                }
            }
            other => {
                ui.label("Trindrome is not running.");
                if let Some(s) = other {
                    ui.label(format!("{} candidates searched so far.", group(s.searched)));
                }
                if ui.button("Start Trindrome").clicked() {
                    self.start_agent();
                }
            }
        }
    }

    fn when_section(&mut self, ui: &mut egui::Ui) {
        let e = &mut self.edit;
        ui.heading("When to compute");
        ui.checkbox(&mut e.idle_enabled, "Only while I'm away from the computer");
        ui.add_enabled_ui(e.idle_enabled, |ui| {
            ui.horizontal(|ui| {
                ui.label("Start after no keyboard or mouse input for");
                ui.add(egui::DragValue::new(&mut e.idle_minutes).range(1..=240).suffix(" min"));
            });
            ui.horizontal(|ui| {
                ui.label("…and only if the GPU was below");
                ui.add(
                    egui::DragValue::new(&mut e.gpu_start_threshold_percent)
                        .range(1..=100)
                        .suffix(" %"),
                );
                ui.label("busy for");
                ui.add(
                    egui::DragValue::new(&mut e.gpu_idle_sample_seconds)
                        .range(5..=600)
                        .suffix(" s"),
                );
            });
            ui.small("Any keyboard or mouse input pauses the search at once.");
        });
        ui.checkbox(&mut e.require_ac_power, "Only when plugged in");
        ui.checkbox(
            &mut e.prevent_automatic_sleep,
            "Keep the computer awake while computing on AC power",
        )
        .on_hover_text(
            "Never blocks sleeping on request, shutting down or closing the lid, and never keeps the display on.",
        );
    }

    fn apps_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Pause while these are running");
        ui.small("Games played with a controller don't count as keyboard/mouse activity, so list them here.");
        let mut remove = None;
        for (i, app) in self.edit.exclusive_gpu_apps.iter().enumerate() {
            ui.horizontal(|ui| {
                if ui.small_button("Remove").clicked() {
                    remove = Some(i);
                }
                ui.monospace(app);
            });
        }
        if let Some(i) = remove {
            self.edit.exclusive_gpu_apps.remove(i);
        }
        if self.edit.exclusive_gpu_apps.is_empty() {
            ui.weak("(none)");
        }

        ui.horizontal(|ui| {
            ui.add(egui::TextEdit::singleline(&mut self.new_app).hint_text("game.exe or C:\\path\\to\\game.exe"));
            if ui
                .add_enabled(!self.new_app.trim().is_empty(), egui::Button::new("Add"))
                .clicked()
            {
                let app = self.new_app.trim().to_string();
                add_unique(&mut self.edit.exclusive_gpu_apps, app);
                self.new_app.clear();
            }
        });
        ui.horizontal(|ui| {
            egui::ComboBox::from_id_salt("running")
                .selected_text(if self.picked.is_empty() {
                    "Pick a running application…"
                } else {
                    &self.picked
                })
                .width(260.0)
                .show_ui(ui, |ui| {
                    for name in &self.processes {
                        ui.selectable_value(&mut self.picked, name.clone(), name);
                    }
                });
            if ui
                .add_enabled(!self.picked.is_empty(), egui::Button::new("Add"))
                .clicked()
            {
                add_unique(&mut self.edit.exclusive_gpu_apps, std::mem::take(&mut self.picked));
            }
            if ui
                .button("Refresh")
                .on_hover_text("Refresh the list of running applications")
                .clicked()
            {
                self.processes = running_process_names();
            }
        });
    }

    fn thermal_section(&mut self, ui: &mut egui::Ui) {
        let e = &mut self.edit;
        ui.heading("GPU temperature");
        ui.checkbox(&mut e.thermal_enabled, "Pause when the GPU gets hot");
        ui.add_enabled_ui(e.thermal_enabled, |ui| {
            ui.horizontal(|ui| {
                ui.label("Pause at");
                ui.add(
                    egui::DragValue::new(&mut e.pause_temperature_c)
                        .range(50..=100)
                        .suffix(" °C"),
                );
                ui.label("and resume below");
                ui.add(
                    egui::DragValue::new(&mut e.resume_temperature_c)
                        .range(40..=99)
                        .suffix(" °C"),
                );
            });
        });
        if e.thermal_enabled && e.resume_temperature_c >= e.pause_temperature_c {
            ui.colored_label(
                Color32::from_rgb(0xDC, 0x26, 0x26),
                "The resume temperature must be below the pause temperature.",
            );
        }
    }

    fn startup_section(&mut self, ui: &mut egui::Ui) {
        ui.heading("Startup");
        let mut wanted = self.starts_at_login;
        if ui.checkbox(&mut wanted, "Start Trindrome when I sign in").changed() {
            let result = match (wanted, self.agent_command()) {
                (true, Some(command)) => startup::set(startup::RUN_KEY, startup::VALUE_NAME, &command),
                (true, None) => Err(anyhow::anyhow!("cannot locate trindrome-agent.exe")),
                (false, _) => startup::remove(startup::RUN_KEY, startup::VALUE_NAME),
            };
            match result {
                Ok(()) => self.starts_at_login = wanted,
                Err(e) => self.message = Some((format!("Cannot change startup: {e:#}"), true)),
            }
        }
    }
}

fn add_unique(list: &mut Vec<String>, item: String) {
    if !list.iter().any(|a| a.eq_ignore_ascii_case(&item)) {
        list.push(item);
    }
}

fn group(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

impl eframe::App for SettingsApp {
    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        self.refresh_status();
        ui.ctx().request_repaint_after(Duration::from_secs(1));

        egui::CentralPanel::default().show(ui, |ui| {
            egui::ScrollArea::vertical().show(ui, |ui| {
                if let Some(e) = &self.load_error {
                    ui.colored_label(
                        Color32::from_rgb(0xDC, 0x26, 0x26),
                        format!("The settings file could not be read, so defaults are shown: {e}"),
                    );
                    ui.separator();
                }
                self.status_section(ui);
                ui.separator();
                self.when_section(ui);
                ui.separator();
                self.apps_section(ui);
                ui.separator();
                self.thermal_section(ui);
                ui.separator();
                self.startup_section(ui);
                ui.separator();

                let changed = self.edit != self.saved;
                ui.horizontal(|ui| {
                    if ui.add_enabled(changed, egui::Button::new("Save")).clicked() {
                        self.save();
                    }
                    if ui.add_enabled(changed, egui::Button::new("Revert")).clicked() {
                        self.edit = self.saved.clone();
                        self.message = None;
                    }
                    if changed {
                        ui.weak("Unsaved changes");
                    }
                });
                if let Some((text, is_error)) = &self.message {
                    let colour = if *is_error {
                        Color32::from_rgb(0xDC, 0x26, 0x26)
                    } else {
                        ui.visuals().text_color()
                    };
                    ui.colored_label(colour, text);
                }
                ui.add_space(8.0);
                ui.weak(format!("Settings file: {}", self.path.display()));
            });
        });
    }
}
