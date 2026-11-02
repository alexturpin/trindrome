//! Exact palindrome checks on arbitrary-precision integers.

use num_bigint::BigUint;

pub fn is_palindrome_digits<T: PartialEq>(digits: &[T]) -> bool {
    digits.iter().eq(digits.iter().rev())
}

/// Whether `n` reads the same forwards and backwards in `base` (2..=256).
///
/// Zero is a one-digit palindrome.
pub fn is_palindrome_in_base(n: &BigUint, base: u32) -> bool {
    is_palindrome_digits(&n.to_radix_le(base))
}

/// The reference decimal check: full conversion to a decimal string.
pub fn is_decimal_palindrome(n: &BigUint) -> bool {
    is_palindrome_digits(n.to_str_radix(10).as_bytes())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn big(s: &str) -> BigUint {
        BigUint::parse_bytes(s.as_bytes(), 10).unwrap()
    }

    #[test]
    fn small_values() {
        for v in [0u32, 1, 9, 11, 121, 1221, 12321] {
            assert!(is_decimal_palindrome(&v.into()), "{v}");
        }
        for v in [10u32, 12, 100, 1231, 1001001001] {
            assert_eq!(is_decimal_palindrome(&v.into()), v == 1001001001, "{v}");
        }
        assert!(is_palindrome_in_base(&0b1011101u32.into(), 2));
        assert!(!is_palindrome_in_base(&0b1011u32.into(), 2));
        assert!(is_palindrome_in_base(&0xABCBAu32.into(), 16));
        assert!(!is_palindrome_in_base(&0xABCu32.into(), 16));
    }

    #[test]
    fn decimal_beyond_128_bits() {
        // 10^60 + 1 is about 2^199.
        let one_zeros_one = format!("1{}1", "0".repeat(59));
        assert!(is_decimal_palindrome(&big(&one_zeros_one)));
        let off_by_one = format!("1{}2", "0".repeat(59));
        assert!(!is_decimal_palindrome(&big(&off_by_one)));

        let half = "31415926535897932384626433832795028841971";
        let even: String = half.chars().chain(half.chars().rev()).collect();
        let odd: String = half.chars().chain(half.chars().rev().skip(1)).collect();
        assert!(is_decimal_palindrome(&big(&even)));
        assert!(is_decimal_palindrome(&big(&odd)));

        // Differs only in a digit next to the centre.
        let mut broken = even.into_bytes();
        let mid = broken.len() / 2;
        broken[mid] = if broken[mid] == b'9' { b'8' } else { b'9' };
        assert!(!is_decimal_palindrome(&big(std::str::from_utf8(&broken).unwrap())));
    }

    #[test]
    fn trailing_zeros_are_not_palindromes() {
        assert!(!is_palindrome_in_base(&0x10u32.into(), 16));
        assert!(!is_palindrome_in_base(&0b110u32.into(), 2));
        assert!(!is_decimal_palindrome(&10u32.into()));
    }
}
