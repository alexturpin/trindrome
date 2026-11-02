# trindrome

A GPU-accelerated search for a [number that's palindromic](https://en.wikipedia.org/wiki/Palindromic_number) in binary, decimal, and hexadecimal.

Candidates come from Rick Regan's structure of binary/hexadecimal palindromes, so only the decimal property has to be tested. See [docs/algorithm.md](docs/algorithm.md) for the mathematics and [docs/architecture.md](docs/architecture.md) for how the worker stores progress. The plan lives in [spec.md](spec.md).

**Status:** Phases 1–2 are done: an exact, exhaustively tested CPU search and a resumable local worker. The CUDA backend (Phase 3) and Windows idle scheduling (Phase 4) are next.

## Setup (Windows 11)

- Rust stable with the MSVC toolchain (`winget install Rustlang.Rustup`)
- Visual Studio Build Tools with the C++ workload (needed by Rust, by the bundled SQLite and later by nvcc)
- From Phase 3 onwards: the CUDA Toolkit 13.x. Use a custom install and leave out the bundled driver.

```bash
cargo test --workspace
```

```bash
cargo build --release
```

## Usage

```bash
target/release/trindrome run --now --cpu
```

Copy `config.example.toml` to `trindrome.toml` to change defaults. While a worker runs, use these commands from another terminal:

| Command | Does |
|---|---|
| `trindrome status` | Worker state, throughput, historical and frontier progress, ETA, active chunks |
| `trindrome pause` / `resume` | Pause after the current batch, which releases its chunks, and resume later |
| `trindrome stop` | Checkpoint and exit (Ctrl+C in the worker's terminal does the same) |
| `trindrome coverage [--all]` | Searched index ranges per search space |
| `trindrome discoveries` | Verified matches (the trivial 1, 3, 5, 7 and 9 only appear with `minimum_hex_digits = 1`) |
| `trindrome verify` | Checks the database for gaps, overlaps and unverifiable discoveries |
| `trindrome benchmark` | CPU throughput on historical and frontier workloads (no database) |
| `trindrome config` | Prints the effective configuration |

Progress survives Ctrl+C, forced termination and crashes. At worst, the last uncommitted batches are searched again.

The "frontiers" here are lengths beyond Regan's published 2010 coverage, not necessarily beyond all later work.
