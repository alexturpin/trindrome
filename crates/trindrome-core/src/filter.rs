//! Exact decimal rejection filter, the reference for the GPU kernel.
//!
//! A value can only be a decimal palindrome if its top `W` decimal digits equal
//! its low `W` digits reversed. Testing that takes a residue sum and one bounded
//! multiply instead of a full decimal conversion, and it never rejects a real
//! palindrome. Survivors still need the exact check. See `docs/cuda-design.md`,
//! which this module and `kernels/search.cu` must both follow bit for bit.

use num_bigint::BigUint;
use num_traits::ToPrimitive;

use crate::generator::SearchSpace;

/// Window width used for real searches; a candidate survives with probability ~10^-9.
pub const PRODUCTION_WINDOW: u32 = 9;

/// Every intermediate value the filter computes for one candidate.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Evaluation {
    pub digits: u32,
    pub low: u32,
    pub pass: bool,
    pub borrow: bool,
    pub diff_low32: u32,
    pub limb_xor: u32,
}

impl Evaluation {
    /// Per-candidate contribution to a batch checksum.
    pub fn hash(&self, index: u64) -> u64 {
        let a = self.low as u64 | (self.digits as u64) << 32 | (self.pass as u64) << 40 | (self.borrow as u64) << 41;
        let b = self.diff_low32 as u64 | (self.limb_xor as u64) << 32;
        mix64(a ^ mix64(b ^ mix64(index)))
    }
}

/// The splitmix64 finalizer.
pub fn mix64(mut z: u64) -> u64 {
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Exact decimal digit count (slow; for setup and tests).
pub fn decimal_digits(v: &BigUint) -> u32 {
    v.to_str_radix(10).len() as u32
}

fn reverse_digits(mut x: u32, width: u32) -> u32 {
    let mut reversed = 0;
    for _ in 0..width {
        reversed = reversed * 10 + x % 10;
        x /= 10;
    }
    reversed
}

fn low32(v: &BigUint) -> u32 {
    v.iter_u32_digits().next().unwrap_or(0)
}

/// Evaluates the window test for `v`, given its exact digit count and `p = 10^(digits - window)`.
pub fn evaluate(v: &BigUint, digits: u32, window: u32, p: &BigUint) -> Evaluation {
    assert!(
        (1..=9).contains(&window) && digits >= 2 * window,
        "window {window} invalid for {digits} digits"
    );
    let low = (v % 1_000_000_000u32).to_u32().expect("below 10^9") % 10u32.pow(window);
    let rho = reverse_digits(low, window);
    let rho_p = p * rho;
    let borrow = *v < rho_p;
    let pass = !borrow && (v - &rho_p) < *p;
    Evaluation {
        digits,
        low,
        pass,
        borrow,
        diff_low32: low32(v).wrapping_sub(low32(&rho_p)),
        limb_xor: v.iter_u32_digits().fold(0, |acc, limb| acc ^ limb),
    }
}

/// The filter specialised to one search space.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecimalFilter {
    pub space: SearchSpace,
    pub window: u32,
    /// Decimal digits of the space's smallest candidate.
    pub digits_low: u32,
    /// The power of ten inside the space, if any; candidates at or above it have one more digit.
    pub threshold: Option<BigUint>,
    /// `10^(digits_low - window)` and `10^(digits_low + 1 - window)`.
    pub p_low: BigUint,
    pub p_high: BigUint,
}

impl DecimalFilter {
    /// `None` if some candidate of the space is too short for the window.
    pub fn new(space: SearchSpace, window: u32) -> Option<Self> {
        assert!((1..=9).contains(&window));
        let first = space.candidate(&BigUint::ZERO).expect("index 0 exists");
        let last = space
            .candidate(&(space.candidate_count() - 1u32))
            .expect("last index exists");
        let digits_low = decimal_digits(&first);
        let digits_high = decimal_digits(&last);
        if digits_low < 2 * window {
            return None;
        }
        // Candidates span less than a factor of 2, so at most one power of ten.
        assert!(digits_high <= digits_low + 1);
        let ten = BigUint::from(10u32);
        Some(Self {
            space,
            window,
            digits_low,
            threshold: (digits_high > digits_low).then(|| ten.pow(digits_low)),
            p_low: ten.pow(digits_low - window),
            p_high: ten.pow(digits_low + 1 - window),
        })
    }

    pub fn digits_of(&self, v: &BigUint) -> u32 {
        match &self.threshold {
            Some(t) if v >= t => self.digits_low + 1,
            _ => self.digits_low,
        }
    }

    pub fn evaluate(&self, v: &BigUint) -> Evaluation {
        let digits = self.digits_of(v);
        let p = if digits == self.digits_low {
            &self.p_low
        } else {
            &self.p_high
        };
        evaluate(v, digits, self.window, p)
    }

    /// Checksum and surviving indices of `[start, start + count)`, exactly as the
    /// GPU kernel reports them.
    pub fn scan(&self, start: u64, count: u64) -> Result<(u64, Vec<u64>), crate::CoreError> {
        let end = BigUint::from(start) + count;
        let mut checksum = 0u64;
        let mut survivors = Vec::new();
        for (offset, v) in self.space.candidates(&start.into(), &end)?.enumerate() {
            let index = start + offset as u64;
            let e = self.evaluate(&v);
            checksum = checksum.wrapping_add(e.hash(index));
            if e.pass {
                survivors.push(index);
            }
        }
        Ok((checksum, survivors))
    }
}
