# Algorithm

Trindrome looks for integers that are palindromes in bases 2, 10 and 16 at once. The single hex digits 1, 3, 5, 7 and 9 are trivial examples; the search is for one with at least two hex digits.

## Generating binary/hexadecimal palindromes

Following Rick Regan, [*The Structure of Binary/Hexadecimal Palindromes*](https://www.exploringbinary.com/the-structure-of-binary-hexadecimal-palindromes/), a nonzero number that is palindromic in both base 2 and base 16 must start (and so end) with an odd hex digit, and that digit fixes which digits may appear anywhere else:

| Leading digit | Leading zero bits | Other digits allowed |
|---|---|---|
| `1` | 3 | `0 1` |
| `3` | 2 | `0 3` |
| `5`, `7` | 1 | `0 2 5 7` |
| `9`, `F` | 0 | `0 6 9 F` |

Each allowed digit has the same number of leading zero bits as the leading digit, once its low bits are mirrored. As a result the binary string lines up as a palindrome whatever the nibble boundaries are. The exhaustive test described below confirms this table is complete as well as sound.

### Search spaces and indices

A **search space** is a pair (hex length `n`, leading digit `L`). It has `k = floor((n - 1) / 2)` freely chosen digits and `radix^k` candidates, where `radix` is the size of `L`'s alphabet (2 or 4).

The candidate at `index`:

1. Write `index` in base `radix`, left-padded to exactly `k` digits.
2. Map each digit through the alphabet (ascending order) to get the prefix `L d1 … dk`.
3. Mirror the prefix. Even `n` repeats the last prefix digit; odd `n` uses it once as the centre.

For example, `(n=3, L=5, index=1)` gives prefix `52` and value `0x525 = 0b10100100101`.

Because each alphabet is listed in ascending order, candidate values strictly increase with the index. Index ranges are half-open, `[start, end)`.

Across all six spaces of length `n` there are `N(n) = 2·2^k + 4·4^k` candidates: 6, 6, 20, 20, 72, 72 for lengths 1–6.

| Range | Candidates |
|---|---:|
| Regan 2010: leading 1/3 to 64 digits, 5/7/9/F to 32 digits | 28,633,115,300 |
| Frontier A: length 65, leading 1/3 | 2^33 |
| Frontier B: length 33, leading 5/7/9/F | 2^34 |

These are frontiers beyond Regan's published 2010 coverage. Later researchers may have gone further.

## Decimal check

Only the decimal property remains to be tested. The CPU reference converts the exact integer to a decimal string and compares it with its reverse. It does no early rejection, on purpose: it is the yardstick that faster backends are measured against.

## Invariants and how they are tested

| Invariant | Test |
|---|---|
| Every generated value is a binary and hex palindrome, and every such value is generated exactly once | `trindrome-core/tests/oracle.rs`: a brute-force `u64` digit-reversal oracle over **every integer below 16^6**, independent of the generator and of big integers, compared as sets, with a duplicate check |
| Each value has exactly `n` hex digits | `oracle.rs` |
| Counts match the formula (n ≤ 200), historical and frontier volumes | `enumeration.rs` |
| Fixed index → value mapping, including 260-bit frontier and 510-bit values | `enumeration.rs` (`stable_high_index_fixtures`) |
| Streaming matches random access; values strictly increase; first and last candidates | `enumeration.rs` |
| Invalid lengths, digits, indices and ranges are rejected; nothing is narrowed silently | `enumeration.rs`, `work_units.rs` |
| Work-unit JSON is byte-stable, indices are canonical decimal strings, IDs are deterministic | `work_units.rs` |
| The CPU search finds exactly the brute-force triple palindromes below 16^6 | `trindrome-cpu/tests/differential.rs` |

The oracle test was mutation-checked: swapping one alphabet digit makes it fail at length 3.

## Work units

`WorkChunk` serializes as JSON with decimal-string indices:

```json
{"schema_version":1,"generator_version":1,"hex_digits":65,"leading_hex_digit":1,"start_index":"0","end_index":"4294967296"}
```

Its identity is the canonical string `trindrome:g1:h65:1:0-4294967296`. It is built only from the generator version, length, leading digit and range, so any backend can complete the same unit. The spec allowed a hashed ID; a readable canonical string is just as deterministic and needs no hash dependency.

`GENERATOR_VERSION` must be bumped if the index mapping ever changes. Coverage recorded under one version says nothing about another.

## Deviations from the spec

- Integration tests live in each crate's `tests/` directory (the Cargo convention) rather than in a top-level `tests/`.
- `rug`/GMP is not used. It does not build with the MSVC toolchain, and `num-bigint` is sufficient for the reference.
