# trindrome

A GPU-accelerated search for a [number that's palindromic](https://en.wikipedia.org/wiki/Palindromic_number) in binary, decimal, and hexadecimal.

Candidates come from Rick Regan's structure of binary/hexadecimal palindromes, so only the decimal property has to be tested. See [docs/algorithm.md](docs/algorithm.md) for the mathematics and [docs/architecture.md](docs/architecture.md) for the worker, its resource policy and storage. The plan lives in [spec.md](spec.md), and the design for distributing the search over Cloudflare in [docs/distributed.md](docs/distributed.md).

**Status:** Phases 1–4 are done; phase 5 (distribution) is under way.

- **Exact CPU reference search**, exhaustively tested.
- **Resumable local worker** that survives crashes and kills.
- **CUDA backend** at 6–16 billion candidates/s on an RTX 3070 Laptop GPU (per-length kernels, with batches overlapped with host work), with every survivor verified on the CPU ([docs/cuda-design.md](docs/cuda-design.md), [docs/benchmarks.md](docs/benchmarks.md)). A full replay of Regan's 2010 coverage (28.6 billion candidates) takes 19 seconds and finds no nontrivial triple palindrome.
- **Passive Windows operation:** it computes only while you're away, pauses for games, battery power and heat, and runs as a windowless agent at login.
- **Distributed mode (experimental, phase 5):** with `[coordinator] url` set, workers lease work from a Cloudflare Worker and Durable Object coordinator and prove each result with its filter survivor set. It is tested locally with `wrangler dev` but not deployed, and has no replication yet, so it is for machines you trust. See [docs/distributed.md](docs/distributed.md).

## Setup (Windows 11)

- Rust stable with the MSVC toolchain (`winget install Rustlang.Rustup`)
- Visual Studio Build Tools with the C++ workload (needed by Rust, the bundled SQLite and nvcc)
- The CUDA Toolkit 13.x, to build the GPU kernels. Use a custom install and leave out the bundled driver.

```bash
cargo test --workspace
```

```bash
cargo build --release
```

Running needs only the NVIDIA driver. A build made without the CUDA Toolkit still works with `--cpu` and reports CUDA as unavailable.

## Running it in the background

```bash
target/release/trindrome startup enable
```

This starts `trindrome-agent.exe` at every login, with no console window and no administrator rights. It appears as "Trindrome" in Task Manager → Startup apps. Start it now by running `trindrome-agent.exe` once. It computes on the GPU only while:

- the keyboard and mouse have been idle for `idle_minutes` (default 5), and the GPU was otherwise quiet;
- no application in `exclusive_gpu_apps` is running (add your games; controllers aren't seen by idle detection);
- the laptop is on AC power, and the GPU is below `pause_temperature_c`.

Input, a game starting, unplugging or overheating pause it within about a second. Pausing also frees its GPU memory.

The agent shows a **tray icon**:

| Colour | Meaning |
|---|---|
| Green | Computing |
| Blue | Waiting |
| Grey | Paused |
| Red | Stopped with an error |

Its tooltip says why, and its menu offers Pause/Resume, **Settings…**, Open data folder, Restart worker (after an error) and Quit. Settings opens `trindrome-settings.exe`, a small window for the options people change:

- **When to compute:** the idle period, the GPU-quiet check, plugged-in only, overnight mode.
- **Applications to pause for:** type a name or path, or pick from running applications.
- **GPU temperature limits.**
- **Start at sign-in.**

It edits `trindrome.toml` in place, keeping your comments, and the agent applies the changes within seconds.

## Using it

Settings live in `%LOCALAPPDATA%\Trindrome\trindrome.toml`; copy `config.example.toml` there to start. Changes apply **while it runs**, except the few `trindrome config` lists as needing a restart.

| Command | Does |
|---|---|
| `trindrome status [--json]` | State and *why* (e.g. "waiting for the computer to be idle (3:10 of 5:00)"), throughput, progress, ETA |
| `trindrome run [--now] [--cpu]` | Run the worker in this terminal. `--now` skips the inactivity requirements; game, battery and temperature limits still apply |
| `trindrome pause` / `resume` / `stop` | Control whichever worker is running, agent or terminal |
| `trindrome coverage [--all]` | Searched index ranges per search space, coverage by backend, GPU audits |
| `trindrome discoveries` | Verified matches |
| `trindrome verify` | Checks the database for gaps, overlaps and unverifiable discoveries |
| `trindrome benchmark` | GPU and CPU throughput on historical and frontier workloads (no database) |
| `trindrome config` | Effective configuration and where its files are |
| `trindrome startup enable` / `disable` / `status` | Start the agent at login |

Progress survives Ctrl+C, forced termination, crashes, sleep and logoff. At worst, the last uncommitted batches are searched again. The agent logs to `%LOCALAPPDATA%\Trindrome\logs`.

The "frontiers" here are lengths beyond Regan's published 2010 coverage, not necessarily beyond all later work.
