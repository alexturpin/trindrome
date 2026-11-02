//! CUDA search backend.
//!
//! The GPU builds candidates and applies the exact decimal window filter from
//! `trindrome_core::filter`. It never accepts anything: survivors come back as
//! indices for exact verification on the CPU. See `docs/cuda-design.md`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use cudarc::driver::sys::CUdevice_attribute;
use cudarc::driver::{
    CudaContext, CudaFunction, CudaSlice, CudaStream, DeviceRepr, DriverError, LaunchConfig, PushKernelArg,
};
use num_bigint::BigUint;
use trindrome_core::filter::DecimalFilter;
use trindrome_core::{CoreError, SearchSpace};

pub const IMPLEMENTATION: &str = concat!("cuda/", env!("CARGO_PKG_VERSION"));

/// Kernel widths compiled into the fatbin, in 32-bit limbs.
pub const LIMB_WIDTHS: [usize; 4] = [4, 8, 12, 16];
const MAX_LIMBS: usize = 16;
pub const DEFAULT_SURVIVOR_CAPACITY: u32 = 4096;
const THREADS_PER_BLOCK: u32 = 256;
const BLOCKS_PER_SM: u32 = 32;

#[cfg(trindrome_kernels)]
static FATBIN: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/search.fatbin"));

#[derive(Debug, thiserror::Error)]
pub enum CudaError {
    #[error("this build has no CUDA kernels (no CUDA toolkit was found when it was compiled)")]
    NotBuilt,
    #[error("CUDA is unavailable: {0}")]
    Unavailable(String),
    #[error("CUDA driver error: {0}")]
    Driver(#[from] DriverError),
    #[error("not supported on the GPU: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Core(#[from] CoreError),
}

/// Must match `struct Params` in `kernels/search.cu`.
#[repr(C)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Params {
    hex_digits: u32,
    leading: u32,
    free_digits: u32,
    radix_bits: u32,
    alphabet: [u32; 4],
    window: u32,
    ten_window: u32,
    digits_low: u32,
    has_threshold: u32,
    residues: [u32; MAX_LIMBS],
    threshold: [u32; MAX_LIMBS],
    p_low: [u32; MAX_LIMBS],
    p_high: [u32; MAX_LIMBS],
}

// SAFETY: plain-old-data with the same `repr(C)` layout as the kernel's struct.
unsafe impl DeviceRepr for Params {}

/// Encodes a constant for a kernel that reads only `width` limbs, refusing
/// anything that would not fit rather than truncating it.
fn limbs(v: &BigUint, width: usize) -> Result<[u32; MAX_LIMBS], CudaError> {
    let digits = v.to_u32_digits();
    if digits.len() > width {
        return Err(CudaError::Unsupported(format!(
            "{}-bit constant in a {}-bit kernel",
            v.bits(),
            32 * width
        )));
    }
    let mut out = [0; MAX_LIMBS];
    out[..digits.len()].copy_from_slice(&digits);
    Ok(out)
}

/// Host-side precomputation for one (search space, window) pair.
#[derive(Clone, Debug)]
pub struct Prepared {
    pub space: SearchSpace,
    pub window: u32,
    pub limbs: usize,
    params: Params,
}

impl Prepared {
    pub fn new(space: SearchSpace, window: u32) -> Result<Self, CudaError> {
        let needed = (4 * space.hex_digits() as usize).div_ceil(32);
        let limbs_n = *LIMB_WIDTHS
            .iter()
            .find(|&&w| w >= needed)
            .ok_or_else(|| CudaError::Unsupported(format!("{} hex digits exceeds 512 bits", space.hex_digits())))?;
        let filter = DecimalFilter::new(space, window).ok_or_else(|| {
            CudaError::Unsupported(format!(
                "{space:?} has candidates with fewer than {} decimal digits",
                2 * window
            ))
        })?;

        let mut alphabet = [0; 4];
        for (slot, &d) in alphabet.iter_mut().zip(space.alphabet()) {
            *slot = d as u32;
        }
        let mut residues = [0; MAX_LIMBS];
        let mut r = 1u64;
        for slot in &mut residues {
            *slot = r as u32;
            r = ((r as u128) << 32).rem_euclid(1_000_000_000) as u64;
        }

        Ok(Self {
            space,
            window,
            limbs: limbs_n,
            params: Params {
                hex_digits: space.hex_digits(),
                leading: space.leading() as u32,
                free_digits: space.free_digits(),
                radix_bits: space.radix().trailing_zeros(),
                alphabet,
                window,
                ten_window: 10u32.pow(window),
                digits_low: filter.digits_low,
                has_threshold: filter.threshold.is_some() as u32,
                residues,
                // Without a power of ten inside the space, every candidate uses
                // p_low; p_high may then not even fit the kernel width.
                threshold: filter
                    .threshold
                    .as_ref()
                    .map(|t| limbs(t, limbs_n))
                    .transpose()?
                    .unwrap_or_default(),
                p_low: limbs(&filter.p_low, limbs_n)?,
                p_high: match filter.threshold {
                    Some(_) => limbs(&filter.p_high, limbs_n)?,
                    None => [0; MAX_LIMBS],
                },
            },
        })
    }
}

/// What one kernel launch reports.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchResult {
    pub start: u64,
    pub count: u64,
    /// Candidates the kernel says it processed; must equal `count`.
    pub processed: u64,
    pub checksum: u64,
    /// Absolute indices of filter survivors, sorted. Incomplete if `overflowed`.
    pub survivors: Vec<u64>,
    /// More survivors than the buffer holds; the batch must be redone exactly.
    pub overflowed: bool,
    pub elapsed: Duration,
}

struct Kernels {
    search: Vec<(usize, CudaFunction)>,
    construct: Vec<(usize, CudaFunction)>,
}

impl Kernels {
    fn get(list: &[(usize, CudaFunction)], limbs: usize) -> &CudaFunction {
        &list
            .iter()
            .find(|(w, _)| *w == limbs)
            .expect("width validated by Prepared")
            .1
    }
}

/// One set of device buffers on its own stream, so one batch can be collected
/// while the next is already running.
struct Slot {
    stream: Arc<CudaStream>,
    survivors: CudaSlice<u32>,
    counters: CudaSlice<u64>,
    busy: bool,
}

/// A batch that has been launched but not yet collected.
#[must_use = "a launched batch must be collected (or discarded by dropping the backend)"]
#[derive(Debug)]
pub struct Pending {
    slot: usize,
    start: u64,
    count: u64,
    launched: Instant,
}

impl Pending {
    pub fn start(&self) -> u64 {
        self.start
    }

    pub fn count(&self) -> u64 {
        self.count
    }
}

/// Batches that may be in flight at once.
pub const SLOTS: usize = 2;

pub struct CudaBackend {
    stream: Arc<CudaStream>,
    kernels: Kernels,
    slots: Vec<Slot>,
    capacity: u32,
    max_blocks: u32,
    name: String,
    pci_bus_id: String,
}

impl CudaBackend {
    pub fn new(device: usize) -> Result<Self, CudaError> {
        Self::with_capacity(device, DEFAULT_SURVIVOR_CAPACITY)
    }

    pub fn with_capacity(device: usize, capacity: u32) -> Result<Self, CudaError> {
        let image = fatbin()?;
        // cudarc panics if the driver library cannot be loaded at all.
        let ctx = std::panic::catch_unwind(|| CudaContext::new(device))
            .map_err(|_| CudaError::Unavailable("the NVIDIA driver (nvcuda) could not be loaded".into()))?
            .map_err(|e| CudaError::Unavailable(format!("device {device}: {e}")))?;
        let module = ctx.load_module(cudarc::nvrtc::Ptx::from_binary(image.to_vec()))?;
        let mut kernels = Kernels {
            search: Vec::new(),
            construct: Vec::new(),
        };
        for w in LIMB_WIDTHS {
            kernels.search.push((w, module.load_function(&format!("search_{w}"))?));
            kernels
                .construct
                .push((w, module.load_function(&format!("construct_{w}"))?));
        }
        let stream = ctx.default_stream();
        let sms = ctx.attribute(CUdevice_attribute::CU_DEVICE_ATTRIBUTE_MULTIPROCESSOR_COUNT)? as u32;
        // CUDA and NVML may number devices differently; the PCI address is unambiguous.
        let pci_bus_id = format!(
            "{:x}:{:x}:{:x}.0",
            ctx.attribute(CUdevice_attribute::CU_DEVICE_ATTRIBUTE_PCI_DOMAIN_ID)?,
            ctx.attribute(CUdevice_attribute::CU_DEVICE_ATTRIBUTE_PCI_BUS_ID)?,
            ctx.attribute(CUdevice_attribute::CU_DEVICE_ATTRIBUTE_PCI_DEVICE_ID)?
        );
        let mut slots = Vec::with_capacity(SLOTS);
        for _ in 0..SLOTS {
            let slot_stream = ctx.new_stream()?;
            slots.push(Slot {
                survivors: slot_stream.alloc_zeros::<u32>(capacity.max(1) as usize)?,
                counters: slot_stream.alloc_zeros::<u64>(3)?,
                stream: slot_stream,
                busy: false,
            });
        }
        Ok(Self {
            slots,
            kernels,
            capacity,
            max_blocks: sms * BLOCKS_PER_SM,
            name: ctx.name()?,
            pci_bus_id,
            stream,
        })
    }

    pub fn device_name(&self) -> &str {
        &self.name
    }

    /// PCI address as `domain:bus:device.function` in hex, for matching NVML telemetry.
    pub fn pci_bus_id(&self) -> &str {
        &self.pci_bus_id
    }

    fn check_range(prepared: &Prepared, start: u64, count: u64, max_count: u64) -> Result<(), CudaError> {
        if count == 0 || count > max_count {
            return Err(CudaError::Unsupported(format!(
                "batch of {count} candidates (limit {max_count})"
            )));
        }
        let end = start
            .checked_add(count)
            .ok_or_else(|| CudaError::Unsupported("batch end beyond 2^64".into()))?;
        let total = prepared.space.candidate_count();
        if BigUint::from(end) > total {
            return Err(CoreError::InvalidRange {
                start: start.into(),
                end: end.into(),
                count: total,
            }
            .into());
        }
        Ok(())
    }

    /// Filters `[start, start + count)` on the GPU and waits for the result.
    /// `count` may be at most 2^32.
    pub fn search(&mut self, prepared: &Prepared, start: u64, count: u64) -> Result<BatchResult, CudaError> {
        let pending = self.launch(prepared, start, count)?;
        self.collect(pending)
    }

    /// Starts filtering `[start, start + count)` without waiting. At most
    /// [`SLOTS`] batches may be in flight; each must be passed to [`Self::collect`].
    pub fn launch(&mut self, prepared: &Prepared, start: u64, count: u64) -> Result<Pending, CudaError> {
        Self::check_range(prepared, start, count, 1 << 32)?;
        let index = self
            .slots
            .iter()
            .position(|s| !s.busy)
            .ok_or_else(|| CudaError::Unsupported(format!("more than {SLOTS} batches in flight")))?;
        let launched = Instant::now();
        let blocks = count.div_ceil(THREADS_PER_BLOCK as u64).min(self.max_blocks as u64) as u32;
        let cfg = LaunchConfig {
            grid_dim: (blocks, 1, 1),
            block_dim: (THREADS_PER_BLOCK, 1, 1),
            shared_mem_bytes: 0,
        };
        let func = Kernels::get(&self.kernels.search, prepared.limbs);
        let capacity = self.capacity;
        let slot = &mut self.slots[index];
        let stream = Arc::clone(&slot.stream);
        stream.memset_zeros(&mut slot.counters)?;
        let mut launch = stream.launch_builder(func);
        launch
            .arg(&prepared.params)
            .arg(&start)
            .arg(&count)
            .arg(&mut slot.survivors)
            .arg(&capacity)
            .arg(&mut slot.counters);
        // SAFETY: argument types and order match `search_N` in search.cu; the
        // survivor buffer holds `capacity` entries and the kernel bounds-checks.
        unsafe { launch.launch(cfg) }?;
        slot.busy = true;
        Ok(Pending {
            slot: index,
            start,
            count,
            launched,
        })
    }

    /// Waits for a launched batch and returns what it found.
    pub fn collect(&mut self, pending: Pending) -> Result<BatchResult, CudaError> {
        let capacity = self.capacity as u64;
        let slot = &mut self.slots[pending.slot];
        slot.busy = false;
        let counters = slot.stream.clone_dtoh(&slot.counters)?;
        let found = counters[0];
        let mut survivors: Vec<u64> = if found == 0 {
            Vec::new()
        } else {
            let offsets = slot.stream.clone_dtoh(&slot.survivors)?;
            offsets[..found.min(capacity) as usize]
                .iter()
                .map(|&o| pending.start + o as u64)
                .collect()
        };
        survivors.sort_unstable();
        Ok(BatchResult {
            start: pending.start,
            count: pending.count,
            processed: counters[1],
            checksum: counters[2],
            survivors,
            overflowed: found > capacity,
            elapsed: pending.launched.elapsed(),
        })
    }

    /// Builds candidates on the GPU and returns them (for testing construction).
    pub fn construct(&mut self, prepared: &Prepared, start: u64, count: u32) -> Result<Vec<BigUint>, CudaError> {
        Self::check_range(prepared, start, count as u64, 1 << 20)?;
        let n = prepared.limbs;
        let stream = Arc::clone(&self.stream);
        let mut out = stream.alloc_zeros::<u32>(count as usize * n)?;
        let cfg = LaunchConfig {
            grid_dim: (count.div_ceil(THREADS_PER_BLOCK), 1, 1),
            block_dim: (THREADS_PER_BLOCK, 1, 1),
            shared_mem_bytes: 0,
        };
        let func = Kernels::get(&self.kernels.construct, n);
        let mut launch = stream.launch_builder(func);
        launch.arg(&prepared.params).arg(&start).arg(&count).arg(&mut out);
        // SAFETY: matches `construct_N`; `out` holds `count * N` limbs.
        unsafe { launch.launch(cfg) }?;
        let limbs = stream.clone_dtoh(&out)?;
        Ok(limbs.chunks(n).map(BigUint::from_slice).collect())
    }
}

#[cfg(trindrome_kernels)]
fn fatbin() -> Result<&'static [u8], CudaError> {
    Ok(FATBIN)
}

#[cfg(not(trindrome_kernels))]
fn fatbin() -> Result<&'static [u8], CudaError> {
    Err(CudaError::NotBuilt)
}
