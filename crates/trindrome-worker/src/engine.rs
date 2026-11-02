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
use trindrome_cuda::{CudaBackend, CudaError, Prepared};

use crate::configuration::Compute;

pub const CPU_BACKEND: &str = trindrome_cpu::IMPLEMENTATION;
pub const CUDA_BACKEND: &str = trindrome_cuda::IMPLEMENTATION;

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

    pub fn cuda(compute: &Compute) -> Result<Self, CudaError> {
        Ok(Self::Cuda(Box::new(CudaEngine {
            backend: CudaBackend::new(compute.gpu_device as usize)?,
            cached: None,
            audit_every: compute.audit_every_batches,
            audit_candidates: compute.audit_candidates,
            batches: 0,
            rng: 0x9E37_79B9_7F4A_7C15 ^ crate::persistence::now_ms() as u64,
        })))
    }

    pub fn hardware(&self) -> String {
        match self {
            Self::Cpu { hardware } => hardware.clone(),
            Self::Cuda(e) => format!("gpu:{}", e.backend.device_name()),
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

    pub fn run_batch(&mut self, space: SearchSpace, start: &BigUint, end: &BigUint) -> Result<BatchOutput> {
        match self {
            Self::Cpu { .. } => cpu_batch(space, start, end),
            Self::Cuda(engine) => engine.run_batch(space, start, end),
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
    backend: CudaBackend,
    cached: Option<(Prepared, DecimalFilter)>,
    audit_every: u32,
    audit_candidates: u64,
    batches: u64,
    rng: u64,
}

impl CudaEngine {
    fn prepared(&mut self, space: SearchSpace) -> Result<Option<&(Prepared, DecimalFilter)>> {
        if self.cached.as_ref().is_none_or(|(p, _)| p.space != space) {
            self.cached = match Prepared::new(space, PRODUCTION_WINDOW) {
                Ok(p) => Some((
                    p,
                    DecimalFilter::new(space, PRODUCTION_WINDOW).expect("Prepared checked this"),
                )),
                Err(CudaError::Unsupported(_)) if space.hex_digits() <= CPU_INLINE_MAX_HEX_DIGITS => None,
                Err(e) => return Err(e).context(format!("{space:?} cannot run on the GPU")),
            };
        }
        Ok(self.cached.as_ref())
    }

    fn next_random(&mut self) -> u64 {
        self.rng ^= self.rng << 13;
        self.rng ^= self.rng >> 7;
        self.rng ^= self.rng << 17;
        self.rng
    }

    fn run_batch(&mut self, space: SearchSpace, start: &BigUint, end: &BigUint) -> Result<BatchOutput> {
        if self.prepared(space)?.is_none() {
            return cpu_batch(space, start, end);
        }
        let began = Instant::now();
        let start64 = start
            .to_u64()
            .ok_or_else(|| anyhow!("index {start} exceeds the GPU's 64-bit range"))?;
        let count = (end - start).to_u64().ok_or_else(|| anyhow!("batch too large"))?;

        let (prepared, _) = self.cached.as_ref().unwrap();
        let result = self.backend.search(prepared, start64, count)?;
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
        if self.audit_every > 0 && self.batches.is_multiple_of(self.audit_every as u64) {
            let audit = self.audit(start64, count)?;
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
            elapsed: began.elapsed(),
            backend: CUDA_BACKEND,
            audits,
        })
    }

    /// Recomputes a random slice of the batch on the GPU and with the Rust filter.
    fn audit(&mut self, start: u64, count: u64) -> Result<Audit> {
        let len = self.audit_candidates.min(count).max(1);
        let offset = self.next_random() % (count - len + 1);
        let (prepared, filter) = self.cached.as_ref().unwrap();
        let gpu = self.backend.search(prepared, start + offset, len)?;
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
