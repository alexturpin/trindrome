//! The decimal filter must equal its specification exactly and never reject a
//! decimal palindrome.

use num_bigint::BigUint;
use trindrome_core::filter::{DecimalFilter, PRODUCTION_WINDOW, decimal_digits, evaluate};
use trindrome_core::{LEADING_DIGITS, SearchSpace};

/// Small deterministic generator so the tests need no extra dependency.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: u64) -> u64 {
        self.next() % n
    }

    fn digit_string(&mut self, len: usize) -> String {
        (0..len)
            .map(|i| {
                let lo = if i == 0 { 1 } else { 0 };
                char::from(b'0' + (lo + self.below(10 - lo)) as u8)
            })
            .collect()
    }
}

fn dec(s: &str) -> BigUint {
    BigUint::parse_bytes(s.as_bytes(), 10).unwrap()
}

fn p_for(digits: u32, window: u32) -> BigUint {
    BigUint::from(10u32).pow(digits - window)
}

/// The specification: top `window` digits equal the low `window` digits reversed.
fn spec(s: &str, window: usize) -> bool {
    let top = &s[..window];
    let low_reversed: String = s[s.len() - window..].chars().rev().collect();
    top == low_reversed
}

fn check(s: &str, window: u32) -> bool {
    let v = dec(s);
    let digits = s.len() as u32;
    let e = evaluate(&v, digits, window, &p_for(digits, window));
    assert_eq!(e.pass, spec(s, window as usize), "{s} window {window}");
    assert_eq!(e.low as u64, (&v % 10u64.pow(window)).try_into().unwrap());
    e.pass
}

#[test]
fn every_decimal_palindrome_passes() {
    let mut rng = Rng(0x5EED);
    // 154 digits is the largest decimal length of a 512-bit value.
    for len in 18..=154usize {
        for _ in 0..40 {
            let half = rng.digit_string(len.div_ceil(2));
            let tail: String = half.chars().rev().skip(len % 2).collect();
            let palindrome = format!("{half}{tail}");
            assert_eq!(palindrome.len(), len);
            for window in 1..=PRODUCTION_WINDOW {
                assert!(check(&palindrome, window), "rejected palindrome {palindrome}");
            }
        }
    }
}

#[test]
fn edge_case_palindromes_pass() {
    let cases = [
        format!("1{}1", "0".repeat(16)),
        format!("1{}1", "0".repeat(150)),
        "9".repeat(18),
        "9".repeat(154),
        format!("100000000{}000000001", "7".repeat(5)),
        format!("900000001{}100000009", "0".repeat(40)),
        "123456789987654321".to_string(),
        "1234567890987654321".to_string(),
    ];
    for s in &cases {
        for window in 1..=PRODUCTION_WINDOW {
            assert!(check(s, window), "{s}");
        }
    }
}

#[test]
fn pass_equals_the_specification_on_random_and_near_miss_values() {
    let mut rng = Rng(0xC0FFEE);
    let mut passes = 0;
    for _ in 0..20_000 {
        let len = 18 + rng.below(137) as usize;
        let window = 1 + rng.below(PRODUCTION_WINDOW as u64) as u32;
        let s = match rng.below(3) {
            0 => rng.digit_string(len),
            // A palindrome with one digit changed, inside or outside the window.
            _ => {
                let half = rng.digit_string(len.div_ceil(2));
                let tail: String = half.chars().rev().skip(len % 2).collect();
                let mut bytes = format!("{half}{tail}").into_bytes();
                let at = rng.below(len as u64) as usize;
                let lo = if at == 0 { b'1' } else { b'0' };
                bytes[at] = lo + (bytes[at] - lo + 1 + rng.below(8) as u8) % (b'9' - lo + 1);
                String::from_utf8(bytes).unwrap()
            }
        };
        passes += check(&s, window) as u32;
    }
    assert!(
        passes > 1000,
        "near misses should exercise the passing branch ({passes})"
    );
}

#[test]
fn space_filters_know_the_exact_digit_count() {
    for n in 14..=60u32 {
        for leading in LEADING_DIGITS {
            let space = SearchSpace::new(n, leading).unwrap();
            let Some(filter) = DecimalFilter::new(space, PRODUCTION_WINDOW) else {
                let first = space.candidate(&BigUint::ZERO).unwrap();
                assert!(decimal_digits(&first) < 18, "({n}, {leading:X}) wrongly unsupported");
                continue;
            };
            let count = space.candidate_count();
            let mut probes = vec![BigUint::ZERO, count.clone() - 1u32];
            if let Some(t) = &filter.threshold {
                // Candidates increase with index: binary search the first one at or above T.
                let (mut lo, mut hi) = (BigUint::ZERO, count.clone());
                while lo < hi {
                    let mid = (&lo + &hi) / 2u32;
                    if space.candidate(&mid).unwrap() >= *t {
                        hi = mid
                    } else {
                        lo = mid + 1u32
                    }
                }
                for d in 0..3u32 {
                    probes.push(lo.clone() + d);
                    if lo >= BigUint::from(d + 1) {
                        probes.push(lo.clone() - (d + 1));
                    }
                }
            }
            for i in probes.into_iter().filter(|i| *i < count) {
                let v = space.candidate(&i).unwrap();
                assert_eq!(filter.digits_of(&v), decimal_digits(&v), "({n}, {leading:X}) index {i}");
            }
        }
    }
    assert!(DecimalFilter::new(SearchSpace::new(15, 0x1).unwrap(), PRODUCTION_WINDOW).is_none());
    for leading in LEADING_DIGITS {
        assert!(DecimalFilter::new(SearchSpace::new(16, leading).unwrap(), PRODUCTION_WINDOW).is_some());
    }
}

#[test]
fn every_candidate_of_small_spaces_matches_the_specification() {
    for (n, leading) in [(16, 0x1), (17, 0x3), (16, 0x5), (17, 0xF), (18, 0x9)] {
        let space = SearchSpace::new(n, leading).unwrap();
        for window in [1, 2, PRODUCTION_WINDOW] {
            let filter = DecimalFilter::new(space, window).unwrap();
            let end = space.candidate_count();
            for v in space.candidates(&BigUint::ZERO, &end).unwrap() {
                let s = v.to_str_radix(10);
                assert_eq!(
                    filter.evaluate(&v).pass,
                    spec(&s, window as usize),
                    "{s} window {window}"
                );
            }
        }
    }
}

#[test]
fn checksums_are_additive_over_range_splits() {
    // (33, 5) candidates start with decimal 1; every candidate is odd, so spaces
    // whose leading decimal digit is even can have no survivors at all.
    let space = SearchSpace::new(33, 0x5).unwrap();
    let filter = DecimalFilter::new(space, 1).unwrap();
    let start = 1_000_000_007u64;
    let (whole, survivors) = filter.scan(start, 3000).unwrap();
    let (a, mut s1) = filter.scan(start, 1234).unwrap();
    let (b, s2) = filter.scan(start + 1234, 3000 - 1234).unwrap();
    assert_eq!(whole, a.wrapping_add(b));
    s1.extend(s2);
    assert_eq!(survivors, s1);
    assert!(!survivors.is_empty());
    let (shifted, _) = filter.scan(start + 1, 3000).unwrap();
    assert_ne!(whole, shifted);
}
