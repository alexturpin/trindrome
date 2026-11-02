//! Deterministic mathematics for the Trindrome search.
//!
//! This crate generates integers that are palindromic in both base 2 and base 16
//! (following Rick Regan's structural result), checks palindromicity exactly, and
//! defines the hardware-independent work units that every backend executes.
//!
//! It deliberately knows nothing about Windows, CUDA, SQLite or networking.

pub mod generator;
pub mod palindrome;
pub mod verification;
pub mod work;

use num_bigint::BigUint;
use thiserror::Error;

pub use generator::{Candidates, GENERATOR_VERSION, LEADING_DIGITS, MAX_HEX_DIGITS, SearchSpace};
pub use verification::Verification;
pub use work::{WORK_SCHEMA_VERSION, WorkChunk};

#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum CoreError {
    #[error("hexadecimal length {0} is not supported (must be between 1 and 65536)")]
    InvalidHexDigits(u32),

    #[error("leading hex digit {0:#X} cannot start a binary/hexadecimal palindrome (expected 1, 3, 5, 7, 9 or F)")]
    InvalidLeadingDigit(u8),

    #[error("candidate index {index} is outside [0, {count})")]
    IndexOutOfRange { index: BigUint, count: BigUint },

    #[error("index range [{start}, {end}) is invalid for a space of {count} candidates")]
    InvalidRange {
        start: BigUint,
        end: BigUint,
        count: BigUint,
    },

    #[error("range of {0} candidates is too large to enumerate in one pass")]
    RangeTooLarge(BigUint),

    #[error("unsupported generator version {0} (this build supports {GENERATOR_VERSION})")]
    UnsupportedGeneratorVersion(u32),

    #[error("unsupported work schema version {0} (this build supports {WORK_SCHEMA_VERSION})")]
    UnsupportedSchemaVersion(u32),
}
