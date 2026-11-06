use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand, ValueEnum};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use serde_json::json;
use trindrome_core::SearchSpace;
use trindrome_core::filter::PRODUCTION_WINDOW;
use trindrome_cpu::search_range;
use trindrome_cuda::{CudaBackend, Prepared};
use trindrome_worker::configuration::{Config, config_path, data_dir};
use trindrome_worker::engine::{CPU_BACKEND, cpu_hardware, engines_for};
use trindrome_worker::identity::Identity;
use trindrome_worker::persistence::{Family, Requested, Store, now_ms};
use trindrome_worker::remote::{Client, importable_coverage};
use trindrome_worker::scheduler::{self, RunOptions};
use trindrome_worker::startup;

#[derive(Parser)]
#[command(
    name = "trindrome",
    version,
    about = "Search for integers palindromic in bases 2, 10 and 16"
)]
struct Cli {
    /// Configuration file (default: TRINDROME_CONFIG, else trindrome.toml in the data directory)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the worker in this terminal; it computes only while the computer is idle
    Run {
        /// Start immediately, skipping the inactivity requirements (safety limits still apply)
        #[arg(long)]
        now: bool,
        /// Use the CPU reference backend
        #[arg(long)]
        cpu: bool,
        /// CPU search threads (overrides compute.cpu_threads)
        #[arg(long)]
        threads: Option<usize>,
    },
    /// Ask the running worker to pause after its current batch
    Pause,
    /// Clear a pause request
    Resume,
    /// Ask the running worker to checkpoint and exit
    Stop,
    /// Show worker state, throughput and progress
    Status {
        /// Machine-readable output for scripts and front ends
        #[arg(long)]
        json: bool,
    },
    /// Show searched index ranges per search space
    Coverage {
        /// Include spaces with no progress yet
        #[arg(long)]
        all: bool,
    },
    /// List verified discoveries
    Discoveries,
    /// Check the database for coverage gaps, overlaps and unverifiable discoveries
    Verify,
    /// Measure GPU and CPU throughput on representative search spaces (no database)
    Benchmark {
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long)]
        threads: Option<usize>,
    },
    /// Print the effective configuration and where it comes from
    Config,
    /// Start the background agent automatically when you log in
    Startup {
        #[command(subcommand)]
        action: StartupAction,
    },
    /// Distributed mode: this installation's key, and moving local progress to a coordinator
    Coordinator {
        #[command(subcommand)]
        action: CoordinatorAction,
    },
}

#[derive(Subcommand)]
enum CoordinatorAction {
    /// Print this installation's public key (for the coordinator's OWNER_PUBLIC_KEYS)
    Key,
    /// Record this database's local search as done on the coordinator, so
    /// distributed work continues from it (owner keys only)
    Import {
        /// Coordinator URL (default: coordinator.url from the configuration)
        #[arg(long)]
        url: Option<String>,
        /// Show what would be imported without contacting the coordinator
        #[arg(long)]
        dry_run: bool,
    },
    /// List every worker with its private details: computer name and network (owner keys only)
    Workers {
        /// Coordinator URL (default: coordinator.url from the configuration)
        #[arg(long)]
        url: Option<String>,
    },
    /// Set a worker's trust level (owner keys only). Results from `new`
    /// workers are searched again by another worker before they count as checked
    Trust {
        /// Worker ID, as `coordinator workers` lists it, or `self` for this installation
        worker: String,
        level: TrustLevel,
        /// Coordinator URL (default: coordinator.url from the configuration)
        #[arg(long)]
        url: Option<String>,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum TrustLevel {
    /// Every result is searched again by another worker
    New,
    /// Earned by matching replicas; results are no longer searched again
    Established,
    /// Your own machines: never searched again, and breaks ties
    Trusted,
}

impl TrustLevel {
    fn as_str(self) -> &'static str {
        match self {
            TrustLevel::New => "new",
            TrustLevel::Established => "established",
            TrustLevel::Trusted => "trusted",
        }
    }
}

#[derive(Subcommand)]
enum StartupAction {
    /// Start trindrome-agent at login (current user, no administrator rights)
    Enable,
    /// Stop starting it at login
    Disable,
    /// Show whether it starts at login
    Status,
}

pub fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    let level: tracing::Level = config.logging.level.parse().expect("validated");
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .with_writer(std::io::stderr)
        .init();
    let db = &config.storage.database_path;

    match cli.command {
        Command::Run { now, cpu, threads } => run(&config, cli.config.as_deref(), now, cpu, threads),
        Command::Pause => request(
            &config,
            Requested::Pause,
            "pause requested; the worker will pause after its current batch",
        ),
        Command::Resume => request(&config, Requested::Run, "resume requested"),
        Command::Stop => request(
            &config,
            Requested::Stop,
            "stop requested; the worker will checkpoint and exit",
        ),
        Command::Status { json } if json => status_json(&config),
        Command::Status { .. } => status(&config),
        Command::Coverage { all } => coverage(&Store::open(db)?, all),
        Command::Discoveries => discoveries(&Store::open(db)?),
        Command::Verify => verify(&Store::open(db)?),
        Command::Benchmark { seconds, threads } => {
            benchmark(seconds, threads.unwrap_or(config.compute.effective_cpu_threads()))
        }
        Command::Config => {
            let (path, _) = config_path(cli.config.as_deref());
            let exists = if path.exists() {
                ""
            } else {
                " (not present; using defaults)"
            };
            println!("# configuration: {}{exists}", path.display());
            println!("# database:      {}", db.display());
            println!("# data dir:      {}", data_dir().display());
            print!("{}", toml::to_string_pretty(&config)?);
            for setting in config.unenforced_settings() {
                println!("# not enforced on this platform: {setting}");
            }
            Ok(ExitCode::SUCCESS)
        }
        Command::Startup { action } => startup_command(cli.config.as_deref(), action),
        Command::Coordinator { action } => coordinator_command(&config, action),
    }
}

fn coordinator_command(config: &Config, action: CoordinatorAction) -> Result<ExitCode> {
    let db = &config.storage.database_path;
    let identity_dir = db.parent().unwrap_or(Path::new("."));
    match action {
        CoordinatorAction::Key => {
            println!("{}", Identity::load_or_create(identity_dir)?.public_key_base64());
        }
        CoordinatorAction::Import { url, dry_run } => {
            let store = Store::open(db)?;
            if !store.leased_chunk_ids()?.is_empty() {
                bail!("this database already searches for a coordinator; only a purely local search can be imported");
            }
            if !dry_run && scheduler::worker_is_running(db) {
                bail!("stop the worker first (`trindrome stop`), so its progress cannot change during the import");
            }
            let spaces = importable_coverage(&store)?;
            let total: BigUint = spaces.iter().map(|s| &s.searched).sum();
            println!(
                "{} spaces, {} candidates searched from index 0",
                spaces.len(),
                human(to_f64(&total))
            );
            if dry_run {
                for s in spaces.iter().filter(|s| s.searched > BigUint::ZERO) {
                    println!("  h{}:{:X}  {}", s.hex_digits, s.leading_hex_digit, s.searched);
                }
                return Ok(ExitCode::SUCCESS);
            }
            let Some(url) = url.or_else(|| config.coordinator.url.clone()) else {
                bail!("no coordinator: pass --url or set coordinator.url");
            };
            let client = Client::new(&url, identity_dir, "import")?;
            let (imported, candidates) = client.import_coverage(&spaces, config.coordinator.name.as_deref())?;
            println!(
                "{} recorded {imported} spaces ({candidates} candidates) as searched",
                client.url()
            );
            println!("historical lengths are ignored; spaces it already had are unchanged");
        }
        CoordinatorAction::Workers { url } => {
            let Some(url) = url.or_else(|| config.coordinator.url.clone()) else {
                bail!("no coordinator: pass --url or set coordinator.url");
            };
            let client = Client::new(&url, identity_dir, "owner")?;
            let workers = client.workers()?;
            println!(
                "{:<24} {:<20} {:<16} {:<20} {:<16} {:>10} {:>5}  last seen",
                "worker", "name", "computer", "network", "trust", "searched", "found"
            );
            for w in workers {
                println!(
                    "{:<24} {:<20} {:<16} {:<20} {:<16} {:>10} {:>5}  {} ago",
                    w.worker_id,
                    w.name.as_deref().unwrap_or("-"),
                    w.hostname.as_deref().unwrap_or("-"),
                    w.network,
                    match w.trust.as_str() {
                        "new" => format!("new ({} ok)", w.matched),
                        other => other.to_string(),
                    },
                    human(w.candidates),
                    w.discoveries,
                    duration(now_ms() - w.last_seen)
                );
                if let Some(gpu) = &w.gpu {
                    println!("{:<24} {gpu}", "");
                }
            }
        }
        CoordinatorAction::Trust { worker, level, url } => {
            let Some(url) = url.or_else(|| config.coordinator.url.clone()) else {
                bail!("no coordinator: pass --url or set coordinator.url");
            };
            let client = Client::new(&url, identity_dir, "owner")?;
            let worker = if worker == "self" {
                client
                    .worker_id()
                    .context("this installation hasn't registered with that coordinator yet")?
            } else {
                worker
            };
            let cancelled = client.set_trust(&worker, level.as_str())?;
            println!("{worker} is now {}", level.as_str());
            if cancelled > 0 {
                println!("dropped {cancelled} queued replicas of its earlier results");
            }
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn run(config: &Config, explicit: Option<&Path>, now: bool, cpu: bool, threads: Option<usize>) -> Result<ExitCode> {
    let engines = engines_for(&config.compute, cpu, threads)?;
    let options = RunOptions {
        immediate: now,
        config_file: Some(config_path(explicit)),
    };

    let interrupted = Arc::new(AtomicBool::new(false));
    let flag = Arc::clone(&interrupted);
    ctrlc::set_handler(move || {
        if flag.swap(true, Ordering::SeqCst) {
            eprintln!("exiting immediately; uncommitted work will be repeated next run");
            std::process::exit(130);
        }
        eprintln!("stopping after the current batch (Ctrl+C again to exit immediately)");
    })?;

    let reason = scheduler::run(config, engines, &options, &interrupted)?;
    println!("worker stopped: {reason}");
    let found = Store::open(&config.storage.database_path)?
        .discoveries()?
        .iter()
        .any(|d| d.nontrivial);
    if found {
        println!("*** a verified nontrivial discovery is recorded; see `trindrome discoveries` ***");
    }
    Ok(ExitCode::SUCCESS)
}

fn startup_command(explicit: Option<&Path>, action: StartupAction) -> Result<ExitCode> {
    match action {
        StartupAction::Enable => {
            let agent = std::env::current_exe()?.with_file_name(if cfg!(windows) {
                "trindrome-agent.exe"
            } else {
                "trindrome-agent"
            });
            if !agent.exists() {
                bail!("{} not found next to this executable", agent.display());
            }
            let mut command = format!("\"{}\"", agent.display());
            if let Some(path) = explicit {
                let path = std::fs::canonicalize(path).with_context(|| format!("{}", path.display()))?;
                command.push_str(&format!(" --config \"{}\"", path.display()));
            }
            startup::set(startup::RUN_KEY, startup::VALUE_NAME, &command)?;
            println!("trindrome-agent will start when you log in:\n  {command}");
            println!(
                "(listed in Task Manager â†’ Startup apps as \"{}\")",
                startup::VALUE_NAME
            );
        }
        StartupAction::Disable => {
            startup::remove(startup::RUN_KEY, startup::VALUE_NAME)?;
            println!("trindrome-agent will no longer start at login");
        }
        StartupAction::Status => match startup::get(startup::RUN_KEY, startup::VALUE_NAME)? {
            Some(command) => println!("starts at login: {command}"),
            None => println!("does not start at login"),
        },
    }
    Ok(ExitCode::SUCCESS)
}

fn request(config: &Config, requested: Requested, message: &str) -> Result<ExitCode> {
    let db = &config.storage.database_path;
    Store::open(db)?.set_requested(requested)?;
    println!("{message}");
    if !scheduler::worker_is_running(db) && requested != Requested::Run {
        println!("(no worker is running right now; a pause will apply when one starts)");
    }
    Ok(ExitCode::SUCCESS)
}

/// The same facts as `status`, for scripts and front ends. Large counts are
/// decimal strings; `detail` carries the structured reason.
fn status_json(config: &Config) -> Result<ExitCode> {
    let db = &config.storage.database_path;
    let store = Store::open(db)?;
    let worker = store.worker()?;
    let running = scheduler::worker_is_running(db);
    let totals = store.totals()?;
    let window = Duration::from_secs((2 * config.compute.checkpoint_interval_seconds).max(60));
    let rate = if running {
        store.recent_rate(window, worker.started_at)?
    } else {
        0.0
    };
    let coverage = store.coverage()?;
    let families: Vec<_> = [Family::Historical, Family::Frontier]
        .into_iter()
        .map(|family| {
            let spaces: Vec<_> = coverage.iter().filter(|c| c.family == family).collect();
            let total: BigUint = spaces.iter().map(|c| c.total.clone()).sum();
            let covered: BigUint = spaces.iter().map(|c| c.covered.clone()).sum();
            json!({
                "family": family.as_str(),
                "spaces": spaces.len(),
                "covered": covered.to_string(),
                "total": total.to_string(),
                "percent": percent(&covered, &total),
            })
        })
        .collect();
    let found = store.discoveries()?;
    let nontrivial = found.iter().filter(|d| d.nontrivial).count();
    let (audits, audited) = store.audit_summary()?;
    let backend = store.last_backend()?;
    let detail: Option<serde_json::Value> = worker.detail.as_deref().and_then(|d| serde_json::from_str(d).ok());
    let report = json!({
        "worker": {
            "running": running,
            "state": if running { worker.state.as_str() } else { "stopped" },
            "reason": worker.reason,
            "reason_code": if running { worker.reason_code.clone() } else { None },
            "detail": if running { detail } else { None },
            "requested": worker.requested,
            "pid": worker.pid,
            "started_at_ms": worker.started_at,
            "heartbeat_at_ms": worker.heartbeat_at,
            "last_error": worker.last_error,
        },
        "backend": backend.as_ref().map(|(b, _)| b),
        "hardware": backend.as_ref().map(|(_, h)| h),
        "candidates_per_second": rate,
        "searched_candidates": totals.candidates.to_string(),
        "active_ms": totals.active_ms,
        "last_checkpoint_at_ms": totals.last_checkpoint_at,
        "families": families,
        "discoveries": { "nontrivial": nontrivial, "trivial": found.len() - nontrivial },
        "audits": { "slices": audits, "candidates": audited.to_string() },
    });
    println!("{}", serde_json::to_string_pretty(&report)?);
    Ok(ExitCode::SUCCESS)
}

fn status(config: &Config) -> Result<ExitCode> {
    let db = &config.storage.database_path;
    let store = Store::open(db)?;
    let worker = store.worker()?;
    let running = scheduler::worker_is_running(db);
    let now = now_ms();

    if running {
        let up = worker
            .started_at
            .map(|t| format!(", up {}", duration(now - t)))
            .unwrap_or_default();
        let beat = worker
            .heartbeat_at
            .map(|t| format!(", heartbeat {} ago", duration(now - t)))
            .unwrap_or_default();
        println!(
            "Worker:      {} (pid {}){up}{beat}",
            worker.state,
            worker.pid.unwrap_or_default()
        );
        if worker.state != "running"
            && let Some(reason) = &worker.reason
        {
            println!("Why:         {reason}");
        }
    } else {
        let reason = worker.reason.as_deref().unwrap_or("never run");
        println!("Worker:      not running (last stop: {reason})");
    }
    if worker.requested != "run" {
        println!("Requested:   {}", worker.requested);
    }
    if let Some(e) = &worker.last_error {
        println!("Last error:  {e}");
    }
    if let Some((backend, hardware)) = store.last_backend()? {
        println!("Backend:     {backend} on {hardware}");
    }

    let totals = store.totals()?;
    let window = Duration::from_secs((2 * config.compute.checkpoint_interval_seconds).max(60));
    let rate = store.recent_rate(window, worker.started_at)?;
    let per_thread = totals.candidates as f64 / (totals.active_ms as f64 / 1000.0).max(1e-9);
    if running {
        println!(
            "Throughput:  {}/s recently, {}/s per thread on average",
            human(rate),
            human(per_thread)
        );
    } else if totals.candidates > 0 {
        println!("Throughput:  {}/s per thread on average", human(per_thread));
    }
    println!(
        "Searched:    {} candidates, {} active thread time",
        human(totals.candidates as f64),
        duration(totals.active_ms)
    );
    if let Some(t) = totals.last_checkpoint_at {
        println!("Checkpoint:  {} ago", duration(now - t));
    }

    let coverage = store.coverage()?;
    for family in [Family::Historical, Family::Frontier] {
        let spaces: Vec<_> = coverage.iter().filter(|c| c.family == family).collect();
        if spaces.is_empty() {
            continue;
        }
        let total: BigUint = spaces.iter().map(|c| c.total.clone()).sum();
        let covered: BigUint = spaces.iter().map(|c| c.covered.clone()).sum();
        let remaining = to_f64(&(&total - &covered));
        let eta = if running && rate > 0.0 && remaining > 0.0 {
            format!(", ETA {}", duration((remaining / rate * 1000.0) as i64))
        } else {
            String::new()
        };
        let label = match family {
            Family::Historical => "Historical:",
            Family::Frontier => "Frontier:  ",
        };
        println!(
            "{label} {:.4}% ({} of {} in {} spaces){eta}",
            percent(&covered, &total),
            human(to_f64(&covered)),
            human(to_f64(&total)),
            spaces.len()
        );
    }

    let chunks = store.running_chunks()?;
    if running && !chunks.is_empty() {
        println!("Active chunks (committed position; up to checkpoint_interval_seconds behind):");
        for c in chunks {
            let start = c.id.rsplit(':').next().and_then(|r| r.split('-').next()).unwrap_or("0");
            let start = BigUint::parse_bytes(start.as_bytes(), 10).unwrap_or_default();
            let done = percent(&(&c.next_index - &start), &(&c.end_index - &start));
            println!("  {} at {} ({done:.1}%)", c.id, c.next_index);
        }
    }

    let found = store.discoveries()?;
    let nontrivial = found.iter().filter(|d| d.nontrivial).count();
    println!(
        "Discoveries: {nontrivial} nontrivial, {} trivial",
        found.len() - nontrivial
    );
    Ok(ExitCode::SUCCESS)
}

fn coverage(store: &Store, all: bool) -> Result<ExitCode> {
    println!(
        "{:<11} {:>5} {:>4} {:>22} {:>22} {:>9} {:>9}",
        "family", "hex", "lead", "covered", "total", "percent", "intervals"
    );
    for c in store.coverage()? {
        if !all && c.covered == BigUint::ZERO {
            continue;
        }
        println!(
            "{:<11} {:>5} {:>4X} {:>22} {:>22} {:>8.3}% {:>9}",
            c.family.as_str(),
            c.space.hex_digits(),
            c.space.leading(),
            c.covered,
            c.total,
            percent(&c.covered, &c.total),
            c.intervals.len()
        );
    }
    println!();
    println!("Searched by backend:");
    for (backend, candidates) in store.coverage_by_backend()? {
        let note = if backend.contains("cuda") {
            "GPU exact filter; survivors verified on the CPU"
        } else {
            "exact CPU reference"
        };
        println!("  {backend:<32} {candidates:>22}  ({note})");
    }
    let (audits, audited) = store.audit_summary()?;
    println!("GPU audits:  {audits} slices, {audited} candidates recomputed on the CPU, all checksums matched");
    Ok(ExitCode::SUCCESS)
}

fn discoveries(store: &Store) -> Result<ExitCode> {
    let found = store.discoveries()?;
    if found.is_empty() {
        println!("no discoveries recorded");
    }
    for d in found {
        let kind = if d.nontrivial { "NONTRIVIAL" } else { "trivial" };
        println!("{kind} [{}] {}", d.verification, d.decimal);
        println!("    hex 0x{}  binary {}", d.hexadecimal, d.binary);
        println!("    chunk {} index {}", d.chunk_id, d.candidate_index);
    }
    Ok(ExitCode::SUCCESS)
}

fn verify(store: &Store) -> Result<ExitCode> {
    let problems = store.verify_integrity()?;
    if problems.is_empty() {
        println!(
            "database consistent: chunks tile every allocation without gaps or overlaps, and all discoveries reverify"
        );
        Ok(ExitCode::SUCCESS)
    } else {
        for p in &problems {
            println!("PROBLEM: {p}");
        }
        Ok(ExitCode::FAILURE)
    }
}

fn benchmark(seconds: u64, threads: usize) -> Result<ExitCode> {
    // One historical and both first frontier families, starting mid-space.
    let cases = [
        (32u32, 0x9u8, "historical 9-family"),
        (65, 0x1, "frontier A"),
        (33, 0x5, "frontier B"),
    ];
    let per_case = Duration::from_secs(seconds.max(cases.len() as u64)) / cases.len() as u32;
    match CudaBackend::new(0) {
        Ok(gpu) => benchmark_gpu(gpu, per_case)?,
        Err(e) => println!("GPU: unavailable ({e})"),
    }
    println!(
        "{CPU_BACKEND} on {}, {threads} threads, {:.1}s per case",
        cpu_hardware(),
        per_case.as_secs_f64()
    );
    for (n, lead, label) in cases {
        let space = SearchSpace::new(n, lead)?;
        let count = space.candidate_count();
        let middle = &count / 2u32;
        let stride = &middle / threads as u64;
        let checked = AtomicU64::new(0);
        let deadline = Instant::now() + per_case;
        std::thread::scope(|scope| {
            for t in 0..threads as u64 {
                let (middle, stride, count, checked) = (&middle, &stride, &count, &checked);
                scope.spawn(move || {
                    let mut start = middle + stride * t;
                    while Instant::now() < deadline {
                        let end = (&start + 16_384u32).min(count.clone());
                        if start >= end {
                            break;
                        }
                        let result = search_range(space, &start, &end).expect("benchmark range is valid");
                        checked.fetch_add(result.candidates_checked, Ordering::Relaxed);
                        start = end;
                    }
                });
            }
        });
        let rate = checked.load(Ordering::Relaxed) as f64 / per_case.as_secs_f64();
        println!(
            "  {label:<20} {:>3} hex digits ({:>3} bits): {:>8}/s total, {:>8}/s per thread",
            n,
            space.candidate(&middle)?.bits(),
            human(rate),
            human(rate / threads as f64)
        );
    }
    Ok(ExitCode::SUCCESS)
}

/// GPU filter throughput, including survivor transfer but not CPU verification.
fn benchmark_gpu(mut gpu: CudaBackend, per_case: Duration) -> Result<()> {
    let cases = [
        (32u32, 0x9u8, "historical 9-family"),
        (64, 0x1, "historical 1-family"),
        (65, 0x1, "frontier A"),
        (33, 0x5, "frontier B"),
        (128, 0x3, "512-bit limit"),
    ];
    println!(
        "{} on gpu:{}, {:.1}s per case",
        trindrome_cuda::IMPLEMENTATION,
        gpu.device_name(),
        per_case.as_secs_f64()
    );
    println!("  wall-clock throughput with two batches in flight; kernel: generic | specialized");
    for (n, lead, label) in cases {
        let space = SearchSpace::new(n, lead)?;
        let generic = Prepared::new(space, PRODUCTION_WINDOW)?;
        let specialized = Prepared::specialized(space, PRODUCTION_WINDOW)?;
        let mut rates = Vec::new();
        let mut survivors = 0;
        for prepared in [&generic, &specialized] {
            let (rate, found) = gpu_throughput(&mut gpu, prepared, per_case / 2)?;
            rates.push(rate);
            survivors += found;
        }
        println!(
            "  {label:<20} {n:>3} hex digits ({:>2} limbs): {:>8}/s | {:>8}/s  ({:+.0}%), {survivors} survivors",
            generic.limbs,
            human(rates[0]),
            human(rates[1]),
            100.0 * (rates[1] / rates[0] - 1.0)
        );
    }
    Ok(())
}

/// Candidates per wall-clock second, keeping two batches in flight as the
/// worker does, and the number of filter survivors seen.
fn gpu_throughput(gpu: &mut CudaBackend, prepared: &Prepared, duration: Duration) -> Result<(f64, usize)> {
    let total = prepared.space.candidate_count().to_u64().unwrap_or(u64::MAX);
    let batch = (1u64 << 30).min(total / 4);
    let mut start = total / 4;
    let next = |start: &mut u64| {
        if *start + batch > total {
            *start = 0;
        }
        let s = *start;
        *start += batch;
        s
    };
    let (mut done, mut survivors) = (0u64, 0usize);
    let began = Instant::now();
    let mut pending = gpu.launch(prepared, next(&mut start), batch)?;
    while began.elapsed() < duration {
        let following = gpu.launch(prepared, next(&mut start), batch)?;
        let r = gpu.collect(std::mem::replace(&mut pending, following))?;
        if r.processed != batch {
            bail!("GPU processed {} of {batch} candidates", r.processed);
        }
        done += batch;
        survivors += r.survivors.len();
    }
    let r = gpu.collect(pending)?;
    done += r.processed;
    survivors += r.survivors.len();
    Ok((done as f64 / began.elapsed().as_secs_f64(), survivors))
}

fn to_f64(v: &BigUint) -> f64 {
    v.to_f64().unwrap_or(f64::INFINITY)
}

fn percent(part: &BigUint, whole: &BigUint) -> f64 {
    if *whole == BigUint::ZERO {
        0.0
    } else {
        100.0 * to_f64(part) / to_f64(whole)
    }
}

fn human(v: f64) -> String {
    let (scaled, suffix) = match v {
        v if v >= 1e12 => (v / 1e12, "T"),
        v if v >= 1e9 => (v / 1e9, "B"),
        v if v >= 1e6 => (v / 1e6, "M"),
        v if v >= 1e3 => (v / 1e3, "K"),
        v => return format!("{v:.0}"),
    };
    format!("{scaled:.2}{suffix}")
}

fn duration(ms: i64) -> String {
    let s = ms.max(0) / 1000;
    match s {
        s if s >= 86_400 => format!("{}d {}h", s / 86_400, s % 86_400 / 3600),
        s if s >= 3600 => format!("{}h {}m", s / 3600, s % 3600 / 60),
        s if s >= 60 => format!("{}m {}s", s / 60, s % 60),
        s => format!("{s}s"),
    }
}
