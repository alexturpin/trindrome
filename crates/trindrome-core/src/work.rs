//! Hardware-independent work units.
//!
//! A [`WorkChunk`] names a half-open index range of one search space. Its identity
//! depends only on the mathematics (generator version, length, leading digit and
//! range), never on which backend or machine executes it.

use num_bigint::BigUint;
use num_traits::Zero;
use serde::{Deserialize, Serialize};

use crate::CoreError;
use crate::generator::{GENERATOR_VERSION, SearchSpace};

/// Version of the serialized [`WorkChunk`] format.
pub const WORK_SCHEMA_VERSION: u32 = 1;

/// Indices serialize as canonical decimal strings so they survive JSON consumers
/// (such as JavaScript) that cannot represent large integers exactly.
#[derive(Clone, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkChunk {
    pub schema_version: u32,
    pub generator_version: u32,
    pub hex_digits: u32,
    pub leading_hex_digit: u8,
    #[serde(with = "decimal_string")]
    pub start_index: BigUint,
    #[serde(with = "decimal_string")]
    pub end_index: BigUint,
}

impl WorkChunk {
    pub fn new(space: SearchSpace, start_index: BigUint, end_index: BigUint) -> Result<Self, CoreError> {
        let chunk = Self {
            schema_version: WORK_SCHEMA_VERSION,
            generator_version: GENERATOR_VERSION,
            hex_digits: space.hex_digits(),
            leading_hex_digit: space.leading(),
            start_index,
            end_index,
        };
        chunk.validate()?;
        Ok(chunk)
    }

    /// The whole of a search space as a single chunk.
    pub fn whole(space: SearchSpace) -> Self {
        Self::new(space, BigUint::zero(), space.candidate_count())
            .expect("every search space has at least one candidate")
    }

    /// Checks every invariant and returns the chunk's search space.
    ///
    /// The fields are public for (de)serialization, so call this before trusting
    /// a chunk from anywhere else.
    pub fn validate(&self) -> Result<SearchSpace, CoreError> {
        if self.schema_version != WORK_SCHEMA_VERSION {
            return Err(CoreError::UnsupportedSchemaVersion(self.schema_version));
        }
        if self.generator_version != GENERATOR_VERSION {
            return Err(CoreError::UnsupportedGeneratorVersion(self.generator_version));
        }
        let space = SearchSpace::new(self.hex_digits, self.leading_hex_digit)?;
        let count = space.candidate_count();
        if self.start_index >= self.end_index || self.end_index > count {
            return Err(CoreError::InvalidRange {
                start: self.start_index.clone(),
                end: self.end_index.clone(),
                count,
            });
        }
        Ok(space)
    }

    pub fn len(&self) -> BigUint {
        &self.end_index - &self.start_index
    }

    /// Deterministic, human-readable identity, e.g. `trindrome:g1:h65:1:0-4294967296`.
    pub fn id(&self) -> String {
        format!(
            "trindrome:g{}:h{}:{:X}:{}-{}",
            self.generator_version, self.hex_digits, self.leading_hex_digit, self.start_index, self.end_index
        )
    }

    /// Splits into consecutive chunks of at most `max_len` candidates that exactly
    /// cover this one.
    pub fn split(&self, max_len: &BigUint) -> Vec<WorkChunk> {
        assert!(!max_len.is_zero(), "chunk length must be positive");
        let mut chunks = Vec::new();
        let mut start = self.start_index.clone();
        while start < self.end_index {
            let end = (&start + max_len).min(self.end_index.clone());
            chunks.push(WorkChunk {
                start_index: start,
                end_index: end.clone(),
                ..self.clone()
            });
            start = end;
        }
        chunks
    }

    /// Whether `other` begins exactly where this chunk ends in the same space.
    pub fn is_followed_by(&self, other: &WorkChunk) -> bool {
        self.generator_version == other.generator_version
            && self.hex_digits == other.hex_digits
            && self.leading_hex_digit == other.leading_hex_digit
            && self.end_index == other.start_index
    }
}

mod decimal_string {
    use num_bigint::BigUint;
    use serde::{Deserialize, Deserializer, Serializer, de::Error};

    pub fn serialize<S: Serializer>(value: &BigUint, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&value.to_str_radix(10))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<BigUint, D::Error> {
        let s = String::deserialize(deserializer)?;
        let canonical = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
        if !canonical {
            return Err(D::Error::custom(format!("{s:?} is not a canonical decimal index")));
        }
        Ok(BigUint::parse_bytes(s.as_bytes(), 10).expect("checked digits"))
    }
}
