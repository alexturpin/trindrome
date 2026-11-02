//! Exact CPU reference backend.
//!
//! This is the authoritative implementation that faster backends are tested
//! against, so it favours obviously-correct code over speed: every candidate is
//! built as an arbitrary-precision integer and fully converted to decimal.

use std::time::{Duration, Instant};

use num_bigint::BigUint;
use trindrome_core::palindrome::is_decimal_palindrome;
use trindrome_core::{CoreError, SearchSpace, Verification, WorkChunk};

pub const IMPLEMENTATION: &str = concat!("cpu-reference/", env!("CARGO_PKG_VERSION"));

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Match {
    pub index: BigUint,
    pub verification: Verification,
}

#[derive(Clone, Debug)]
pub struct RangeResult {
    pub space: SearchSpace,
    pub start_index: BigUint,
    pub end_index: BigUint,
    /// Candidates actually examined; equals `end_index - start_index` on success.
    pub candidates_checked: u64,
    /// Every decimal palindrome found, including trivial single-digit ones.
    pub matches: Vec<Match>,
    pub elapsed: Duration,
}

/// Searches `[start, end)` of `space` for candidates that are also decimal palindromes.
///
/// Fails rather than truncating if the range is invalid or longer than `u64::MAX`.
pub fn search_range(space: SearchSpace, start: &BigUint, end: &BigUint) -> Result<RangeResult, CoreError> {
    let began = Instant::now();
    let candidates = space.candidates(start, end)?;
    let mut checked = 0u64;
    let mut matches = Vec::new();

    for (offset, value) in candidates.enumerate() {
        checked += 1;
        if !is_decimal_palindrome(&value) {
            continue;
        }
        let verification = Verification::of(&value);
        assert!(
            verification.is_triple_palindrome(),
            "generator invariant violated: {space:?} produced 0x{} which is not a binary/hex palindrome",
            verification.hexadecimal
        );
        matches.push(Match {
            index: start + offset as u64,
            verification,
        });
    }

    Ok(RangeResult {
        space,
        start_index: start.clone(),
        end_index: end.clone(),
        candidates_checked: checked,
        matches,
        elapsed: began.elapsed(),
    })
}

pub fn search_chunk(chunk: &WorkChunk) -> Result<RangeResult, CoreError> {
    let space = chunk.validate()?;
    search_range(space, &chunk.start_index, &chunk.end_index)
}
