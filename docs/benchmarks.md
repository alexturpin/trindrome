# Benchmarks

All figures were measured, not projected. Reproduce them with `trindrome benchmark --seconds 15 --threads 8` on a release build.

## 2026-11-02 — RTX 3070 Laptop GPU, i7-12700H (Phase 3, first GPU kernel)

The GPU was otherwise idle. On the CPU side, the user's other applications were running. The GPU was at about 57–59 °C after each run, drawing about 31 W at idle afterwards.

| Workload | Kernel width | GPU (`cuda/0.1.0`) | CPU reference, 8 threads | Ratio |
|---|---|---:|---:|---:|
| Historical, 32 hex digits, leading 9 | 128-bit | **7.70B/s** | 13.96M/s | ~550× |
| Historical, 64 hex digits, leading 1 | 256-bit | **3.22B/s** | — | |
| Frontier A, 65 hex digits, leading 1 | 384-bit | **3.01B/s** | 10.33M/s | ~290× |
| Frontier B, 33 hex digits, leading 5 | 256-bit | **5.58B/s** | 14.23M/s | ~390× |
| 128 hex digits, leading 3 | 512-bit | **1.62B/s** | — | |

GPU rates cover the filter kernel plus survivor transfer, timed per launch at about 2^30 candidates per batch. Frontier B produced 70 survivors in 2.9×10^10 candidates, about 2.4×10^−9 each, matching the expected rate of about 10^−9 per possible leading digit. All were rejected by the exact CPU check. Spaces whose leading decimal digit is even produce no survivors at all, because every candidate is odd.

The single-thread CPU reference reaches about 3.5M/s at short lengths and about 1.3–1.8M/s at 130–260 bits, on a hybrid P/E-core CPU.

### Observations

- **Construction appears to dominate at long lengths.** The 64-digit space (31 free digits, 8 limbs) runs at about the same rate as the 65-digit one (32 free digits, 12 limbs). The 32-digit space (15 free digits, 4 limbs) is 2.4× faster. The kernel currently builds candidates in local memory, because nibble positions depend on the runtime length. Specialising kernels by hex length at compile time is the obvious next optimisation; measure it before adopting it.
- **Batches are well above launch overhead:** about 200 ms each, roughly 10^9 candidates.

## Historical replay (the same day)

A full replay of Regan's 2010 coverage, lengths 2–64 for leading 1/3 and 2–32 for 5/7/9/F, **28,633,115,294 candidates**:

- **19 s wall clock**, 6 s of active GPU time. The rest is chunk bookkeeping, `synchronous = FULL` commits and CPU audits.
- 28,633,070,976 candidates were searched on the GPU and 44,318 on the CPU (spaces too short for the filter).
- **Seven random audit slices** (204,800 candidates) were recomputed on the CPU. All checksums matched.
- **No nontrivial triple palindrome**, consistent with Regan. Per the addendum, this coverage is *GPU-searched with CPU-verified survivors and sampled audits*, not exhaustively CPU-verified.

## Projections from the measured rates

| Range | Candidates | At the measured rate |
|---|---:|---:|
| Frontier A (length 65, leading 1/3) | 2^33 ≈ 8.6×10^9 | about 3 s |
| Frontier B (length 33, leading 5/7/9/F) | 2^34 ≈ 1.7×10^10 | about 3 s |
| One day at about 3×10^9/s | about 2.6×10^14 ≈ 2^48 | reaches about length 95 (1/3) or about 47 (5/7/9/F) |

Every further length doubles the work (1/3) or quadruples it (5/7/9/F), so raw speed alone buys only a few lengths per order of magnitude. Further progress needs proven pruning (Phase 6).
