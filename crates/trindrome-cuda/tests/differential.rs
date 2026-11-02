//! GPU against the Rust reference filter: identical candidates, survivors and
//! checksums. Tests skip when no CUDA device is present unless
//! `TRINDROME_REQUIRE_GPU` is set.

use num_bigint::BigUint;
use trindrome_core::SearchSpace;
use trindrome_core::filter::{DecimalFilter, PRODUCTION_WINDOW};
use trindrome_cuda::{CudaBackend, CudaError, Prepared};

fn backend(capacity: u32) -> Option<CudaBackend> {
    match CudaBackend::with_capacity(0, capacity) {
        Ok(b) => Some(b),
        Err(e) if std::env::var_os("TRINDROME_REQUIRE_GPU").is_some() => panic!("GPU required: {e}"),
        Err(e) => {
            eprintln!("skipping GPU test: {e}");
            None
        }
    }
}

const COUNT: u64 = 3000;

/// (hex digits, leading digit, start, count) covering every family, all four
/// kernel widths, first and last indices, and indices near 2^64.
fn cases() -> Vec<(u32, u8, u64, u64)> {
    raw_cases()
        .into_iter()
        .map(|(n, l, start)| {
            let total = SearchSpace::new(n, l).unwrap().candidate_count();
            let count = u64::try_from(total - start).map_or(COUNT, |left| left.min(COUNT));
            (n, l, start, count)
        })
        .collect()
}

fn raw_cases() -> Vec<(u32, u8, u64)> {
    let last = |n: u32, l: u8| -> u64 {
        let total = SearchSpace::new(n, l).unwrap().candidate_count();
        u64::try_from(total - COUNT).unwrap_or(u64::MAX - COUNT)
    };
    vec![
        (16, 0x1, 0),
        (16, 0x5, 0),
        (17, 0xF, 777),
        (20, 0x9, 100_000),
        (31, 0x3, last(31, 0x3)),
        (32, 0xF, last(32, 0xF)),
        (33, 0x7, (1 << 32) - COUNT),
        (40, 0x5, 123_456_789),
        (64, 0x1, last(64, 0x1)),
        (65, 0x1, (1 << 32) - COUNT),
        (65, 0x3, 0),
        (66, 0xF, u64::MAX - COUNT),
        (96, 0x9, 1 << 40),
        (97, 0x3, last(97, 0x3)),
        (128, 0x3, (1 << 63) - COUNT),
        (128, 0xF, u64::MAX - COUNT),
    ]
}

#[test]
fn gpu_builds_the_same_candidates() {
    let Some(mut gpu) = backend(16) else { return };
    for (n, l, start, count) in cases() {
        let space = SearchSpace::new(n, l).unwrap();
        let prepared = Prepared::new(space, PRODUCTION_WINDOW).unwrap();
        let ours = gpu.construct(&prepared, start, count as u32).unwrap();
        let end = BigUint::from(start) + count;
        let reference: Vec<BigUint> = space.candidates(&start.into(), &end).unwrap().collect();
        assert_eq!(ours, reference, "({n}, {l:X}) from {start}");
    }
}

#[test]
fn gpu_matches_reference_survivors_and_checksums() {
    let Some(mut gpu) = backend(4096) else { return };
    for (n, l, start, count) in cases() {
        let space = SearchSpace::new(n, l).unwrap();
        for window in [PRODUCTION_WINDOW, 3, 2, 1] {
            let prepared = Prepared::new(space, window).unwrap();
            let filter = DecimalFilter::new(space, window).unwrap();
            let gpu_result = gpu.search(&prepared, start, count).unwrap();
            let (checksum, survivors) = filter.scan(start, count).unwrap();
            let label = format!("({n}, {l:X}) from {start}, window {window}");
            assert_eq!(gpu_result.processed, count, "{label}");
            assert!(!gpu_result.overflowed, "{label}");
            assert_eq!(gpu_result.survivors, survivors, "{label}");
            assert_eq!(gpu_result.checksum, checksum, "{label}");
        }
    }
}

#[test]
fn small_windows_produce_survivors_to_compare() {
    // Guard against the comparison above being vacuous.
    let Some(mut gpu) = backend(4096) else { return };
    let space = SearchSpace::new(33, 0x5).unwrap();
    let prepared = Prepared::new(space, 1).unwrap();
    let result = gpu.search(&prepared, 5_000_000, COUNT).unwrap();
    assert!(
        result.survivors.len() > 100,
        "only {} survivors",
        result.survivors.len()
    );
}

#[test]
fn survivor_overflow_is_detected() {
    let Some(mut gpu) = backend(16) else { return };
    let space = SearchSpace::new(33, 0x5).unwrap();
    let prepared = Prepared::new(space, 1).unwrap();
    let (_, all) = DecimalFilter::new(space, 1).unwrap().scan(0, COUNT).unwrap();
    assert!(all.len() > 16);
    let result = gpu.search(&prepared, 0, COUNT).unwrap();
    assert!(result.overflowed);
    assert_eq!(result.survivors.len(), 16);
    assert!(result.survivors.iter().all(|s| all.contains(s)));
    assert_eq!(result.processed, COUNT, "overflow must not lose coverage accounting");
}

#[test]
fn overlapping_batches_match_sequential_ones() {
    let Some(mut gpu) = backend(4096) else { return };
    let space = SearchSpace::new(33, 0x5).unwrap();
    let prepared = Prepared::new(space, 1).unwrap();
    let (a, b) = (1_000_000u64, 1_003_000u64);
    let first = gpu.launch(&prepared, a, COUNT).unwrap();
    let second = gpu.launch(&prepared, b, COUNT).unwrap();
    assert!(gpu.launch(&prepared, 0, COUNT).is_err(), "only two slots");
    // Collect out of order, too.
    let r2 = gpu.collect(second).unwrap();
    let r1 = gpu.collect(first).unwrap();
    for (r, start) in [(r1, a), (r2, b)] {
        let (checksum, survivors) = DecimalFilter::new(space, 1).unwrap().scan(start, COUNT).unwrap();
        assert_eq!((r.processed, r.checksum, &r.survivors), (COUNT, checksum, &survivors));
    }
    // Slots are free again.
    let again = gpu.search(&prepared, a, COUNT).unwrap();
    assert_eq!(again.processed, COUNT);
}

#[test]
fn invalid_batches_and_spaces_are_rejected() {
    assert!(matches!(
        Prepared::new(SearchSpace::new(15, 0x1).unwrap(), PRODUCTION_WINDOW),
        Err(CudaError::Unsupported(_))
    ));
    assert!(matches!(
        Prepared::new(SearchSpace::new(129, 0x1).unwrap(), PRODUCTION_WINDOW),
        Err(CudaError::Unsupported(_))
    ));
    let Some(mut gpu) = backend(16) else { return };
    let small = Prepared::new(SearchSpace::new(16, 0x1).unwrap(), PRODUCTION_WINDOW).unwrap(); // 128 candidates
    assert!(gpu.search(&small, 0, 0).is_err());
    assert!(gpu.search(&small, 0, 129).is_err());
    assert!(gpu.search(&small, 100, 29).is_err());
    let big = Prepared::new(SearchSpace::new(128, 0xF).unwrap(), PRODUCTION_WINDOW).unwrap();
    assert!(gpu.search(&big, 0, (1 << 32) + 1).is_err());
    assert!(gpu.search(&big, u64::MAX, 1).is_err());
}

#[test]
fn large_batches_account_for_every_candidate() {
    let Some(mut gpu) = backend(4096) else { return };
    for (n, l) in [(32u32, 0x9u8), (65, 0x1), (128, 0x3)] {
        let prepared = Prepared::new(SearchSpace::new(n, l).unwrap(), PRODUCTION_WINDOW).unwrap();
        let count = 1 << 26;
        let result = gpu.search(&prepared, 1 << 20, count).unwrap();
        assert_eq!(result.processed, count);
        assert!(!result.overflowed);
        eprintln!(
            "({n}, {l:X}): {count} candidates in {:?} = {:.2e}/s, {} survivors",
            result.elapsed,
            count as f64 / result.elapsed.as_secs_f64(),
            result.survivors.len()
        );
    }
}
