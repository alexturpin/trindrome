//! NVIDIA GPU telemetry through NVML, which ships with the driver.

use nvml_wrapper::Nvml;
use nvml_wrapper::enum_wrappers::device::TemperatureSensor;

use crate::policy::GpuReading;

pub struct GpuMonitor {
    nvml: Nvml,
    pci_bus_id: String,
}

impl GpuMonitor {
    /// Telemetry for the GPU at `pci_bus_id` (as reported by the CUDA backend).
    pub fn new(pci_bus_id: &str) -> Result<Self, String> {
        let nvml = Nvml::init().map_err(|e| format!("NVML unavailable: {e}"))?;
        nvml.device_by_pci_bus_id(pci_bus_id)
            .map_err(|e| format!("NVML cannot find GPU {pci_bus_id}: {e}"))?;
        Ok(Self {
            nvml,
            pci_bus_id: pci_bus_id.to_string(),
        })
    }

    /// Utilization and temperature are required; power draw is reported when
    /// the GPU and driver support it.
    pub fn read(&self) -> Result<GpuReading, String> {
        let device = self
            .nvml
            .device_by_pci_bus_id(self.pci_bus_id.as_str())
            .map_err(|e| e.to_string())?;
        let utilization = device.utilization_rates().map_err(|e| format!("utilization: {e}"))?;
        let temperature = device
            .temperature(TemperatureSensor::Gpu)
            .map_err(|e| format!("temperature: {e}"))?;
        Ok(GpuReading {
            utilization_percent: utilization.gpu,
            temperature_c: temperature,
            power_watts: device.power_usage().ok().map(|mw| mw as f32 / 1000.0),
        })
    }
}
