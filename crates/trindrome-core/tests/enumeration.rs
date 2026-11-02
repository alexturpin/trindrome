use num_bigint::BigUint;
use trindrome_core::{CoreError, LEADING_DIGITS, SearchSpace, Verification};

fn space(n: u32, leading: u8) -> SearchSpace {
    SearchSpace::new(n, leading).unwrap()
}

fn hex(s: &str) -> BigUint {
    BigUint::parse_bytes(s.as_bytes(), 16).unwrap()
}

fn big(v: u64) -> BigUint {
    BigUint::from(v)
}

#[test]
fn total_counts_for_small_lengths() {
    let expected = [6u32, 6, 20, 20, 72, 72];
    for (n, &want) in (1..).zip(&expected) {
        let total: BigUint = SearchSpace::all_of_length(n)
            .unwrap()
            .iter()
            .map(|s| s.candidate_count())
            .sum();
        assert_eq!(total, BigUint::from(want), "length {n}");
    }
}

#[test]
fn counts_follow_the_formula() {
    for n in 1..=200u32 {
        let k = (n - 1) / 2;
        let formula =
            BigUint::from(2u32) * BigUint::from(2u32).pow(k) + BigUint::from(4u32) * BigUint::from(4u32).pow(k);
        let total: BigUint = SearchSpace::all_of_length(n)
            .unwrap()
            .iter()
            .map(|s| s.candidate_count())
            .sum();
        assert_eq!(total, formula, "length {n}");
    }
}

#[test]
fn historical_and_frontier_volumes() {
    let total = |n: u32, leads: &[u8]| -> BigUint { leads.iter().map(|&l| space(n, l).candidate_count()).sum() };
    let historical: BigUint = (1..=64).map(|n| total(n, &[0x1, 0x3])).sum::<BigUint>()
        + (1..=32).map(|n| total(n, &[0x5, 0x7, 0x9, 0xF])).sum::<BigUint>();
    assert_eq!(historical, big(28_633_115_300));
    assert_eq!(total(65, &[0x1, 0x3]), big(1 << 33));
    assert_eq!(total(33, &[0x5, 0x7, 0x9, 0xF]), big(1 << 34));
}

#[test]
fn known_examples_at_expected_indices() {
    for (n, leading, index, value) in [
        (3, 0x5, 1, "525"),
        (4, 0x5, 1, "5225"),
        (5, 0x7, 1, "70207"),
        (5, 0xF, 1, "F060F"),
        (3, 0x9, 3, "9F9"),
    ] {
        let v = space(n, leading).candidate(&big(index)).unwrap();
        assert_eq!(v, hex(value), "{value}");
        let check = Verification::of(&v);
        assert!(check.binary_palindrome && check.hexadecimal_palindrome, "{value}");
    }
    assert_eq!(Verification::of(&hex("525")).binary, "10100100101");
    assert_eq!(Verification::of(&hex("5225")).binary, "101001000100101");
}

#[test]
fn odd_and_even_lengths_mirror_correctly() {
    assert_eq!(space(1, 0x7).candidate(&big(0)).unwrap(), hex("7"));
    assert_eq!(space(2, 0x7).candidate(&big(0)).unwrap(), hex("77"));
    // Odd: the centre digit appears once. Even: it is repeated.
    // Index 7 is 13 in base 4, selecting 6 and F from [0, 6, 9, F].
    assert_eq!(space(5, 0x9).candidate(&big(7)).unwrap(), hex("96F69"));
    assert_eq!(space(6, 0x9).candidate(&big(7)).unwrap(), hex("96FF69"));
}

#[test]
fn first_and_last_candidates() {
    for leading in LEADING_DIGITS {
        let max = *space(1, leading).alphabet().last().unwrap();
        for n in 2..=40u32 {
            let s = space(n, leading);
            let last_index = s.candidate_count() - 1u32;
            let first = s.candidate(&big(0)).unwrap();
            let last = s.candidate(&last_index).unwrap();

            let l = format!("{leading:X}");
            let zeros = "0".repeat(n as usize - 2);
            assert_eq!(first, hex(&format!("{l}{zeros}{l}")), "first of ({n}, {l})");
            let maxes = format!("{max:X}").repeat(n as usize - 2);
            assert_eq!(last, hex(&format!("{l}{maxes}{l}")), "last of ({n}, {l})");
        }
    }
}

#[test]
fn stable_high_index_fixtures() {
    // Lowest and highest candidates of the first unexplored 1-family length.
    let s = space(65, 0x1);
    assert_eq!(s.candidate_count(), big(1 << 32));
    assert_eq!(s.candidate(&big(0)).unwrap(), (BigUint::from(16u32).pow(64)) + 1u32);
    assert_eq!(s.candidate(&big((1 << 32) - 1)).unwrap(), hex(&"1".repeat(65)));

    let s = space(33, 0xF);
    assert_eq!(
        s.candidate(&big((1 << 32) - 1)).unwrap(),
        BigUint::from(16u32).pow(33) - 1u32
    );

    // 12345 = 3000321 in base 4, selecting 0,7,0,0,0,7,5,2 from [0, 2, 5, 7].
    assert_eq!(
        space(33, 0x5).candidate(&big(12345)).unwrap(),
        hex("500000000070007525700070000000005")
    );

    // 512-bit territory: 128 hex digits.
    let s = space(128, 0x3);
    let all_threes = hex(&"3".repeat(128));
    assert_eq!(s.candidate(&(s.candidate_count() - 1u32)).unwrap(), all_threes);
    assert_eq!(all_threes.bits(), 510);
}

#[test]
fn candidates_strictly_increase_with_index() {
    for leading in LEADING_DIGITS {
        for n in 1..=9u32 {
            let s = space(n, leading);
            let values: Vec<BigUint> = s.candidates(&big(0), &s.candidate_count()).unwrap().collect();
            assert!(values.windows(2).all(|w| w[0] < w[1]), "({n}, {leading:X})");
        }
    }
}

#[test]
fn iterator_agrees_with_random_access() {
    for (n, leading, start, len) in [
        (65u32, 0x1u8, (1u64 << 32) - 1000, 1000u64),
        (33, 0x7, 123_456_789, 500),
        (12, 0x9, 0, 1024),
    ] {
        let s = space(n, leading);
        let end = big(start + len);
        let streamed: Vec<BigUint> = s.candidates(&big(start), &end).unwrap().collect();
        assert_eq!(streamed.len() as u64, len);
        for (i, v) in streamed.iter().enumerate() {
            assert_eq!(
                *v,
                s.candidate(&big(start + i as u64)).unwrap(),
                "({n}, {leading:X}) index {}",
                start + i as u64
            );
        }
    }
}

#[test]
fn empty_range_yields_nothing() {
    let s = space(10, 0x5);
    assert_eq!(s.candidates(&big(7), &big(7)).unwrap().count(), 0);
}

#[test]
fn invalid_inputs_are_rejected() {
    assert_eq!(SearchSpace::new(0, 0x1), Err(CoreError::InvalidHexDigits(0)));
    assert!(SearchSpace::new(70_000, 0x1).is_err());
    for bad in [0x0, 0x2, 0x4, 0x6, 0x8, 0xA, 0xB, 0xC, 0xD, 0xE, 0x10] {
        assert_eq!(SearchSpace::new(5, bad), Err(CoreError::InvalidLeadingDigit(bad)));
    }

    let s = space(5, 0x5);
    let count = s.candidate_count();
    assert!(matches!(s.candidate(&count), Err(CoreError::IndexOutOfRange { .. })));
    assert!(matches!(
        s.candidates(&big(3), &big(2)),
        Err(CoreError::InvalidRange { .. })
    ));
    assert!(matches!(
        s.candidates(&big(0), &(count + 1u32)),
        Err(CoreError::InvalidRange { .. })
    ));

    let huge = space(200, 0x9);
    assert!(matches!(
        huge.candidates(&big(0), &huge.candidate_count()),
        Err(CoreError::RangeTooLarge(_))
    ));
}
