//! Proof of search for distributed work.
//!
//! The *survivor set* of an index range is the sorted list of indices whose
//! candidates pass the production decimal filter. Producing the complete set
//! takes a search of the range; checking any reported survivor takes one filter
//! evaluation. A distributed worker submits the survivor set of every segment it
//! searched, so the coordinator can reject fabricated survivors outright and
//! compare independent replicas exactly. See `docs/distributed.md`.

use num_bigint::BigUint;
use sha2::{Digest, Sha256};

use crate::CoreError;
use crate::filter::{DecimalFilter, PRODUCTION_WINDOW};
use crate::generator::{GENERATOR_VERSION, SearchSpace};
use crate::palindrome::is_decimal_palindrome;
use crate::verification::Verification;

/// Version of the filter that defines survivor sets: the production window test
/// of `filter.rs`. Any change to which candidates survive must bump it.
pub const FILTER_VERSION: u32 = 1;

/// Deterministic identity of a segment, e.g. `trindrome:g1:f1:h65:1:0-4294967296`.
pub fn segment_id(space: SearchSpace, start: u64, end: u64) -> String {
    format!(
        "trindrome:g{GENERATOR_VERSION}:f{FILTER_VERSION}:h{}:{:X}:{start}-{end}",
        space.hex_digits(),
        space.leading()
    )
}

fn invalid(message: impl Into<String>) -> CoreError {
    CoreError::InvalidProof(message.into())
}

/// Encodes absolute survivor indices of `[start, end)` as LEB128 varints of
/// their offsets from `start`: the first offset, then the gap to each next one.
pub fn encode_survivors(start: u64, end: u64, survivors: &[u64]) -> Result<Vec<u8>, CoreError> {
    let mut out = Vec::with_capacity(survivors.len() * 5);
    let mut previous: Option<u64> = None;
    for &index in survivors {
        if index < start || index >= end {
            return Err(invalid(format!("survivor {index} outside [{start}, {end})")));
        }
        let mut value = match previous {
            None => index - start,
            Some(p) if index > p => index - p,
            Some(p) => return Err(invalid(format!("survivor {index} does not follow {p}"))),
        };
        loop {
            let byte = (value & 0x7F) as u8;
            value >>= 7;
            if value == 0 {
                out.push(byte);
                break;
            }
            out.push(byte | 0x80);
        }
        previous = Some(index);
    }
    Ok(out)
}

/// Decodes [`encode_survivors`] output, rejecting anything non-canonical.
pub fn decode_survivors(start: u64, end: u64, bytes: &[u8]) -> Result<Vec<u64>, CoreError> {
    let mut survivors = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        let mut value = 0u64;
        let mut shift = 0;
        loop {
            let byte = *bytes.get(i).ok_or_else(|| invalid("truncated varint"))?;
            i += 1;
            if shift == 63 && byte > 1 || shift > 63 {
                return Err(invalid("varint overflows 64 bits"));
            }
            value |= u64::from(byte & 0x7F) << shift;
            shift += 7;
            if byte & 0x80 == 0 {
                if byte == 0 && shift > 7 {
                    return Err(invalid("non-canonical varint"));
                }
                break;
            }
        }
        let index = match survivors.last() {
            None => start.checked_add(value),
            Some(_) if value == 0 => return Err(invalid("repeated survivor")),
            Some(&p) => u64::checked_add(p, value),
        }
        .filter(|&index| index < end)
        .ok_or_else(|| invalid(format!("survivor beyond the end of [{start}, {end})")))?;
        survivors.push(index);
    }
    Ok(survivors)
}

/// SHA-256 over the segment's identity and its encoded survivors.
pub fn segment_digest(space: SearchSpace, start: u64, end: u64, encoded: &[u8]) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update(segment_id(space, start, end).as_bytes());
    hasher.update([0]);
    hasher.update(encoded);
    hasher.finalize().into()
}

/// An order-independent fingerprint of a set of survivor indices: the
/// wrapping sum of a 64-bit mix of each index. Fingerprints of disjoint ranges
/// add up to the fingerprint of their union, so two workers that split a
/// range into different segments can still be compared.
///
/// It needs no cryptographic strength: every survivor in an accepted set is
/// re-checked, so a worker can only leave survivors out, not choose them.
pub fn survivor_fingerprint(survivors: &[u64]) -> u64 {
    survivors.iter().fold(0u64, |sum, &index| sum.wrapping_add(mix64(index)))
}

/// SplitMix64's finalizer.
fn mix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9e37_79b9_7f4a_7c15);
    z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    z ^ (z >> 31)
}

/// What a valid survivor set says about its segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SegmentCheck {
    pub survivors: u64,
    pub digest: [u8; 32],
    /// [`survivor_fingerprint`] of the set.
    pub fingerprint: u64,
    /// Survivors that are decimal palindromes, and so triple palindromes.
    pub matches: Vec<(u64, Verification)>,
}

/// Checks a submitted survivor set: canonical encoding, every survivor inside
/// the segment and passing the filter, and every survivor checked exactly.
///
/// This proves the set contains no fabricated survivors and finds every triple
/// palindrome among them. It cannot prove the set is complete; only an
/// independent replica can.
pub fn check_segment(space: SearchSpace, start: u64, end: u64, encoded: &[u8]) -> Result<SegmentCheck, CoreError> {
    check_with_window(space, start, end, encoded, PRODUCTION_WINDOW)
}

fn check_with_window(
    space: SearchSpace,
    start: u64,
    end: u64,
    encoded: &[u8],
    window: u32,
) -> Result<SegmentCheck, CoreError> {
    let count = space.candidate_count();
    if start >= end || BigUint::from(end) > count {
        return Err(CoreError::InvalidRange {
            start: start.into(),
            end: end.into(),
            count,
        });
    }
    let filter = DecimalFilter::new(space, window)
        .ok_or_else(|| invalid(format!("{space:?} is too short for the decimal filter")))?;
    let survivors = decode_survivors(start, end, encoded)?;
    let mut matches = Vec::new();
    for &index in &survivors {
        let value = space.candidate(&index.into())?;
        if !filter.evaluate(&value).pass {
            return Err(invalid(format!("index {index} does not pass the filter")));
        }
        if is_decimal_palindrome(&value) {
            matches.push((index, Verification::of(&value)));
        }
    }
    Ok(SegmentCheck {
        survivors: survivors.len() as u64,
        digest: segment_digest(space, start, end, encoded),
        fingerprint: survivor_fingerprint(&survivors),
        matches,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoding_round_trips() {
        let survivors = [100, 101, 228, 1 << 40, u64::MAX - 1];
        let bytes = encode_survivors(100, u64::MAX, &survivors).unwrap();
        assert_eq!(decode_survivors(100, u64::MAX, &bytes).unwrap(), survivors);
        assert!(encode_survivors(0, 10, &[]).unwrap().is_empty());
        assert!(decode_survivors(0, 10, &[]).unwrap().is_empty());
    }

    #[test]
    fn encoding_rejects_bad_input() {
        assert!(encode_survivors(10, 20, &[9]).is_err());
        assert!(encode_survivors(10, 20, &[20]).is_err());
        assert!(encode_survivors(10, 20, &[12, 12]).is_err());
        assert!(encode_survivors(10, 20, &[13, 12]).is_err());
        assert!(decode_survivors(10, 20, &[10]).is_err(), "beyond the end");
        assert!(decode_survivors(10, 20, &[1, 0]).is_err(), "repeat");
        assert!(decode_survivors(10, 20, &[0x81]).is_err(), "truncated");
        assert!(decode_survivors(10, 20, &[0x81, 0x00]).is_err(), "non-canonical");
        assert!(decode_survivors(0, u64::MAX, &[0xFF; 10]).is_err(), "overflow");
    }

    #[test]
    fn segment_check_accepts_the_true_survivor_set_only() {
        // W = 9 survives about 1 in 10^9, too rare to find here; W = 1 has
        // plenty of survivors and exercises the same checks.
        let window = 1;
        let space = SearchSpace::new(33, 0x5).unwrap();
        let filter = DecimalFilter::new(space, window).unwrap();
        let (start, end) = (1000u64, 1 << 16);
        let (_, survivors) = filter.scan(start, end - start).unwrap();
        assert!(survivors.len() > 100);
        let bytes = encode_survivors(start, end, &survivors).unwrap();
        let check = check_with_window(space, start, end, &bytes, window).unwrap();
        assert_eq!(check.survivors, survivors.len() as u64);
        assert_eq!(check.digest, segment_digest(space, start, end, &bytes));
        assert_eq!(check.fingerprint, survivor_fingerprint(&survivors));

        // Fingerprints of a split add up to the whole; a missing survivor changes it.
        let mid = start + (end - start) / 3;
        let (left, right): (Vec<u64>, Vec<u64>) = survivors.iter().partition(|&&i| i < mid);
        let halves = [(start, mid, left), (mid, end, right)].map(|(a, b, part)| {
            check_with_window(space, a, b, &encode_survivors(a, b, &part).unwrap(), window).unwrap()
        });
        assert_eq!(halves[0].fingerprint.wrapping_add(halves[1].fingerprint), check.fingerprint);
        assert_ne!(survivor_fingerprint(&survivors[1..]), check.fingerprint);

        // A non-survivor is rejected.
        let fake = (start..end).find(|i| !survivors.contains(i)).unwrap();
        let mut forged = survivors.clone();
        forged.push(fake);
        forged.sort();
        let bytes = encode_survivors(start, end, &forged).unwrap();
        assert!(check_with_window(space, start, end, &bytes, window).is_err());
        // So is any survivor at the production window.
        let bytes = encode_survivors(start, end, &[fake]).unwrap();
        assert!(check_segment(space, start, end, &bytes).is_err());

        // Spaces too short for the filter cannot carry a proof.
        let short = SearchSpace::new(9, 0x5).unwrap();
        assert!(check_segment(short, 0, 10, &[]).is_err());
    }

    #[test]
    fn digest_depends_on_identity() {
        let space = SearchSpace::new(65, 0x1).unwrap();
        assert_ne!(segment_digest(space, 0, 10, &[]), segment_digest(space, 0, 11, &[]));
        assert_eq!(segment_id(space, 0, 10), "trindrome:g1:f1:h65:1:0-10");
    }
}
