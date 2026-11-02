//! Collects a [`Readings`] snapshot from the real machine.

use std::time::{Duration, Instant};

use crate::gpu_monitor::GpuMonitor;
use crate::policy::{GpuReading, Readings};
use crate::{idle, power};

/// The process list is the most expensive reading, so it is refreshed less often.
const PROCESS_SCAN_INTERVAL: Duration = Duration::from_secs(2);

pub struct SystemSensors {
    exclusions: Vec<String>,
    gpu: Option<Result<GpuMonitor, String>>,
    last_scan: Option<(Instant, Result<Vec<String>, String>)>,
}

impl SystemSensors {
    /// `pci_bus_id` names the GPU being searched on, if any.
    pub fn new(exclusions: Vec<String>, pci_bus_id: Option<&str>) -> Self {
        Self {
            exclusions,
            gpu: pci_bus_id.map(GpuMonitor::new),
            last_scan: None,
        }
    }

    pub fn set_exclusions(&mut self, exclusions: Vec<String>) {
        if exclusions != self.exclusions {
            self.exclusions = exclusions;
            self.last_scan = None;
        }
    }

    pub fn gpu_error(&self) -> Option<&str> {
        match &self.gpu {
            Some(Err(e)) => Some(e),
            _ => None,
        }
    }

    pub fn read(&mut self) -> Readings {
        let now = Instant::now();
        let stale = self
            .last_scan
            .as_ref()
            .is_none_or(|(at, _)| now.duration_since(*at) >= PROCESS_SCAN_INTERVAL);
        if stale {
            self.last_scan = Some((now, idle::running_exclusions(&self.exclusions)));
        }
        Readings {
            at: now,
            user_idle: idle::user_idle(),
            excluded_running: self.last_scan.as_ref().unwrap().1.clone(),
            on_ac_power: power::on_ac_power(),
            gpu: self.read_gpu(),
        }
    }

    fn read_gpu(&self) -> Option<GpuReading> {
        match &self.gpu {
            Some(Ok(monitor)) => monitor.read().ok(),
            _ => None,
        }
    }
}
