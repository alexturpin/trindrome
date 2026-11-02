//! Executes one batch of a search space on the chosen backend.
//!
//! Whatever the backend, a batch yields CPU-verified matches only: the CUDA path
//! rebuilds every GPU survivor from its index and checks it exactly, redoes the
//! batch on the CPU if the survivor buffer overflowed, and periodically audits a
//! slice of the batch against the Rust reference filter before it is committed.

use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail, ensure};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tracing::warn;
use trindrome_core::filter::{DecimalFilter, PRODUCTION_WINDOW};
use trindrome_core::palindrome::is_decimal_palindrome;
use trindrome_core::{SearchSpace, Verification};
use trindrome_cpu::{Match, search_range};
use trindrome_cuda::{CudaBackend, CudaError, Pending, Prepared};

use crate::configuration::{Backend, Compute, KernelChoice};

pub const CPU_BACKEND: &str = trindrome_cpu::IMPLEMENTATION;
pub const CUDA_BACKEND: &str = trindrome_cuda::IMPLEMENTATION;
/// Coverage records which kernel family searched it.
pub const CUDA_SPECIALIZED_BACKEND: &str = concat!("cuda-specialized/", env!("CARGO_PKG_VERSION"));

/// A CPU/GPU checksum comparison over `[start, start + count)`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Audit {
    pub start: u64,
    pub count: u64,
    pub gpu_checksum: u64,
    pub cpu_checksum: u64,
}

impl Audit {
    pub fn matched(&self) -> bool {
        self.gpu_checksum == self.cpu_checksum
    }
}

pub struct BatchOutput {
    pub candidates: u64,
    pub matches: Vec<Match>,
    pub elapsed: Duration,
    pub backend: &'static str,
    pub audits: Vec<Audit>,
}

pub fn cpu_hardware() -> String {
    let cpu = std::env::var("PROCESSOR_IDENTIFIER").unwrap_or_else(|_| std::env::consts::ARCH.to_string());
    format!("cpu:{cpu}")
}

/// Whether an error is a transient GPU/driver failure worth retrying after a
/// back-off, as opposed to a correctness failure (audit mismatch, invariant
/// violation) or a database error, which must stop the worker.
pub fn is_retryable(error: &anyhow::Error) -> bool {
    matches!(
        error.downcast_ref::<CudaError>(),
        Some(CudaError::Driver(_) | CudaError::Unavailable(_))
    )
}

/// The engines a worker runs: one per CPU thread with `cpu`, or the GPU.
/// `auto` and `cuda` both require the GPU and never fall back to the CPU silently.
pub fn engines_for(compute: &Compute, cpu: bool, threads: Option<usize>) -> Result<Vec<Engine>> {
    if cpu || compute.backend == Backend::Cpu {
        let threads = threads.unwrap_or(compute.effective_cpu_threads()).max(1);
        return Ok((0..threads).map(|_| Engine::cpu()).collect());
    }
    match Engine::cuda(compute) {
        Ok(engine) => Ok(vec![engine]),
        Err(e) => bail!(
            "the CUDA backend is unavailable ({e}). compute.backend = {:?} never falls back to the CPU \
             silently; use the CPU backend explicitly instead (`run --cpu` or compute.backend = \"cpu\").",
            compute.backend
        ),
    }
}

pub enum Engine {
    Cpu { hardware: String },
    Cuda(Box<CudaEngine>),
}

impl Engine {
    pub fn cpu() -> Self {
        Self::Cpu {
            hardware: cpu_hardware(),
        }
    }

    /// Opens the GPU now, so an unusable device is reported before starting.
    pub fn cuda(compute: &Compute) -> Result<Self, CudaError> {
        let device = compute.gpu_device as usize;
        let backend = CudaBackend::new(device)?;
        Ok(Self::Cuda(Box::new(CudaEngine {
            device,
            name: backend.device_name().to_string(),
            pci_bus_id: backend.pci_bus_id().to_string(),
            backend: Some(backend),
            specialized: compute.kernel == KernelChoice::Specialized,
            cached: None,
            inflight: None,
            last_collected: None,
            batches: 0,
            rng: 0x9E37_79B9_7F4A_7C15 ^ crate::persistence::now_ms() as u64,
        })))
    }

    /// End of the batch already running ahead for `space` from `start`, if any.
    /// The scheduler searches exactly that range next, so the prefetch is used.
    pub fn prefetched_end(&self, space: SearchSpace, start: &BigUint) -> Option<BigUint> {
        match self {
            Self::Cuda(e) => match &e.inflight {
                Some((s, p)) if *s == space && BigUint::from(p.start()) == *start => {
                    Some(BigUint::from(p.start() + p.count()))
                }
                _ => None,
            },
            Self::Cpu { .. } => None,
        }
    }

    pub fn hardware(&self) -> String {
        match self {
            Self::Cpu { hardware } => hardware.clone(),
            Self::Cuda(e) => format!("gpu:{}", e.name),
        }
    }

    /// The GPU's PCI address, for matching NVML telemetry; `None` for the CPU.
    pub fn pci_bus_id(&self) -> Option<&str> {
        match self {
            Self::Cpu { .. } => None,
            Self::Cuda(e) => Some(&e.pci_bus_id),
        }
    }

    pub fn initial_batch(&self) -> u64 {
        match self {
            Self::Cpu { .. } => 1 << 12,
            Self::Cuda(_) => 1 << 24,
        }
    }

    pub fn max_batch(&self) -> u64 {
        match self {
            Self::Cpu { .. } => 1 << 40,
            Self::Cuda(_) => 1 << 32,
        }
    }

    /// Frees the CUDA context (and its VRAM) while paused; reopened on demand.
    pub fn release_resources(&mut self) {
        if let Self::Cuda(e) = self {
            // Any prefetched batch is simply discarded: it was never recorded.
            e.inflight = None;
            e.last_collected = None;
            if e.backend.take().is_some() {
                tracing::debug!("released the CUDA context");
            }
        }
    }

    /// Searches `[start, end)`. With `next_end`, the GPU engine launches
    /// `[end, next_end)` before collecting this batch, so the host's work on
    /// this batch overlaps the next kernel.
    pub fn run_batch(
        &mut self,
        space: SearchSpace,
        start: &BigUint,
        end: &BigUint,
        compute: &Compute,
        next_end: Option<&BigUint>,
    ) -> Result<BatchOutput> {
        match self {
            Self::Cpu { .. } => cpu_batch(space, start, end),
            Self::Cuda(engine) => engine.run_batch(space, start, end, compute, next_end),
        }
    }
}

fn cpu_batch(space: SearchSpace, start: &BigUint, end: &BigUint) -> Result<BatchOutput> {
    let r = search_range(space, start, end)?;
    Ok(BatchOutput {
        candidates: r.candidates_checked,
        matches: r.matches,
        elapsed: r.elapsed,
        backend: CPU_BACKEND,
        audits: Vec::new(),
    })
}

/// The smallest search spaces have too few decimal digits for the GPU filter;
/// together they hold about four million candidates, which the CPU reference
/// finishes in moments, so handling them inline is not a silent CPU fallback.
const CPU_INLINE_MAX_HEX_DIGITS: u32 = 15;

pub struct CudaEngine {
    device: usize,
    name: String,
    pci_bus_id: String,
    backend: Option<CudaBackend>,
    specialized: bool,
    cached: Option<(Prepared, DecimalFilter)>,
    /// The next batch, already running on the GPU.
    inflight: Option<(SearchSpace, Pending)>,
    last_collected: Option<Instant>,
    batches: u64,
    rng: u64,
}

impl CudaEngine {
    fn prepared(&mut self, space: SearchSpace) -> Result<bool> {
        if self.cached.as_ref().is_none_or(|(p, _)| p.space != space) {
            let prepared = if self.specialized {
                Prepared::specialized(space, PRODUCTION_WINDOW)
            } else {
                Prepared::new(space, PRODUCTION_WINDOW)
            };
            self.cached = match prepared {
                Ok(p) => Some((
                    p,
                    DecimalFilter::new(space, PRODUCTION_WINDOW).expect("Prepared checked this"),
                )),
                Err(CudaError::Unsupported(_)) if space.hex_digits() <= CPU_INLINE_MAX_HEX_DIGITS => None,
                Err(e) => return Err(e).context(format!("{space:?} cannot run on the GPU")),
            };
        }
        Ok(self.cached.is_some())
    }

    fn backend(&mut self) -> Result<&mut CudaBackend, CudaError> {
        if self.backend.is_none() {
            self.backend = Some(CudaBackend::new(self.device)?);
            tracing::debug!("opened the CUDA context");
        }
        Ok(self.backend.as_mut().unwrap())
    }

    fn next_random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn run_batch(
        &mut self,
        space: SearchSpace,
        start: &BigUint,
        end: &BigUint,
        compute: &Compute,
        next_end: Option<&BigUint>,
    ) -> Result<BatchOutput> {
        if !self.prepared(space)? {
            self.inflight = None;
            return cpu_batch(space, start, end);
        }
        let start64 = start
            .to_u64()
            .ok_or_else(|| anyhow!("index {start} exceeds the GPU's 64-bit range"))?;
        let count = (end - start).to_u64().ok_or_else(|| anyhow!("batch too large"))?;
        let prepared = self.cached.as_ref().unwrap().0.clone();

        // Use the prefetched batch if it is exactly this one; otherwise drop it.
        let current = match self.inflight.take() {
            Some((s, p)) if s == space && p.start() == start64 && p.count() == count => p,
            Some((_, stale)) => {
                let _ = self.backend()?.collect(stale);
                self.backend()?.launch(&prepared, start64, count)?
            }
            None => self.backend()?.launch(&prepared, start64, count)?,
        };
        if let Some(next_end) = next_end.filter(|n| *n > end)
            && let (Some(end64), Some(next_count)) = (end.to_u64(), (next_end - end).to_u64())
        {
            let next = self.backend()?.launch(&prepared, end64, next_count)?;
            self.inflight = Some((space, next));
        }
        let result = self.backend()?.collect(current)?;
        // A prefetched kernel could only start once its predecessor finished, so
        // its GPU time runs from the later of its launch and the previous collect.
        let gpu_time = match self.last_collected {
            Some(previous) => result.elapsed.min(previous.elapsed()),
            None => result.elapsed,
        };
        self.last_collected = Some(Instant::now());
        ensure!(
            result.processed == count,
            "GPU processed {} of {count} candidates in [{start}, {end}); refusing to record coverage",
            result.processed
        );

        let matches = if result.overflowed {
            warn!(%start, %end, "GPU survivor buffer overflowed; searching the batch exactly on the CPU");
            search_range(space, start, end)?.matches
        } else {
            let mut matches = Vec::new();
            for &index in &result.survivors {
                let value = space.candidate(&index.into())?;
                if is_decimal_palindrome(&value) {
                    matches.push(Match {
                        index: index.into(),
                        verification: Verification::of(&value),
                    });
                }
            }
            matches
        };

        self.batches += 1;
        let mut audits = Vec::new();
        let every = compute.audit_every_batches as u64;
        if every > 0 && self.batches.is_multiple_of(every) {
            let audit = self.audit(start64, count, compute.audit_candidates)?;
            if !audit.matched() {
                bail!(
                    "GPU audit FAILED on [{}, {}) of {space:?}: GPU checksum {:#x}, CPU {:#x}; batch not recorded",
                    audit.start,
                    audit.start + audit.count,
                    audit.gpu_checksum,
                    audit.cpu_checksum
                );
            }
            audits.push(audit);
        }

        Ok(BatchOutput {
            candidates: count,
            matches,
            elapsed: gpu_time,
            backend: if prepared.specialized {
                CUDA_SPECIALIZED_BACKEND
            } else {
                CUDA_BACKEND
            },
            audits,
        })
    }

    /// Recomputes a random slice of the batch on the GPU and with the Rust filter.
    fn audit(&mut self, start: u64, count: u64, candidates: u64) -> Result<Audit> {
        let len = candidates.min(count).max(1);
        let offset = self.next_random() % (count - len + 1);
        let (prepared, filter) = self.cached.clone().unwrap();
        let gpu = self.backend()?.search(&prepared, start + offset, len)?;
        ensure!(
            gpu.processed == len,
            "GPU processed {} of {len} audit candidates",
            gpu.processed
        );
        let (cpu_checksum, _) = filter.scan(start + offset, len)?;
        Ok(Audit {
            start: start + offset,
            count: len,
            gpu_checksum: gpu.checksum,
            cpu_checksum,
        })
    }
}
