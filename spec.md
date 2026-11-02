# Trindrome — GPU-Accelerated Multi-Base Palindrome Search

## Mission

Build a Windows-first, Rust-based mathematical search application that finds integers simultaneously palindromic in bases **2, 10, and 16**.

The application will use an NVIDIA GPU when the computer is idle, persist its progress across restarts, and eventually support distributed volunteer computing through Cloudflare infrastructure.

**Start with a fully local application. Do not implement distributed computing during the MVP.**

### Primary hardware

- Windows 11 x64
- NVIDIA RTX 3070 Laptop GPU
- Potential future desktop NVIDIA GPUs
- Future CPU-only workers on other hardware

### Primary goals

1. Independently reproduce Rick Regan's 2010 search results.
2. Continue searching beyond his published boundaries.
3. Maximize sustained GPU search throughput without disrupting gaming or ordinary computer use.
4. Run unattended for days, weeks, or months with reliable checkpointing.
5. Preserve a clean path toward distributed execution using Cloudflare Workers and Durable Objects.

### Non-goals

- No primality testing or prime-specific optimizations.
- No Cloudflare implementation initially.
- No networking requirement.
- No public dashboard initially.
- No GUI framework initially.
- No GPU overclocking or automatic power-limit modifications.
- No speculative mathematical pruning without a correctness proof.

---

# 1. Mathematical foundation

## Reference research

Read these articles before implementing the generator:

1. Rick Regan, *The Structure of Binary/Hexadecimal Palindromes*  
   https://www.exploringbinary.com/the-structure-of-binary-hexadecimal-palindromes/

2. Rick Regan, *Counting Binary/Hexadecimal Palindromes*  
   https://www.exploringbinary.com/counting-binary-hexadecimal-palindromes/

3. Rick Regan, *In Search of Decimal/Binary/Hexadecimal Palindromes*  
   https://www.exploringbinary.com/in-search-of-decimal-binary-hexadecimal-palindromes/

Do not substitute a generic hexadecimal palindrome generator.

Rick's structural discovery lets us generate integers already known to be palindromic in binary and hexadecimal. Only decimal palindromicity needs to be checked during the main search.

## 1.1 Exact candidate-generation rules

Every nonzero binary/hexadecimal palindrome has one of six possible leading hexadecimal digits.

Each leading digit determines a restricted alphabet for interior digits.

| Leading hex digit | Allowed interior digits |
|---|---|
| `1` | `0, 1` |
| `3` | `0, 3` |
| `5` | `0, 2, 5, 7` |
| `7` | `0, 2, 5, 7` |
| `9` | `0, 6, 9, F` |
| `F` | `0, 6, 9, F` |

This restriction is essential.

Mirroring arbitrary hexadecimal digits does not guarantee binary palindromicity. Mirroring digits under these specific rules does.

### Deterministic generation

Inputs:

- `n`: number of hexadecimal digits
- `L`: leading hexadecimal digit
- `index`: candidate index within the selected subset

Algorithm:

```text
alphabet = allowed_digits(L)
radix = length(alphabet)
k = floor((n - 1) / 2)

assert 0 <= index < radix^k

selection = index expressed in radix,
            left-padded to exactly k digits

prefix = [L]

for each digit d in selection,
    from most-significant to least:
    prefix.append(alphabet[d])

if n is even:
    result = prefix + reverse(prefix)
else:
    result = prefix + reverse(prefix excluding its last digit)
```

Interpret the resulting hexadecimal digit sequence as an unsigned integer.

All calculations must be exact.

Leading zeros are prohibited in the complete number, but zero-valued interior positions are allowed.

### Examples

```text
n = 3
L = 5
index = 1

alphabet = [0, 2, 5, 7]
prefix = 52
result = 525

0x525 = 0b10100100101
```

Both representations are palindromes.

```text
n = 4
L = 5
index = 1

prefix = 52
result = 5225

0x5225 = 0b101001000100101
```

Again, both representations are palindromes.

For odd lengths, do not duplicate the center digit.

### Generator invariants

The generator must satisfy:

- Every generated number is palindromic in bases 2 and 16.
- Every valid binary/hexadecimal palindrome appears in exactly one subset and at exactly one index.
- Each index maps to exactly one candidate.
- Candidate generation is deterministic across platforms.
- Candidate generation does not depend on the execution backend.
- Random access to any candidate index is supported.
- Enumeration does not require retaining previous candidates.

Implement an independent, brute-force oracle for small values to test completeness.

Do not rely exclusively on the same construction algorithm to validate itself.

## 1.2 Candidate counts

Define:

`k = floor((n - 1) / 2)`

For each leading digit:

- Leading 1 or 3: `2^k` candidates.
- Leading 5, 7, 9, or F: `4^k` candidates.

For all six leading-digit subsets combined:

`N(n) = 2 * 2^k + 4 * 4^k`

The counts for small lengths are:

| Hexadecimal digits | Total candidates |
|---|---:|
| 1 | 6 |
| 2 | 6 |
| 3 | 20 |
| 4 | 20 |
| 5 | 72 |
| 6 | 72 |

Use these as unit-test fixtures.

The single-digit values are useful for testing, but the project is seeking a nontrivial triple-base palindrome having at least two hexadecimal digits.

## 1.3 Decimal palindrome testing

For every generated binary/hexadecimal palindrome, determine whether its ordinary base-10 representation is also a palindrome.

The final check must be exact.

Start with an uncomplicated CPU implementation using arbitrary-precision integers and decimal string conversion.

Later investigate GPU-specific optimizations:

- Decimal digit-count calculation.
- Exact extraction of leading and trailing decimal digits.
- Precomputed powers of ten.
- Fixed-width multiprecision division.
- Constant-divisor optimizations.
- Early rejection based on differing outer decimal digits.
- Partial conversion instead of full decimal conversion.

Every rejection optimization must be mathematically sound.

Do not introduce probabilistic rejection filters that could discard a genuine result.

Do not apply prime-specific pruning rules. Even-length hexadecimal palindromes remain valid search candidates.

---

# 2. Historical reproduction and new search frontiers

## 2.1 Historical baseline

Rick Regan reported completing these ranges in April 2010:

| Search family | Hexadecimal lengths completed |
|---|---|
| Leading 1 or 3 | 1 through 64 |
| Leading 5, 7, 9, F | 1 through 32 |

He reported no nontrivial decimal/binary/hexadecimal palindrome in these ranges.

His largest reported individual search run processed approximately 4.295 billion candidates in almost 60 hours on an Intel Core Duo.

The cumulative historical coverage contains approximately **28.63 billion candidates**.

This distinction matters when estimating total replay time.

## 2.2 Start from scratch

The application must construct its own local coverage database.

Do not mark historical ranges as verified solely because Rick reported searching them.

The intended workflow is:

1. Exhaustively verify short lengths using the CPU oracle.
2. Implement GPU acceleration.
3. Benchmark representative large ranges.
4. Reproduce the historical search with durable progress tracking.
5. Independently confirm the absence of nontrivial solutions.
6. Continue beyond the historical frontier.

Historical reproduction is useful both as a correctness test and a meaningful performance benchmark.

If the GPU is slower than expected, allow smaller replay chunks and explicit selection of new frontiers. Do not hardcode the entire historical replay as a prerequisite for every possible exploratory run.

## 2.3 Historical replay throughput

Use measured throughput rather than hardware specifications to estimate completion time.

Illustrative rates:

| Rate | 2^32 candidates | Entire historical coverage |
|---|---|---|
| 100K/s | 11.9 hours | 3.3 days |
| 1M/s | 72 minutes | 8 hours |
| 10M/s | 7.2 minutes | 48 minutes |

These are mathematical projections, not guaranteed performance levels.

The CUDA implementation must be benchmarked before assigning a realistic ETA.

## 2.4 First unexplored frontiers

After reproducing the historical search, prioritize:

**Search family A**

- Hexadecimal length: 65
- Leading digits: 1, 3
- Total: `2^33` candidates
- Approximate volume: 8.59 billion

**Search family B**

- Hexadecimal length: 33
- Leading digits: 5, 7, 9, F
- Total: `2^34` candidates
- Approximate volume: 17.18 billion

Then continue in increasing hexadecimal length within each family.

Allow explicitly configured search priority and range selection.

Document that these are frontiers beyond Regan's published 2010 coverage, not necessarily frontiers beyond all subsequent researchers' work.

## 2.5 Critical integer-width consideration

A 64-digit hexadecimal number requires up to 256 bits.

However, the first unexplored 1/3-family length is **65 hexadecimal digits**, requiring up to 260 bits.

A GPU implementation restricted to 256-bit integers cannot explore that frontier.

Therefore:

- Implement exact arbitrary-precision arithmetic on the CPU.
- Support at least 512-bit unsigned candidate arithmetic on the GPU.
- It is acceptable to specialize GPU kernels for 128-, 256-, and 512-bit widths.
- Explicitly reject or fall back to CPU for larger values.
- Never silently truncate or wrap an integer.

The 512-bit GPU representation covers integers up to 128 hexadecimal digits, sufficient for the foreseeable search.

---

# 3. Technology stack

## Primary language

Rust, targeting native Windows 11 x64 with the MSVC toolchain.

Use Cargo workspaces.

Do not require WSL.

## GPU implementation

Use:

- Rust host application.
- CUDA C++ kernels.
- `cudarc` for Rust/CUDA interoperability.
- NVIDIA CUDA Toolkit for kernel compilation.
- NVIDIA Driver API for kernel execution.

Prefer a reproducible build producing versioned, compatible GPU artifacts.

The runtime should not require the complete CUDA development toolchain when distributing a precompiled binary, where practical.

## Supporting libraries

| Responsibility | Preferred solution |
|---|---|
| CLI | `clap` |
| Configuration | `serde` + TOML |
| Arbitrary-precision CPU arithmetic | `num-bigint` |
| Optional CPU optimization | GMP through `rug` |
| GPU execution | `cudarc` |
| NVIDIA telemetry | `nvml-wrapper` |
| Windows platform APIs | `windows` |
| Persistence | SQLite through `rusqlite` |
| Logging | `tracing` |
| Error handling | `thiserror`, `anyhow` |

Choose current compatible library versions and commit `Cargo.lock`.

Avoid introducing experimental native Rust GPU toolchains in the Windows MVP.

The core mathematical crate should remain platform-independent to support future Linux workers.

---

# 4. Application architecture

Separate deterministic mathematical computation from machine-specific orchestration.

Use four main responsibilities.

## 4.1 Search core

Owns:

- Generator rules.
- Work specifications.
- Candidate indexing.
- Palindrome validation.
- Exact CPU reference operations.
- Generator versioning.

Must not depend on:

- Windows.
- CUDA.
- SQLite.
- HTTP.
- Cloudflare.
- Idle detection.

## 4.2 Compute backends

Implement a CPU backend and CUDA backend that execute identical logical work ranges.

Conceptual interface:

```rust
fn search_chunk(
    work: &WorkChunk,
    backend: &impl SearchBackend,
) -> SearchResult
```

The exact API may differ to support streaming progress, cancellation, or incremental execution.

Use explicit batch-level boundaries so that cancellation and checkpointing do not require processing an entire large work unit.

## 4.3 Local work scheduler

Owns:

- Search-space selection.
- Chunk allocation.
- Completed-range tracking.
- Checkpoint progression.
- Failure recovery.
- Search priority.

Initially backed entirely by local SQLite.

## 4.4 Resource policy

Owns:

- User-idle detection.
- Game process detection.
- GPU availability.
- Thermal protection.
- Battery/AC policy.
- Sleep behavior.
- Pause/resume decisions.

The search algorithm must never depend on the resource-policy implementation.

Future distributed workers will have their own local resource policies.

---

# 5. Deterministic work-unit design

Work units are the foundation for eventual distribution.

Define the search problem independently of execution hardware.

Suggested structure:

```rust
struct WorkChunk {
    schema_version: u32,
    generator_version: u32,
    hex_digits: u32,
    leading_hex_digit: u8,
    start_index: BigUint,
    end_index: BigUint,
}
```

All index ranges are half-open:

`[start_index, end_index)`

For serialization, encode indices as canonical decimal strings.

This avoids JavaScript number precision problems in a future Cloudflare API.

Validate that:

- `start_index < end_index`.
- Both indices are inside the selected subset.
- The leading digit is permitted.
- The generator version is supported.
- The hexadecimal length is valid.

### Work-unit identity

Generate a deterministic ID from:

- Generator version.
- Hexadecimal length.
- Leading digit.
- Start and end indices.

Do not include the GPU model, execution backend, or optimization algorithm version in the identity.

A faster implementation must be capable of completing exactly the same logical work unit.

### Index representation

The current and near-term search spaces have manageable per-subset indices.

The local CUDA implementation may use efficient `u64` offsets where valid, while preserving arbitrary-precision logical indices in the host-side format.

Explicitly validate representability before launching kernels.

For future ranges beyond native GPU index capacity, either encode the full starting index as a digit array or split work into compatible subranges.

Never narrow an index silently.

### Execution results

A result should contain:

- Work-unit ID.
- Completed candidate count.
- Exact covered range.
- Potential discoveries.
- Active execution time.
- Backend and implementation version.
- Diagnostic counters.
- Error status, if applicable.

The completed-range claim must correspond to actual finished work.

---

# 6. CPU reference implementation

Implement this first.

Requirements:

- Exact candidate generation.
- Decimal palindrome validation.
- Independent binary and hexadecimal palindrome checks.
- Arbitrary-precision integer support.
- Deterministic indexing.
- Search over explicit ranges.
- Unit and integration tests.

### Mandatory correctness tests

**Exhaustive small-space comparison**

For all integers representable using up to five hexadecimal digits:

1. Independently check whether each integer is palindromic in binary and hexadecimal.
2. Generate the corresponding sets with the optimized generator.
3. Assert set equality.

**Known examples**

Include:

- `0x525`
- `0x5225`
- `0x70207`
- `0xF060F`

Verify their binary and hexadecimal palindromicity independently.

**Enumeration tests**

Verify:

- Exact subset counts.
- Odd/even length behavior.
- Stable index mapping.
- No duplicates.
- First and last candidate.
- Index-range splitting.
- Invalid input rejection.
- Cross-platform serialization.

**Decimal checking**

Include positive and negative decimal palindrome examples and values exceeding 128 bits.

The CPU implementation remains the authoritative reference for GPU differential testing.

---

# 7. CUDA backend

## 7.1 Core execution

The GPU should receive:

- Search family and length.
- Starting index.
- Candidate count.
- Necessary constants.

Each CUDA thread processes one or more candidate indices.

Prefer grid-stride enumeration.

Generate candidates directly on the GPU rather than constructing and transferring a massive candidate array from the CPU.

Keep memory consumption bounded.

## 7.2 Candidate representation

Implement unsigned multiprecision integers using fixed arrays of 32-bit or 64-bit limbs.

Benchmark both approaches if practical.

Initially prioritize correctness with a 512-bit representation.

Optimize candidate construction and decimal digit operations only after differential tests pass.

## 7.3 Decimal checking

Begin with a correct GPU implementation that mirrors CPU semantics.

Investigate progressively cheaper rejection stages.

For example:

1. Generate exact candidate.
2. Determine decimal digit count exactly.
3. Compare outer decimal digits.
4. Continue comparing toward the center.
5. Report a candidate only when all digits match.

Avoid full decimal string conversion if exact arithmetic permits something faster.

However, benchmark before adopting more complex methods.

Integer division and multiprecision operations may dominate execution.

GPU throughput cannot be predicted reliably from CUDA core counts or FP32 performance.

## 7.4 GPU result reporting

Prefer returning candidate indices rather than complete large integers.

The CPU reconstructs every reported candidate independently and verifies its representations in all three bases.

Use a bounded result buffer and explicit overflow detection.

Never silently discard matches because a result buffer filled.

If the buffer overflows, rerun the affected range with a larger buffer or CPU verification.

## 7.5 Short execution batches

Target GPU kernel execution times of approximately 100–250 milliseconds initially.

Make the target configurable and benchmark it.

Use a short CUDA launch followed by a host-side scheduling decision.

Do not rely on CUDA kernel cancellation for ordinary pause behavior.

Avoid long-running kernels and do not change Windows GPU timeout registry settings.

The actual pause latency also depends on host scheduling and synchronization overhead.

## 7.6 Differential verification

For selected work units, run both CPU and GPU implementations.

Verify:

- Identical generated candidate indices.
- Identical candidate values.
- Identical decimal acceptance decisions.
- Identical match sets.
- Correct completed candidate counts.

Include high-index and large-width cases.

Periodically run sampled CPU/GPU differential checks during long searches, not just during development.

Verifying only GPU-reported matches cannot detect GPU false negatives, so broader differential testing is necessary.

---

# 8. Durable local checkpointing

Use SQLite.

No cloud connection is needed.

## Data model

Suggested tables:

### `search_spaces`

- Generator version.
- Hexadecimal length.
- Leading digit.
- Total candidates.
- Search priority.
- Completion state.

### `work_chunks`

- Work-unit ID.
- Search-space identity.
- Start index.
- End index.
- Next unfinished index.
- State.
- Attempt count.
- Last checkpoint.
- Completion timestamp.

### `discoveries`

- Exact integer value.
- Binary representation.
- Decimal representation.
- Hexadecimal representation.
- Verification state.
- Work-unit identity.
- Discovery timestamp.

### `performance_samples`

- Hardware identifier.
- Backend.
- Integer width.
- Search family.
- Candidates processed.
- Active duration.
- Telemetry and throughput.

### `worker_state`

- User-selected state.
- Current workload.
- Pause reason.
- Recent errors.
- Lifetime statistics.

## Checkpoint semantics

The worker must never checkpoint progress ahead of computation that has actually completed.

For each batch:

1. Launch computation.
2. Wait for the batch to complete.
3. Collect and verify reported matches.
4. Atomically persist any discoveries and the new cursor.
5. Advance to the next batch.

If the process crashes before committing, repeat that batch.

Duplicate work is acceptable.

Skipped work is not.

Use SQLite transactions, unique discovery constraints, and appropriate journaling settings.

The database must remain consistent across normal exits, Ctrl+C, forced termination, shutdown, and GPU errors.

## Coverage tracking

Track which index intervals have been fully verified.

Do not infer global completion from a single running cursor unless all previous intervals are demonstrably complete.

Support sparse coverage and merging adjacent completed intervals.

This becomes important when work is eventually assigned to multiple computers.

---

# 9. BOINC-inspired Windows idle scheduling

The GPU worker must yield to user activity, especially games.

Use an explicit local scheduling state machine:

- Disabled.
- Waiting for idle.
- Running.
- Pausing.
- Paused.
- Error.

## 9.1 Start policy

Default conditions:

- User idle for five minutes.
- GPU utilization below 10% over a recent sampling interval.
- No excluded gaming/application process running.
- AC power connected.
- GPU temperature below configured threshold.
- No manual pause requested.
- Resource monitoring functioning.

If automatic idle monitoring fails, do not blindly start consuming the GPU.

## 9.2 Windows APIs

Use `GetLastInputInfo()` for keyboard/mouse idle detection.

Note that it reports activity for the calling session, not every Windows session.

Use process enumeration for excluded executables.

Use `GetSystemPowerStatus()` for battery/AC state.

Use NVML for supported NVIDIA GPU metrics:

- Utilization.
- Temperature.
- Available memory.
- Power usage, where supported.

Some telemetry capabilities vary by driver and GPU.

Represent unsupported measurements explicitly.

## 9.3 Game detection

Keyboard/mouse idle time is insufficient for controller-based gaming.

Implement configurable executable exclusions.

Example:

```toml
exclusive_gpu_apps = [
  "FlightSimulator2024.exe",
  "game.exe"
]
```

Detect excluded processes regardless of whether they are foreground or background.

Allow full-path matching where appropriate.

Optional future improvements:

- XInput controller activity.
- Other supported controller APIs.
- Foreground/fullscreen application detection.
- Per-process GPU usage.

Avoid building a complicated game-recognition database in the MVP.

## 9.4 Startup versus running utilization

Total GPU utilization should primarily determine whether to **start**.

Once the worker is active, its own CUDA execution will raise GPU utilization.

Do not implement a naive rule that pauses whenever total GPU usage exceeds 10%.

While running, prioritize:

- User input.
- Excluded applications.
- Manual pause.
- Power transitions.
- Thermal limits.
- Explicitly detected competing GPU workloads.

## 9.5 Pause behavior

On a pause request:

1. Stop launching new GPU batches.
2. Finish or safely abandon the current bounded batch.
3. Persist completed work.
4. Release unnecessary GPU resources.
5. Transition to a paused state.
6. Resume only after the configured conditions are satisfied again.

Avoid rapid start/stop cycles through scheduling hysteresis.

The user must be able to force an immediate logical pause without waiting for an entire multi-minute work chunk.

---

# 10. Windows power and sleep behavior

Support two modes.

## Normal mode — default

- Allow Windows to sleep.
- Preserve checkpoints.
- Resume automatically after wake when policies permit.
- Do not wake the computer for computing.
- Respect laptop lid closure.

## Overnight mode — opt-in

Allow the application to prevent automatic system sleep while it is actively computing on AC power.

Use `SetThreadExecutionState` with `ES_CONTINUOUS | ES_SYSTEM_REQUIRED`.

Do not force the display to remain on.

Clear the execution-state request whenever computing pauses or stops.

Never intentionally override explicit user sleep, shutdown, or lid-close actions.

Handle power/suspend notifications where appropriate, but do not depend on receiving them for durable progress.

## Thermal and resource limits

Provide configurable limits and hysteresis.

Suggested initial values:

- Pause at 80°C GPU temperature.
- Resume below 72°C.
- AC power required.
- No automatic clock or voltage changes.

These are configurable starting defaults, not universal hardware safety specifications.

Benchmark whether reducing sustained intensity improves efficiency, temperature, or responsiveness.

---

# 11. CLI application

Working executable name:

`trindrome.exe`

Implement a straightforward CLI.

Suggested commands:

```text
trindrome run
trindrome run --now
trindrome run --cpu
trindrome pause
trindrome resume
trindrome status
trindrome benchmark
trindrome verify
trindrome coverage
trindrome discoveries
trindrome config
```

## Automatic operation

`trindrome run`

Starts the persistent local worker with idle and safety policies enabled.

## Immediate execution

`trindrome run --now`

Starts immediately for benchmarking or deliberate manual execution, bypassing normal inactivity requirements without silently bypassing hard thermal and hardware safety restrictions.

## Status reporting

Display:

- Worker state and pause reason.
- Current GPU.
- Current search family.
- Hexadecimal length.
- Candidate index and work-unit range.
- Current and average throughput.
- Total independently completed candidates.
- Active search time.
- Elapsed wall-clock time.
- Coverage percentage.
- Last checkpoint.
- Estimated completion time, when meaningful.
- Verified discoveries.

Provide aggregate progress separately for historical reproduction and unexplored ranges.

## Startup

Support optional Windows Task Scheduler startup at interactive user login.

Do not require administrative privileges for normal operation.

Do not use a Windows service for the initial implementation.

Ensure a single active worker controls a given local database/GPU assignment.

For CLI pause/resume/status communication, use a simple reliable local mechanism, such as named-pipe IPC or an appropriately synchronized SQLite control record.

Avoid building a network server simply to support local commands.

---

# 12. Suggested configuration

```toml
[compute]
backend = "auto"
gpu_device = 0
batch_target_ms = 200
checkpoint_interval_seconds = 30

[idle]
enabled = true
idle_minutes = 5
gpu_start_threshold_percent = 10
gpu_idle_sample_seconds = 30
require_ac_power = true
exclusive_gpu_apps = []

[thermal]
enabled = true
pause_temperature_c = 80
resume_temperature_c = 72

[power]
prevent_automatic_sleep = false

[search]
mode = "historical_replay"
minimum_hex_digits = 2
stop_on_first_verified_discovery = true

[storage]
database_path = "data/search.sqlite"

[logging]
level = "info"
```

Use validated defaults and clearly report unsupported settings.

Do not allow changes to performance or scheduling configuration to invalidate existing coverage.

Search-space identity and generator versions must remain stable.

---

# 13. Performance optimization

Performance is a major project objective, but all optimization decisions must be benchmark-driven.

Measure:

- CPU candidates/second.
- GPU candidates/second.
- Throughput for each search family.
- Throughput by integer width.
- Kernel duration.
- Host/device transfer overhead.
- CPU overhead during GPU work.
- Memory usage.
- GPU utilization.
- GPU power draw, if available.
- Candidates per joule where measurable.
- Checkpoint overhead.

Do not confuse GPU-only power measurements with whole-system energy use.

## Optimization order

1. Correctness.
2. Candidate generation overhead.
3. Decimal palindrome rejection cost.
4. Multiprecision integer representation.
5. CUDA memory usage and occupancy.
6. Launch/batch overhead.
7. Sustainable throughput.
8. Energy efficiency.

Benchmark the actual RTX 3070 Laptop GPU.

Compare against the same CPU reference workloads.

Do not assume a GPU is faster for every integer width or algorithm.

Allow selecting the backend that performs best for a given workload.

Every optimization must preserve exact search semantics.

---

# 14. Future Cloudflare distribution

This section defines future compatibility requirements.

**Do not implement Cloudflare services in the MVP.**

The eventual distributed application should use the same deterministic search engine and work-unit format.

## Proposed Cloudflare architecture

### Cloudflare Workers

Provide authenticated HTTPS endpoints for:

- Worker registration.
- Work acquisition.
- Lease renewal.
- Progress reporting.
- Result submission.
- Public coverage statistics.

Do not use Cloudflare Workers themselves to perform the intensive mathematical search.

### Cloudflare Durable Objects

Use a Durable Object as the authoritative coordinator for work assignment.

Prefer SQLite-backed Durable Object storage.

Responsibilities:

- Track available work.
- Assign work atomically.
- Issue leases.
- Track completions.
- Handle expired leases.
- Prevent lost coverage.
- Accept idempotent submissions.

Start with one logical coordinator.

Partition by search family or length only if necessary.

### Cloudflare D1

Optional later addition for:

- Public statistics.
- Searchable historical results.
- Dashboard queries.
- Analytics.

Do not introduce D1 automatically if the Durable Object's SQLite storage already suffices.

### Cloudflare R2

Optional storage for:

- Verified discovery records.
- Periodic coverage snapshots.
- Audit datasets.
- Distributed search archives.

### Cloudflare Queues

Optional asynchronous processing for:

- Result verification.
- Notifications.
- Statistics aggregation.

Queues should not independently determine authoritative work ownership.

## 14.1 Distributed execution semantics

Use leased work allocation.

Conceptual flow:

1. A worker determines that its computer is available.
2. The worker requests a deterministic chunk.
3. The coordinator assigns the chunk with a lease.
4. The worker executes it locally.
5. The worker checkpoints locally.
6. The worker renews the lease while necessary.
7. The worker submits completed results.
8. The coordinator records completion idempotently.

If a lease expires, the work may be reassigned.

A worker that resumes after a disconnection must tolerate its previous work being reassigned.

If the user starts gaming, the local resource policy takes priority over the coordinator.

The worker may pause or relinquish its lease.

## 14.2 Distributed correctness

Require:

- Stable generator versions.
- Cross-platform serialization.
- Exact large integer indices.
- Deterministic work ranges.
- Unique work IDs.
- Atomic assignments.
- Lease expiration.
- Idempotent completion handling.
- Duplicate detection.
- Independent verification of discoveries.
- Optional redundant execution of work chunks.

A malicious or defective remote worker could falsely report that a chunk contains no matches.

Validating only positive discoveries does not address this.

For future public volunteer participation, consider redundant independent processing, sampling, reputation, or other verification strategies.

## 14.3 Prepare locally without implementing networking

The MVP must:

- Use deterministic work-unit IDs.
- Maintain exact interval coverage.
- Avoid machine-specific assumptions in the search core.
- Serialize work and results in a portable way.
- Keep scheduling separate from computation.
- Support multiple independently executable chunks.
- Preserve generator and schema versioning.

The MVP must not:

- Introduce Cloudflare credentials.
- Implement HTTP clients.
- Add speculative lease machinery.
- Require online accounts.
- Depend on a remote database.
- Implement volunteer reputation systems.

Local-first correctness is the priority.

---

# 15. Recommended repository structure

```text
trindrome/
  Cargo.toml
  Cargo.lock
  README.md
  config.example.toml

  crates/
    trindrome-core/
      src/
        lib.rs
        generator.rs
        palindrome.rs
        work.rs
        verification.rs

    trindrome-cpu/
      src/
        lib.rs

    trindrome-cuda/
      src/
        lib.rs
      kernels/
        search.cu

    trindrome-worker/
      src/
        main.rs
        cli.rs
        scheduler.rs
        idle.rs
        gpu_monitor.rs
        power.rs
        persistence.rs
        configuration.rs

  tests/
    fixtures/
    integration/

  scripts/
    setup-windows.ps1
    benchmark.ps1

  docs/
    algorithm.md
    architecture.md
    historical-coverage.md
    benchmarks.md
    distributed-design.md
```

Keep module boundaries simple.

Avoid creating abstraction layers without an immediate purpose.

---

# 16. Implementation roadmap

## Phase 1 — Correct CPU search

- [ ] Create Rust workspace.
- [ ] Implement exact candidate generator.
- [ ] Implement arbitrary-precision integer support.
- [ ] Implement independent palindrome checks.
- [ ] Implement deterministic index mapping.
- [ ] Add exhaustive brute-force comparison tests.
- [ ] Add published known examples.
- [ ] Verify candidate-count formulas.
- [ ] Document invariants.

**Deliverable:** A tested CPU search engine that can enumerate and validate arbitrary work ranges.

## Phase 2 — Checkpointing and CLI

- [ ] Implement SQLite schema.
- [ ] Implement search-space initialization.
- [ ] Implement chunk allocation.
- [ ] Implement durable progress tracking.
- [ ] Implement recovery after interruption.
- [ ] Implement coverage reporting.
- [ ] Implement basic CLI.
- [ ] Prevent accidental concurrent execution.

**Deliverable:** A resumable CPU search application.

## Phase 3 — CUDA backend

- [ ] Set up native Windows CUDA toolchain.
- [ ] Integrate `cudarc`.
- [ ] Implement GPU candidate generation.
- [ ] Implement 512-bit exact integer support.
- [ ] Implement decimal palindrome checks.
- [ ] Implement bounded CUDA batches.
- [ ] Implement GPU result collection.
- [ ] Add CPU verification of reported matches.
- [ ] Add CPU/GPU differential tests.
- [ ] Benchmark actual throughput.

**Deliverable:** A correct GPU-accelerated search backend.

## Phase 4 — Passive Windows operation

- [ ] Implement `GetLastInputInfo` integration.
- [ ] Implement game/application exclusions.
- [ ] Implement NVIDIA telemetry.
- [ ] Implement AC-power detection.
- [ ] Implement thermal monitoring.
- [ ] Implement scheduler state machine.
- [ ] Implement pause/resume commands.
- [ ] Implement sleep management.
- [ ] Implement optional startup at login.
- [ ] Test gaming interference and controller usage.

**Deliverable:** A reliable background GPU search worker.

## Phase 5 — Historical search reproduction

- [ ] Benchmark historical-size workloads.
- [ ] Initialize complete historical search spaces.
- [ ] Search leading 1/3 through 64 hex digits.
- [ ] Search leading 5/7/9/F through 32 hex digits.
- [ ] Verify all coverage records.
- [ ] Run periodic CPU/GPU differential audits.
- [ ] Publish reproducible benchmark and coverage summaries.

**Deliverable:** An independently verified reproduction of Regan's reported 2010 coverage.

## Phase 6 — Explore new frontiers

- [ ] Begin 65-digit 1/3-family search.
- [ ] Begin 33-digit 5/7/9/F-family search.
- [ ] Benchmark deeper 512-bit workloads.
- [ ] Improve decimal rejection performance.
- [ ] Investigate mathematically proven pruning.
- [ ] Continue increasing supported lengths.
- [ ] Track active runtime and cumulative coverage.
- [ ] Verify and record any discoveries.

**Deliverable:** New independently verified search coverage beyond the historical baseline.

## Phase 7 — Distributed system (future)

- [ ] Implement Cloudflare coordinator.
- [ ] Add remote work acquisition.
- [ ] Add leased assignments.
- [ ] Add authenticated worker reporting.
- [ ] Add distributed checkpoint reconciliation.
- [ ] Add result verification strategy.
- [ ] Add multi-machine workers.
- [ ] Add optional public dashboard.

Do not begin Phase 7 until the local search engine is demonstrated to be correct, stable, and worth scaling.

---

# 17. Agent development requirements

The coding agent should work incrementally and validate each major phase.

Requirements:

1. Prefer functioning, tested code over speculative architecture.
2. Read and understand Regan's mathematical construction before implementing it.
3. Use independent tests to prove generator completeness for small ranges.
4. Ensure every optimization preserves exact results.
5. Never silently truncate integers or indices.
6. Do not assume CUDA acceleration is guaranteed.
7. Benchmark before introducing sophisticated optimizations.
8. Ensure every checkpoint represents completed computation.
9. Preserve responsive pause behavior on the gaming computer.
10. Keep the mathematical core portable.
11. Do not implement Cloudflare infrastructure during the MVP.
12. Document setup instructions for Windows 11.
13. Keep the architecture and benchmark documentation current.
14. Report unsupported hardware or execution modes clearly.
15. Prefer simple, maintainable Rust.

### Before implementing CUDA

Produce a short implementation note explaining:

- Proposed 512-bit integer representation.
- Candidate index-to-digit mapping.
- Exact GPU decimal palindrome algorithm.
- CUDA result-buffer design.
- Overflow handling.
- CPU/GPU correctness validation.
- Estimated computational bottlenecks.

This is a design checkpoint, not a reason to stop other independent implementation work.

### Definition of MVP completion

The application is complete when:

- CPU enumeration is exhaustively tested against independent small-space searches.
- CUDA and CPU produce identical results on differential tests.
- The GPU supports the intended historical and first unexplored frontiers.
- Search progress survives unexpected termination.
- Search coverage cannot silently skip indices.
- Windows idle scheduling reliably pauses for ordinary use.
- Game process exclusions work.
- Laptop power and sleep preferences are respected.
- GPU temperature and execution duration are bounded appropriately.
- Benchmark results are reproducible.
- Historical and new search coverage are clearly distinguishable.
- Future distribution can reuse the same deterministic work units.
- No remote infrastructure is required.

**Guiding principle:** Reproduce the mathematics correctly, measure the real GPU performance, build a reliable idle worker, and only then expand into a long-running search. Cloudflare distribution is a future scaling mechanism, not part of the initial solution.

--------
ADDENDUM

## Architecture Clarifications — Verification and CPU Fallback

These clarifications supplement the existing Trindrome plan. Do not rewrite the plan or expand MVP scope.

### GPU correctness and verification

Distinguish three levels of confidence:

1. **Correctness testing:** Exhaustive small-space tests and randomized CPU/GPU differential tests, including boundary cases and large integer widths.
2. **Runtime verification:** Independently verify every GPU-reported discovery on the CPU. Periodically audit randomly selected completed GPU ranges using the CPU reference implementation.
3. **Search coverage:** A successfully completed GPU chunk counts as computationally searched, but not as exhaustively CPU-verified.

Recognize that positive-result verification cannot detect GPU false negatives. Periodic audits mitigate this risk without eliminating it. Record the distinction in coverage metadata and documentation.

### CPU fallback policy

- `auto`: Prefer CUDA. If CUDA becomes unavailable, fails, or cannot represent the requested integer width, pause and report the issue rather than silently switching to intensive CPU execution.
- `cpu`: Explicit CPU execution, respecting its own configurable concurrency and idle policies.
- `cuda`: Require CUDA and report unsupported workloads clearly.
- CPU verification tasks may run independently of the selected search backend.
- Never mark unsupported or failed chunks complete.
- Reserve automatic CPU fallback for future explicitly configured headless workers.

### Scope control

Treat these as implementation clarifications, not reasons to introduce new frameworks or infrastructure.

Prioritize correctness, measurable throughput, checkpoint reliability, and unobtrusive Windows execution. Defer nonessential design refinements until benchmarks or actual operational failures justify them.