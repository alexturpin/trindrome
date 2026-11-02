//! Candidate generation following Regan's binary/hexadecimal palindrome structure.
//!
//! A nonzero integer that is palindromic in both base 2 and base 16 starts with one
//! of six hex digits, and that leading digit restricts every other digit to a small
//! alphabet. A *search space* is one (length, leading digit) pair; within it, each
//! candidate is addressed by an index whose radix-`|alphabet|` digits select the
//! free half of the palindrome. See `docs/algorithm.md`.

use num_bigint::BigUint;
use num_traits::{ToPrimitive, Zero};

use crate::CoreError;

/// Version of the index-to-candidate mapping. Bump it if the mapping ever changes;
/// coverage recorded under one version says nothing about another.
pub const GENERATOR_VERSION: u32 = 1;

/// Every possible leading hex digit of a nonzero binary/hexadecimal palindrome.
pub const LEADING_DIGITS: [u8; 6] = [0x1, 0x3, 0x5, 0x7, 0x9, 0xF];

/// Upper bound on supported hexadecimal lengths, as a guard against absurd inputs.
pub const MAX_HEX_DIGITS: u32 = 1 << 16;

/// Allowed non-leading digits for a given leading digit, in ascending order.
///
/// Ascending order makes candidate values strictly increasing with their index.
pub fn alphabet(leading: u8) -> Option<&'static [u8]> {
    match leading {
        0x1 => Some(&[0x0, 0x1]),
        0x3 => Some(&[0x0, 0x3]),
        0x5 | 0x7 => Some(&[0x0, 0x2, 0x5, 0x7]),
        0x9 | 0xF => Some(&[0x0, 0x6, 0x9, 0xF]),
        _ => None,
    }
}

/// All binary/hexadecimal palindromes with a given hex length and leading digit.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SearchSpace {
    hex_digits: u32,
    leading: u8,
}

impl SearchSpace {
    pub fn new(hex_digits: u32, leading: u8) -> Result<Self, CoreError> {
        if hex_digits == 0 || hex_digits > MAX_HEX_DIGITS {
            return Err(CoreError::InvalidHexDigits(hex_digits));
        }
        if alphabet(leading).is_none() {
            return Err(CoreError::InvalidLeadingDigit(leading));
        }
        Ok(Self { hex_digits, leading })
    }

    /// The six spaces that together hold every binary/hex palindrome of this length.
    pub fn all_of_length(hex_digits: u32) -> Result<[Self; 6], CoreError> {
        let mut spaces = [Self::new(hex_digits, LEADING_DIGITS[0])?; 6];
        for (space, &leading) in spaces.iter_mut().zip(&LEADING_DIGITS) {
            *space = Self::new(hex_digits, leading)?;
        }
        Ok(spaces)
    }

    pub fn hex_digits(&self) -> u32 {
        self.hex_digits
    }

    pub fn leading(&self) -> u8 {
        self.leading
    }

    pub fn alphabet(&self) -> &'static [u8] {
        alphabet(self.leading).expect("validated in SearchSpace::new")
    }

    pub fn radix(&self) -> u32 {
        self.alphabet().len() as u32
    }

    /// Number of freely chosen digits, `k = floor((n - 1) / 2)`.
    pub fn free_digits(&self) -> u32 {
        (self.hex_digits - 1) / 2
    }

    /// Total candidates in this space, `radix^k`.
    pub fn candidate_count(&self) -> BigUint {
        BigUint::from(self.radix()).pow(self.free_digits())
    }

    pub fn check_index(&self, index: &BigUint) -> Result<(), CoreError> {
        let count = self.candidate_count();
        if *index >= count {
            return Err(CoreError::IndexOutOfRange {
                index: index.clone(),
                count,
            });
        }
        Ok(())
    }

    /// Hex digits (most significant first) of the candidate at `index`.
    pub fn candidate_hex_digits(&self, index: &BigUint) -> Result<Vec<u8>, CoreError> {
        self.check_index(index)?;
        let k = self.free_digits() as usize;
        let selection = if index.is_zero() {
            Vec::new()
        } else {
            index.to_radix_be(self.radix())
        };
        debug_assert!(selection.len() <= k);

        let alphabet = self.alphabet();
        let mut prefix = Vec::with_capacity(k + 1);
        prefix.push(self.leading);
        prefix.resize(k + 1 - selection.len(), alphabet[0]);
        prefix.extend(selection.iter().map(|&d| alphabet[d as usize]));
        Ok(mirror(&prefix, self.hex_digits))
    }

    /// The candidate at `index` as an exact integer.
    pub fn candidate(&self, index: &BigUint) -> Result<BigUint, CoreError> {
        Ok(hex_digits_to_value(&self.candidate_hex_digits(index)?))
    }

    /// Streams the candidates of `[start, end)` in index order without retaining any.
    pub fn candidates(&self, start: &BigUint, end: &BigUint) -> Result<Candidates, CoreError> {
        Candidates::new(*self, start, end)
    }
}

/// Completes a palindrome from its first `k + 1` digits. Odd lengths share the
/// centre digit; even lengths repeat it.
fn mirror(prefix: &[u8], hex_digits: u32) -> Vec<u8> {
    let reflected = if hex_digits % 2 == 0 {
        prefix
    } else {
        &prefix[..prefix.len() - 1]
    };
    let mut digits = Vec::with_capacity(hex_digits as usize);
    digits.extend_from_slice(prefix);
    digits.extend(reflected.iter().rev());
    debug_assert_eq!(digits.len(), hex_digits as usize);
    digits
}

fn hex_digits_to_value(digits: &[u8]) -> BigUint {
    BigUint::from_radix_be(digits, 16).expect("generator only emits digits below 16")
}

/// Iterator over candidate values for a half-open index range.
///
/// It advances the selection digits like an odometer, so each step is cheap and
/// nothing about earlier candidates is kept.
#[derive(Clone, Debug)]
pub struct Candidates {
    space: SearchSpace,
    /// Positions into the alphabet for the `k` free digits, most significant first.
    selection: Vec<u8>,
    remaining: u64,
}

impl Candidates {
    fn new(space: SearchSpace, start: &BigUint, end: &BigUint) -> Result<Self, CoreError> {
        let count = space.candidate_count();
        if start > end || *end > count {
            return Err(CoreError::InvalidRange {
                start: start.clone(),
                end: end.clone(),
                count,
            });
        }
        let len = end - start;
        let remaining = len.to_u64().ok_or(CoreError::RangeTooLarge(len))?;

        let k = space.free_digits() as usize;
        let mut selection = vec![0u8; k];
        if !start.is_zero() {
            let digits = start.to_radix_be(space.radix());
            selection[k - digits.len()..].copy_from_slice(&digits);
        }
        Ok(Self {
            space,
            selection,
            remaining,
        })
    }

    pub fn remaining(&self) -> u64 {
        self.remaining
    }
}

impl Iterator for Candidates {
    type Item = BigUint;

    fn next(&mut self) -> Option<BigUint> {
        if self.remaining == 0 {
            return None;
        }
        self.remaining -= 1;

        let alphabet = self.space.alphabet();
        let mut prefix = Vec::with_capacity(self.selection.len() + 1);
        prefix.push(self.space.leading);
        prefix.extend(self.selection.iter().map(|&d| alphabet[d as usize]));
        let value = hex_digits_to_value(&mirror(&prefix, self.space.hex_digits));

        // Advance; the final carry out only happens after the last index.
        let radix = self.space.radix() as u8;
        for d in self.selection.iter_mut().rev() {
            *d += 1;
            if *d < radix {
                break;
            }
            *d = 0;
        }
        Some(value)
    }

    fn size_hint(&self) -> (usize, Option<usize>) {
        match usize::try_from(self.remaining) {
            Ok(n) => (n, Some(n)),
            Err(_) => (usize::MAX, None),
        }
    }
}
