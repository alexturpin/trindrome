//! Completeness of the generator against an independent brute-force oracle.
//!
//! The oracle never touches the generator or big integers: it tests every integer
//! with at most `MAX_ORACLE_DIGITS` hex digits using plain `u64` digit reversal.

use num_bigint::BigUint;
use num_traits::ToPrimitive;
use trindrome_core::SearchSpace;

/// The spec requires five hex digits; six is cheap and covers one more even length.
const MAX_ORACLE_DIGITS: u32 = 6;

fn reverse_in_base(mut v: u64, base: u64) -> u64 {
    let mut reversed = 0;
    while v > 0 {
        reversed = reversed * base + v % base;
        v /= base;
    }
    reversed
}

/// Digit reversal equals the original only for palindromes (a trailing zero
/// shortens the reversal, so such numbers are rejected correctly).
fn oracle_is_binary_hex_palindrome(v: u64) -> bool {
    reverse_in_base(v, 16) == v && reverse_in_base(v, 2) == v
}

fn generated(hex_digits: u32) -> Vec<u64> {
    let mut values = Vec::new();
    for space in SearchSpace::all_of_length(hex_digits).unwrap() {
        let end = space.candidate_count();
        for value in space.candidates(&BigUint::ZERO, &end).unwrap() {
            values.push(value.to_u64().unwrap());
        }
    }
    values
}

#[test]
fn generator_matches_brute_force_exactly() {
    for n in 1..=MAX_ORACLE_DIGITS {
        let low = if n == 1 { 1 } else { 16u64.pow(n - 1) };
        let high = 16u64.pow(n);
        let oracle: Vec<u64> = (low..high).filter(|&v| oracle_is_binary_hex_palindrome(v)).collect();

        let mut ours = generated(n);
        let produced = ours.len();
        ours.sort_unstable();
        ours.dedup();
        assert_eq!(ours.len(), produced, "length {n}: generator produced duplicates");
        assert_eq!(ours, oracle, "length {n}: generator and brute force disagree");
    }
}

#[test]
fn every_candidate_has_the_requested_length() {
    for n in 1..=MAX_ORACLE_DIGITS {
        for v in generated(n) {
            assert_eq!(format!("{v:X}").len(), n as usize, "0x{v:X}");
        }
    }
}
