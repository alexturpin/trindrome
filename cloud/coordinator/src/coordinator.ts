// The single authority over work assignment, leases, accepted coverage and
// worker identities (docs/distributed.md). A Durable Object handles one call at
// a time, and no method awaits anything, so every method is atomic.

import { DurableObject } from "cloudflare:workers";
import { candidateCount, chunkId, versions } from "./core";

/** Leading digits sharing an alphabet size, and the first length beyond Regan's 2010 coverage. */
const TRACKS: [number[], number][] = [
  [[0x1, 0x3], 65],
  [[0x5, 0x7, 0x9, 0xf], 33],
];

/** Workers report survivors as 64-bit indices, so spaces must fit. */
const MAX_INDEX = 1n << 64n;
const MIN_CHUNK = 1n << 20n;
const MAX_CHUNK = 1n << 46n;
/** No real worker is faster; caps how much a worker can claim at once. */
const MAX_RATE = 2e11;

const SCHEMA = `
CREATE TABLE IF NOT EXISTS spaces (
  hex_digits INTEGER NOT NULL,
  leading INTEGER NOT NULL,
  total TEXT NOT NULL,
  next_unallocated TEXT NOT NULL,
  PRIMARY KEY (hex_digits, leading)
);
CREATE TABLE IF NOT EXISTS chunks (
  id TEXT PRIMARY KEY,
  hex_digits INTEGER NOT NULL,
  leading INTEGER NOT NULL,
  start_index TEXT NOT NULL,
  end_index TEXT NOT NULL,
  cursor TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending', 'leased', 'done')),
  lease_id TEXT,
  worker_id TEXT,
  epoch INTEGER NOT NULL DEFAULT 0,
  expires_at INTEGER,
  created_at INTEGER NOT NULL
);
CREATE INDEX IF NOT EXISTS chunks_by_state ON chunks (state);
CREATE TABLE IF NOT EXISTS leases (
  id TEXT PRIMARY KEY,
  chunk_id TEXT NOT NULL,
  worker_id TEXT NOT NULL,
  epoch INTEGER NOT NULL,
  issued_at INTEGER NOT NULL,
  ended_at INTEGER,
  outcome TEXT
);
CREATE TABLE IF NOT EXISTS segments (
  id INTEGER PRIMARY KEY,
  chunk_id TEXT NOT NULL,
  start_index TEXT NOT NULL,
  end_index TEXT NOT NULL,
  candidates REAL NOT NULL,
  worker_id TEXT NOT NULL,
  lease_id TEXT NOT NULL,
  epoch INTEGER NOT NULL,
  filter_version INTEGER NOT NULL,
  survivor_count INTEGER NOT NULL,
  digest TEXT NOT NULL,
  r2_key TEXT NOT NULL,
  backend TEXT,
  status TEXT NOT NULL CHECK (status IN ('searched', 'confirmed', 'voided')),
  accepted_at INTEGER NOT NULL,
  UNIQUE (chunk_id, start_index, end_index, worker_id)
);
CREATE INDEX IF NOT EXISTS segments_by_time ON segments (accepted_at);
CREATE TABLE IF NOT EXISTS workers (
  id TEXT PRIMARY KEY,
  public_key TEXT NOT NULL UNIQUE,
  trust TEXT NOT NULL DEFAULT 'new' CHECK (trust IN ('new', 'established', 'trusted')),
  network TEXT NOT NULL,
  agent_version TEXT,
  gpu TEXT,
  gpu_hash TEXT,
  rate REAL,
  registered_at INTEGER NOT NULL,
  last_seen INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS discoveries (
  decimal TEXT PRIMARY KEY,
  hexadecimal TEXT NOT NULL,
  nontrivial INTEGER NOT NULL,
  chunk_id TEXT NOT NULL,
  candidate_index TEXT NOT NULL,
  worker_id TEXT NOT NULL,
  found_at INTEGER NOT NULL
);
CREATE TABLE IF NOT EXISTS imports (
  hex_digits INTEGER NOT NULL,
  leading INTEGER NOT NULL,
  searched TEXT NOT NULL,
  worker_id TEXT NOT NULL,
  imported_at INTEGER NOT NULL,
  PRIMARY KEY (hex_digits, leading)
);
CREATE TABLE IF NOT EXISTS limits (
  key TEXT PRIMARY KEY,
  count INTEGER NOT NULL,
  expires_at INTEGER NOT NULL
);
`;

export interface Capabilities {
  max_hex_digits: number;
  /** Measured candidates per second. */
  rate: number;
  /** Display name chosen by the worker's owner, if any. */
  name: string | null;
  /** The computer's name. Private: never shown on the public page. */
  hostname: string | null;
}

/** A worker as its owner sees it, private details included. */
export interface WorkerInfo {
  worker_id: string;
  name: string | null;
  hostname: string | null;
  gpu: string | null;
  network: string;
  candidates: number;
  discoveries: number;
  registered_at: number;
  last_seen: number;
  [key: string]: SqlStorageValue;
}

export interface WorkChunkJson {
  schema_version: number;
  generator_version: number;
  hex_digits: number;
  leading_hex_digit: number;
  start_index: string;
  end_index: string;
}

export interface Lease {
  lease_id: string;
  epoch: number;
  chunk: WorkChunkJson;
  /** Where the worker starts: everything before it is already accepted. */
  cursor: string;
  expires_at: number;
  report_interval_seconds: number;
}

export interface Segment {
  workerId: string;
  leaseId: string;
  epoch: number;
  chunkId: string;
  start: string;
  end: string;
  survivors: number;
  digest: string;
  r2Key: string;
  backend: string | null;
  matches: { index: string; decimal: string; hexadecimal: string; nontrivial: boolean }[];
}

/** A space searched before the coordinator existed: `[0, searched)` is done. */
export interface ImportedSpace {
  hex_digits: number;
  leading_hex_digit: number;
  searched: bigint;
}

export type ImportResult =
  | { status: "imported"; spaces: number; candidates: string }
  | { status: "rejected"; reason: string };

export type SegmentResult =
  | { status: "accepted"; cursor: string; complete: boolean; expires_at: number | null }
  | { status: "duplicate" }
  | { status: "superseded" }
  | { status: "rejected"; reason: string };

interface ChunkRow {
  id: string;
  hex_digits: number;
  leading: number;
  start_index: string;
  end_index: string;
  cursor: string;
  state: "pending" | "leased" | "done";
  lease_id: string | null;
  worker_id: string | null;
  epoch: number;
  expires_at: number | null;
  [key: string]: SqlStorageValue;
}

interface SpaceRow {
  hex_digits: number;
  leading: number;
  total: string;
  next_unallocated: string;
  [key: string]: SqlStorageValue;
}

function minBig(a: bigint, b: bigint): bigint {
  return a < b ? a : b;
}

function maxBig(a: bigint, b: bigint): bigint {
  return a > b ? a : b;
}

class ImportRejected extends Error {}

function spaceName(hexDigits: number, leading: number): string {
  return `h${hexDigits}:${leading.toString(16).toUpperCase()}`;
}

function setting(env: Env, name: keyof Env, fallback: number): number {
  const value = Number(env[name]);
  return Number.isFinite(value) && value > 0 ? value : fallback;
}

export class Coordinator extends DurableObject<Env> {
  private sql: SqlStorage;

  constructor(ctx: DurableObjectState, env: Env) {
    super(ctx, env);
    this.sql = ctx.storage.sql;
    this.sql.exec(SCHEMA);
    // Columns added after the first deployment.
    const columns = this.sql.exec<{ name: string }>("PRAGMA table_info(workers)").toArray();
    for (const column of ["name", "hostname"]) {
      if (!columns.some((c) => c.name === column)) {
        this.sql.exec(`ALTER TABLE workers ADD COLUMN ${column} TEXT`);
      }
    }
  }

  private get leaseTtlMs(): number {
    return setting(this.env, "LEASE_TTL_SECONDS", 900) * 1000;
  }

  // --- Identities ---------------------------------------------------------

  /** Registers a public key, or returns the worker it already belongs to. */
  register(
    publicKey: string,
    network: string,
    agentVersion: string,
    gpu: string | null,
    gpuHash: string | null,
    hostname: string | null,
  ): { worker_id: string } | { error: "rate_limited" } {
    const existing = this.sql.exec<{ id: string }>("SELECT id FROM workers WHERE public_key = ?", publicKey).toArray();
    if (existing.length > 0) {
      this.sql.exec("UPDATE workers SET hostname = COALESCE(?, hostname) WHERE id = ?", hostname, existing[0].id);
      return { worker_id: existing[0].id };
    }
    const now = Date.now();
    const day = 86_400_000;
    const hour = 3_600_000;
    const perNetwork = setting(this.env, "REGISTRATIONS_PER_NETWORK_PER_DAY", 5);
    const perHour = setting(this.env, "NEW_WORKERS_PER_HOUR", 100);
    const allowed = this.ctx.storage.transactionSync(() => {
      this.sql.exec("DELETE FROM limits WHERE expires_at < ?", now);
      const network_ok = this.take(`register:network:${network}`, perNetwork, now + day);
      const global_ok = network_ok && this.take("register:global", perHour, now + hour);
      return global_ok;
    });
    if (!allowed) {
      return { error: "rate_limited" };
    }
    const id = "w_" + crypto.randomUUID().replaceAll("-", "").slice(0, 20);
    this.sql.exec(
      `INSERT INTO workers (id, public_key, network, agent_version, gpu, gpu_hash, hostname, registered_at, last_seen)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      id,
      publicKey,
      network,
      agentVersion,
      gpu,
      gpuHash,
      hostname,
      now,
      now,
    );
    return { worker_id: id };
  }

  /** Counts one use of `key` in a fixed window; false once `limit` is reached. */
  private take(key: string, limit: number, expiresAt: number): boolean {
    const row = this.sql.exec<{ count: number }>("SELECT count FROM limits WHERE key = ?", key).toArray()[0];
    if (row && row.count >= limit) {
      return false;
    }
    this.sql.exec(
      `INSERT INTO limits (key, count, expires_at) VALUES (?, 1, ?)
       ON CONFLICT (key) DO UPDATE SET count = count + 1`,
      key,
      expiresAt,
    );
    return true;
  }

  /** The worker's public key, or null if it is unknown. */
  publicKey(workerId: string): string | null {
    const row = this.sql
      .exec<{ public_key: string }>("SELECT public_key FROM workers WHERE id = ?", workerId)
      .toArray()[0];
    if (!row) {
      return null;
    }
    this.sql.exec("UPDATE workers SET last_seen = ? WHERE id = ?", Date.now(), workerId);
    return row.public_key;
  }

  // --- Leases -------------------------------------------------------------

  /**
   * Leases work to a worker: a lease it already holds (so a restarted agent
   * gets its work back), else an abandoned remainder, else a new chunk.
   */
  acquire(workerId: string, caps: Capabilities): Lease | null {
    return this.ctx.storage.transactionSync(() => {
      const now = Date.now();
      const rate = Math.min(Math.max(caps.rate, 1), MAX_RATE);
      this.sql.exec(
        "UPDATE workers SET rate = ?, last_seen = ?, name = ?, hostname = COALESCE(?, hostname) WHERE id = ?",
        rate,
        now,
        caps.name,
        caps.hostname,
        workerId,
      );
      const trust = this.sql.exec<{ trust: string }>("SELECT trust FROM workers WHERE id = ?", workerId).one().trust;
      const held = this.sql
        .exec<ChunkRow>(
          "SELECT * FROM chunks WHERE state = 'leased' AND worker_id = ? AND expires_at >= ? ORDER BY expires_at",
          workerId,
          now,
        )
        .toArray();
      if (held.length >= (trust === "new" ? 1 : 2)) {
        return this.leaseOf(held[0]);
      }

      let chunk = this.sql
        .exec<ChunkRow>(
          `SELECT * FROM chunks
           WHERE (state = 'pending' OR (state = 'leased' AND expires_at < ?)) AND hex_digits <= ?
           ORDER BY created_at LIMIT 1`,
          now,
          caps.max_hex_digits,
        )
        .toArray()[0];
      if (!chunk) {
        const size = BigInt(Math.floor(rate * setting(this.env, "LEASE_TARGET_SECONDS", 1800)));
        const carved = this.carve(maxBig(MIN_CHUNK, minBig(size, MAX_CHUNK)), caps.max_hex_digits, now);
        if (!carved) {
          return null;
        }
        chunk = carved;
      }
      if (chunk.lease_id) {
        this.sql.exec(
          "UPDATE leases SET ended_at = ?, outcome = 'expired' WHERE id = ? AND ended_at IS NULL",
          now,
          chunk.lease_id,
        );
      }
      const leaseId = crypto.randomUUID();
      const epoch = chunk.epoch + 1;
      const expiresAt = now + this.leaseTtlMs;
      this.sql.exec(
        "UPDATE chunks SET state = 'leased', lease_id = ?, worker_id = ?, epoch = ?, expires_at = ? WHERE id = ?",
        leaseId,
        workerId,
        epoch,
        expiresAt,
        chunk.id,
      );
      this.sql.exec(
        "INSERT INTO leases (id, chunk_id, worker_id, epoch, issued_at) VALUES (?, ?, ?, ?, ?)",
        leaseId,
        chunk.id,
        workerId,
        epoch,
        now,
      );
      return this.leaseOf({ ...chunk, lease_id: leaseId, worker_id: workerId, epoch, expires_at: expiresAt });
    });
  }

  private leaseOf(chunk: ChunkRow): Lease {
    return {
      lease_id: chunk.lease_id!,
      epoch: chunk.epoch,
      chunk: {
        schema_version: versions.work_schema,
        generator_version: versions.generator,
        hex_digits: chunk.hex_digits,
        leading_hex_digit: chunk.leading,
        start_index: chunk.start_index,
        end_index: chunk.end_index,
      },
      cursor: chunk.cursor,
      expires_at: chunk.expires_at!,
      report_interval_seconds: setting(this.env, "REPORT_INTERVAL_SECONDS", 300),
    };
  }

  /** Allocates a new chunk from the cheapest frontier space the worker can search. */
  private carve(size: bigint, maxHexDigits: number, now: number): ChunkRow | null {
    this.openFrontier();
    const space = this.sql
      .exec<SpaceRow>("SELECT * FROM spaces WHERE hex_digits <= ?", maxHexDigits)
      .toArray()
      .filter((s) => BigInt(s.next_unallocated) < BigInt(s.total))
      .sort((a, b) => {
        const [ta, tb] = [BigInt(a.total), BigInt(b.total)];
        return ta !== tb ? (ta < tb ? -1 : 1) : a.hex_digits - b.hex_digits || a.leading - b.leading;
      })[0];
    if (!space) {
      return null;
    }
    const start = BigInt(space.next_unallocated);
    const end = minBig(start + size, BigInt(space.total));
    const id = chunkId(space.hex_digits, space.leading, start, end);
    this.sql.exec(
      "UPDATE spaces SET next_unallocated = ? WHERE hex_digits = ? AND leading = ?",
      end.toString(),
      space.hex_digits,
      space.leading,
    );
    this.sql.exec(
      `INSERT INTO chunks (id, hex_digits, leading, start_index, end_index, cursor, state, created_at)
       VALUES (?, ?, ?, ?, ?, ?, 'pending', ?)`,
      id,
      space.hex_digits,
      space.leading,
      start.toString(),
      end.toString(),
      start.toString(),
      now,
    );
    return this.sql.exec<ChunkRow>("SELECT * FROM chunks WHERE id = ?", id).one();
  }

  /** Opens each track's next length once its current spaces are fully allocated. */
  private openFrontier(): void {
    const spaces = this.sql.exec<SpaceRow>("SELECT * FROM spaces").toArray();
    for (const [leads, first] of TRACKS) {
      const track = spaces.filter((s) => leads.includes(s.leading));
      if (track.some((s) => BigInt(s.next_unallocated) < BigInt(s.total))) {
        continue;
      }
      const next = track.length ? Math.max(...track.map((s) => s.hex_digits)) + 1 : first;
      for (const lead of leads) {
        const total = candidateCount(next, lead);
        if (total <= MAX_INDEX) {
          this.sql.exec(
            "INSERT OR IGNORE INTO spaces (hex_digits, leading, total, next_unallocated) VALUES (?, ?, ?, '0')",
            next,
            lead,
            total.toString(),
          );
        }
      }
    }
  }

  /** Extends a lease the caller still holds. */
  renew(workerId: string, leaseId: string): { status: "ok"; expires_at: number } | { status: "superseded" } {
    const chunk = this.heldChunk(workerId, leaseId);
    if (!chunk) {
      return { status: "superseded" };
    }
    const expiresAt = Date.now() + this.leaseTtlMs;
    this.sql.exec("UPDATE chunks SET expires_at = ? WHERE id = ?", expiresAt, chunk.id);
    return { status: "ok", expires_at: expiresAt };
  }

  /** Returns the unsearched remainder of a lease to the pool. */
  release(workerId: string, leaseId: string): { status: "released" | "superseded" } {
    return this.ctx.storage.transactionSync(() => {
      const chunk = this.heldChunk(workerId, leaseId);
      if (!chunk) {
        return { status: "superseded" as const };
      }
      this.sql.exec(
        "UPDATE chunks SET state = 'pending', lease_id = NULL, worker_id = NULL, expires_at = NULL WHERE id = ?",
        chunk.id,
      );
      this.sql.exec("UPDATE leases SET ended_at = ?, outcome = 'released' WHERE id = ?", Date.now(), leaseId);
      return { status: "released" as const };
    });
  }

  /** The chunk a lease is current for: not reassigned, even if past its expiry. */
  private heldChunk(workerId: string, leaseId: string): ChunkRow | null {
    return (
      this.sql
        .exec<ChunkRow>(
          "SELECT * FROM chunks WHERE lease_id = ? AND worker_id = ? AND state = 'leased'",
          leaseId,
          workerId,
        )
        .toArray()[0] ?? null
    );
  }

  // --- Results ------------------------------------------------------------

  /**
   * Accepts a checked segment if it continues the chunk exactly from its
   * cursor and the lease is current, or the chunk is unclaimed. Resubmitting
   * an accepted segment is harmless.
   */
  submitSegment(s: Segment): SegmentResult {
    return this.ctx.storage.transactionSync((): SegmentResult => {
      const now = Date.now();
      const lease = this.sql
        .exec<{ chunk_id: string; worker_id: string; epoch: number }>(
          "SELECT chunk_id, worker_id, epoch FROM leases WHERE id = ?",
          s.leaseId,
        )
        .toArray()[0];
      if (!lease || lease.worker_id !== s.workerId || lease.chunk_id !== s.chunkId || lease.epoch !== s.epoch) {
        return { status: "rejected", reason: "no such lease for this worker, chunk and epoch" };
      }
      const chunk = this.sql.exec<ChunkRow>("SELECT * FROM chunks WHERE id = ?", s.chunkId).one();
      const [start, end] = [BigInt(s.start), BigInt(s.end)];
      if (start >= end || start < BigInt(chunk.start_index) || end > BigInt(chunk.end_index)) {
        return { status: "rejected", reason: `segment [${s.start}, ${s.end}) is outside chunk ${chunk.id}` };
      }
      const previous = this.sql
        .exec<{ digest: string }>(
          "SELECT digest FROM segments WHERE chunk_id = ? AND start_index = ? AND end_index = ? AND worker_id = ?",
          s.chunkId,
          s.start,
          s.end,
          s.workerId,
        )
        .toArray()[0];
      if (previous) {
        return previous.digest === s.digest
          ? { status: "duplicate" }
          : { status: "rejected", reason: "resubmitted with a different survivor set" };
      }

      const current = chunk.lease_id === s.leaseId && chunk.state === "leased";
      const unclaimed = chunk.state === "pending";
      const cursor = BigInt(chunk.cursor);
      if (!current && !unclaimed) {
        return { status: "superseded" };
      }
      if (start > cursor) {
        return { status: "rejected", reason: `segment starts at ${s.start}, after the chunk's cursor ${chunk.cursor}` };
      }
      if (start < cursor) {
        return current
          ? { status: "rejected", reason: `segment starts at ${s.start}, before the chunk's cursor ${chunk.cursor}` }
          : { status: "superseded" };
      }

      this.sql.exec(
        `INSERT INTO segments (chunk_id, start_index, end_index, candidates, worker_id, lease_id, epoch,
                               filter_version, survivor_count, digest, r2_key, backend, status, accepted_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'searched', ?)`,
        s.chunkId,
        s.start,
        s.end,
        Number(end - start),
        s.workerId,
        s.leaseId,
        s.epoch,
        versions.filter,
        s.survivors,
        s.digest,
        s.r2Key,
        s.backend,
        now,
      );
      for (const m of s.matches) {
        this.sql.exec(
          `INSERT OR IGNORE INTO discoveries (decimal, hexadecimal, nontrivial, chunk_id, candidate_index, worker_id, found_at)
           VALUES (?, ?, ?, ?, ?, ?, ?)`,
          m.decimal,
          m.hexadecimal,
          m.nontrivial ? 1 : 0,
          s.chunkId,
          m.index,
          s.workerId,
          now,
        );
      }

      const complete = end === BigInt(chunk.end_index);
      let expiresAt: number | null = null;
      if (complete) {
        this.sql.exec(
          "UPDATE chunks SET cursor = ?, state = 'done', lease_id = NULL, worker_id = NULL, expires_at = NULL WHERE id = ?",
          s.end,
          chunk.id,
        );
        this.sql.exec(
          "UPDATE leases SET ended_at = ?, outcome = 'complete' WHERE id = ? AND ended_at IS NULL",
          now,
          s.leaseId,
        );
      } else if (current) {
        expiresAt = now + this.leaseTtlMs;
        this.sql.exec("UPDATE chunks SET cursor = ?, expires_at = ? WHERE id = ?", s.end, expiresAt, chunk.id);
      } else {
        this.sql.exec("UPDATE chunks SET cursor = ? WHERE id = ?", s.end, chunk.id);
      }
      return { status: "accepted", cursor: s.end, complete, expires_at: expiresAt };
    });
  }

  // --- Imports ------------------------------------------------------------

  /**
   * Records an owner's earlier local search as done, so the frontier continues
   * from it instead of repeating it. Each track's lengths must be contiguous
   * from its first frontier length; shorter (historical) lengths are ignored.
   * Importing the same coverage again is harmless; anything that conflicts with
   * work the coordinator already handed out is refused, all or nothing.
   */
  importCoverage(workerId: string, imported: ImportedSpace[], name: string | null): ImportResult {
    this.sql.exec("UPDATE workers SET name = COALESCE(?, name) WHERE id = ?", name, workerId);
    try {
      // Throwing ImportRejected rolls back whatever this import already wrote.
      return this.ctx.storage.transactionSync((): ImportResult => {
        const now = Date.now();
        let spaces = 0;
        let candidates = 0n;
        for (const [leads, first] of TRACKS) {
          const track = imported.filter((s) => leads.includes(s.leading_hex_digit) && s.hex_digits >= first);
          if (track.length === 0) {
            continue;
          }
          const last = Math.max(...track.map((s) => s.hex_digits));
          for (let n = first; n <= last; n++) {
            for (const lead of leads) {
              const name = spaceName(n, lead);
              const entries = track.filter((s) => s.hex_digits === n && s.leading_hex_digit === lead);
              if (entries.length !== 1) {
                throw new ImportRejected(`${name} must appear exactly once`);
              }
              const total = candidateCount(n, lead);
              const searched = entries[0].searched;
              if (searched < 0n || searched > total) {
                throw new ImportRejected(`${name} searched ${searched} of ${total}`);
              }
              if (total > MAX_INDEX) {
                throw new ImportRejected(`${name} is too large to lease`);
              }
              if (this.importSpace(n, lead, total, searched, workerId, now)) {
                spaces++;
                candidates += searched;
              }
            }
          }
        }
        return { status: "imported", spaces, candidates: candidates.toString() };
      });
    } catch (e) {
      if (e instanceof ImportRejected) {
        return { status: "rejected", reason: e.message };
      }
      throw e;
    }
  }

  /** Imports one space; false if this exact import was already recorded. */
  private importSpace(n: number, lead: number, total: bigint, searched: bigint, workerId: string, now: number): boolean {
    const name = spaceName(n, lead);
    const previous = this.sql
      .exec<{ searched: string }>("SELECT searched FROM imports WHERE hex_digits = ? AND leading = ?", n, lead)
      .toArray()[0];
    if (previous) {
      if (BigInt(previous.searched) === searched) {
        return false;
      }
      throw new ImportRejected(`${name} was already imported with ${previous.searched} searched`);
    }
    const chunks = this.sql
      .exec<{ n: number }>("SELECT COUNT(*) AS n FROM chunks WHERE hex_digits = ? AND leading = ?", n, lead)
      .one().n;
    if (chunks > 0) {
      throw new ImportRejected(`the coordinator has already handed out work in ${name}`);
    }
    this.sql.exec(
      `INSERT INTO spaces (hex_digits, leading, total, next_unallocated) VALUES (?, ?, ?, ?)
       ON CONFLICT (hex_digits, leading) DO UPDATE SET next_unallocated = excluded.next_unallocated`,
      n,
      lead,
      total.toString(),
      searched.toString(),
    );
    if (searched > 0n) {
      const id = chunkId(n, lead, 0n, searched);
      this.sql.exec(
        `INSERT INTO chunks (id, hex_digits, leading, start_index, end_index, cursor, state, created_at)
         VALUES (?, ?, ?, '0', ?, ?, 'done', ?)`,
        id,
        n,
        lead,
        searched.toString(),
        searched.toString(),
        now,
      );
    }
    this.sql.exec(
      "INSERT INTO imports (hex_digits, leading, searched, worker_id, imported_at) VALUES (?, ?, ?, ?, ?)",
      n,
      lead,
      searched.toString(),
      workerId,
      now,
    );
    return true;
  }

  // --- Statistics ---------------------------------------------------------

  /** Every worker with its private details, for the owner only. */
  workers(): WorkerInfo[] {
    return this.sql
      .exec<WorkerInfo>(
        `SELECT w.id AS worker_id, w.name, w.hostname, w.gpu, w.network, w.registered_at, w.last_seen,
                COALESCE((SELECT SUM(candidates) FROM segments s WHERE s.worker_id = w.id), 0) +
                COALESCE((SELECT SUM(CAST(searched AS REAL)) FROM imports i WHERE i.worker_id = w.id), 0) AS candidates,
                (SELECT COUNT(*) FROM discoveries d WHERE d.worker_id = w.id) AS discoveries
         FROM workers w ORDER BY w.last_seen DESC`,
      )
      .toArray();
  }

  coverage() {
    const now = Date.now();
    const chunks = this.sql.exec<ChunkRow>("SELECT * FROM chunks").toArray();
    const spaces = this.sql
      .exec<SpaceRow>("SELECT * FROM spaces ORDER BY hex_digits, leading")
      .toArray()
      .map((s) => {
        const mine = chunks.filter((c) => c.hex_digits === s.hex_digits && c.leading === s.leading);
        const searched = mine.reduce((sum, c) => sum + BigInt(c.cursor) - BigInt(c.start_index), 0n);
        return {
          hex_digits: s.hex_digits,
          leading_hex_digit: s.leading,
          total: s.total,
          allocated: s.next_unallocated,
          searched: searched.toString(),
          leased_chunks: mine.filter((c) => c.state === "leased" && c.expires_at! >= now).length,
        };
      });
    const lastHour = this.sql
      .exec<{ candidates: number; workers: number }>(
        "SELECT COALESCE(SUM(candidates), 0) AS candidates, COUNT(DISTINCT worker_id) AS workers FROM segments WHERE accepted_at >= ?",
        now - 3_600_000,
      )
      .one();
    const contributors = this.sql
      .exec<{ worker: string; name: string | null; candidates: number; last_seen: number }>(
        `SELECT w.id AS worker, w.name AS name, w.last_seen AS last_seen,
                COALESCE((SELECT SUM(candidates) FROM segments s WHERE s.worker_id = w.id), 0) +
                COALESCE((SELECT SUM(CAST(searched AS REAL)) FROM imports i WHERE i.worker_id = w.id), 0) AS candidates
         FROM workers w ORDER BY candidates DESC LIMIT 25`,
      )
      .toArray()
      .filter((c) => c.candidates > 0);
    return {
      versions,
      spaces,
      contributors,
      workers_registered: this.sql.exec<{ n: number }>("SELECT COUNT(*) AS n FROM workers").one().n,
      workers_last_hour: lastHour.workers,
      candidates_per_second_last_hour: lastHour.candidates / 3600,
      discoveries: this.sql
        .exec(
          `SELECT d.*, w.name AS worker_name FROM discoveries d LEFT JOIN workers w ON w.id = d.worker_id
           ORDER BY d.found_at`,
        )
        .toArray(),
    };
  }
}

