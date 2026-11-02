# Architecture

```text
trindrome-core     generator, palindrome checks, verification, WorkChunk   (no platform deps)
trindrome-cpu      exact reference search over an index range               (depends on core)
trindrome-cuda     GPU exact rejection filter, kernels + cudarc host side   (depends on core)
trindrome-worker   config, SQLite store, engines, resource policy, scheduler
  ├─ trindrome.exe        console CLI
  └─ trindrome-agent.exe  the worker on a background thread + tray icon, no console window
trindrome-settings → trindrome-settings.exe  egui settings window (opened from the tray)
```

The settings window is a separate process, so the always-running agent carries no GUI framework, only a tray icon and a message loop. The window never talks to the agent directly. It edits `trindrome.toml` through `settings_file.rs`, which preserves comments and other keys and validates the complete configuration before atomically replacing the file. It reads status from the database, and the agent's live reload does the rest.

The mathematics knows nothing about scheduling. The resource policy only decides *whether* to run, never *what* to compute.

## Processes and files

All state lives in one per-user data directory, `%LOCALAPPDATA%\Trindrome` (or `TRINDROME_HOME`), so the CLI, the agent and a future GUI always share it:

| File | Purpose |
|---|---|
| `trindrome.toml` | Configuration. Optional; edited by hand or later by the GUI, and **reloaded live** when it changes. |
| `search.sqlite` (+ `.lock`) | All progress, discoveries, audits and worker state. |
| `logs\agent.log.<date>` | The agent's daily log. The CLI logs to stderr. |

`--config` or `TRINDROME_CONFIG` selects a different configuration file, and the database path is resolved relative to it.

**`trindrome-agent.exe`** is built for the Windows GUI subsystem, so Windows never gives it a console window. `trindrome startup enable` registers it under `HKCU\Software\Microsoft\Windows\CurrentVersion\Run`. That needs no administrator rights, and users can see it in Task Manager → Startup apps. It runs in the user's session, which idle detection requires, so it is deliberately not a Windows service. The spec suggested Task Scheduler; the `Run` key is the convention for tray applications, and the agent's idle wait already provides the delayed start Task Scheduler would add.

The CLI controls a running agent exactly as it controls a worker it started itself: `status [--json]`, `pause`, `resume` and `stop` all go through the database.

## Persistence (`persistence.rs`)

A single SQLite database, in WAL mode with `synchronous = FULL`. Schema version 2; version 1 databases migrate automatically on open.

| Table | Holds |
|---|---|
| `search_spaces` | One row per (generator version, hex length, leading digit), with its family (`historical` or `frontier`), total candidates and allocation cursor `next_unallocated`. |
| `work_chunks` | Allocated ranges `[start_index, end_index)` with a committed cursor `next_index`, state (`pending`, `running` or `complete`), attempts, contributing backends and active time. |
| `discoveries` | Every verified match (trivial ones too, flagged), keyed by decimal value. |
| `performance_samples` | One row per checkpoint: hardware, backend, integer width, candidates and active time. |
| `audits` | GPU/CPU checksum comparisons of random slices of GPU batches. Only matched ones are recorded; a mismatch stops the worker. |
| `worker_state` | The requested state (`run`, `pause` or `stop`), the observed state, a human-readable reason, a stable `reason_code`, JSON `detail`, a heartbeat and the last error. |

Indices are stored as canonical decimal strings, the same encoding the work units use.

**Coverage is derived, not separately recorded.** The covered part of a space is the union of `[start_index, next_index)` over its chunks, merged in Rust. Coverage can therefore never disagree with the chunk ledger, and sparse or out-of-order completion (as future distributed workers will produce) merges naturally (see [distributed.md](distributed.md)). `trindrome verify` checks that each space's chunks tile `[0, next_unallocated)` exactly, that every cursor lies inside its chunk, that every state matches its cursor, that every ID is canonical, and that every discovery reverifies from its index.

### Checkpoint rule

A checkpoint is one `BEGIN IMMEDIATE` transaction that:

1. Requires the chunk to be `running`, with its stored cursor equal to the start of the reported progress. The reported candidate count must equal the length of the range.
2. Rebuilds every reported match from its index and rechecks all three bases independently, then inserts it (`INSERT OR IGNORE`, unique by value).
3. Stores the batch's audits, advances `next_index` (marking the chunk `complete` if it reached the end), and records a performance sample.

The cursor is only ever moved to the end of work that has already finished. A crash loses at most the uncommitted batches, which are then searched again; nothing is skipped. Commits happen when a batch produced a match, when a chunk finishes, when pausing or stopping, and otherwise at most every `checkpoint_interval_seconds`.

## Resource policy (`policy.rs`, `sensors.rs`, `idle.rs`, `power.rs`, `gpu_monitor.rs`)

`Policy` is a pure state machine over a `Readings` snapshot, unit-tested without Windows. `SystemSensors` fills the snapshot once a second:

| Reading | Source | Notes |
|---|---|---|
| Keyboard/mouse idle time | `GetLastInputInfo` | Current session only. Controllers are not seen; use exclusions for games. |
| Excluded processes | ToolHelp process snapshot, every 2 s | Matched by file name (case-insensitive) or, for patterns containing a path separator, by full image path via `QueryFullProcessImageNameW`. Foreground or background makes no difference. |
| AC power | `GetSystemPowerStatus` | |
| GPU utilization, temperature, power | NVML, matched to the CUDA device by PCI bus id | NVML ships with the driver. Power draw is optional. |

The rules, in order of precedence:

1. **Excluded app running** → pause. This applies even with `run --now`.
2. **On battery** while `require_ac_power` is set → pause.
3. **GPU temperature at or above `pause_temperature_c`** → pause until it drops *below* `resume_temperature_c` (hysteresis). This applies even with `--now`.
4. `--now` or `idle.enabled = false` → run.
5. **User idle for less than `idle_minutes`** → wait. Input while running pauses at once (`user_active`), and resuming needs the full idle period again.
6. **To start** (GPU search only), average GPU utilization over the last `gpu_idle_sample_seconds`, sampled while not running, must be below `gpu_start_threshold_percent`. While running, total utilization is ignored, because our own kernels raise it.
7. **Any reading a rule needs being unavailable** → do not run (`monitoring_unavailable`), never start blindly. CPU searches don't need GPU telemetry.

Every decision carries a `Reason` with a stable code and details. Examples are `waiting_for_idle {idle_seconds, required_seconds}`, `excluded_app {app}` and `thermal {temperature_c, resume_below_c}`. The supervisor stores it in `worker_state`; `status` shows the human text, and `status --json` the structured form for front ends.

## Scheduler (`scheduler.rs`)

- **The supervisor** (main thread) runs every 250 ms. In order of precedence it applies: stop requests (CLI, Ctrl+C, verified discovery) → manual pause → error back-off → the policy decision, reevaluated every second. It writes the heartbeat with the reason and logs state changes.
- **Workers start paused.** Nothing computes until the policy has been consulted.
- **Live reload.** The configuration file is checked every 2 s. A valid change updates the policy, the exclusion list and the batch/checkpoint/audit/search settings immediately. Settings that need a restart (backend, GPU device, CPU threads, database path, log level) are reported and kept. An invalid file is logged and ignored.
- **Pausing** finishes the current batch, commits, releases the chunk to `pending`, and **frees the CUDA context**, giving its VRAM back to, for example, a game. The context reopens on resume.
- **Transient GPU errors** (driver errors, CUDA unavailable) release the chunk and back off for 15 s, doubling up to 10 min, before retrying with a fresh context. **Correctness failures** (an audit mismatch, a processed count that doesn't match the batch, an invariant violation) and database errors stop the worker.
- **Sleep.** Normally Windows sleeps whenever it wants. Progress is checkpointed, and the worker continues after wake if the policy allows. With `power.prevent_automatic_sleep`, the supervisor holds `ES_SYSTEM_REQUIRED` only while it is actually computing on AC power. That never blocks explicit sleep, shutdown or lid close, and never keeps the display on.
- **Claims, recovery and batches** are unchanged:
  - Interrupted chunks resume first. Otherwise chunks are allocated historical-first, then cheapest-first across both alphabet tracks, and the frontier opens lazily.
  - Chunks left `running` by a crash return to `pending` at startup.
  - Batches are sized from the measured rate to about `batch_target_ms`.
- **One worker per database**, enforced by an exclusive OS lock on `<database>.lock`. `status` uses the same lock to tell whether a worker is really alive.

## Engines and backend policy (`engine.rs`)

Each search thread owns one engine, which turns a batch `[start, end)` into CPU-verified matches.

- **CPU engine:** the exact reference, one per CPU thread.
- **CUDA engine:** one thread. It launches the filter kernel, then:
  - requires the kernel's processed count to equal the batch size;
  - rebuilds every survivor from its index and checks it exactly on the CPU;
  - if the survivor buffer overflowed, searches the whole batch again on the CPU;
  - every `audit_every_batches` batches, recomputes a random slice on the GPU and with the Rust reference filter before committing.
- **Short spaces on a GPU worker.** Spaces too short for the filter (about 15 hex digits or fewer, roughly 4M candidates in all) are searched inline by the CPU reference.

`work_chunks.backend` lists every backend that contributed to a chunk, and `trindrome coverage` reports searched candidates by backend, so GPU-searched and CPU-reference coverage stay distinguishable. `compute.backend = "auto"` (or `"cuda"`) never silently switches to intensive CPU execution: if CUDA is unavailable, the worker reports why.
