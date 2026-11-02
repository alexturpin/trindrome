use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use clap::{Parser, Subcommand};
use num_bigint::BigUint;
use num_traits::ToPrimitive;
use trindrome_core::SearchSpace;
use trindrome_core::filter::PRODUCTION_WINDOW;
use trindrome_cpu::search_range;
use trindrome_cuda::{CudaBackend, Prepared};
use trindrome_worker::configuration::{Backend, Config};
use trindrome_worker::engine::{CPU_BACKEND, Engine, cpu_hardware};
use trindrome_worker::persistence::{Family, Requested, Store, now_ms};
use trindrome_worker::scheduler;

#[derive(Parser)]
#[command(
    name = "trindrome",
    version,
    about = "Search for integers palindromic in bases 2, 10 and 16"
)]
struct Cli {
    /// Configuration file (default: ./trindrome.toml if present)
    #[arg(long, global = true)]
    config: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the worker in the foreground
    Run {
        /// Start immediately instead of waiting for the computer to be idle
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
    Status,
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
    /// Measure CPU throughput on representative search spaces (no database)
    Benchmark {
        #[arg(long, default_value_t = 10)]
        seconds: u64,
        #[arg(long)]
        threads: Option<usize>,
    },
    /// Print the effective configuration
    Config,
}

pub fn main() -> Result<ExitCode> {
    let cli = Cli::parse();
    let config = Config::load(cli.config.as_deref())?;
    let level: tracing::Level = config.logging.level.parse().expect("validated");
    tracing_subscriber::fmt()
        .with_max_level(level)
        .with_target(false)
        .init();
    let db = &config.storage.database_path;

    match cli.command {
        Command::Run { now, cpu, threads } => run(&config, now, cpu, threads),
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
        Command::Status => status(&config),
        Command::Coverage { all } => coverage(&Store::open(db)?, all),
        Command::Discoveries => discoveries(&Store::open(db)?),
        Command::Verify => verify(&Store::open(db)?),
        Command::Benchmark { seconds, threads } => {
            benchmark(seconds, threads.unwrap_or(config.compute.effective_cpu_threads()))
        }
        Command::Config => {
            print!("{}", toml::to_string_pretty(&config)?);
            for setting in config.unenforced_settings() {
                println!("# not yet enforced: {setting}");
            }
            Ok(ExitCode::SUCCESS)
        }
    }
}

fn run(config: &Config, now: bool, cpu: bool, threads: Option<usize>) -> Result<ExitCode> {
    if !now && config.idle.enabled {
        bail!(
            "idle detection is not implemented yet, so automatic mode cannot start safely. \
             Use `trindrome run --now` to start immediately."
        );
    }
    let engines = if cpu || config.compute.backend == Backend::Cpu {
        let threads = threads.unwrap_or(config.compute.effective_cpu_threads()).max(1);
        (0..threads).map(|_| Engine::cpu()).collect()
    } else {
        // `auto` and `cuda` both require the GPU: never fall back to the CPU silently.
        match Engine::cuda(&config.compute) {
            Ok(engine) => {
                eprintln!(
                    "note: GPU temperature limits are not enforced yet (Phase 4); the GPU's own thermal \
                     protection still applies"
                );
                vec![engine]
            }
            Err(e) => bail!(
                "the CUDA backend is unavailable ({e}). compute.backend = {:?} never falls back to the CPU \
                 silently; pass --cpu to search on the CPU instead.",
                config.compute.backend
            ),
        }
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

    let reason = scheduler::run(config, engines, &interrupted)?;
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

fn request(config: &Config, requested: Requested, message: &str) -> Result<ExitCode> {
    let db = &config.storage.database_path;
    Store::open(db)?.set_requested(requested)?;
    println!("{message}");
    if !scheduler::worker_is_running(db) && requested != Requested::Run {
        println!("(no worker is running right now; a pause will apply when one starts)");
    }
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
    for (n, lead, label) in cases {
        let space = SearchSpace::new(n, lead)?;
        let prepared = Prepared::new(space, PRODUCTION_WINDOW)?;
        let total = space.candidate_count().to_u64().unwrap_or(u64::MAX);
        let batch = (1u64 << 30).min(total / 4);
        let (mut start, mut done, mut survivors) = (total / 4, 0u64, 0usize);
        let mut busy = Duration::ZERO;
        let began = Instant::now();
        while began.elapsed() < per_case {
            if start + batch > total {
                start = 0;
            }
            let r = gpu.search(&prepared, start, batch)?;
            if r.processed != batch {
                bail!("GPU processed {} of {batch} candidates", r.processed);
            }
            busy += r.elapsed;
            done += batch;
            survivors += r.survivors.len();
            start += batch;
        }
        println!(
            "  {label:<20} {n:>3} hex digits ({:>2}-limb kernel): {:>8}/s, {survivors} survivors in {}",
            prepared.limbs,
            human(done as f64 / busy.as_secs_f64()),
            human(done as f64)
        );
    }
    Ok(())
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
