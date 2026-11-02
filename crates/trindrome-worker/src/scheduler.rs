//! The local worker: claims chunks, searches them in short batches and checkpoints.
//!
//! Each search thread owns one [`Engine`] and its own database connection and
//! claims chunks in `BEGIN IMMEDIATE` transactions, so threads coordinate through
//! SQLite exactly as independent processes (or, later, machines) would. The main
//! thread only relays control requests and writes heartbeats.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tracing::{debug, info, warn};
use trindrome_cpu::Match;

use crate::configuration::{Config, Search};
use crate::engine::{Audit, Engine};
use crate::persistence::{Checkpoint, Claim, Requested, Store};

const CONTROL_POLL: Duration = Duration::from_millis(250);

/// Exclusive ownership of a database by one worker process.
pub struct InstanceLock {
    _file: File,
}

pub fn lock_path(database: &Path) -> PathBuf {
    let mut path = database.as_os_str().to_owned();
    path.push(".lock");
    path.into()
}

pub fn acquire_lock(database: &Path) -> Result<InstanceLock> {
    let path = lock_path(database);
    if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
        std::fs::create_dir_all(dir)?;
    }
    let file = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(&path)
        .with_context(|| format!("cannot open lock file {}", path.display()))?;
    match file.try_lock() {
        Ok(()) => Ok(InstanceLock { _file: file }),
        Err(TryLockError::WouldBlock) => {
            bail!("another trindrome worker is already running on {}", database.display())
        }
        Err(TryLockError::Error(e)) => Err(e).context("cannot lock the database"),
    }
}

/// Whether a worker currently holds the database (without disturbing it).
pub fn worker_is_running(database: &Path) -> bool {
    let Ok(file) = OpenOptions::new().write(true).open(lock_path(database)) else {
        return false;
    };
    matches!(file.try_lock(), Err(TryLockError::WouldBlock))
}

const RUN: u8 = 0;
const PAUSE: u8 = 1;
const STOP: u8 = 2;

#[derive(Default)]
struct Control {
    desired: AtomicU8,
    stop_reason: Mutex<Option<String>>,
    discovered: AtomicBool,
}

impl Control {
    fn stop(&self, reason: impl Into<String>) {
        let mut current = self.stop_reason.lock().unwrap();
        current.get_or_insert_with(|| reason.into());
        self.desired.store(STOP, Ordering::SeqCst);
    }

    /// Switches between running and paused; never revokes a stop.
    fn set_paused(&self, paused: bool) {
        let (from, to) = if paused { (RUN, PAUSE) } else { (PAUSE, RUN) };
        let _ = self
            .desired
            .compare_exchange(from, to, Ordering::SeqCst, Ordering::SeqCst);
    }

    fn state(&self) -> u8 {
        self.desired.load(Ordering::SeqCst)
    }

    /// Blocks while paused. Returns false once a stop is requested.
    fn wait_until_runnable(&self) -> bool {
        loop {
            match self.state() {
                RUN => return true,
                STOP => return false,
                _ => thread::sleep(Duration::from_millis(100)),
            }
        }
    }
}

struct WorkerContext {
    database: PathBuf,
    search: Search,
    chunk_candidates: u64,
    batch_target: Duration,
    checkpoint_interval: Duration,
    control: Control,
}

/// Runs one search thread per engine until the plan is exhausted, a stop is
/// requested, or `interrupted` becomes true. Returns the reason it stopped.
pub fn run(config: &Config, engines: Vec<Engine>, interrupted: &AtomicBool) -> Result<String> {
    let database = config.storage.database_path.clone();
    let _lock = acquire_lock(&database)?;
    let mut store = Store::open(&database)?;
    let added = store.plan(&config.search)?;
    let recovered = store.recover_interrupted()?;
    store.worker_started()?;
    let hardware = engines.first().map(Engine::hardware).unwrap_or_default();
    info!(database = %database.display(), threads = engines.len(), %hardware, added_spaces = added, recovered_chunks = recovered, "worker starting");

    let ctx = Arc::new(WorkerContext {
        database,
        search: config.search.clone(),
        chunk_candidates: config.compute.chunk_candidates,
        batch_target: Duration::from_millis(config.compute.batch_target_ms),
        checkpoint_interval: Duration::from_secs(config.compute.checkpoint_interval_seconds),
        control: Control::default(),
    });

    let mut handles = Vec::with_capacity(engines.len());
    for (i, engine) in engines.into_iter().enumerate() {
        let thread_ctx = Arc::clone(&ctx);
        let handle = thread::Builder::new().name(format!("search-{i}")).spawn(move || {
            let result = search_thread(&thread_ctx, engine);
            if let Err(e) = &result {
                thread_ctx.control.stop(format!("error: {e:#}"));
            }
            result
        });
        match handle {
            Ok(h) => handles.push(h),
            Err(e) => {
                ctx.control.stop("could not start search threads");
                warn!("failed to spawn search thread: {e}");
                break;
            }
        }
    }

    let monitor = monitor(&store, &ctx, &handles, interrupted);
    if monitor.is_err() {
        ctx.control.stop("control loop failed");
    }

    let mut first_error = monitor.err();
    for handle in handles {
        let result = handle
            .join()
            .unwrap_or_else(|_| Err(anyhow!("a search thread panicked")));
        if let Err(e) = result {
            first_error.get_or_insert(e);
        }
    }

    let reason = if let Some(e) = &first_error {
        format!("error: {e:#}")
    } else if let Some(reason) = ctx.control.stop_reason.lock().unwrap().clone() {
        reason
    } else {
        "search plan exhausted".to_string()
    };
    let error_text = first_error.as_ref().map(|e| format!("{e:#}"));
    store.worker_stopped(&reason, error_text.as_deref())?;
    info!(%reason, "worker stopped");
    match first_error {
        Some(e) => Err(e),
        None => Ok(reason),
    }
}

fn monitor(
    store: &Store,
    ctx: &WorkerContext,
    handles: &[thread::JoinHandle<Result<()>>],
    interrupted: &AtomicBool,
) -> Result<()> {
    let mut last_state = "";
    while !handles.iter().all(|h| h.is_finished()) {
        if interrupted.load(Ordering::SeqCst) {
            ctx.control.stop("interrupted");
        }
        match store.requested()? {
            Requested::Run => ctx.control.set_paused(false),
            Requested::Pause => ctx.control.set_paused(true),
            Requested::Stop => ctx.control.stop("stop requested"),
        }
        let (state, reason) = match ctx.control.state() {
            RUN => ("running", None),
            PAUSE => ("paused", Some("manual pause")),
            _ => (
                "stopping",
                ctx.control.stop_reason.lock().unwrap().clone().map(|_| "stopping"),
            ),
        };
        if state != last_state {
            info!(state, "worker state");
            last_state = state;
        }
        store.heartbeat(state, reason)?;
        thread::sleep(CONTROL_POLL);
    }
    Ok(())
}

fn search_thread(ctx: &WorkerContext, mut engine: Engine) -> Result<()> {
    let mut store = Store::open(&ctx.database)?;
    let hardware = engine.hardware();
    let mut batch = engine.initial_batch();
    while ctx.control.wait_until_runnable() {
        let Some(claim) = store.claim(&ctx.search, ctx.chunk_candidates)? else {
            return Ok(());
        };
        search_claim(ctx, &mut store, &mut engine, &hardware, claim, &mut batch)?;
    }
    Ok(())
}

/// Progress on the current chunk that is computed but possibly not yet committed.
struct Progress {
    chunk_id: String,
    committed: BigUint,
    cursor: BigUint,
    matches: Vec<Match>,
    audits: Vec<Audit>,
    active: Duration,
    backend: &'static str,
    last_commit: Instant,
}

impl Progress {
    fn commit(&mut self, store: &mut Store, hardware: &str) -> Result<bool> {
        let candidates = (&self.cursor - &self.committed)
            .to_u64()
            .ok_or_else(|| anyhow!("uncommitted progress exceeds u64"))?;
        let complete = store.checkpoint(&Checkpoint {
            chunk_id: &self.chunk_id,
            from: &self.committed,
            to: &self.cursor,
            matches: &self.matches,
            audits: &self.audits,
            candidates,
            active: self.active,
            hardware,
            backend: self.backend,
        })?;
        self.committed = self.cursor.clone();
        self.matches.clear();
        self.audits.clear();
        self.active = Duration::ZERO;
        self.last_commit = Instant::now();
        Ok(complete)
    }
}

fn search_claim(
    ctx: &WorkerContext,
    store: &mut Store,
    engine: &mut Engine,
    hardware: &str,
    claim: Claim,
    batch: &mut u64,
) -> Result<()> {
    let space = claim.chunk.validate()?;
    let end = claim.chunk.end_index.clone();
    let mut progress = Progress {
        chunk_id: claim.chunk.id(),
        committed: claim.next_index.clone(),
        cursor: claim.next_index,
        matches: Vec::new(),
        audits: Vec::new(),
        active: Duration::ZERO,
        backend: "",
        last_commit: Instant::now(),
    };
    debug!(chunk = %progress.chunk_id, resume_from = %progress.cursor, "claimed chunk");

    while progress.cursor < end && ctx.control.state() == RUN {
        let batch_end = (&progress.cursor + *batch).min(end.clone());
        let output = engine.run_batch(space, &progress.cursor, &batch_end)?;
        *batch = next_batch_size(output.candidates, output.elapsed, ctx.batch_target).min(engine.max_batch());
        if output.backend != progress.backend && progress.cursor > progress.committed {
            // Never attribute one backend's work to another.
            progress.commit(store, hardware)?;
        }
        progress.backend = output.backend;
        progress.active += output.elapsed;
        progress.cursor = batch_end;

        let nontrivial: Vec<String> = output
            .matches
            .iter()
            .filter(|m| m.verification.is_nontrivial())
            .map(|m| m.verification.decimal.clone())
            .collect();
        let found = !output.matches.is_empty();
        progress.matches.extend(output.matches);
        progress.audits.extend(output.audits);

        if found || progress.cursor == end || progress.last_commit.elapsed() >= ctx.checkpoint_interval {
            progress.commit(store, hardware)?;
        }
        for decimal in nontrivial {
            warn!(%decimal, chunk = %progress.chunk_id, "VERIFIED NONTRIVIAL TRIPLE PALINDROME");
            ctx.control.discovered.store(true, Ordering::SeqCst);
            if ctx.search.stop_on_first_verified_discovery {
                ctx.control.stop("verified nontrivial discovery");
            }
        }
    }

    if progress.cursor > progress.committed {
        progress.commit(store, hardware)?;
    }
    if progress.cursor < end {
        store.release(&progress.chunk_id)?;
        info!(chunk = %progress.chunk_id, at = %progress.cursor, "released chunk");
    } else {
        info!(chunk = %progress.chunk_id, "completed chunk");
    }
    Ok(())
}

/// Steers batch size toward the target duration from the last batch's measured rate.
///
/// Shrinks immediately when batches run long (pause latency matters), but grows to
/// at most twice what was actually searched, so batches cut short at a chunk
/// boundary cannot inflate the size.
fn next_batch_size(searched: u64, elapsed: Duration, target: Duration) -> u64 {
    let searched = searched as f64;
    let ideal = searched * target.as_secs_f64() / elapsed.as_secs_f64().max(1e-6);
    ideal.min(searched * 2.0).clamp(64.0, (1u64 << 40) as f64) as u64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_size_tracks_measured_rate() {
        let target = Duration::from_millis(200);
        assert_eq!(next_batch_size(1000, Duration::from_millis(200), target), 1000);
        assert_eq!(next_batch_size(1000, Duration::from_millis(100), target), 2000);
        assert_eq!(
            next_batch_size(1000, Duration::from_millis(1), target),
            2000,
            "growth is capped"
        );
        assert_eq!(
            next_batch_size(1000, Duration::from_secs(5), target),
            64,
            "overlong batches shrink at once"
        );
        assert_eq!(next_batch_size(100_000, Duration::from_secs(1), target), 20_000);
        assert_eq!(next_batch_size(1, Duration::from_nanos(10), target), 64);
    }
}
