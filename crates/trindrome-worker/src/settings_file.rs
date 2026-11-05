//! The user-facing settings in `trindrome.toml`, edited in place.
//!
//! Front ends (the settings window) change only these keys. Comments, layout
//! and every other key in the file are preserved. A change is validated as a
//! complete configuration before the file is replaced, so a running worker,
//! which reloads the file live, never sees an invalid one.

use std::path::Path;

use anyhow::{Context, Result};
use toml_edit::{Array, DocumentMut, Item, Table, value};

use crate::configuration::Config;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UserSettings {
    pub idle_enabled: bool,
    pub idle_minutes: u32,
    pub gpu_start_threshold_percent: u32,
    pub gpu_idle_sample_seconds: u32,
    pub require_ac_power: bool,
    pub exclusive_gpu_apps: Vec<String>,
    pub thermal_enabled: bool,
    pub pause_temperature_c: u32,
    pub resume_temperature_c: u32,
    pub prevent_automatic_sleep: bool,
    /// Display name on the coordinator's public page; empty means none.
    pub coordinator_name: String,
}

impl UserSettings {
    pub fn from_config(c: &Config) -> Self {
        Self {
            idle_enabled: c.idle.enabled,
            idle_minutes: c.idle.idle_minutes,
            gpu_start_threshold_percent: c.idle.gpu_start_threshold_percent,
            gpu_idle_sample_seconds: c.idle.gpu_idle_sample_seconds,
            require_ac_power: c.idle.require_ac_power,
            exclusive_gpu_apps: c.idle.exclusive_gpu_apps.clone(),
            thermal_enabled: c.thermal.enabled,
            pause_temperature_c: c.thermal.pause_temperature_c,
            resume_temperature_c: c.thermal.resume_temperature_c,
            prevent_automatic_sleep: c.power.prevent_automatic_sleep,
            coordinator_name: c.coordinator.name.clone().unwrap_or_default(),
        }
    }

    pub fn apply_to(&self, c: &mut Config) {
        c.idle.enabled = self.idle_enabled;
        c.idle.idle_minutes = self.idle_minutes;
        c.idle.gpu_start_threshold_percent = self.gpu_start_threshold_percent;
        c.idle.gpu_idle_sample_seconds = self.gpu_idle_sample_seconds;
        c.idle.require_ac_power = self.require_ac_power;
        c.idle.exclusive_gpu_apps = self.exclusive_gpu_apps.clone();
        c.thermal.enabled = self.thermal_enabled;
        c.thermal.pause_temperature_c = self.pause_temperature_c;
        c.thermal.resume_temperature_c = self.resume_temperature_c;
        c.power.prevent_automatic_sleep = self.prevent_automatic_sleep;
        c.coordinator.name = Some(self.coordinator_name.trim().to_string()).filter(|n| !n.is_empty());
    }
}

fn section<'a>(doc: &'a mut DocumentMut, name: &str) -> &'a mut Table {
    if !doc.as_table().get(name).is_some_and(Item::is_table) {
        doc[name] = Item::Table(Table::new());
    }
    doc[name].as_table_mut().expect("just ensured")
}

/// Rewrites the settings keys of the file at `path` (created if missing).
/// Fails, leaving the file untouched, if the result would be invalid.
pub fn save(path: &Path, s: &UserSettings) -> Result<()> {
    let text = match std::fs::read_to_string(path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", path.display())),
    };
    let mut doc: DocumentMut = text
        .parse()
        .with_context(|| format!("{} is not valid TOML", path.display()))?;

    let idle = section(&mut doc, "idle");
    idle["enabled"] = value(s.idle_enabled);
    idle["idle_minutes"] = value(s.idle_minutes as i64);
    idle["gpu_start_threshold_percent"] = value(s.gpu_start_threshold_percent as i64);
    idle["gpu_idle_sample_seconds"] = value(s.gpu_idle_sample_seconds as i64);
    idle["require_ac_power"] = value(s.require_ac_power);
    let mut apps = Array::new();
    for app in s.exclusive_gpu_apps.iter().map(|a| a.trim()).filter(|a| !a.is_empty()) {
        apps.push(app);
    }
    idle["exclusive_gpu_apps"] = value(apps);

    let thermal = section(&mut doc, "thermal");
    thermal["enabled"] = value(s.thermal_enabled);
    thermal["pause_temperature_c"] = value(s.pause_temperature_c as i64);
    thermal["resume_temperature_c"] = value(s.resume_temperature_c as i64);

    section(&mut doc, "power")["prevent_automatic_sleep"] = value(s.prevent_automatic_sleep);

    let name = s.coordinator_name.trim();
    if !name.is_empty() {
        section(&mut doc, "coordinator")["name"] = value(name);
    } else if let Some(coordinator) = doc.get_mut("coordinator").and_then(Item::as_table_mut) {
        coordinator.remove("name");
    }

    let updated = doc.to_string();
    let config: Config = toml::from_str(&updated).context("the new settings do not form a valid configuration")?;
    config.validate()?;

    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let temporary = path.with_extension("toml.saving");
    std::fs::write(&temporary, updated)?;
    std::fs::rename(&temporary, path).with_context(|| format!("cannot replace {}", path.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ORIGINAL: &str = r#"# My notes about this machine.
[compute]
batch_target_ms = 150   # tuned by hand

[idle]
# five minutes is too eager
idle_minutes = 10
exclusive_gpu_apps = ["game.exe"]

[search]
mode = "frontier"
"#;

    #[test]
    fn saving_changes_only_settings_and_keeps_comments() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trindrome.toml");
        std::fs::write(&path, ORIGINAL).unwrap();
        let mut s = UserSettings::from_config(&Config::load_from(&path, true).unwrap());
        assert_eq!(s.idle_minutes, 10);
        s.idle_minutes = 3;
        s.exclusive_gpu_apps.push(r"C:\Games\Sim\sim.exe".into());
        s.prevent_automatic_sleep = true;
        s.coordinator_name = " Alex ".into();
        save(&path, &s).unwrap();

        let text = std::fs::read_to_string(&path).unwrap();
        for kept in [
            "# My notes about this machine.",
            "# tuned by hand",
            "# five minutes is too eager",
            r#"mode = "frontier""#,
        ] {
            assert!(text.contains(kept), "lost {kept:?}:\n{text}");
        }
        let reloaded = Config::load_from(&path, true).unwrap();
        s.coordinator_name = "Alex".into();
        assert_eq!(UserSettings::from_config(&reloaded), s);
        assert_eq!(reloaded.compute.batch_target_ms, 150);

        // Clearing the name removes the key rather than writing an empty one.
        s.coordinator_name.clear();
        save(&path, &s).unwrap();
        assert_eq!(Config::load_from(&path, true).unwrap().coordinator.name, None);
    }

    #[test]
    fn invalid_settings_leave_the_file_untouched() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("trindrome.toml");
        std::fs::write(&path, ORIGINAL).unwrap();
        let mut s = UserSettings::from_config(&Config::load_from(&path, true).unwrap());
        s.resume_temperature_c = s.pause_temperature_c;
        assert!(save(&path, &s).is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), ORIGINAL);
    }

    #[test]
    fn a_missing_file_is_created() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("sub").join("trindrome.toml");
        let mut s = UserSettings::from_config(&Config::default());
        s.require_ac_power = false;
        save(&path, &s).unwrap();
        assert!(!Config::load_from(&path, true).unwrap().idle.require_ac_power);
    }
}
