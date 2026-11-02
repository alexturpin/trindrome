//! End-to-end tests against the real `trindrome` executable: forced termination,
//! recovery, single-instance enforcement and pause/resume/stop control.

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::time::{Duration, Instant};

use num_bigint::BigUint;
use trindrome_worker::persistence::Store;

fn trindrome(config: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_trindrome"));
    cmd.arg("--config").arg(config).args(args);
    cmd
}

fn run_ok(config: &Path, args: &[&str]) -> Output {
    let out = trindrome(config, args).output().unwrap();
    assert!(
        out.status.success(),
        "trindrome {args:?} failed:\n{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    out
}

fn spawn_worker(config: &Path) -> Child {
    trindrome(config, &["run", "--now", "--cpu"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn setup(max_hex_digits: u32) -> (tempfile::TempDir, PathBuf, PathBuf) {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("trindrome.toml");
    std::fs::write(
        &config,
        format!(
            r#"
            [compute]
            backend = "cpu"
            cpu_threads = 2
            chunk_candidates = 200000
            batch_target_ms = 20
            checkpoint_interval_seconds = 0

            [search]
            mode = "historical_replay"
            minimum_hex_digits = 2
            maximum_hex_digits = {max_hex_digits}

            [storage]
            database_path = "db/search.sqlite"

            [logging]
            level = "warn"
            "#
        ),
    )
    .unwrap();
    let db = dir.path().join("db/search.sqlite");
    (dir, config, db)
}

fn wait_for(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

fn searched(db: &Path) -> u64 {
    Store::open(db).map(|s| s.totals().unwrap().candidates).unwrap_or(0)
}

fn assert_complete(db: &Path) {
    let store = Store::open(db).unwrap();
    assert_eq!(store.verify_integrity().unwrap(), Vec::<String>::new());
    for c in store.coverage().unwrap() {
        assert_eq!(c.covered, c.total, "{:?} incomplete", c.space);
    }
}

#[test]
fn killed_worker_resumes_without_gaps() {
    let (_dir, config, db) = setup(22);
    // A kill can land in the instant between finishing one chunk and claiming the
    // next; retry until one strands a chunk mid-search so recovery is exercised.
    let mut stranded = false;
    for _ in 0..5 {
        let before = searched(&db);
        let mut child = spawn_worker(&config);
        wait_for("new checkpoint", Duration::from_secs(60), || searched(&db) > before);
        child.kill().unwrap();
        child.wait().unwrap();
        if !Store::open(&db).unwrap().running_chunks().unwrap().is_empty() {
            stranded = true;
            break;
        }
    }
    assert!(stranded, "no kill left a chunk marked running");

    let store = Store::open(&db).unwrap();
    assert!(
        store.verify_integrity().unwrap().is_empty(),
        "consistent even after a forced kill"
    );
    let planned: BigUint = store.coverage().unwrap().iter().map(|c| c.total.clone()).sum();
    let covered: BigUint = store.coverage().unwrap().iter().map(|c| c.covered.clone()).sum();
    assert!(
        covered < planned,
        "the kill should land mid-search; raise maximum_hex_digits if this fails"
    );
    drop(store);

    let out = run_ok(&config, &["run", "--now", "--cpu"]);
    assert!(String::from_utf8_lossy(&out.stdout).contains("search plan exhausted"));
    assert_complete(&db);
    let verify = run_ok(&config, &["verify"]);
    assert!(String::from_utf8_lossy(&verify.stdout).contains("database consistent"));
}

#[test]
fn pause_resume_and_stop_control_a_running_worker() {
    let (_dir, config, db) = setup(30);
    let mut child = spawn_worker(&config);
    wait_for("first checkpoint", Duration::from_secs(60), || searched(&db) > 0);

    let second = trindrome(&config, &["run", "--now", "--cpu"]).output().unwrap();
    assert!(!second.status.success());
    assert!(String::from_utf8_lossy(&second.stderr).contains("already running"));

    // Let small chunks give way to large ones first: batches cut short at chunk
    // boundaries must not inflate the batch size and with it the pause latency.
    std::thread::sleep(Duration::from_secs(2));
    run_ok(&config, &["pause"]);
    let worker_state = || Store::open(&db).unwrap().worker().unwrap().state;
    wait_for("paused state", Duration::from_secs(10), || worker_state() == "paused");
    wait_for("chunks released promptly", Duration::from_secs(3), || {
        Store::open(&db).unwrap().running_chunks().unwrap().is_empty()
    });
    let at_pause = searched(&db);
    std::thread::sleep(Duration::from_millis(600));
    assert_eq!(searched(&db), at_pause, "no progress while paused");
    let status = run_ok(&config, &["status"]);
    assert!(String::from_utf8_lossy(&status.stdout).contains("paused"));

    run_ok(&config, &["resume"]);
    wait_for("progress after resume", Duration::from_secs(30), || {
        searched(&db) > at_pause
    });

    run_ok(&config, &["stop"]);
    wait_for("worker exit", Duration::from_secs(30), || {
        child.try_wait().unwrap().is_some()
    });
    assert!(child.wait().unwrap().success());
    let store = Store::open(&db).unwrap();
    assert!(store.verify_integrity().unwrap().is_empty());
    assert!(
        store.running_chunks().unwrap().is_empty(),
        "a clean stop releases every chunk"
    );
    assert_eq!(store.worker().unwrap().reason.as_deref(), Some("stop requested"));
}
