//! Independent verification of a reported value in all three bases.
//!
//! This deliberately does not use the generator: it starts from the integer alone,
//! so a generator or backend bug cannot vouch for itself.

use num_bigint::BigUint;

use crate::palindrome::is_palindrome_digits;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verification {
    pub value: BigUint,
    pub binary: String,
    pub decimal: String,
    pub hexadecimal: String,
    pub binary_palindrome: bool,
    pub decimal_palindrome: bool,
    pub hexadecimal_palindrome: bool,
}

impl Verification {
    pub fn of(value: &BigUint) -> Self {
        let binary = value.to_str_radix(2);
        let decimal = value.to_str_radix(10);
        let hexadecimal = value.to_str_radix(16).to_ascii_uppercase();
        Self {
            value: value.clone(),
            binary_palindrome: is_palindrome_digits(binary.as_bytes()),
            decimal_palindrome: is_palindrome_digits(decimal.as_bytes()),
            hexadecimal_palindrome: is_palindrome_digits(hexadecimal.as_bytes()),
            binary,
            decimal,
            hexadecimal,
        }
    }

    pub fn is_triple_palindrome(&self) -> bool {
        self.binary_palindrome && self.decimal_palindrome && self.hexadecimal_palindrome
    }

    /// Single hex digits (1, 3, 5, 7, 9) are trivially triple palindromes; the
    /// project is looking for one with at least two hex digits.
    pub fn is_nontrivial(&self) -> bool {
        self.hexadecimal.len() >= 2
    }
}
