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
    /// GPU kernel family: "generic" (one kernel per integer width) or
    /// "specialized" (one per hex length, candidates built in registers).
    pub kernel: KernelChoice,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KernelChoice {
    Generic,
    Specialized,
}

impl Default for Compute {
    fn default() -> Self {
        Self {
            backend: Backend::Auto,
            gpu_device: 0,
            batch_target_ms: 200,
            checkpoint_interval_seconds: 30,
            cpu_threads: 0,
            // ~15 s of GPU work: long enough for many pipelined batches per claim.
            // Progress inside a chunk is still checkpointed by its cursor.
            chunk_candidates: 1 << 36,
            audit_every_batches: 25,
            audit_candidates: 1 << 15,
            kernel: KernelChoice::Generic,
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
            // The replay takes seconds on a GPU; an always-on worker should go on
            // to the frontier rather than stop. (The spec suggested replay only.)
            mode: SearchMode::Both,
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
            database_path: PathBuf::from("search.sqlite"),
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

/// Per-user home for configuration, database and logs, shared by the CLI, the
/// agent and any future GUI: `%LOCALAPPDATA%\Trindrome` on Windows. Overridden
/// by `TRINDROME_HOME`.
pub fn data_dir() -> PathBuf {
    if let Some(home) = std::env::var_os("TRINDROME_HOME") {
        return PathBuf::from(home);
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return Path::new(&local).join("Trindrome");
    }
    if let Some(home) = std::env::var_os("HOME") {
        return Path::new(&home).join(".local/share/trindrome");
    }
    PathBuf::from(".")
}

/// Which file a configuration comes from: `--config`, then `TRINDROME_CONFIG`,
/// then `trindrome.toml` in [`data_dir`]. Only the last may be absent.
pub fn config_path(explicit: Option<&Path>) -> (PathBuf, bool) {
    match explicit {
        Some(p) => (p.to_path_buf(), true),
        None => match std::env::var_os("TRINDROME_CONFIG") {
            Some(p) => (PathBuf::from(p), true),
            None => (data_dir().join(DEFAULT_CONFIG_FILE), false),
        },
    }
}

impl Config {
    /// Loads the configuration chosen by [`config_path`], or built-in defaults if
    /// the default file does not exist. A relative database path is resolved
    /// against the configuration file's directory.
    pub fn load(explicit: Option<&Path>) -> Result<Self> {
        let (path, required) = config_path(explicit);
        Self::load_from(&path, required)
    }

    pub fn load_from(path: &Path, required: bool) -> Result<Self> {
        let path = path.to_path_buf();
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

    /// Settings that parse but cannot be enforced on this platform.
    pub fn unenforced_settings(&self) -> Vec<&'static str> {
        if cfg!(windows) {
            Vec::new()
        } else {
            vec!["[idle] and [power] (implemented for Windows only)"]
        }
    }

    /// Settings whose change only takes effect after a restart.
    pub fn restart_required_changes(&self, newer: &Config) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.compute.backend != newer.compute.backend {
            changed.push("compute.backend");
        }
        if self.compute.gpu_device != newer.compute.gpu_device {
            changed.push("compute.gpu_device");
        }
        if self.compute.kernel != newer.compute.kernel {
            changed.push("compute.kernel");
        }
        if self.compute.cpu_threads != newer.compute.cpu_threads {
            changed.push("compute.cpu_threads");
        }
        if self.storage != newer.storage {
            changed.push("storage.database_path");
        }
        if self.logging != newer.logging {
            changed.push("logging.level");
        }
        changed
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
            mode = "both"
            minimum_hex_digits = 2
            stop_on_first_verified_discovery = true

            [storage]
            database_path = "search.sqlite"

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
