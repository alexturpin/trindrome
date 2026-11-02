# Architecture

```text
trindrome-core     generator, palindrome checks, verification, WorkChunk   (no platform deps)
trindrome-cpu      exact reference search over an index range               (depends on core)
trindrome-cuda     GPU exact rejection filter, kernels + cudarc host side   (depends on core)
trindrome-worker   trindrome.exe: config, SQLite store, engines, scheduler, CLI
```

The mathematics knows nothing about scheduling, and scheduling knows nothing about Windows yet. Idle detection, GPU telemetry and power handling arrive in Phase 4, as a separate resource policy that only decides *whether* to run.

## Persistence (`persistence.rs`)

A single SQLite database, in WAL mode with `synchronous = FULL`.

| Table | Holds |
|---|---|
| `search_spaces` | One row per (generator version, hex length, leading digit), with its family (`historical` or `frontier`), total candidates and allocation cursor `next_unallocated`. |
| `work_chunks` | Allocated ranges `[start_index, end_index)` with a committed cursor `next_index`, state (`pending`, `running` or `complete`), attempts, backend and active time. |
| `discoveries` | Every verified match (trivial ones too, flagged), keyed by decimal value. |
| `performance_samples` | One row per checkpoint: hardware, backend, integer width, candidates and active time. |
| `audits` | GPU/CPU checksum comparisons of random slices of GPU batches (only matched ones are recorded; a mismatch stops the worker). |
| `worker_state` | The requested state (`run`, `pause` or `stop`, set by the CLI), the observed state, a heartbeat and the last error. |

Indices are stored as canonical decimal strings, the same encoding the work units use.

**Coverage is derived, not separately recorded.** The covered part of a space is the union of `[start_index, next_index)` over its chunks, merged in Rust. Coverage can therefore never disagree with the chunk ledger, and sparse or out-of-order completion (as future distributed workers will produce) merges naturally. `trindrome verify` checks that each space's chunks tile `[0, next_unallocated)` exactly, that every cursor lies inside its chunk, that every state matches its cursor, that every ID is canonical, and that every discovery reverifies from its index.

### Checkpoint rule

A checkpoint is one `BEGIN IMMEDIATE` transaction that:

1. Requires the chunk to be `running`, with its stored cursor equal to the start of the reported progress. The reported candidate count must equal the length of the range.
2. Rebuilds every reported match from its index and rechecks all three bases independently, then inserts it (`INSERT OR IGNORE`, unique by value).
3. Advances `next_index`, marking the chunk `complete` if it reached the end, and records a performance sample.

The cursor is only ever moved to the end of work that has already finished. A crash loses at most the uncommitted batches, which are then searched again; nothing is skipped.

Commits happen when a batch produced a match, when a chunk finishes, when pausing or stopping, and otherwise at most every `checkpoint_interval_seconds`. The spec suggested committing every batch; committing on an interval keeps the database write rate low without weakening the rule, because discoveries are never held back.

## Scheduler (`scheduler.rs`)

- **One worker per database.** `trindrome run` takes an exclusive OS lock on `<database>.lock` for its lifetime. `status` uses the same lock to tell whether a worker is really alive, rather than trusting the heartbeat.
- **Threads coordinate through SQLite.** Each search thread opens its own connection and claims chunks in `BEGIN IMMEDIATE` transactions, just as independent processes or future remote workers would.
- **Claim order.** Interrupted (`pending`) chunks resume first. Otherwise a new chunk is allocated from the next eligible space, ordered by historical before frontier, then fewest candidates, then length, then leading digit, so both alphabet tracks advance together.
- **The frontier opens lazily.** When every frontier space of a track (1/3, or 5/7/9/F) is fully allocated, the next length of that track is registered, starting at 65 or 33.
- **Recovery.** At startup, chunks left `running` by a crash or forced kill go back to `pending` with their committed cursor intact.
- **Batches** are sized from the measured rate to about `batch_target_ms`. They shrink immediately when one runs long, and grow to at most twice what was actually searched, so a batch cut short at a chunk boundary cannot inflate the size and with it the pause latency.
- **Pause, resume and stop.** The CLI writes `worker_state.requested`, and the main thread polls it every 250 ms. On pause or stop, each search thread finishes its current batch, commits, and releases its chunk back to `pending`. A pause persists across restarts; a stop is one-shot. Ctrl+C acts as a stop, and a second Ctrl+C exits immediately, which is safe because of the checkpoint rule.

## Engines and backend policy (`engine.rs`)

Each search thread owns one engine, which turns a batch `[start, end)` into CPU-verified matches.

- **CPU engine:** the exact reference, one per CPU thread.
- **CUDA engine:** one thread. It launches the filter kernel, then:
  - requires the kernel's processed count to equal the batch size;
  - rebuilds every survivor from its index and checks it exactly on the CPU;
  - if the survivor buffer overflowed, searches the whole batch again on the CPU;
  - every `audit_every_batches` batches, recomputes a random slice on the GPU and with the Rust reference filter. A checksum mismatch fails the batch before anything is committed.
- **Short spaces on a GPU worker.** Spaces too short for the filter (about 15 hex digits or fewer, roughly 4M candidates in all) are searched inline by the CPU reference.

Matched audits are stored in `audits` within the same checkpoint transaction. `work_chunks.backend` lists every backend that contributed to a chunk, and `trindrome coverage` reports searched candidates by backend. This keeps GPU-searched coverage distinguishable from CPU-reference coverage, as the addendum requires.

`compute.backend = "auto"` (or `"cuda"`) never silently switches to intensive CPU execution: if CUDA is unavailable, `run` reports why and suggests `--cpu`. With idle detection not yet implemented, automatic mode refuses to start; run with `--now`, or set `idle.enabled = false`. GPU temperature limits arrive with NVML in Phase 4, and until then only the GPU's own thermal protection applies.
