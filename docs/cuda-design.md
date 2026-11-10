# CUDA backend design

This is the design checkpoint the spec asks for before CUDA work starts. The governing rule:

> **The GPU only rejects.** It applies a filter that is exact and provably never rejects a decimal palindrome. The rare candidates that survive go to the CPU, whose exact check makes every acceptance decision.

## Integer representation

- A candidate is held in registers as `N` little-endian 32-bit limbs. The kernel is a template instantiated for `N = 4, 8, 12, 16`, i.e. 128, 256, 384 and 512 bits.
- The host chooses the smallest `N` with `32·N ≥ 4·n`. Length 65 (260 bits) uses `N = 12`. Lengths above 128 hex digits are rejected; nothing is truncated.
- Limbs are 32-bit because Ampere has no native 64-bit integer multiply, so 64-bit limbs would only be emulated with 32-bit instructions.

## Index-to-digit mapping

Identical to `trindrome-core` (see `algorithm.md`). The radix is 2 or 4, so free digit `i` (most significant first, prefix position `p = i + 1`) is selected by index bits `[(k−1−i)·b, (k−i)·b)`, where `b = log2(radix)`. Its alphabet digit is ORed into nibbles `p` and `n−1−p`, counting nibbles from the least significant. Odd lengths write the centre nibble twice, which is harmless. The leading digit goes into nibbles `0` and `n−1`.

Construction is bit placement only, with no arithmetic. A selection shift of 64 or more yields digit 0, which is correct because every GPU index is below 2^64.

## Decimal filter

Let `D` be the number of decimal digits of `v`, and let `W` be the window width (9 in production, `D ≥ 2W` required).

1. **Digit count.** Every candidate of a space lies in `[L·16^(n−1), (L+1)·16^(n−1))`, a ratio below 10, so the space contains at most one power of ten `T`. With `d` the digit count of the smallest candidate, `D = d + [v ≥ T]`.
2. **Low digits.** `v mod 10^9 = Σ limbᵢ · (2^(32i) mod 10^9) (mod 10^9)`, reduced every three terms so 64-bit sums cannot overflow. Then `low = v mod 10^W`.
3. **Reverse** the `W` digits of `low` to get `ρ`, so `ρ`'s most significant digit is `v`'s last digit.
4. **Window test.** Let `P = 10^(D−W)`. The top `W` digits of `v` equal `ρ` exactly when `ρ·P ≤ v < ρ·P + P`. Compute `ρ·P` in `N+1` limbs, then `diff = v − ρ·P` with borrow. **Pass iff there is no borrow and `diff < P`.**

**Soundness.** If `v` is a decimal palindrome, its top `W` digits read as an integer equal `ρ`, by definition. Because `D ≥ 2W` the two windows do not overlap, and every step is exact integer arithmetic, so a palindrome always passes. The filter is a pure function of `v`, and the Rust reference `trindrome_core::filter` implements the same steps.

`W` is a runtime parameter only so tests can force many survivors (`W = 1` passes roughly one candidate in nine). Production always uses 9, and a candidate survives with probability about 10^−9.

Spaces where any candidate has fewer than 18 digits (`n ≤ 15`) are not sent to the GPU. They total about 4 million candidates and the CPU finishes them in milliseconds.

## Kernel interface and result buffer

```text
search_N(params, start: u64, count: u64, window: u32,
         survivors: u32*, capacity: u32, counters: u64[3])
```

- **Grid-stride loop** over `[start, start + count)`, with 256 threads per block and a grid sized from the SM count.
- **Survivors** are written as `u32` offsets from `start`. The host keeps `count ≤ 2^32`.
- `counters[0]` is the survivor count. It keeps counting past `capacity`, so overflow is always detected.
- `counters[1]` is the number of candidates processed; each thread adds its own tally once. The host requires it to equal `count`.
- `counters[2]` is the batch checksum (below).

**Overflow:** if `counters[0] > capacity`, the batch is searched again on the GPU in halves, recursively, until each piece's survivors fit. Long spaces pass about 1.4 survivors per million candidates at 96 hex digits, so a 2^32 batch can overflow; the CPU reference would take most of an hour on it. A batch is committed only when its processed count matches, and never on a CUDA error.

**Host checks before launch:** the space is supported, `start + count` does not overflow a `u64` or exceed the space, `count ≤ 2^32`, and `N` fits the length.

## Checksum

For each candidate:

```text
a = low | D << 32 | pass << 40 | borrow << 41
b = (low32(v) − low32(ρ·P)) mod 2^32 | (XOR of all limbs of v) << 32
h = mix64(a ^ mix64(b ^ mix64(index)))      // mix64 = splitmix64 finalizer
checksum = Σ h  (wrapping)
```

The checksum depends on every candidate's construction (through the limb XOR), its digit count, its low digits and its window arithmetic. Because it is a sum keyed by the absolute index, it does not depend on thread order or on how a range is split. The CPU computes it with the Rust filter.

## Validation

1. **Rust filter versus specification.** On random values up to 512 bits, `pass` must equal the string condition "top `W` digits = reverse of low `W` digits". Every decimal palindrome must pass, for random palindromes of 18–154 digits and edge cases near powers of ten and with zeros at the window edges. The digit count must be exact around each space's power-of-ten threshold.
2. **GPU versus Rust:** construction output, survivor sets and checksums must be identical. The cases cover every family; 128-, 256-, 384- and 512-bit widths; the first and last indices of spaces; indices near 2^64; and `W = 1..3`, which fills the survivor buffer and deliberately overflows it.
3. **Runtime audits.** Every few batches, a random slice of the batch just searched is recomputed on the GPU and on the CPU (Rust filter), and the two checksums are compared before the batch is committed. A mismatch stops the worker with an error and commits nothing. Audits are recorded in the database.
4. **Every survivor** is rebuilt from its index and checked exactly on the CPU. GPU-searched coverage stays distinguishable from CPU-reference coverage through the chunk's backend.

Survivor checks alone cannot catch GPU false negatives. The checksum audits can, because they compare every candidate's intermediate values, not just the matches.

## Expected bottlenecks (to be replaced by measurements)

- Construction is about 2k bit operations. The residue sum is about `N` multiply-adds plus a few constant-divisor reductions, the reversal 9 constant divisions, and the window test about `3N` limb operations. Including the checksum, that comes to roughly 150–250 integer instructions per candidate.
- At roughly 3–4×10^12 32-bit integer operations per second on the RTX 3070 Laptop, the ceiling is about 10^10 candidates/s. My guess is 2–8×10^9/s sustained, before any tuning.
- Launch and synchronisation overhead is negligible at 200 ms batches. Host-side survivor verification is about one big-integer check per 10^9 candidates.

## Later additions

- **Pipelining.** The backend has two buffer slots on separate streams (`launch` and `collect`). The engine launches the next batch before collecting the current one, so survivor verification, audits, checkpoints and claims overlap the next kernel. Only collected batches with a matching processed count are ever recorded. A prefetched batch that is no longer wanted is discarded.
- **Per-length kernels** (`compute.kernel = "specialized"`, opt-in). `search_h<n>` is compiled for each hex length from 15 to 128, so every nibble position is a compile-time constant and the candidate is built in registers. A digit is computed as `(sel & 1)·a₁ + (sel >> 1)·a₂`, which relies on each radix-4 alphabet being `{0, a₁, a₂, a₁+a₂}` (`{0,2,5,7}`, `{0,6,9,F}`), instead of a table lookup. The filter and checksum code are shared with the generic kernels. The same differential test (survivors and checksums, including every limb) covers about 70 spaces in both modes, and a deliberately swapped digit formula is caught. Adding them grows the fatbin to 6.4 MB and the kernel build to about 20 s.

## Build and distribution

- `trindrome-cuda/build.rs` compiles `kernels/search.cu` with nvcc into one fatbin: `sm_86`, `sm_89` and `sm_120` machine code plus `compute_86` PTX for later GPUs. The fatbin is embedded in the binary.
- `build.rs` locates `cl.exe` through the `cc` crate and passes `-ccbin`, so no Developer Prompt is needed.
- Without a CUDA toolkit, the crate still builds and reports the backend as unavailable.
- `cudarc` loads the driver dynamically, so `trindrome.exe` runs on machines without CUDA and needs only the NVIDIA driver to use the GPU.
