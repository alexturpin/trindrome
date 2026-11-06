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
-- A second, independent search of one accepted segment (docs/distributed.md#replication-and-trust).
-- A tiebreak is a third search, by a trusted worker, after a replica disagreed.
CREATE TABLE IF NOT EXISTS replicas (
  id TEXT PRIMARY KEY,
  segment_id INTEGER NOT NULL,
  purpose TEXT NOT NULL CHECK (purpose IN ('replica', 'tiebreak')),
  chunk_id TEXT NOT NULL,
  hex_digits INTEGER NOT NULL,
  leading INTEGER NOT NULL,
  start_index TEXT NOT NULL,
  end_index TEXT NOT NULL,
  cursor TEXT NOT NULL,
  state TEXT NOT NULL CHECK (state IN ('pending', 'leased', 'done', 'cancelled')),
  lease_id TEXT,
  worker_id TEXT,
  epoch INTEGER NOT NULL DEFAULT 0,
  expires_at INTEGER,
  survivors INTEGER NOT NULL DEFAULT 0,
  fingerprint TEXT NOT NULL DEFAULT '0',
  outcome TEXT,
  created_at INTEGER NOT NULL,
  finished_at INTEGER
);
CREATE INDEX IF NOT EXISTS replicas_by_state ON replicas (state, created_at);
CREATE INDEX IF NOT EXISTS replicas_by_segment ON replicas (segment_id);
CREATE TABLE IF NOT EXISTS replica_segments (
  id INTEGER PRIMARY KEY,
  replica_id TEXT NOT NULL,
  start_index TEXT NOT NULL,
  end_index TEXT NOT NULL,
  candidates REAL NOT NULL,
  worker_id TEXT NOT NULL,
  lease_id TEXT NOT NULL,
  epoch INTEGER NOT NULL,
  survivor_count INTEGER NOT NULL,
  fingerprint TEXT NOT NULL,
  digest TEXT NOT NULL,
  r2_key TEXT NOT NULL,
  backend TEXT,
  accepted_at INTEGER NOT NULL,
  UNIQUE (replica_id, start_index, end_index, worker_id)
);
CREATE INDEX IF NOT EXISTS replica_segments_by_time ON replica_segments (accepted_at);
CREATE INDEX IF NOT EXISTS segments_by_worker ON segments (worker_id, status);
`;

/** Columns added after the first deployment: table → [column, definition]. */
const ADDED_COLUMNS: [string, string, string][] = [
  ["workers", "name", "TEXT"],
  ["workers", "hostname", "TEXT"],
  // Segments confirmed by a trusted or established replica.
  ["workers", "matched", "INTEGER NOT NULL DEFAULT 0"],
  ["segments", "fingerprint", "TEXT"],
  // 1 while the segment still needs an independent replica to be confirmed.
  ["segments", "replicate", "INTEGER NOT NULL DEFAULT 0"],
  // How a voided segment's range went back to the pool: 'chunk' (its whole
  // chunk was reopened) or 'redo' (a new chunk covers just the segment).
  ["segments", "reopened", "TEXT"],
  ["leases", "replica_id", "TEXT"],
];

const MASK64 = (1n << 64n) - 1n;

export type Trust = "new" | "established" | "trusted";

export interface Capabilities {
  max_hex_digits: number;
  /** Measured candidates per second. */
  rate: number;
  /** Display name chosen by the worker's owner, if any. */
  name: string | null;
  /** The computer's name. Private: never shown on the public page. */
  hostname: string | null;
  /** Signed with an owner key: such a worker is always trusted. */
  owner: boolean;
}

/** A worker as its owner sees it, private details included. */
export interface WorkerInfo {
  worker_id: string;
  name: string | null;
  hostname: string | null;
  gpu: string | null;
  network: string;
  trust: Trust;
  matched: number;
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
  /** survivor_fingerprint as a decimal u64. */
  fingerprint: string;
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

interface ReplicaRow {
  id: string;
  segment_id: number;
  purpose: "replica" | "tiebreak";
  chunk_id: string;
  hex_digits: number;
  leading: number;
  start_index: string;
  end_index: string;
  cursor: string;
  state: "pending" | "leased" | "done" | "cancelled";
  lease_id: string | null;
  worker_id: string | null;
  epoch: number;
  expires_at: number | null;
  survivors: number;
  fingerprint: string;
  outcome: string | null;
  [key: string]: SqlStorageValue;
}

interface SegmentRow {
  id: number;
  chunk_id: string;
  start_index: string;
  end_index: string;
  worker_id: string;
  survivor_count: number;
  fingerprint: string | null;
  status: "searched" | "confirmed" | "voided";
  replicate: number;
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

/**
 * A worker's contribution, for `workers w`: its accepted segments that weren't
 * voided, its imports, and the replicas it searched for others.
 */
const WORK_DONE = `(
  COALESCE((SELECT SUM(candidates) FROM segments s WHERE s.worker_id = w.id AND s.status != 'voided'), 0) +
  COALESCE((SELECT SUM(CAST(searched AS REAL)) FROM imports i WHERE i.worker_id = w.id), 0) +
  COALESCE((SELECT SUM(candidates) FROM replica_segments r WHERE r.worker_id = w.id), 0))`;

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
    const added: string[] = [];
    for (const [table, column, definition] of ADDED_COLUMNS) {
      const columns = this.sql.exec<{ name: string }>(`PRAGMA table_info(${table})`).toArray();
      if (!columns.some((c) => c.name === column)) {
        this.sql.exec(`ALTER TABLE ${table} ADD COLUMN ${column} ${definition}`);
        added.push(`${table}.${column}`);
      }
    }
    if (added.includes("segments.replicate")) {
      // Segments accepted before replication existed need replicas like any
      // other from a `new` worker (until their worker is marked trusted).
      ctx.storage.transactionSync(() => {
        const now = Date.now();
        const old = this.sql
          .exec<{ id: number; start_index: string; end_index: string; hex_digits: number; leading: number }>(
            `SELECT s.id, s.start_index, s.end_index, c.hex_digits, c.leading
             FROM segments s JOIN chunks c ON c.id = s.chunk_id JOIN workers w ON w.id = s.worker_id
             WHERE s.status = 'searched' AND w.trust = 'new'`,
          )
          .toArray();
        for (const s of old) {
          this.sql.exec("UPDATE segments SET replicate = 1 WHERE id = ?", s.id);
          this.queueReplica(s.id, "replica", s.hex_digits, s.leading, BigInt(s.start_index), BigInt(s.end_index), now);
        }
      });
    }
  }

  private get matchesToEstablish(): number {
    return setting(this.env, "MATCHES_TO_ESTABLISH", 20);
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
    if (caps.owner) {
      this.setTrust(workerId, "trusted");
    }
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
      const me = this.sql
        .exec<{ trust: Trust; network: string }>("SELECT trust, network FROM workers WHERE id = ?", workerId)
        .one();
      const held: (ChunkRow | ReplicaRow)[] = [
        ...this.sql
          .exec<ChunkRow>(
            "SELECT * FROM chunks WHERE state = 'leased' AND worker_id = ? AND expires_at >= ? ORDER BY expires_at",
            workerId,
            now,
          )
          .toArray(),
        ...this.sql
          .exec<ReplicaRow>(
            "SELECT * FROM replicas WHERE state = 'leased' AND worker_id = ? AND expires_at >= ? ORDER BY expires_at",
            workerId,
            now,
          )
          .toArray(),
      ];
      if (held.length >= (me.trust === "new" ? 1 : 2)) {
        return this.leaseOf(held[0]);
      }

      // Checking other workers' results comes before new work.
      const replica = this.nextReplica(workerId, me.trust, me.network, caps.max_hex_digits, now);
      if (replica) {
        return this.leaseReplica(replica, workerId, now);
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

  /**
   * The oldest replica this worker may search: never its own segment, nor one
   * in a chunk it ever held, and for a `new` worker only one from another
   * network. Tiebreaks go to trusted workers only, and come first for them.
   */
  private nextReplica(workerId: string, trust: Trust, network: string, maxHexDigits: number, now: number): ReplicaRow | null {
    const open = "(r.state = 'pending' OR (r.state = 'leased' AND r.expires_at < ?))";
    if (trust === "trusted") {
      const tiebreak = this.sql
        .exec<ReplicaRow>(
          `SELECT r.* FROM replicas r JOIN segments s ON s.id = r.segment_id
           WHERE r.purpose = 'tiebreak' AND ${open} AND r.hex_digits <= ? AND s.worker_id != ?
             AND NOT EXISTS (SELECT 1 FROM replicas p WHERE p.segment_id = r.segment_id AND p.purpose = 'replica' AND p.worker_id = ?)
           ORDER BY r.created_at LIMIT 1`,
          now,
          maxHexDigits,
          workerId,
          workerId,
        )
        .toArray()[0];
      if (tiebreak) {
        return tiebreak;
      }
    }
    return (
      this.sql
        .exec<ReplicaRow>(
          `SELECT r.* FROM replicas r JOIN segments s ON s.id = r.segment_id JOIN workers o ON o.id = s.worker_id
           WHERE r.purpose = 'replica' AND ${open} AND r.hex_digits <= ? AND s.worker_id != ?
             AND (? != 'new' OR o.network != ?)
             AND NOT EXISTS (SELECT 1 FROM leases l WHERE l.chunk_id = s.chunk_id AND l.worker_id = ?)
           ORDER BY r.created_at LIMIT 1`,
          now,
          maxHexDigits,
          workerId,
          trust,
          network,
          workerId,
        )
        .toArray()[0] ?? null
    );
  }

  /** Leases a replica from its start: a replica is always searched by one worker. */
  private leaseReplica(replica: ReplicaRow, workerId: string, now: number): Lease {
    if (replica.lease_id) {
      this.sql.exec(
        "UPDATE leases SET ended_at = ?, outcome = 'expired' WHERE id = ? AND ended_at IS NULL",
        now,
        replica.lease_id,
      );
    }
    const leaseId = crypto.randomUUID();
    const epoch = replica.epoch + 1;
    const expiresAt = now + this.leaseTtlMs;
    this.sql.exec(
      `UPDATE replicas SET state = 'leased', lease_id = ?, worker_id = ?, epoch = ?, expires_at = ?,
                           cursor = start_index, survivors = 0, fingerprint = '0'
       WHERE id = ?`,
      leaseId,
      workerId,
      epoch,
      expiresAt,
      replica.id,
    );
    this.sql.exec(
      "INSERT INTO leases (id, chunk_id, worker_id, epoch, issued_at, replica_id) VALUES (?, ?, ?, ?, ?, ?)",
      leaseId,
      replica.chunk_id,
      workerId,
      epoch,
      now,
      replica.id,
    );
    return this.leaseOf({
      ...replica,
      cursor: replica.start_index,
      lease_id: leaseId,
      worker_id: workerId,
      epoch,
      expires_at: expiresAt,
    });
  }

  private leaseOf(chunk: ChunkRow | ReplicaRow): Lease {
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
    const expiresAt = Date.now() + this.leaseTtlMs;
    const chunk = this.heldChunk(workerId, leaseId);
    if (chunk) {
      this.sql.exec("UPDATE chunks SET expires_at = ? WHERE id = ?", expiresAt, chunk.id);
      return { status: "ok", expires_at: expiresAt };
    }
    const replica = this.heldReplica(workerId, leaseId);
    if (replica) {
      this.sql.exec("UPDATE replicas SET expires_at = ? WHERE id = ?", expiresAt, replica.id);
      return { status: "ok", expires_at: expiresAt };
    }
    return { status: "superseded" };
  }

  /** Returns the unsearched remainder of a lease to the pool. */
  release(workerId: string, leaseId: string): { status: "released" | "superseded" } {
    return this.ctx.storage.transactionSync(() => {
      const chunk = this.heldChunk(workerId, leaseId);
      const replica = chunk ? null : this.heldReplica(workerId, leaseId);
      if (chunk) {
        this.sql.exec(
          "UPDATE chunks SET state = 'pending', lease_id = NULL, worker_id = NULL, expires_at = NULL WHERE id = ?",
          chunk.id,
        );
      } else if (replica) {
        // A replica is searched by one worker from its start, so progress is dropped.
        this.sql.exec(
          `UPDATE replicas SET state = 'pending', lease_id = NULL, worker_id = NULL, expires_at = NULL,
                               cursor = start_index, survivors = 0, fingerprint = '0'
           WHERE id = ?`,
          replica.id,
        );
      } else {
        return { status: "superseded" as const };
      }
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

  private heldReplica(workerId: string, leaseId: string): ReplicaRow | null {
    return (
      this.sql
        .exec<ReplicaRow>(
          "SELECT * FROM replicas WHERE lease_id = ? AND worker_id = ? AND state = 'leased'",
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
        .exec<{ chunk_id: string; worker_id: string; epoch: number; replica_id: string | null }>(
          "SELECT chunk_id, worker_id, epoch, replica_id FROM leases WHERE id = ?",
          s.leaseId,
        )
        .toArray()[0];
      if (!lease || lease.worker_id !== s.workerId || lease.chunk_id !== s.chunkId || lease.epoch !== s.epoch) {
        return { status: "rejected", reason: "no such lease for this worker, chunk and epoch" };
      }
      if (lease.replica_id) {
        return this.submitReplicaSegment(s, lease.replica_id, now);
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

      const trust = this.sql.exec<{ trust: Trust }>("SELECT trust FROM workers WHERE id = ?", s.workerId).one().trust;
      const replicate = trust === "new";
      const segmentId = this.sql
        .exec<{ id: number }>(
          `INSERT INTO segments (chunk_id, start_index, end_index, candidates, worker_id, lease_id, epoch,
                                 filter_version, survivor_count, digest, fingerprint, r2_key, backend, status,
                                 replicate, accepted_at)
           VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, 'searched', ?, ?) RETURNING id`,
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
          s.fingerprint,
          s.r2Key,
          s.backend,
          replicate ? 1 : 0,
          now,
        )
        .one().id;
      if (replicate) {
        this.queueReplica(segmentId, "replica", chunk.hex_digits, chunk.leading, start, end, now);
      }
      this.recordMatches(s, now);

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

  /** Every checked match is a verified value, whoever found it and whatever happens to its segment. */
  private recordMatches(s: Segment, now: number): void {
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
  }

  private queueReplica(
    segmentId: number,
    purpose: "replica" | "tiebreak",
    hexDigits: number,
    leading: number,
    start: bigint,
    end: bigint,
    now: number,
  ): void {
    this.sql.exec(
      `INSERT INTO replicas (id, segment_id, purpose, chunk_id, hex_digits, leading, start_index, end_index, cursor, state, created_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, 'pending', ?)`,
      crypto.randomUUID(),
      segmentId,
      purpose,
      chunkId(hexDigits, leading, start, end),
      hexDigits,
      leading,
      start.toString(),
      end.toString(),
      start.toString(),
      now,
    );
  }

  /** A segment of a replica: accepted only from its current holder, exactly from its cursor. */
  private submitReplicaSegment(s: Segment, replicaId: string, now: number): SegmentResult {
    const replica = this.sql.exec<ReplicaRow>("SELECT * FROM replicas WHERE id = ?", replicaId).one();
    const [start, end] = [BigInt(s.start), BigInt(s.end)];
    if (start >= end || start < BigInt(replica.start_index) || end > BigInt(replica.end_index)) {
      return { status: "rejected", reason: `segment [${s.start}, ${s.end}) is outside the replica` };
    }
    const previous = this.sql
      .exec<{ digest: string }>(
        "SELECT digest FROM replica_segments WHERE replica_id = ? AND start_index = ? AND end_index = ? AND worker_id = ? AND epoch = ?",
        replicaId,
        s.start,
        s.end,
        s.workerId,
        s.epoch,
      )
      .toArray()[0];
    if (previous) {
      return previous.digest === s.digest
        ? { status: "duplicate" }
        : { status: "rejected", reason: "resubmitted with a different survivor set" };
    }
    if (replica.lease_id !== s.leaseId || replica.state !== "leased") {
      return { status: "superseded" };
    }
    if (start !== BigInt(replica.cursor)) {
      return { status: "rejected", reason: `segment starts at ${s.start}, not at the replica's cursor ${replica.cursor}` };
    }

    this.sql.exec(
      `INSERT INTO replica_segments (replica_id, start_index, end_index, candidates, worker_id, lease_id, epoch,
                                     survivor_count, fingerprint, digest, r2_key, backend, accepted_at)
       VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)`,
      replicaId,
      s.start,
      s.end,
      Number(end - start),
      s.workerId,
      s.leaseId,
      s.epoch,
      s.survivors,
      s.fingerprint,
      s.digest,
      s.r2Key,
      s.backend,
      now,
    );
    // A replica can find a match its original left out.
    this.recordMatches(s, now);
    const survivors = replica.survivors + s.survivors;
    const fingerprint = ((BigInt(replica.fingerprint) + BigInt(s.fingerprint)) & MASK64).toString();
    const complete = end === BigInt(replica.end_index);
    let expiresAt: number | null = null;
    if (complete) {
      this.sql.exec(
        `UPDATE replicas SET cursor = ?, survivors = ?, fingerprint = ?, state = 'done', lease_id = NULL,
                             expires_at = NULL, finished_at = ?
         WHERE id = ?`,
        s.end,
        survivors,
        fingerprint,
        now,
        replicaId,
      );
      this.sql.exec("UPDATE leases SET ended_at = ?, outcome = 'complete' WHERE id = ? AND ended_at IS NULL", now, s.leaseId);
      this.settle({ ...replica, cursor: s.end, survivors, fingerprint, worker_id: s.workerId }, now);
    } else {
      expiresAt = now + this.leaseTtlMs;
      this.sql.exec(
        "UPDATE replicas SET cursor = ?, survivors = ?, fingerprint = ?, expires_at = ? WHERE id = ?",
        s.end,
        survivors,
        fingerprint,
        expiresAt,
        replicaId,
      );
    }
    return { status: "accepted", cursor: s.end, complete, expires_at: expiresAt };
  }

  /**
   * Compares a finished replica with its segment. A match confirms the
   * segment. A mismatch with a trusted replica voids the segment's worker; any
   * other mismatch asks a trusted worker to break the tie.
   */
  private settle(replica: ReplicaRow, now: number): void {
    const segment = this.sql.exec<SegmentRow>("SELECT * FROM segments WHERE id = ?", replica.segment_id).one();
    const setOutcome = (outcome: string) =>
      this.sql.exec("UPDATE replicas SET outcome = ? WHERE id = ?", outcome, replica.id);
    if (segment.status !== "searched") {
      // Voided (or confirmed) while this replica ran.
      setOutcome("moot");
      return;
    }
    const agrees = (r: { survivors: number; fingerprint: string }) =>
      r.survivors === segment.survivor_count && (segment.fingerprint === null || r.fingerprint === segment.fingerprint);
    const checker = this.sql
      .exec<{ trust: Trust }>("SELECT trust FROM workers WHERE id = ?", replica.worker_id)
      .one().trust;

    if (replica.purpose === "replica") {
      if (agrees(replica)) {
        setOutcome("match");
        this.confirm(segment, checker !== "new");
      } else if (checker === "trusted") {
        setOutcome("mismatch");
        this.demote(segment.worker_id, now);
      } else {
        setOutcome("mismatch");
        const chunk = this.sql
          .exec<{ hex_digits: number; leading: number }>("SELECT hex_digits, leading FROM chunks WHERE id = ?", segment.chunk_id)
          .one();
        this.queueReplica(
          segment.id,
          "tiebreak",
          chunk.hex_digits,
          chunk.leading,
          BigInt(segment.start_index),
          BigInt(segment.end_index),
          now,
        );
      }
      return;
    }

    // A tiebreak: its trusted result decides who was wrong.
    const disputed = this.sql
      .exec<ReplicaRow>(
        "SELECT * FROM replicas WHERE segment_id = ? AND purpose = 'replica' AND outcome = 'mismatch' ORDER BY finished_at DESC LIMIT 1",
        segment.id,
      )
      .toArray()[0];
    const same = (r: ReplicaRow) => r.survivors === replica.survivors && r.fingerprint === replica.fingerprint;
    if (agrees(replica)) {
      setOutcome("original right");
      this.confirm(segment, true);
      if (disputed?.worker_id) {
        this.demote(disputed.worker_id, now);
      }
    } else {
      setOutcome(disputed && same(disputed) ? "replica right" : "both wrong");
      this.demote(segment.worker_id, now);
      if (disputed?.worker_id && !same(disputed)) {
        this.demote(disputed.worker_id, now);
      }
    }
  }

  /** Marks a segment confirmed; a check by a trusted or established worker also earns its worker trust. */
  private confirm(segment: SegmentRow, earnsTrust: boolean): void {
    this.sql.exec("UPDATE segments SET status = 'confirmed', replicate = 0 WHERE id = ?", segment.id);
    if (!earnsTrust) {
      return;
    }
    this.sql.exec("UPDATE workers SET matched = matched + 1 WHERE id = ?", segment.worker_id);
    this.sql.exec(
      "UPDATE workers SET trust = 'established' WHERE id = ? AND trust = 'new' AND matched >= ?",
      segment.worker_id,
      this.matchesToEstablish,
    );
  }

  /**
   * A worker was caught with a wrong result: it goes back to `new`, and every
   * segment of its that nobody confirmed goes back to the pool. Trusted
   * workers are only ever changed by hand.
   */
  private demote(workerId: string, now: number): void {
    const trust = this.sql.exec<{ trust: Trust }>("SELECT trust FROM workers WHERE id = ?", workerId).one().trust;
    if (trust === "trusted") {
      console.error(`trusted worker ${workerId} disagreed with a tiebreak; check it by hand`);
      return;
    }
    console.warn(`worker ${workerId} returned a wrong result; voiding its unconfirmed segments`);
    this.sql.exec("UPDATE workers SET trust = 'new', matched = 0 WHERE id = ?", workerId);
    const segments = this.sql
      .exec<SegmentRow>("SELECT * FROM segments WHERE worker_id = ? AND status = 'searched'", workerId)
      .toArray();
    for (const segment of segments) {
      this.voidSegment(segment, now);
    }
  }

  /** Voids a segment and returns its range to the pool. */
  private voidSegment(segment: SegmentRow, now: number): void {
    this.sql.exec(
      "UPDATE replicas SET state = 'cancelled', lease_id = NULL, expires_at = NULL WHERE segment_id = ? AND state IN ('pending', 'leased')",
      segment.id,
    );
    const chunk = this.sql.exec<ChunkRow>("SELECT * FROM chunks WHERE id = ?", segment.chunk_id).one();
    let reopened: "chunk" | "redo";
    if (segment.start_index === chunk.start_index && segment.end_index === chunk.end_index) {
      // The segment was the whole chunk: search the chunk again.
      this.sql.exec(
        "UPDATE chunks SET state = 'pending', cursor = start_index, lease_id = NULL, worker_id = NULL, expires_at = NULL WHERE id = ?",
        chunk.id,
      );
      reopened = "chunk";
    } else {
      this.sql.exec(
        `INSERT INTO chunks (id, hex_digits, leading, start_index, end_index, cursor, state, created_at)
         VALUES (?, ?, ?, ?, ?, ?, 'pending', ?)`,
        chunkId(chunk.hex_digits, chunk.leading, BigInt(segment.start_index), BigInt(segment.end_index)),
        chunk.hex_digits,
        chunk.leading,
        segment.start_index,
        segment.end_index,
        segment.start_index,
        now,
      );
      reopened = "redo";
    }
    this.sql.exec("UPDATE segments SET status = 'voided', replicate = 0, reopened = ? WHERE id = ?", reopened, segment.id);
  }

  // --- Trust --------------------------------------------------------------

  /**
   * Sets a worker's trust by hand (owner only). Making a worker trusted or
   * established drops the replicas still queued for its segments.
   */
  setTrust(workerId: string, trust: Trust): { status: "ok"; cancelled_replicas: number } | { status: "unknown_worker" } {
    return this.ctx.storage.transactionSync(() => {
      const exists = this.sql.exec("SELECT 1 FROM workers WHERE id = ?", workerId).toArray().length > 0;
      if (!exists) {
        return { status: "unknown_worker" as const };
      }
      const current = this.sql.exec<{ trust: Trust }>("SELECT trust FROM workers WHERE id = ?", workerId).one().trust;
      this.sql.exec("UPDATE workers SET trust = ? WHERE id = ?", trust, workerId);
      let cancelled = 0;
      if (current !== trust && trust !== "new") {
        console.log(`worker ${workerId} is now ${trust}`);
      }
      if (trust !== "new") {
        cancelled = this.sql
          .exec<{ n: number }>(
            `SELECT COUNT(*) AS n FROM replicas
             WHERE state IN ('pending', 'leased') AND segment_id IN (SELECT id FROM segments WHERE worker_id = ? AND status = 'searched')`,
            workerId,
          )
          .one().n;
        this.sql.exec(
          `UPDATE replicas SET state = 'cancelled', lease_id = NULL, expires_at = NULL
           WHERE state IN ('pending', 'leased') AND segment_id IN (SELECT id FROM segments WHERE worker_id = ? AND status = 'searched')`,
          workerId,
        );
        this.sql.exec("UPDATE segments SET replicate = 0 WHERE worker_id = ? AND status = 'searched'", workerId);
      }
      return { status: "ok" as const, cancelled_replicas: cancelled };
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
        `SELECT w.id AS worker_id, w.name, w.hostname, w.gpu, w.network, w.trust, w.matched, w.registered_at, w.last_seen,
                ${WORK_DONE} AS candidates,
                (SELECT COUNT(*) FROM discoveries d WHERE d.worker_id = w.id) AS discoveries
         FROM workers w ORDER BY w.last_seen DESC`,
      )
      .toArray();
  }

  coverage() {
    const now = Date.now();
    const chunks = this.sql.exec<ChunkRow>("SELECT * FROM chunks").toArray();
    // Voided ranges a redo chunk covers again: its parent chunk's cursor still counts them.
    const redone = this.sql
      .exec<{ hex_digits: number; leading: number; start_index: string; end_index: string }>(
        `SELECT c.hex_digits, c.leading, s.start_index, s.end_index
         FROM segments s JOIN chunks c ON c.id = s.chunk_id WHERE s.status = 'voided' AND s.reopened = 'redo'`,
      )
      .toArray();
    const spaces = this.sql
      .exec<SpaceRow>("SELECT * FROM spaces ORDER BY hex_digits, leading")
      .toArray()
      .map((s) => {
        const mine = chunks.filter((c) => c.hex_digits === s.hex_digits && c.leading === s.leading);
        const searched =
          mine.reduce((sum, c) => sum + BigInt(c.cursor) - BigInt(c.start_index), 0n) -
          redone
            .filter((v) => v.hex_digits === s.hex_digits && v.leading === s.leading)
            .reduce((sum, v) => sum + BigInt(v.end_index) - BigInt(v.start_index), 0n);
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
        `SELECT COALESCE(SUM(candidates), 0) AS candidates, COUNT(DISTINCT worker_id) AS workers FROM (
           SELECT candidates, worker_id FROM segments WHERE accepted_at >= ?1
           UNION ALL SELECT candidates, worker_id FROM replica_segments WHERE accepted_at >= ?1)`,
        now - 3_600_000,
      )
      .one();
    const contributors = this.sql
      .exec<{ worker: string; name: string | null; candidates: number; last_seen: number }>(
        `SELECT w.id AS worker, w.name AS name, w.last_seen AS last_seen, ${WORK_DONE} AS candidates
         FROM workers w ORDER BY candidates DESC LIMIT 25`,
      )
      .toArray()
      .filter((c) => c.candidates > 0);
    // Checked: confirmed by a replica, or from a trusted or established worker
    // (or imported by an owner). Awaiting: from a `new` worker, replica pending.
    const checks = this.sql
      .exec<{ checked: number; awaiting: number }>(
        `SELECT COALESCE(SUM(CASE WHEN replicate = 0 THEN candidates END), 0) AS checked,
                COALESCE(SUM(CASE WHEN replicate = 1 THEN candidates END), 0) AS awaiting
         FROM segments WHERE status != 'voided'`,
      )
      .one();
    const imported = this.sql
      .exec<{ n: number }>("SELECT COALESCE(SUM(CAST(searched AS REAL)), 0) AS n FROM imports")
      .one().n;
    return {
      versions,
      spaces,
      contributors,
      candidates_checked: checks.checked + imported,
      candidates_awaiting_check: checks.awaiting,
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

