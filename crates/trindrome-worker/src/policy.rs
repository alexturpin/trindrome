//! Resource policy: decides from sensor readings whether the worker may compute.
//!
//! This is pure logic over a [`Readings`] snapshot, so every rule is unit-tested
//! without Windows. The search never depends on it; it only drives pause/resume.
//!
//! Start requires every condition; while running, total GPU utilization is
//! ignored (our own kernels raise it), but user input, excluded applications,
//! power, temperature and monitoring failures still pause. Hysteresis comes from
//! the idle requirement (input resets it) and the thermal resume band.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::configuration::Config;

#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GpuReading {
    pub utilization_percent: u32,
    pub temperature_c: u32,
    pub power_watts: Option<f32>,
}

/// One snapshot of everything the policy looks at. `None` means unavailable.
#[derive(Clone, Debug)]
pub struct Readings {
    pub at: Instant,
    pub user_idle: Option<Duration>,
    /// Excluded applications currently running, or why the scan failed.
    pub excluded_running: Result<Vec<String>, String>,
    pub on_ac_power: Option<bool>,
    pub gpu: Option<GpuReading>,
}

#[derive(Clone, Debug, PartialEq)]
pub struct PolicySettings {
    pub idle_enabled: bool,
    pub idle_required: Duration,
    pub gpu_start_threshold_percent: u32,
    pub gpu_sample_window: Duration,
    pub require_ac_power: bool,
    pub thermal_enabled: bool,
    pub pause_temperature_c: u32,
    pub resume_temperature_c: u32,
    /// The search uses the GPU, so GPU utilization and temperature matter.
    pub uses_gpu: bool,
    /// `run --now`: skip the inactivity requirements, keep every safety limit.
    pub immediate: bool,
}

impl PolicySettings {
    pub fn from_config(config: &Config, uses_gpu: bool, immediate: bool) -> Self {
        Self {
            idle_enabled: config.idle.enabled,
            idle_required: Duration::from_secs(config.idle.idle_minutes as u64 * 60),
            gpu_start_threshold_percent: config.idle.gpu_start_threshold_percent,
            gpu_sample_window: Duration::from_secs(config.idle.gpu_idle_sample_seconds as u64),
            require_ac_power: config.idle.require_ac_power,
            thermal_enabled: config.thermal.enabled,
            pause_temperature_c: config.thermal.pause_temperature_c,
            resume_temperature_c: config.thermal.resume_temperature_c,
            uses_gpu,
            immediate,
        }
    }
}

/// Why the worker is or is not computing. Serialized as `{"code": ..., ...details}`.
#[derive(Clone, Debug, PartialEq, Serialize)]
#[serde(tag = "code", rename_all = "snake_case")]
pub enum Reason {
    Running,
    ManualPause,
    WaitingForIdle {
        idle_seconds: u64,
        required_seconds: u64,
    },
    UserActive {
        required_seconds: u64,
    },
    MeasuringGpu {
        sampled_seconds: u64,
        required_seconds: u64,
    },
    GpuBusy {
        utilization_percent: u32,
        threshold_percent: u32,
    },
    ExcludedApp {
        app: String,
    },
    OnBattery,
    Thermal {
        temperature_c: u32,
        resume_below_c: u32,
    },
    MonitoringUnavailable {
        what: String,
    },
    Error {
        message: String,
        retry_in_seconds: u64,
    },
}

impl Reason {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Running => "running",
            Self::ManualPause => "manual_pause",
            Self::WaitingForIdle { .. } => "waiting_for_idle",
            Self::UserActive { .. } => "user_active",
            Self::MeasuringGpu { .. } => "measuring_gpu",
            Self::GpuBusy { .. } => "gpu_busy",
            Self::ExcludedApp { .. } => "excluded_app",
            Self::OnBattery => "on_battery",
            Self::Thermal { .. } => "thermal",
            Self::MonitoringUnavailable { .. } => "monitoring_unavailable",
            Self::Error { .. } => "error",
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Self::Running => "running".into(),
            Self::ManualPause => "paused by request".into(),
            Self::WaitingForIdle {
                idle_seconds,
                required_seconds,
            } => format!(
                "waiting for the computer to be idle ({} of {})",
                mmss(*idle_seconds),
                mmss(*required_seconds)
            ),
            Self::UserActive { required_seconds } => {
                format!("keyboard/mouse in use; resumes after {} idle", mmss(*required_seconds))
            }
            Self::MeasuringGpu {
                sampled_seconds,
                required_seconds,
            } => {
                format!("measuring GPU utilization ({sampled_seconds}s of {required_seconds}s)")
            }
            Self::GpuBusy {
                utilization_percent,
                threshold_percent,
            } => {
                format!("GPU busy with something else ({utilization_percent}% ≥ {threshold_percent}%)")
            }
            Self::ExcludedApp { app } => format!("{app} is running"),
            Self::OnBattery => "on battery power".into(),
            Self::Thermal {
                temperature_c,
                resume_below_c,
            } => {
                format!("GPU at {temperature_c} °C; resumes below {resume_below_c} °C")
            }
            Self::MonitoringUnavailable { what } => format!("cannot monitor {what}; not starting blindly"),
            Self::Error {
                message,
                retry_in_seconds,
            } => format!("error, retrying in {retry_in_seconds}s: {message}"),
        }
    }

    /// JSON details, e.g. `{"code":"thermal","temperature_c":81,"resume_below_c":72}`.
    pub fn detail_json(&self) -> String {
        serde_json::to_string(self).expect("reasons serialize")
    }
}

fn mmss(seconds: u64) -> String {
    format!("{}:{:02}", seconds / 60, seconds % 60)
}

#[derive(Clone, Debug, PartialEq)]
pub struct Decision {
    pub run: bool,
    pub reason: Reason,
}

impl Decision {
    fn pause(reason: Reason) -> Self {
        Self { run: false, reason }
    }
}

pub struct Policy {
    settings: PolicySettings,
    running: bool,
    thermal_tripped: bool,
    /// GPU utilization observed while *not* running, for the start check.
    gpu_samples: VecDeque<(Instant, u32)>,
    since: Option<Instant>,
}

impl Policy {
    pub fn new(settings: PolicySettings) -> Self {
        Self {
            settings,
            running: false,
            thermal_tripped: false,
            gpu_samples: VecDeque::new(),
            since: None,
        }
    }

    pub fn settings(&self) -> &PolicySettings {
        &self.settings
    }

    pub fn update_settings(&mut self, settings: PolicySettings) {
        self.settings = settings;
    }

    pub fn evaluate(&mut self, r: &Readings) -> Decision {
        let decision = self.decide(r);
        if decision.run && !self.running {
            // Our own kernels will dominate utilization from now on.
            self.gpu_samples.clear();
            self.since = None;
        }
        self.running = decision.run;
        decision
    }

    fn decide(&mut self, r: &Readings) -> Decision {
        let s = self.settings.clone();
        let unavailable = |what: &str| Decision::pause(Reason::MonitoringUnavailable { what: what.into() });

        match &r.excluded_running {
            Ok(apps) if !apps.is_empty() => return Decision::pause(Reason::ExcludedApp { app: apps.join(", ") }),
            Ok(_) => {}
            Err(_) => return unavailable("running applications"),
        }

        if s.require_ac_power {
            match r.on_ac_power {
                Some(true) => {}
                Some(false) => return Decision::pause(Reason::OnBattery),
                None => return unavailable("power source"),
            }
        }

        if s.uses_gpu && s.thermal_enabled {
            let Some(gpu) = r.gpu else {
                return unavailable("GPU temperature");
            };
            if gpu.temperature_c >= s.pause_temperature_c {
                self.thermal_tripped = true;
            } else if gpu.temperature_c < s.resume_temperature_c {
                self.thermal_tripped = false;
            }
            if self.thermal_tripped {
                return Decision::pause(Reason::Thermal {
                    temperature_c: gpu.temperature_c,
                    resume_below_c: s.resume_temperature_c,
                });
            }
        }

        if s.immediate || !s.idle_enabled {
            return Decision {
                run: true,
                reason: Reason::Running,
            };
        }

        let Some(idle) = r.user_idle else {
            return unavailable("keyboard/mouse activity");
        };
        let required = s.idle_required.as_secs();
        if idle < s.idle_required {
            return Decision::pause(if self.running {
                Reason::UserActive {
                    required_seconds: required,
                }
            } else {
                Reason::WaitingForIdle {
                    idle_seconds: idle.as_secs(),
                    required_seconds: required,
                }
            });
        }

        if s.uses_gpu && !self.running {
            let Some(gpu) = r.gpu else {
                return unavailable("GPU utilization");
            };
            self.gpu_samples.push_back((r.at, gpu.utilization_percent));
            let since = *self.since.get_or_insert(r.at);
            while self
                .gpu_samples
                .front()
                .is_some_and(|(t, _)| r.at.duration_since(*t) > s.gpu_sample_window)
            {
                self.gpu_samples.pop_front();
            }
            let sampled = r.at.duration_since(since);
            if sampled < s.gpu_sample_window {
                return Decision::pause(Reason::MeasuringGpu {
                    sampled_seconds: sampled.as_secs(),
                    required_seconds: s.gpu_sample_window.as_secs(),
                });
            }
            let average =
                self.gpu_samples.iter().map(|(_, u)| *u as u64).sum::<u64>() / self.gpu_samples.len().max(1) as u64;
            if average >= s.gpu_start_threshold_percent as u64 {
                return Decision::pause(Reason::GpuBusy {
                    utilization_percent: average as u32,
                    threshold_percent: s.gpu_start_threshold_percent,
                });
            }
        }

        Decision {
            run: true,
            reason: Reason::Running,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> PolicySettings {
        PolicySettings {
            idle_enabled: true,
            idle_required: Duration::from_secs(300),
            gpu_start_threshold_percent: 10,
            gpu_sample_window: Duration::from_secs(30),
            require_ac_power: true,
            thermal_enabled: true,
            pause_temperature_c: 80,
            resume_temperature_c: 72,
            uses_gpu: true,
            immediate: false,
        }
    }

    struct Sim {
        t0: Instant,
        clock: Duration,
        idle: Option<Duration>,
        apps: Result<Vec<String>, String>,
        ac: Option<bool>,
        gpu: Option<GpuReading>,
    }

    impl Sim {
        fn new() -> Self {
            Self {
                t0: Instant::now(),
                clock: Duration::ZERO,
                idle: Some(Duration::from_secs(600)),
                apps: Ok(Vec::new()),
                ac: Some(true),
                gpu: Some(GpuReading {
                    utilization_percent: 2,
                    temperature_c: 50,
                    power_watts: None,
                }),
            }
        }

        /// Advances time; idle time grows with it unless input happens.
        fn tick(&mut self, policy: &mut Policy, seconds: u64) -> Decision {
            self.clock += Duration::from_secs(seconds);
            if let Some(idle) = &mut self.idle {
                *idle += Duration::from_secs(seconds);
            }
            policy.evaluate(&Readings {
                at: self.t0 + self.clock,
                user_idle: self.idle,
                excluded_running: self.apps.clone(),
                on_ac_power: self.ac,
                gpu: self.gpu,
            })
        }

        fn set_temp(&mut self, c: u32) {
            self.gpu.as_mut().unwrap().temperature_c = c;
        }
    }

    /// Runs the policy until it starts (after the GPU sampling window).
    fn started(sim: &mut Sim, policy: &mut Policy) {
        for _ in 0..40 {
            if sim.tick(policy, 1).run {
                return;
            }
        }
        panic!("never started");
    }

    #[test]
    fn starts_only_after_sampling_a_quiet_gpu_while_idle() {
        let mut sim = Sim::new();
        let mut p = Policy::new(settings());
        let first = sim.tick(&mut p, 1);
        assert!(matches!(first.reason, Reason::MeasuringGpu { .. }), "{first:?}");
        started(&mut sim, &mut p);
    }

    #[test]
    fn busy_gpu_blocks_start_but_not_a_running_search() {
        let mut sim = Sim::new();
        sim.gpu.as_mut().unwrap().utilization_percent = 60;
        let mut p = Policy::new(settings());
        for _ in 0..40 {
            assert!(!sim.tick(&mut p, 1).run);
        }
        assert!(matches!(
            sim.tick(&mut p, 1).reason,
            Reason::GpuBusy {
                utilization_percent: 60,
                ..
            }
        ));

        let mut sim = Sim::new();
        let mut p = Policy::new(settings());
        started(&mut sim, &mut p);
        sim.gpu.as_mut().unwrap().utilization_percent = 100; // our own kernels
        for _ in 0..100 {
            assert!(sim.tick(&mut p, 1).run);
        }
    }

    #[test]
    fn input_pauses_and_resuming_needs_the_full_idle_period_again() {
        let mut sim = Sim::new();
        let mut p = Policy::new(settings());
        started(&mut sim, &mut p);
        sim.idle = Some(Duration::ZERO);
        let d = sim.tick(&mut p, 1);
        assert_eq!(
            d,
            Decision {
                run: false,
                reason: Reason::UserActive { required_seconds: 300 }
            }
        );
        let d = sim.tick(&mut p, 100);
        assert!(
            matches!(
                d.reason,
                Reason::WaitingForIdle {
                    idle_seconds: 101,
                    required_seconds: 300
                }
            ),
            "{d:?}"
        );
        sim.tick(&mut p, 199);
        // Idle requirement met; the GPU must now be re-sampled while paused.
        assert!(matches!(sim.tick(&mut p, 1).reason, Reason::MeasuringGpu { .. }));
        started(&mut sim, &mut p);
    }

    #[test]
    fn excluded_apps_pause_even_in_immediate_mode() {
        let mut sim = Sim::new();
        let mut p = Policy::new(PolicySettings {
            immediate: true,
            ..settings()
        });
        assert!(sim.tick(&mut p, 1).run, "immediate mode skips idle and GPU sampling");
        sim.apps = Ok(vec!["FlightSimulator2024.exe".into()]);
        assert_eq!(
            sim.tick(&mut p, 1).reason,
            Reason::ExcludedApp {
                app: "FlightSimulator2024.exe".into()
            }
        );
        sim.apps = Ok(Vec::new());
        assert!(sim.tick(&mut p, 1).run);
    }

    #[test]
    fn thermal_limit_uses_a_resume_band() {
        let mut sim = Sim::new();
        let mut p = Policy::new(PolicySettings {
            immediate: true,
            ..settings()
        });
        assert!(sim.tick(&mut p, 1).run);
        sim.set_temp(79);
        assert!(sim.tick(&mut p, 1).run);
        sim.set_temp(80);
        assert_eq!(
            sim.tick(&mut p, 1).reason,
            Reason::Thermal {
                temperature_c: 80,
                resume_below_c: 72
            }
        );
        for c in [79, 75, 72] {
            sim.set_temp(c);
            assert!(!sim.tick(&mut p, 1).run, "still cooling at {c}");
        }
        sim.set_temp(71);
        assert!(sim.tick(&mut p, 1).run);
    }

    #[test]
    fn power_and_monitoring_failures_never_start_blindly() {
        let mut sim = Sim::new();
        let mut p = Policy::new(PolicySettings {
            immediate: true,
            ..settings()
        });
        sim.ac = Some(false);
        assert_eq!(sim.tick(&mut p, 1).reason, Reason::OnBattery);
        sim.ac = None;
        assert!(matches!(
            sim.tick(&mut p, 1).reason,
            Reason::MonitoringUnavailable { .. }
        ));
        sim.ac = Some(true);
        sim.gpu = None;
        assert!(matches!(
            sim.tick(&mut p, 1).reason,
            Reason::MonitoringUnavailable { .. }
        ));
        sim.gpu = Some(GpuReading {
            utilization_percent: 0,
            temperature_c: 40,
            power_watts: None,
        });
        sim.apps = Err("snapshot failed".into());
        assert!(matches!(
            sim.tick(&mut p, 1).reason,
            Reason::MonitoringUnavailable { .. }
        ));

        let mut sim = Sim::new();
        let mut p = Policy::new(settings());
        sim.idle = None;
        assert!(matches!(
            sim.tick(&mut p, 1).reason,
            Reason::MonitoringUnavailable { .. }
        ));
    }

    #[test]
    fn cpu_searches_ignore_gpu_telemetry_and_optional_checks_can_be_disabled() {
        let mut sim = Sim::new();
        sim.gpu = None;
        sim.ac = None;
        let mut p = Policy::new(PolicySettings {
            uses_gpu: false,
            require_ac_power: false,
            ..settings()
        });
        assert!(sim.tick(&mut p, 1).run);
    }

    #[test]
    fn reasons_serialize_with_a_stable_code() {
        let r = Reason::Thermal {
            temperature_c: 81,
            resume_below_c: 72,
        };
        assert_eq!(
            r.detail_json(),
            r#"{"code":"thermal","temperature_c":81,"resume_below_c":72}"#
        );
        assert_eq!(r.code(), "thermal");
        assert_eq!(
            Reason::WaitingForIdle {
                idle_seconds: 70,
                required_seconds: 300
            }
            .describe(),
            "waiting for the computer to be idle (1:10 of 5:00)"
        );
    }
}
