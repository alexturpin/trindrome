# Distributed search (design)

Status: **phases 5a and 5b are implemented and tested locally with `wrangler dev`, ready to deploy (see [Deploying](#deploying)).** This is the plan for phase 5, which turns spec §14 into concrete decisions. [Implementation status](#implementation-status) lists what is built and where the implementation differs from the design. Where the design departs from the spec, the reasons are under [Deviations from spec §14](#deviations-from-spec-14).

Friends, and later the public, run the same agent. A coordinator on Cloudflare decides *what* each machine searches. Each machine's local resource policy still decides *whether* it runs. The search engine, the work-unit format and the local checkpoint rule do not change.

## Goals

- **Linear throughput.** Every machine searches ranges that no other machine is searching, apart from deliberate replication.
- **No lost coverage.** A range counts as searched only when a result for it has been accepted. Abandoned or expired work always returns to the pool.
- **No sign-in.** Installing and running the agent is enough to join.
- **Untrusted workers.** A defective or dishonest worker that reports "no match" for work it didn't do is detected, and the coverage it claimed is searched again.
- **Cheap to operate.** One Worker, one Durable Object, R2 for bulk data. Probably the $5 Workers plan; see [Cost](#cost).

Non-goals: accounts, leaderboards, non-NVIDIA GPUs, and running the search on Cloudflare.

## Components

```text
trindrome-agent (each PC)                 Cloudflare
┌────────────────────────┐   HTTPS   ┌──────────────────┐     ┌─────────────────────────────┐
│ policy (idle/games/AC) │ ────────► │ Worker           │ ──► │ Coordinator DO (only one)   │
│ local SQLite journal   │           │ signatures, rate │     │ SQLite: spaces, chunks,     │
│ + leases, outbox       │ ◄──────── │ limits, survivor │     │ segments, workers,          │
│ CUDA engine, unchanged │           │ checks (wasm)    │     │ replicas, discoveries       │
└────────────────────────┘           └────────┬─────────┘     └─────────────────────────────┘
                                              ▼
                                     R2: survivor lists, discoveries, coverage snapshots
```

- **The Worker** (stateless) checks request signatures and rate limits. It does the CPU-heavy checks of submitted survivor lists, stores the lists in R2 and forwards small, already-checked messages to the coordinator.
- **The Coordinator Durable Object** is the single authority over work assignment, leases, accepted coverage and worker trust. It uses SQLite-backed storage and handles all state changes one at a time, so assignment is atomic without extra locking. Even thousands of GPUs checking in every few minutes are only a few requests per second, so it isn't sharded.
- **R2** holds survivor lists (needed to compare replicas), discovery records and periodic coverage snapshots for the public statistics page.
- **No D1, no Queues.** The DO's SQLite is enough. Add them only for a need this design doesn't have.

The Worker and the Durable Object are a standard TypeScript Workers project in `cloud/coordinator`, run with `wrangler`. Everything mathematical is `trindrome-core` itself, compiled to wasm with wasm-bindgen (`crates/trindrome-wasm`). Chunk identities, the decimal filter and discovery verification therefore run **the same code** on the server as on the workers. There is no second big-integer implementation to drift. (An earlier draft proposed workers-rs. A TypeScript Worker is Cloudflare's main path and keeps the tooling standard, at the cost of a thin wasm boundary.)

## Proof of work: survivor sets

Checking only positive results proves nothing. Almost every honest result is "no match", and a cheater can say that without searching. Each result therefore carries evidence that can only be produced by doing the work and is cheap to check.

The GPU's decimal filter (W = 9, [cuda-design.md](cuda-design.md#decimal-filter)) is a pure function of the candidate value. It passes about one candidate in 10⁹. The **survivor set** of a range is the sorted list of indices in that range that pass it. Survivor sets have the properties a proof needs:

| Property | Why it holds |
|---|---|
| Can't be faked | Naming survivors requires evaluating the filter on every candidate, which is the search. A cleverer method of finding them all is fine: a complete survivor set plus exact checks *is* a complete search, because the filter never rejects a palindrome. |
| Cheap to check | The server re-evaluates any reported index in microseconds, using the same Rust code. |
| Roughly predictable size | About 10⁻⁹ × range length on average, but **not uniform** (see the count test below). |
| Identical everywhere | Generic kernels, per-length kernels and the Rust reference already agree exactly on survivors; the differential tests enforce it. |
| Contains every discovery | A triple palindrome always passes the filter, so it is always a survivor. |

**The filter becomes part of the protocol.** It gets a `FILTER_VERSION`, carried in every lease and result like `GENERATOR_VERSION`. Any later optimization (for example, rejecting even-length decimal values by divisibility by 11) must still produce the same W = 9 survivor set, or bump the filter version. A server only accepts versions it knows.

Spaces short enough to be searched by the CPU reference (n ≤ 15, about 4 million candidates in all) don't use the filter. The coordinator searches them once on a trusted machine and never leases them.

### Encoding

A segment's survivors are encoded as `u64` offsets from the segment start, in ascending order, delta-encoded as LEB128 varints. Their digest is the SHA-256 of that encoding, prefixed with the segment identity (`trindrome:g1:f1:h65:1:a-b`). A typical segment uploads about 10–20 KB.

### Checks on every submission

The Worker, before anything reaches the coordinator:

1. **Well-formed:** offsets strictly increasing, all inside the segment.
2. **Filter check:** *every* survivor is re-evaluated through the filter. That is cheaper than the exact check that follows anyway, so there is no point in sampling. A single fabricated survivor rejects the submission.
3. **Exact check:** every survivor is rebuilt and checked exactly. This catches discoveries the worker missed or failed to report.
4. **Count test (not implemented):** the idea was to require the count to lie within a few σ of the expected count. **Survivor density turns out not to be uniform.** Within a range, the top binary digits are fixed, and they fix both the leading decimal digits and the value modulo small powers of two. So the density depends on where the range lies: from exactly zero (where the leading decimal digit is even, so no palindrome can exist) to several times the average. A useful count test needs a per-range expected density, which is still an open problem. Until then, replication is the only guarantee of completeness.

## Work units: chunks, leases and segments

The coordinator keeps the same ledger as the local database ([architecture.md](architecture.md#persistence-persistencers)), with lease columns added:

- **Chunks** are carved from a per-space cursor (`next_unallocated`) in the existing allocation order (cheapest first across the frontier families). A chunk's range, and therefore its ID, never changes.
- **A lease** gives one worker the right to search one chunk until `expires_at`. It has its own `lease_id` and an **epoch**, a fencing token that increases every time the chunk is reassigned. Every request about a lease must quote the epoch. A worker returning from sleep with an old epoch gets `409 superseded` and can't change anything.
- **Segments** are accepted results: a sub-range `[a, b)` of a chunk with its survivor digest and the worker that produced it. Coverage is the union of accepted segments, derived exactly as local coverage is derived from cursors today.

### Lifecycle

1. **Acquire.** The worker sends its capabilities: generator and filter versions, maximum hex digits (128 for the 512-bit kernels), backend and measured rate. The coordinator returns, in order of preference:
   1. a pending replica assignment the worker is allowed to take (see [Replication](#replication-and-trust));
   2. the oldest returned remainder;
   3. a newly carved chunk.

   New chunks are sized to about **30 minutes** at the reported rate, clamped to sensible bounds. Rates are capped at a plausible maximum, so a worker can't claim a huge range.
2. **Report progress** about every **5 minutes**: the newly committed sub-range `[cursor, cursor′)` with its survivor list. Each report is a segment submission and also renews the lease. A report with no new progress just renews it.
3. **Release** when the local policy pauses for more than a short grace period (a game, the user returning, unplugging). The worker submits its last segment and gives up the lease. The remainder `[cursor, end)` goes back to the pool at once. A paused worker never holds work hostage.
4. **Complete:** the last segment reaches the end of the chunk.
5. **Expiry.** The lease TTL is **15 minutes** (three missed reports). Expiry is checked lazily on every acquire, plus a DO alarm. The unreported remainder returns to the pool. Whatever was already reported stays accepted.

Every submission is idempotent, keyed by `(chunk, epoch, a, b)`. Resubmitting returns the original outcome. This is also why signed requests need no nonce store: a replayed request can only repeat something already recorded.

### Offline workers

The agent keeps searching a lease while offline. Committed segments wait in a local outbox. When the agent reconnects, it uploads them oldest first:

- if the lease is still valid, they are accepted as usual;
- if it was superseded, the coordinator accepts segments that don't overlap anything accepted since. Overlapping ones are compared with the accepted result, which gives a free replica, and then discarded.

Either way, nothing is lost and nothing is double-counted.

## Replication and trust

Identities are free to create, so trust is earned from verified work, never assumed from an identity.

| Trust | How a worker gets there | Replication of its segments |
|---|---|---|
| `new` | Registration | **Every** segment is searched again by a worker with trust ≥ `established`, or by a different `new` worker in a different network, and both must agree. |
| `established` | 20 matched segments over at least 2 days | A random **3%** of segments, plus the count test on all of them. |
| `trusted` | Set by hand (your own machines) | Same sampling. Also breaks ties between disagreeing workers. |

A **replica** is a segment issued to a second worker with its range fixed. The two survivor digests must match exactly.

- **Assignment rules:** the two workers must differ in key, IP network (/24 for IPv4, /48 for IPv6) and registration day. This makes it hard for one person with several keys to receive both copies.
- **Match:** the segment is *confirmed*, and both workers gain credit.
- **Mismatch:** a third, trusted worker searches it. The wrong worker drops to `new`, and **every unconfirmed segment it submitted since its last confirmed match goes back to the pool.** Cheating therefore can't keep coverage it didn't earn, however long it went undetected.

Public statistics report two levels: **searched** (accepted segments) and **confirmed** (matched by an independent replica). At 3% sampling, a trusted worker that skipped work in a fraction *p* of its segments is caught after about 33/*p* segments, and everything since its last match is then redone. A failing GPU is caught the same way.

## Identity and rate limiting

There are no accounts, invite codes or sign-in.

### Keys

- On first run the agent creates an **Ed25519 key pair**. It stores the private key in the data folder, encrypted with DPAPI for the current user, and calls `POST /v1/register` with the public key. The server returns a `worker_id`.
- Every request is signed over `timestamp \n method \n path \n sha256(body)`, and the server accepts a clock skew of ±5 minutes.
- Hardware IDs are **not** identities: they are easy to spoof, raise privacy questions and change when a GPU is replaced. Registration does include a salted hash of the NVML GPU UUID, but only so statistics don't count one machine twice.
- Deleting the key creates a new identity at trust `new`. That gains nothing, because new identities are fully replicated.

### Rate limits

Two layers, because Cloudflare's built-in limiter only counts over 10- or 60-second periods:

1. **Workers Rate Limiting binding**, for short-term flood control.
   - Per IP on `/v1/register`: a few per minute.
   - Per `worker_id` on authenticated endpoints: about 60 per minute.

   It is approximate and counted separately in each Cloudflare location, which is fine for stopping floods. It runs before any signature check or DO call, so rejected traffic costs almost nothing.
2. **Exact limits in the coordinator DO**, for anything over longer periods or anything that must be exact:
   - registrations per IP network: at most 5 per day per /24 (IPv4) or /48 (IPv6);
   - a global cap on new identities per hour, as a circuit breaker during abuse;
   - leases held at once: 1 for a `new` worker, 2 for an `established` one (so a GPU never waits between chunks).

Registration needs no human interaction, so the circuit breaker is the defence against a flood of new keys. It doesn't block existing workers; it only slows down new ones.

## API

All bodies are JSON. Indices are canonical decimal strings, as in `WorkChunk`. Every request except registration and the public coverage endpoint is signed.

| Endpoint | Purpose |
|---|---|
| `POST /v1/register` | Public key, agent version, GPU name and UUID hash → `worker_id` |
| `POST /v1/leases` | Capabilities → a lease (`lease_id`, `epoch`, the `WorkChunk`, `expires_at`, `kind`: `primary` or `replica`), or `204` with `retry_after` |
| `POST /v1/leases/:id/segments` | `epoch`, `[a, b)`, survivor list → `accepted`, `duplicate`, `superseded` or `rejected` with a reason; also renews the lease |
| `POST /v1/leases/:id/renew` | `epoch` → new `expires_at` (when there's no new progress) |
| `POST /v1/leases/:id/release` | `epoch` → remainder returned to the pool |
| `GET /v1/coverage` | Public: searched and confirmed coverage per space, discoveries, aggregate throughput. Served from an R2 snapshot refreshed every few minutes |

A discovery needs no separate endpoint. It is a survivor, and the exact check in [Checks on every submission](#checks-on-every-submission) finds it whether or not the worker noticed. Each one is stored in the DO and in R2 and triggers a notification.

## Coordinator schema

```sql
spaces      (generator_version, hex_digits, leading_hex_digit, family, total, next_unallocated,
             expected_survivor_rate)
chunks      (id, space, start_index, end_index, state, lease_id, worker_id, epoch, expires_at,
             cursor)                                         -- state: pending | leased | done
segments    (chunk_id, start_index, end_index, worker_id, epoch, filter_version, survivor_count,
             digest, r2_key, status, accepted_at)            -- status: searched | confirmed | voided
replicas    (segment_id, worker_id, epoch, expires_at, digest, outcome)
workers     (id, public_key, trust, registered_at, ip_network, gpu_hash, rate, matched, last_seen)
discoveries (value, chunk_id, index, worker_id, verified_at)
limits      (key, window_start, count)
```

Indices are canonical decimal strings, as locally. `chunks.cursor` mirrors the end of the last accepted segment, so a returned remainder is `[cursor, end_index)`.

## Changes to the agent

- **Remote mode** in `trindrome-worker`, chosen in `trindrome.toml` (`[coordinator] url = …`), with local-only mode staying the default. In remote mode the coordinator owns allocation, and the local `next_unallocated` cursor is unused.
- **Local journal additions** (store schema v3):
  - a `leases` table: lease ID, epoch, expiry, the end of the accepted work, and when the worker paused;
  - a `segments` table: every checkpoint of a leased chunk with its survivor set, kept until the coordinator accepts or supersedes it.

  Leased chunks are ordinary `work_chunks` rows, so the checkpoint rule, crash recovery and `trindrome verify` cover them. They are left out of local allocation and of the local tiling check.
- **Survivors from both engines.** The CUDA engine already returns each batch's survivors; on overflow the Rust reference filter recomputes them. The CPU engine runs the reference filter alongside the exact search.
- **Search threads lease work** when they have none, and wait (shown as a back-off) when the coordinator can't be reached or has nothing to give.
- **A sync thread** uploads segments, renews leases and hands back paused ones. It never blocks the engine: if the coordinator can't be reached, segments wait in the database. When the worker stops, the sync thread stops after the search threads and spends up to 15 s handing everything over.
- **Policy is unchanged.** A pause longer than `release_after_pause_seconds` (default 120) releases the lease. Shorter pauses keep it.
- `trindrome status` doesn't show the coordinator connection yet.

## Failure cases

| Situation | Outcome |
|---|---|
| Agent killed mid-chunk | Local checkpoint as today. On restart it reports from its committed cursor if the lease is still valid; otherwise the outbox drains as described under offline workers. |
| Laptop sleeps for hours | The lease expires and the remainder is reassigned. On wake, `superseded`; reported segments stay, the outbox drains. |
| User starts a game | The engine pauses within a batch. After the grace period the agent releases the lease and the remainder goes back to the pool. |
| Coordinator unreachable | The agent keeps searching its lease, then waits. Nothing is lost: segments wait in the outbox. |
| Faulty GPU | Survivors differ from replicas, or the count test fails. The worker drops to `new` and its unconfirmed segments are redone. |
| Fabricated survivors | The spot check rejects the submission. |
| Many keys from one person | Registration limits per network, the global circuit breaker, and replica rules that require different networks and registration days. |

## Public results

The claim "nothing up to length *n*" should be checkable without trusting the organizer, the coordinator or any worker. Phase 5d publishes the evidence the coordinator already keeps.

- **Snapshot:** a periodic dump to a public R2 bucket (downloads cost nothing). It holds one row per accepted segment:
  - space and index range;
  - status (`searched`, `confirmed` or `voided`);
  - the worker's public ID (never its computer name or network);
  - `FILTER_VERSION`, survivor count and digest;
  - the path of its survivor file.

  It also holds the discoveries, each with the segment it came from, and a manifest with the SHA-256 of every file.
- **Survivor files:** the R2 objects the coordinator already stores, published as they are. At about one survivor per 10⁹ candidates, a quadrillion candidates is about a million survivors: tens of MB in all.
- **`trindrome audit <space> <range>`:** searches any range locally and compares its survivors and digest with the snapshot. A sample over the whole snapshot is an independent spot check of the claimed coverage. This uses the same check as replication, run by anyone.
- **An archived copy:** a frozen snapshot on Zenodo or similar, with a DOI, at milestones and with any discovery.

**Imported coverage has no evidence.** `trindrome coordinator import` records the organizer's local coverage as plain ranges. Local chunks that weren't leased never kept their survivor sets, so these rows are published as `imported` with no survivor file. They rest on the organizer's word, and on anyone re-searching them with `trindrome audit`. Re-searching them under leases, at idle priority, would turn them into ordinary segments over time.

## Cost

Requests are few: a few per GPU every 5 minutes. The bulk of the cost is CPU time for the exact survivor checks, which needs more than the free plan's 10 ms per request. Plan on the **$5/month Workers Paid** plan. Its included requests, Durable Object usage and R2 storage should cover a few hundred workers. Confirm with real numbers from phase 5b before inviting more people.

## Phases

| Phase | Delivers |
|---|---|
| **5a. Protocol in core** | `FILTER_VERSION`, segment survivor encoding and digest, a wasm build of `trindrome-core` in CI, survivor offsets stored in the local journal. Useful locally too: it strengthens `trindrome verify`. |
| **5b. Coordinator** | The Worker and DO with registration, rate limits, leases, segments and survivor checks. Your machines are `trusted`; no replication yet. |
| **5c. Friends** | Replication and trust levels, voiding on mismatch, the agent's outbox hardened against flaky connections. |
| **5d. Public** | The coverage page from R2 snapshots, [public results](#public-results) anyone can check, discovery notifications, tuned limits. |

## Implementation status

Built (phases 5a and 5b):

- **`trindrome-core::proof`:** `FILTER_VERSION`, the survivor encoding, digest and `check_segment`.
- **`crates/trindrome-wasm`:** the core exported to the Worker.
- **`cloud/coordinator`:**
  - the Worker: signatures, both rate-limit layers, survivor checks, R2;
  - the Coordinator Durable Object: registration, leases, epochs, lazy expiry, segments, coverage.
- **Coverage import and the public page:** `POST /v1/import` (owner keys from the `OWNER_PUBLIC_KEYS` secret only) and `GET /`, which lists progress, discoveries with their finder's display name, and contributors.
- **The agent's distributed mode:**
  - `[coordinator] url` and an optional `name`;
  - `trindrome coordinator key`, `import`, and `workers` (owner keys only: every worker with its computer name and network, recorded privately so a discoverer can be found even under an anonymous display name);
  - the display name in the settings window, applied at the next lease request without a restart;
  - the identity key under DPAPI;
  - `remote.rs` (client and sync thread);
  - store schema v3 with `leases` and `segments`;
  - survivors returned by both engines.

Tested end to end against `wrangler dev`, with CPU and GPU workers on one machine:
- registration and signed requests;
- leasing, reporting and completing chunks, at about 6.4 billion candidates/s on the RTX 3070 Laptop;
- pause → release after the grace period, and resume → re-lease of the remainder;
- graceful stop handing everything back;
- an expired lease reassigned to a second worker, with the first worker's late report refused as `superseded`;
- rejection of bad signatures, unknown versions, fabricated survivors and out-of-order segments;
- the registration rate limit.

`trindrome verify` passes on both workers' databases.

Not built yet, or different from the design above:

- **No replication or trust levels (phase 5c).** Every worker is `new`, and nothing is confirmed. Run only trusted machines until 5c exists.
- **No count test.** See [Checks on every submission](#checks-on-every-submission).
- **Late uploads under a superseded lease** are accepted only if the chunk is unclaimed and they continue exactly from its cursor. Overlapping uploads are discarded rather than compared as free replicas.
- **One lease at a time for a `new` worker.** A GPU therefore waits about a second between chunks for its last report to be accepted. That doesn't matter at 30-minute chunks, but a multi-threaded CPU worker uses only one thread.
- **Coverage is served live** from the Durable Object, not from R2 snapshots.
- **No DO alarm.** Expiry is lazy only.
- **No GPU UUID hash** at registration.
- **`trindrome status` knows nothing about leases yet.**
- **Spaces with more than 2⁶⁴ candidates are never leased.** Survivors are 64-bit indices, and the GPU is limited to them too.

### Running it locally

Needs the `wasm32-unknown-unknown` Rust target and `wasm-bindgen-cli` 0.2.130, the version `crates/trindrome-wasm` pins:

```bash
rustup target add wasm32-unknown-unknown
```

```bash
cargo install wasm-bindgen-cli --version 0.2.130 --locked
```

Then, in `cloud/coordinator`:

```bash
npm install
```

```bash
cp .dev.vars.example .dev.vars
```

```bash
npm run dev
```

`npm run dev` builds the wasm module and starts `wrangler dev` on port 8787. `.dev.vars` shortens lease timings, so expiry and release can be watched within a minute. Point a worker at it with a configuration like this one, and run `trindrome --config that.toml run --now`:

```toml
[coordinator]
url = "http://127.0.0.1:8787"
release_after_pause_seconds = 10
```

`GET /v1/coverage` shows the coordinator's view, and `GET /` renders it as a page. To test an import locally, pass an owner key with `npx wrangler dev --var OWNER_PUBLIC_KEYS:<key>`.

### Deploying

The coordinator runs at `trindrome.alexturpin.com` (a custom domain in `wrangler.jsonc`, so the `alexturpin.com` zone must be on the same Cloudflare account) and at its `workers.dev` address. In `cloud/coordinator`, after `npx wrangler login`:

```bash
npx wrangler r2 bucket create trindrome-survivors
```

```bash
npm run deploy
```

Then make your machine an owner. `trindrome coordinator key` prints this installation's public key. Store it as a secret:

```bash
npx wrangler secret put OWNER_PUBLIC_KEYS
```

The free Workers plan allows 10 ms of CPU per request, which segment checks can exceed. Use Workers Paid.

### Moving a local search onto the coordinator

A machine that searched locally imports its progress once, so the coordinator continues from it instead of handing the same ranges out again:

1. `trindrome stop`, so the local progress cannot change.
2. `trindrome coordinator import --url https://trindrome.alexturpin.com`. Each space's searched prefix from index 0 becomes a finished chunk. Historical lengths are ignored, and only an owner key may import. Re-running it is harmless. An import that disagrees with an earlier import, or with work the coordinator has already handed out, is refused as a whole.
3. Add `[coordinator]` with `url` (and optionally `name`, shown on the public page) to `trindrome.toml`, then start the agent again.

The local database keeps its unfinished chunk, which is never searched again in distributed mode.

### Packaging for friends

`packaging\package.ps1 -CoordinatorUrl https://trindrome.alexturpin.com` builds `dist\trindrome-<version>.zip`. It contains:
- release binaries with the C runtime linked statically;
- `install.cmd`, which installs to `%LOCALAPPDATA%\Trindrome`, keeps an existing configuration and turns on start at login;
- a `trindrome.toml` with the coordinator's URL;
- a `README.txt` for the friend.

The binaries are not code-signed, so SmartScreen warns on first run. The kernels cover RTX 30-series and newer (sm_86+) and need a driver with CUDA 13.

## Deviations from spec §14

- **Proof of work through survivor sets.** The spec leaves the "false negatives" problem open. Survivor sets solve it more cheaply than full redundant execution, because only a sample of established workers' work is repeated.
- **No authentication by the user.** The spec asks for "authenticated HTTPS endpoints". Requests are authenticated with a key the agent creates itself, so no person ever signs in. Abuse is limited by rate limits and the trust model, not by accounts.
- **Server-side checks in Rust (wasm), called from a TypeScript Worker.** Not in the spec. They keep the server's checks identical to the workers'.
- **Segments instead of one result per chunk.** Progress reports are results, so a paused or vanished worker's finished work is never discarded.
