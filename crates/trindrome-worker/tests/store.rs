use std::sync::atomic::AtomicBool;
use std::time::Duration;

use num_bigint::BigUint;
use trindrome_core::{SearchSpace, Verification};
use trindrome_cpu::{Match, search_range};
use trindrome_worker::configuration::{Config, Search, SearchMode};
use trindrome_worker::engine::Engine;
use trindrome_worker::persistence::{Checkpoint, Claim, Family, Store};
use trindrome_worker::scheduler::{self, RunOptions};

fn big(v: u64) -> BigUint {
    v.into()
}

fn search(mode: SearchMode, min: u32, max: Option<u32>) -> Search {
    Search {
        mode,
        minimum_hex_digits: min,
        maximum_hex_digits: max,
        stop_on_first_verified_discovery: true,
    }
}

fn open(dir: &tempfile::TempDir) -> Store {
    Store::open(&dir.path().join("search.sqlite")).unwrap()
}

/// Searches a claim to completion in one checkpoint, as the worker would.
fn finish(store: &mut Store, claim: &Claim) -> bool {
    let space = claim.chunk.validate().unwrap();
    let result = search_range(space, &claim.next_index, &claim.chunk.end_index).unwrap();
    store
        .checkpoint(&Checkpoint {
            chunk_id: &claim.chunk.id(),
            from: &claim.next_index,
            to: &claim.chunk.end_index,
            matches: &result.matches,
            audits: &[],
            candidates: result.candidates_checked,
            active: result.elapsed,
            hardware: "test",
            backend: "test",
            survivors: None,
        })
        .unwrap()
}

#[test]
fn plan_registers_exactly_the_historical_spaces() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let plan = search(SearchMode::HistoricalReplay, 2, None);
    assert_eq!(store.plan(&plan).unwrap(), 63 * 2 + 31 * 4);
    assert_eq!(store.plan(&plan).unwrap(), 0, "planning is idempotent");

    let spaces = store.spaces().unwrap();
    assert!(spaces.iter().all(|s| s.family == Family::Historical));
    let total: BigUint = spaces.iter().map(|s| s.total.clone()).sum();
    assert_eq!(total, big(28_633_115_300 - 6)); // minus the six one-digit candidates
}

#[test]
fn frontier_opens_lengths_lazily_and_cheapest_first() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let plan = search(SearchMode::Frontier, 2, None);
    store.plan(&plan).unwrap();
    let mut lengths: Vec<(u32, u8)> = store
        .spaces()
        .unwrap()
        .iter()
        .map(|s| (s.space.hex_digits(), s.space.leading()))
        .collect();
    lengths.sort();
    assert_eq!(
        lengths,
        [(33, 0x5), (33, 0x7), (33, 0x9), (33, 0xF), (65, 0x1), (65, 0x3)]
    );

    // Both first frontier spaces hold 2^32 candidates; the shorter length goes first.
    let claim = store.claim(&plan, 1 << 20).unwrap().unwrap();
    assert_eq!(claim.chunk.id(), "trindrome:g1:h33:5:0-1048576");
}

#[test]
fn claims_tile_spaces_and_complete_the_plan() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let plan = search(SearchMode::HistoricalReplay, 1, Some(7));
    store.plan(&plan).unwrap();

    let mut claims = 0;
    while let Some(claim) = store.claim(&plan, 7).unwrap() {
        assert!(finish(&mut store, &claim));
        claims += 1;
    }
    assert!(claims > 20);
    assert!(store.verify_integrity().unwrap().is_empty());
    for c in store.coverage().unwrap() {
        assert_eq!(c.covered, c.total, "{:?}", c.space);
        assert_eq!(c.intervals, vec![(big(0), c.total.clone())]);
    }
    let decimals: Vec<String> = store.discoveries().unwrap().into_iter().map(|d| d.decimal).collect();
    assert_eq!(decimals.len(), 5);
    for d in ["1", "3", "5", "7", "9"] {
        assert!(decimals.iter().any(|x| x == d));
    }
}

#[test]
fn interrupted_chunks_resume_from_their_committed_cursor() {
    let dir = tempfile::tempdir().unwrap();
    let plan = search(SearchMode::HistoricalReplay, 20, Some(20));
    let (id, half) = {
        let mut store = open(&dir);
        store.plan(&plan).unwrap();
        let claim = store.claim(&plan, 1000).unwrap().unwrap();
        let half = &claim.next_index + 500u32;
        let space = claim.chunk.validate().unwrap();
        let r = search_range(space, &claim.next_index, &half).unwrap();
        store
            .checkpoint(&Checkpoint {
                chunk_id: &claim.chunk.id(),
                from: &claim.next_index,
                to: &half,
                matches: &r.matches,
                audits: &[],
                candidates: r.candidates_checked,
                active: r.elapsed,
                hardware: "test",
                backend: "test",
                survivors: None,
            })
            .unwrap();
        (claim.chunk.id(), half)
        // Dropped while still `running`, as after a crash.
    };

    let mut store = open(&dir);
    assert_eq!(store.recover_interrupted().unwrap(), 1);
    let claim = store.claim(&plan, 1000).unwrap().unwrap();
    assert_eq!(claim.chunk.id(), id);
    assert_eq!(claim.next_index, half);
    assert!(finish(&mut store, &claim));
    assert!(store.verify_integrity().unwrap().is_empty());
}

#[test]
fn checkpoints_cannot_overstate_progress() {
    let dir = tempfile::tempdir().unwrap();
    let mut store = open(&dir);
    let plan = search(SearchMode::HistoricalReplay, 9, Some(9));
    store.plan(&plan).unwrap();
    let claim = store.claim(&plan, 100).unwrap().unwrap();
    let id = claim.chunk.id();
    let attempt = |store: &mut Store, from: u64, to: u64, candidates: u64, matches: &[Match]| {
        store.checkpoint(&Checkpoint {
            chunk_id: &id,
            from: &big(from),
            to: &big(to),
            matches,
            audits: &[],
            candidates,
            active: Duration::ZERO,
            hardware: "test",
            backend: "test",
            survivors: None,
        })
    };
    assert!(attempt(&mut store, 0, 101, 101, &[]).is_err(), "beyond the chunk");
    assert!(
        attempt(&mut store, 5, 10, 5, &[]).is_err(),
        "skipping ahead of the cursor"
    );
    assert!(
        attempt(&mut store, 0, 10, 9, &[]).is_err(),
        "count disagrees with range"
    );
    assert!(attempt(&mut store, 0, 0, 0, &[]).is_err(), "empty progress");

    // A fabricated match that is not a decimal palindrome must not be recorded.
    let space = SearchSpace::new(9, 0x1).unwrap();
    let fake = Match {
        index: big(3),
        verification: Verification::of(&space.candidate(&big(3)).unwrap()),
    };
    assert!(attempt(&mut store, 0, 10, 10, &[fake]).is_err());

    attempt(&mut store, 0, 10, 10, &[]).unwrap();
    store.release(&id).unwrap();
    assert!(
        attempt(&mut store, 10, 20, 10, &[]).is_err(),
        "released chunks accept no progress"
    );
    assert!(store.verify_integrity().unwrap().is_empty());
}

#[test]
fn worker_runs_a_small_plan_to_completion() {
    let dir = tempfile::tempdir().unwrap();
    let mut config = Config::default();
    config.storage.database_path = dir.path().join("search.sqlite");
    config.search = search(SearchMode::Both, 1, Some(14));
    config.compute.chunk_candidates = 5000;
    config.compute.batch_target_ms = 5;
    config.idle.require_ac_power = false;

    let reason = scheduler::run(
        &config,
        (0..3).map(|_| Engine::cpu()).collect(),
        &RunOptions {
            immediate: true,
            ..Default::default()
        },
        &AtomicBool::new(false),
    )
    .unwrap();
    assert_eq!(reason, "search plan exhausted");

    let store = Store::open(&config.storage.database_path).unwrap();
    assert!(store.verify_integrity().unwrap().is_empty());
    let coverage = store.coverage().unwrap();
    assert!(
        coverage
            .iter()
            .all(|c| c.family == Family::Historical && c.covered == c.total)
    );
    assert_eq!(store.discoveries().unwrap().len(), 5);
    let totals = store.totals().unwrap();
    let planned: BigUint = coverage.iter().map(|c| c.total.clone()).sum();
    assert_eq!(
        BigUint::from(totals.candidates),
        planned,
        "nothing searched twice without a crash"
    );
}

#[test]
fn only_one_worker_per_database() {
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("search.sqlite");
    let held = scheduler::acquire_lock(&db).unwrap();
    assert!(scheduler::worker_is_running(&db));
    assert!(scheduler::acquire_lock(&db).is_err());
    drop(held);
    assert!(!scheduler::worker_is_running(&db));
    scheduler::acquire_lock(&db).unwrap();
}
