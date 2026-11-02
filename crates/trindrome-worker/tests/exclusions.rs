//! Game-process exclusion and live configuration reload, end to end: the real
//! `trindrome` executable, the real Windows process list, and a stand-in "game"
//! (a renamed copy of ping.exe) started and stopped while the worker runs.

#![cfg(windows)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use trindrome_worker::persistence::Store;

fn write_config(path: &Path, exclusions: &[String]) {
    let list: Vec<String> = exclusions.iter().map(|e| format!("'{e}'")).collect();
    std::fs::write(
        path,
        format!(
            r#"
            [compute]
            backend = "cpu"
            cpu_threads = 2
            chunk_candidates = 200000
            batch_target_ms = 20
            checkpoint_interval_seconds = 0

            [idle]
            require_ac_power = false
            exclusive_gpu_apps = [{}]

            [search]
            mode = "historical_replay"
            maximum_hex_digits = 34

            [storage]
            database_path = "search.sqlite"

            [logging]
            level = "warn"
            "#,
            list.join(", ")
        ),
    )
    .unwrap();
}

fn wait_for(what: &str, timeout: Duration, mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + timeout;
    while !condition() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn state(db: &Path) -> (String, Option<String>, Option<String>) {
    let w = Store::open(db).unwrap().worker().unwrap();
    (w.state, w.reason_code, w.detail)
}

fn searched(db: &Path) -> u64 {
    Store::open(db).map(|s| s.totals().unwrap().candidates).unwrap_or(0)
}

/// A long-running process with a distinctive name.
fn start_game(exe: &Path) -> Child {
    Command::new(exe)
        .args(["-n", "120", "127.0.0.1"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap()
}

fn expect_paused_by_game_then_resume(db: &Path, exe: &Path) {
    let mut game = start_game(exe);
    wait_for("pause for the excluded app", Duration::from_secs(15), || {
        let (s, code, _) = state(db);
        s == "waiting" && code.as_deref() == Some("excluded_app")
    });
    let (_, _, detail) = state(db);
    assert!(detail.unwrap().contains("trindrome-fake-game.exe"));
    wait_for("chunks released", Duration::from_secs(10), || {
        Store::open(db).unwrap().running_chunks().unwrap().is_empty()
    });
    let paused_at = searched(db);
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(searched(db), paused_at, "no progress while the game runs");

    game.kill().unwrap();
    game.wait().unwrap();
    wait_for("resume after the game exits", Duration::from_secs(15), || {
        state(db).0 == "running" && searched(db) > paused_at
    });
}

#[test]
fn excluded_games_pause_the_worker_and_config_changes_apply_live() {
    let dir = tempfile::tempdir().unwrap();
    let config = dir.path().join("trindrome.toml");
    let db = dir.path().join("search.sqlite");
    let exe: PathBuf = dir.path().join("trindrome-fake-game.exe");
    std::fs::copy(r"C:\Windows\System32\PING.EXE", &exe).unwrap();
    write_config(&config, &[]);

    let mut worker = Command::new(env!("CARGO_BIN_EXE_trindrome"))
        .arg("--config")
        .arg(&config)
        .args(["run", "--now", "--cpu"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    wait_for("worker running", Duration::from_secs(30), || {
        state(&db).0 == "running" && searched(&db) > 0
    });

    // Added while the worker runs: by file name, any case.
    write_config(&config, &["TRINDROME-FAKE-GAME.EXE".into()]);
    expect_paused_by_game_then_resume(&db, &exe);

    // By full path, with forward slashes.
    let full = std::fs::canonicalize(&exe)
        .unwrap()
        .to_string_lossy()
        .replace(r"\\?\", "")
        .replace('\\', "/");
    write_config(&config, &[full]);
    expect_paused_by_game_then_resume(&db, &exe);

    // A different path with the same file name must not match.
    write_config(&config, &[r"C:\Elsewhere\trindrome-fake-game.exe".into()]);
    std::thread::sleep(Duration::from_secs(3));
    let mut game = start_game(&exe);
    std::thread::sleep(Duration::from_secs(4));
    assert_eq!(state(&db).0, "running", "path pattern must not match another location");
    game.kill().unwrap();
    let _ = game.wait();

    Command::new(env!("CARGO_BIN_EXE_trindrome"))
        .arg("--config")
        .arg(&config)
        .arg("stop")
        .output()
        .unwrap();
    wait_for("worker exit", Duration::from_secs(30), || {
        worker.try_wait().unwrap().is_some()
    });
    assert!(Store::open(&db).unwrap().verify_integrity().unwrap().is_empty());
}
