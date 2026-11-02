//! `trindrome.toml` configuration.
//!
//! Nothing here can invalidate recorded coverage: search-space identity comes from
//! the generator, and chunk or batch sizes only change how work is cut up.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};

pub const DEFAULT_CONFIG_FILE: &str = "trindrome.toml";

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub compute: Compute,
    pub idle: Idle,
    pub thermal: Thermal,
    pub power: Power,
    pub search: Search,
    pub storage: Storage,
    pub logging: Logging,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Backend {
    Auto,
    Cpu,
    Cuda,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Compute {
    pub backend: Backend,
    pub gpu_device: u32,
    /// Target duration of one uninterruptible batch; bounds pause latency.
    pub batch_target_ms: u64,
    /// Longest stretch of finished work that may be held in memory before committing.
    pub checkpoint_interval_seconds: u64,
    /// CPU search threads; 0 means half the logical processors.
    pub cpu_threads: usize,
    /// Size of each newly allocated work chunk.
    pub chunk_candidates: u64,
    /// Audit one random slice of every Nth GPU batch against the CPU (0 disables).
    pub audit_every_batches: u32,
    /// Candidates per audit slice.
    pub audit_candidates: u64,
}

impl Default for Compute {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            gpu_device: 0,
            batch_target_ms: 200,
            checkpoint_interval_seconds: 30,
            cpu_threads: 0,
            chunk_candidates: 1 << 30,
            audit_every_batches: 25,
            audit_candidates: 1 << 15,
        }
    }
}

impl Compute {
    pub fn effective_cpu_threads(&self) -> usize {
        if self.cpu_threads > 0 {
            return self.cpu_threads;
        }
        let logical = std::thread::available_parallelism().map_or(2, |n| n.get());
        (logical / 2).max(1)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Idle {
    pub enabled: bool,
    pub idle_minutes: u32,
    pub gpu_start_threshold_percent: u32,
    pub gpu_idle_sample_seconds: u32,
    pub require_ac_power: bool,
    pub exclusive_gpu_apps: Vec<String>,
}

impl Default for Idle {
    fn default() -> Self {
        Self {
            enabled: true,
            idle_minutes: 5,
            gpu_start_threshold_percent: 10,
            gpu_idle_sample_seconds: 30,
            require_ac_power: true,
            exclusive_gpu_apps: Vec::new(),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Thermal {
    pub enabled: bool,
    pub pause_temperature_c: u32,
    pub resume_temperature_c: u32,
}

impl Default for Thermal {
    fn default() -> Self {
        Self {
            enabled: true,
            pause_temperature_c: 80,
            resume_temperature_c: 72,
        }
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Power {
    pub prevent_automatic_sleep: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SearchMode {
    /// Regan's 2010 coverage only.
    HistoricalReplay,
    /// Lengths beyond the 2010 coverage only.
    Frontier,
    /// Historical replay first, then the frontier.
    Both,
}

impl SearchMode {
    pub fn includes_historical(self) -> bool {
        matches!(self, Self::HistoricalReplay | Self::Both)
    }

    pub fn includes_frontier(self) -> bool {
        matches!(self, Self::Frontier | Self::Both)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Search {
    pub mode: SearchMode,
    pub minimum_hex_digits: u32,
    /// Optional cap on hexadecimal length, for short or targeted runs.
    pub maximum_hex_digits: Option<u32>,
    pub stop_on_first_verified_discovery: bool,
}

impl Default for Search {
    fn default() -> Self {
        Self {
            mode: SearchMode::HistoricalReplay,
            minimum_hex_digits: 2,
            maximum_hex_digits: None,
            stop_on_first_verified_discovery: true,
        }
    }
}

impl Search {
    pub fn allows_length(&self, hex_digits: u32) -> bool {
        hex_digits >= self.minimum_hex_digits && self.maximum_hex_digits.is_none_or(|max| hex_digits <= max)
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Storage {
    pub database_path: PathBuf,
}

impl Default for Storage {
    fn default() -> Self {
        Self {
            database_path: PathBuf::from("data/search.sqlite"),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Logging {
    pub level: String,
}

impl Default for Logging {
    fn default() -> Self {
        Self { level: "info".into() }
    }
}

impl Config {
    /// Loads `path`, or the default file if it exists, or built-in defaults.
    ///
    /// A relative database path is resolved against the config file's directory.
    pub fn load(path: Option<&Path>) -> Result<Self> {
        let (path, required) = match path {
            Some(p) => (p.to_path_buf(), true),
            None => (PathBuf::from(DEFAULT_CONFIG_FILE), false),
        };
        let mut config = match std::fs::read_to_string(&path) {
            Ok(text) => {
                toml::from_str::<Config>(&text).with_context(|| format!("invalid config {}", path.display()))?
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound && !required => Config::default(),
            Err(e) => return Err(e).with_context(|| format!("cannot read config {}", path.display())),
        };
        if config.storage.database_path.is_relative()
            && let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty())
        {
            config.storage.database_path = dir.join(&config.storage.database_path);
        }
        config.validate()?;
        Ok(config)
    }

    pub fn validate(&self) -> Result<()> {
        let c = &self.compute;
        if c.batch_target_ms == 0 || c.batch_target_ms > 10_000 {
            bail!("compute.batch_target_ms must be between 1 and 10000");
        }
        if c.chunk_candidates == 0 {
            bail!("compute.chunk_candidates must be positive");
        }
        if c.audit_candidates == 0 {
            bail!("compute.audit_candidates must be positive");
        }
        let s = &self.search;
        if s.minimum_hex_digits == 0 {
            bail!("search.minimum_hex_digits must be at least 1");
        }
        if let Some(max) = s.maximum_hex_digits
            && max < s.minimum_hex_digits
        {
            bail!("search.maximum_hex_digits ({max}) is below search.minimum_hex_digits");
        }
        if self.thermal.resume_temperature_c >= self.thermal.pause_temperature_c {
            bail!("thermal.resume_temperature_c must be below thermal.pause_temperature_c");
        }
        if self.logging.level.parse::<tracing::Level>().is_err() {
            bail!("logging.level must be one of trace, debug, info, warn, error");
        }
        Ok(())
    }

    /// Settings that parse but are not enforced by this build yet.
    pub fn unenforced_settings(&self) -> Vec<&'static str> {
        let mut unenforced = vec!["[idle] (idle detection arrives in Phase 4)"];
        if self.thermal.enabled {
            unenforced.push("[thermal] (GPU telemetry arrives in Phase 4)");
        }
        if self.power.prevent_automatic_sleep {
            unenforced.push("power.prevent_automatic_sleep (arrives in Phase 4)");
        }
        unenforced
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_example_parses_to_defaults() {
        let text = r#"
            [compute]
            backend = "auto"
            gpu_device = 0
            batch_target_ms = 200
            checkpoint_interval_seconds = 30

            [idle]
            enabled = true
            idle_minutes = 5
            gpu_start_threshold_percent = 10
            gpu_idle_sample_seconds = 30
            require_ac_power = true
            exclusive_gpu_apps = []

            [thermal]
            enabled = true
            pause_temperature_c = 80
            resume_temperature_c = 72

            [power]
            prevent_automatic_sleep = false

            [search]
            mode = "historical_replay"
            minimum_hex_digits = 2
            stop_on_first_verified_discovery = true

            [storage]
            database_path = "data/search.sqlite"

            [logging]
            level = "info"
        "#;
        let config: Config = toml::from_str(text).unwrap();
        assert_eq!(config, Config::default());
        config.validate().unwrap();
    }

    #[test]
    fn unknown_and_invalid_settings_are_rejected() {
        assert!(toml::from_str::<Config>("[compute]\nbackend = \"opencl\"").is_err());
        assert!(toml::from_str::<Config>("[compute]\nturbo = true").is_err());
        let mut config = Config::default();
        config.search.maximum_hex_digits = Some(1);
        assert!(config.validate().is_err());
    }
}
