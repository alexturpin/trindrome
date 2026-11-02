//! The CPU search against a brute-force triple-palindrome oracle over small values.

use num_bigint::BigUint;
use num_traits::ToPrimitive;
use trindrome_core::{SearchSpace, WorkChunk};
use trindrome_cpu::{search_chunk, search_range};

fn is_palindrome(v: u64, base: u64) -> bool {
    let (mut t, mut r) = (v, 0);
    while t > 0 {
        r = r * base + t % base;
        t /= base;
    }
    r == v
}

#[test]
fn matches_brute_force_triple_palindromes() {
    const MAX_DIGITS: u32 = 6;
    let oracle: Vec<u64> = (1..16u64.pow(MAX_DIGITS))
        .filter(|&v| is_palindrome(v, 16) && is_palindrome(v, 2) && is_palindrome(v, 10))
        .collect();

    let mut found = Vec::new();
    for n in 1..=MAX_DIGITS {
        for space in SearchSpace::all_of_length(n).unwrap() {
            let chunk = WorkChunk::whole(space);
            let result = search_chunk(&chunk).unwrap();
            assert_eq!(BigUint::from(result.candidates_checked), chunk.len());
            for m in result.matches {
                assert_eq!(space.candidate(&m.index).unwrap(), m.verification.value);
                found.push(m.verification.value.to_u64().unwrap());
            }
        }
    }
    found.sort_unstable();
    assert_eq!(found, oracle);
}

#[test]
fn only_trivial_matches_up_to_sixteen_digits() {
    let mut found = Vec::new();
    for n in 1..=16 {
        for space in SearchSpace::all_of_length(n).unwrap() {
            let result = search_chunk(&WorkChunk::whole(space)).unwrap();
            found.extend(result.matches.into_iter().map(|m| m.verification.decimal));
        }
    }
    assert_eq!(found, ["1", "3", "5", "7", "9"]);
}

#[test]
fn split_ranges_find_the_same_matches() {
    let space = SearchSpace::new(1, 0x5).unwrap();
    let whole = search_chunk(&WorkChunk::whole(space)).unwrap();
    assert_eq!(whole.matches.len(), 1);

    let space = SearchSpace::new(11, 0x7).unwrap(); // 1024 candidates
    let whole = search_chunk(&WorkChunk::whole(space)).unwrap();
    let mut pieces = Vec::new();
    for part in WorkChunk::whole(space).split(&BigUint::from(97u32)) {
        pieces.extend(search_range(space, &part.start_index, &part.end_index).unwrap().matches);
    }
    assert_eq!(pieces, whole.matches);
}
