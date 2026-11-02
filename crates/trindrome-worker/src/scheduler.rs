//! The local worker: claims chunks, searches them in short batches and checkpoints.
//!
//! Each search thread owns one [`Engine`] and its own database connection and
//! claims chunks in `BEGIN IMMEDIATE` transactions, so threads coordinate through
//! SQLite exactly as independent processes (or, later, machines) would.
//!
//! The main thread is the supervisor: every second it reads the sensors, asks the
//! [`Policy`] whether computing is allowed, combines that with manual pause/stop
//! requests and error back-off, reloads the configuration file if it changed,
//! and records the state and its reason for the CLI and front ends.

use std::fs::{File, OpenOptions, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, RwLock};
use std::thread;
use std::time::{Duration, Instant, SystemTime};

use anyhow::{Context, Result, anyhow, bail};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use tracing::{debug, error, info, warn};
use trindrome_cpu::Match;

use crate::configuration::Config;
use crate::engine::{Audit, Engine, is_retryable};
use crate::persistence::{Checkpoint, Claim, Requested, Store};
use crate::policy::{Decision, Policy, PolicySettings, Reason};
use crate::power;
use crate::sensors::SystemSensors;

const CONTROL_POLL: Duration = Duration::from_millis(250);
const POLICY_INTERVAL: Duration = Duration::from_secs(1);
const CONFIG_CHECK_INTERVAL: Duration = Duration::from_secs(2);
const INITIAL_BACKOFF: Duration = Duration::from_secs(15);
const MAX_BACKOFF: Duration = Duration::from_secs(600);

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

struct Control {
    desired: AtomicU8,
    stop_reason: Mutex<Option<String>>,
    /// A search thread backing off after a transient error: message and retry time.
    backoff: Mutex<Option<(String, Instant)>>,
}

impl Control {
    /// Starts paused: nothing runs until the policy has been consulted.
    fn new() -> Self {
        Self {
            desired: AtomicU8::new(PAUSE),
            stop_reason: Mutex::new(None),
            backoff: Mutex::new(None),
        }
    }

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

    /// Sleeps for `duration` unless stopped first. Returns false on stop.
    fn sleep_unless_stopped(&self, duration: Duration) -> bool {
        let until = Instant::now() + duration;
        while Instant::now() < until {
            if self.state() == STOP {
                return false;
            }
            thread::sleep(Duration::from_millis(100));
        }
        self.state() != STOP
    }
}

/// How the worker was started.
#[derive(Clone, Debug, Default)]
pub struct RunOptions {
    /// `run --now`: skip the inactivity requirements, keep every safety limit.
    pub immediate: bool,
    /// Configuration file to watch for changes, and whether it must exist.
    pub config_file: Option<(PathBuf, bool)>,
}

struct WorkerContext {
    database: PathBuf,
    settings: RwLock<Config>,
    control: Control,
}

/// Runs one search thread per engine until the plan is exhausted, a stop is
/// requested, or `interrupted` becomes true. Returns the reason it stopped.
pub fn run(config: &Config, engines: Vec<Engine>, options: &RunOptions, interrupted: &AtomicBool) -> Result<String> {
    let database = config.storage.database_path.clone();
    let _lock = acquire_lock(&database)?;
    let mut store = Store::open(&database)?;
    let added = store.plan(&config.search)?;
    let recovered = store.recover_interrupted()?;
    store.worker_started()?;

    let hardware = engines.first().map(Engine::hardware).unwrap_or_default();
    let pci_bus_id = engines.iter().find_map(|e| e.pci_bus_id().map(str::to_string));
    let uses_gpu = pci_bus_id.is_some();
    let sensors = SystemSensors::new(config.idle.exclusive_gpu_apps.clone(), pci_bus_id.as_deref());
    if let Some(e) = sensors.gpu_error() {
        warn!("GPU telemetry unavailable: {e}");
    }
    let policy = Policy::new(PolicySettings::from_config(config, uses_gpu, options.immediate));
    info!(
        database = %database.display(), threads = engines.len(), %hardware, immediate = options.immediate,
        added_spaces = added, recovered_chunks = recovered, "worker starting"
    );

    let ctx = Arc::new(WorkerContext {
        database,
        settings: RwLock::new(config.clone()),
        control: Control::new(),
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

    let mut supervisor = Supervisor {
        store: &store,
        ctx: &ctx,
        sensors,
        policy,
        uses_gpu,
        options,
        config_stamp: options.config_file.as_ref().and_then(|(p, _)| modified(p)),
        last_config_check: Instant::now(),
        last_decision: None,
        awake: false,
        logged: String::new(),
    };
    let supervised = supervisor.run(&handles, interrupted);
    power::keep_system_awake(false);
    if supervised.is_err() {
        ctx.control.stop("control loop failed");
    }

    let mut first_error = supervised.err();
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

fn modified(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path).and_then(|m| m.modified()).ok()
}

struct Supervisor<'a> {
    store: &'a Store,
    ctx: &'a WorkerContext,
    sensors: SystemSensors,
    policy: Policy,
    uses_gpu: bool,
    options: &'a RunOptions,
    config_stamp: Option<SystemTime>,
    last_config_check: Instant,
    last_decision: Option<(Instant, Decision)>,
    awake: bool,
    logged: String,
}

impl Supervisor<'_> {
    fn run(&mut self, handles: &[thread::JoinHandle<Result<()>>], interrupted: &AtomicBool) -> Result<()> {
        while !handles.iter().all(|h| h.is_finished()) {
            self.tick(interrupted)?;
            thread::sleep(CONTROL_POLL);
        }
        Ok(())
    }

    fn tick(&mut self, interrupted: &AtomicBool) -> Result<()> {
        let control = &self.ctx.control;
        if interrupted.load(Ordering::SeqCst) {
            control.stop("interrupted");
        }
        let requested = self.store.requested()?;
        if requested == Requested::Stop {
            control.stop("stop requested");
        }
        if self.last_config_check.elapsed() >= CONFIG_CHECK_INTERVAL {
            self.last_config_check = Instant::now();
            self.reload_config_if_changed();
        }
        let due = self
            .last_decision
            .as_ref()
            .is_none_or(|(at, _)| at.elapsed() >= POLICY_INTERVAL);
        if due {
            let readings = self.sensors.read();
            self.last_decision = Some((Instant::now(), self.policy.evaluate(&readings)));
        }
        let decision = self.last_decision.as_ref().unwrap().1.clone();

        // Precedence: stop, manual pause, error back-off, then the policy.
        let backoff = control.backoff.lock().unwrap().clone();
        let (state, reason) = if control.state() == STOP {
            ("stopping", None)
        } else if requested == Requested::Pause {
            ("paused", Some(Reason::ManualPause))
        } else if let Some((message, until)) = backoff {
            let retry_in_seconds = until.saturating_duration_since(Instant::now()).as_secs();
            (
                "error",
                Some(Reason::Error {
                    message,
                    retry_in_seconds,
                }),
            )
        } else if !decision.run {
            ("waiting", Some(decision.reason))
        } else {
            ("running", Some(Reason::Running))
        };
        control.set_paused(state != "running");

        // Overnight mode: hold off idle sleep only while computing on AC power.
        let settings = self.ctx.settings.read().unwrap();
        let awake = state == "running" && settings.power.prevent_automatic_sleep && power::on_ac_power() == Some(true);
        drop(settings);
        if awake != self.awake {
            power::keep_system_awake(awake);
            self.awake = awake;
        }

        let text = match &reason {
            Some(r) => r.describe(),
            None => control.stop_reason.lock().unwrap().clone().unwrap_or_default(),
        };
        let code = reason.as_ref().map(Reason::code);
        let summary = format!("{state}: {}", code.unwrap_or(""));
        if summary != self.logged {
            info!(state, reason = %text, "worker state");
            self.logged = summary;
        }
        let detail = reason.as_ref().map(Reason::detail_json);
        self.store.heartbeat(state, Some(&text), code, detail.as_deref())?;
        Ok(())
    }

    /// Applies a changed configuration file without restarting. Invalid files
    /// are reported and ignored; settings that need a restart are reported.
    fn reload_config_if_changed(&mut self) {
        let Some((path, required)) = &self.options.config_file else {
            return;
        };
        let stamp = modified(path);
        if stamp == self.config_stamp {
            return;
        }
        self.config_stamp = stamp;
        if stamp.is_none() && !required {
            return;
        }
        match Config::load_from(path, *required) {
            Ok(newer) => {
                let mut settings = self.ctx.settings.write().unwrap();
                for setting in settings.restart_required_changes(&newer) {
                    warn!(
                        setting,
                        "changed in the configuration file; takes effect after a restart"
                    );
                }
                self.policy.update_settings(PolicySettings::from_config(
                    &newer,
                    self.uses_gpu,
                    self.options.immediate,
                ));
                self.sensors.set_exclusions(newer.idle.exclusive_gpu_apps.clone());
                // Keep restart-only settings as they are until a restart.
                let keep = (
                    settings.compute.clone(),
                    settings.storage.clone(),
                    settings.logging.clone(),
                );
                *settings = newer;
                settings.compute.backend = keep.0.backend;
                settings.compute.gpu_device = keep.0.gpu_device;
                settings.compute.cpu_threads = keep.0.cpu_threads;
                settings.storage = keep.1;
                settings.logging = keep.2;
                self.last_decision = None;
                info!(file = %path.display(), "configuration reloaded");
            }
            Err(e) => {
                error!("ignoring invalid configuration change: {e:#}");
                let _ = self
                    .store
                    .record_error(&format!("invalid configuration change ignored: {e:#}"));
            }
        }
    }
}

fn search_thread(ctx: &WorkerContext, mut engine: Engine) -> Result<()> {
    let mut store = Store::open(&ctx.database)?;
    let hardware = engine.hardware();
    let mut batch = engine.initial_batch();
    let mut backoff = INITIAL_BACKOFF;
    loop {
        if ctx.control.state() != RUN {
            // Give the GPU's memory back while paused (e.g. to a game).
            engine.release_resources();
            if !ctx.control.wait_until_runnable() {
                return Ok(());
            }
        }
        let (search, chunk_candidates) = {
            let s = ctx.settings.read().unwrap();
            (s.search.clone(), s.compute.chunk_candidates)
        };
        let Some(claim) = store.claim(&search, chunk_candidates)? else {
            return Ok(());
        };
        match search_claim(ctx, &mut store, &mut engine, &hardware, claim, &mut batch) {
            Ok(()) => backoff = INITIAL_BACKOFF,
            Err(e) if is_retryable(&e) => {
                let message = format!("{e:#}");
                warn!(error = %message, retry_in = ?backoff, "transient GPU error; backing off");
                store.record_error(&message)?;
                engine.release_resources();
                *ctx.control.backoff.lock().unwrap() = Some((message, Instant::now() + backoff));
                let carry_on = ctx.control.sleep_unless_stopped(backoff);
                *ctx.control.backoff.lock().unwrap() = None;
                if !carry_on {
                    return Ok(());
                }
                backoff = (backoff * 2).min(MAX_BACKOFF);
                batch = engine.initial_batch();
            }
            Err(e) => return Err(e),
        }
    }
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

/// Searches a claimed chunk until it is done or the worker pauses. On any
/// error the chunk is handed back (committed progress is kept).
fn search_claim(
    ctx: &WorkerContext,
    store: &mut Store,
    engine: &mut Engine,
    hardware: &str,
    claim: Claim,
    batch: &mut u64,
) -> Result<()> {
    let chunk_id = claim.chunk.id();
    let result = search_claim_inner(ctx, store, engine, hardware, claim, batch);
    if result.is_err() {
        let _ = store.release(&chunk_id);
    }
    result
}

fn search_claim_inner(
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
        let settings = ctx.settings.read().unwrap().clone();
        let batch_end = (&progress.cursor + *batch).min(end.clone());
        let output = engine.run_batch(space, &progress.cursor, &batch_end, &settings.compute)?;
        let target = Duration::from_millis(settings.compute.batch_target_ms);
        *batch = next_batch_size(output.candidates, output.elapsed, target).min(engine.max_batch());
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

        let interval = Duration::from_secs(settings.compute.checkpoint_interval_seconds);
        if found || progress.cursor == end || progress.last_commit.elapsed() >= interval {
            progress.commit(store, hardware)?;
        }
        for decimal in nontrivial {
            warn!(%decimal, chunk = %progress.chunk_id, "VERIFIED NONTRIVIAL TRIPLE PALINDROME");
            if settings.search.stop_on_first_verified_discovery {
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
