//! Durable local state in SQLite.
//!
//! Every write that records progress happens in one transaction together with any
//! discoveries from the same work, so the database can never claim coverage that
//! was not computed. Coverage is derived from the chunk records themselves
//! (`[start_index, next_index)` of every chunk), so there is no second ledger that
//! could drift from them.

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail, ensure};
use num_bigint::BigUint;
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use trindrome_core::{GENERATOR_VERSION, SearchSpace, Verification, WorkChunk};
use trindrome_cpu::Match;

use crate::configuration::Search;

pub const STORE_SCHEMA_VERSION: u32 = 1;

/// Leading digits sharing an alphabet size, with the last length Regan covered in 2010.
/// Each track advances through hexadecimal lengths independently.
pub const TRACKS: [(&[u8], u32); 2] = [(&[0x1, 0x3], 64), (&[0x5, 0x7, 0x9, 0xF], 32)];

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum Family {
    Historical,
    Frontier,
}

impl Family {
    pub fn of(space: SearchSpace) -> Self {
        let historical_max = if space.radix() == 2 { TRACKS[0].1 } else { TRACKS[1].1 };
        if space.hex_digits() <= historical_max {
            Self::Historical
        } else {
            Self::Frontier
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Historical => "historical",
            Self::Frontier => "frontier",
        }
    }

    fn included_by(self, search: &Search) -> bool {
        match self {
            Self::Historical => search.mode.includes_historical(),
            Self::Frontier => search.mode.includes_frontier(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Requested {
    Run,
    Pause,
    Stop,
}

impl Requested {
    fn as_str(self) -> &'static str {
        match self {
            Self::Run => "run",
            Self::Pause => "pause",
            Self::Stop => "stop",
        }
    }
}

pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64)
}

fn parse_big(s: &str) -> Result<BigUint> {
    BigUint::parse_bytes(s.as_bytes(), 10).ok_or_else(|| anyhow!("corrupt index {s:?} in database"))
}

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS meta (
    key TEXT PRIMARY KEY,
    value TEXT NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS search_spaces (
    generator_version INTEGER NOT NULL,
    hex_digits INTEGER NOT NULL,
    leading_digit INTEGER NOT NULL,
    family TEXT NOT NULL CHECK (family IN ('historical', 'frontier')),
    total_candidates TEXT NOT NULL,
    next_unallocated TEXT NOT NULL,
    created_at INTEGER NOT NULL,
    PRIMARY KEY (generator_version, hex_digits, leading_digit)
) STRICT;

CREATE TABLE IF NOT EXISTS work_chunks (
    id TEXT PRIMARY KEY,
    generator_version INTEGER NOT NULL,
    hex_digits INTEGER NOT NULL,
    leading_digit INTEGER NOT NULL,
    start_index TEXT NOT NULL,
    end_index TEXT NOT NULL,
    next_index TEXT NOT NULL,
    state TEXT NOT NULL CHECK (state IN ('pending', 'running', 'complete')),
    attempts INTEGER NOT NULL DEFAULT 0,
    backend TEXT,
    active_ms INTEGER NOT NULL DEFAULT 0,
    created_at INTEGER NOT NULL,
    last_checkpoint_at INTEGER,
    completed_at INTEGER,
    FOREIGN KEY (generator_version, hex_digits, leading_digit) REFERENCES search_spaces
) STRICT;

CREATE INDEX IF NOT EXISTS work_chunks_by_state ON work_chunks (state);

CREATE TABLE IF NOT EXISTS discoveries (
    decimal TEXT PRIMARY KEY,
    hexadecimal TEXT NOT NULL,
    binary TEXT NOT NULL,
    nontrivial INTEGER NOT NULL,
    verification TEXT NOT NULL,
    chunk_id TEXT NOT NULL REFERENCES work_chunks (id),
    candidate_index TEXT NOT NULL,
    discovered_at INTEGER NOT NULL
) STRICT;

CREATE TABLE IF NOT EXISTS performance_samples (
    id INTEGER PRIMARY KEY,
    recorded_at INTEGER NOT NULL,
    hardware TEXT NOT NULL,
    backend TEXT NOT NULL,
    integer_bits INTEGER NOT NULL,
    hex_digits INTEGER NOT NULL,
    leading_digit INTEGER NOT NULL,
    candidates INTEGER NOT NULL,
    active_ms INTEGER NOT NULL
) STRICT;

CREATE INDEX IF NOT EXISTS performance_samples_by_time ON performance_samples (recorded_at);

CREATE TABLE IF NOT EXISTS worker_state (
    id INTEGER PRIMARY KEY CHECK (id = 1),
    requested TEXT NOT NULL DEFAULT 'run' CHECK (requested IN ('run', 'pause', 'stop')),
    state TEXT NOT NULL DEFAULT 'stopped',
    reason TEXT,
    pid INTEGER,
    started_at INTEGER,
    heartbeat_at INTEGER,
    last_error TEXT
) STRICT;

INSERT OR IGNORE INTO worker_state (id) VALUES (1);
"#;

#[derive(Clone, Debug)]
pub struct SpaceRow {
    pub space: SearchSpace,
    pub family: Family,
    pub total: BigUint,
    pub next_unallocated: BigUint,
}

impl SpaceRow {
    fn has_unallocated(&self) -> bool {
        self.next_unallocated < self.total
    }

    /// Scheduling order: historical before frontier, then cheapest space first so
    /// both tracks advance together.
    fn order_key(&self) -> (Family, &BigUint, u32, u8) {
        (self.family, &self.total, self.space.hex_digits(), self.space.leading())
    }
}

/// A chunk this worker now holds, resuming from `next_index`.
#[derive(Clone, Debug)]
pub struct Claim {
    pub chunk: WorkChunk,
    pub next_index: BigUint,
}

pub struct Checkpoint<'a> {
    pub chunk_id: &'a str,
    /// The committed cursor this progress continues from.
    pub from: &'a BigUint,
    /// The new cursor: everything in `[from, to)` has been searched.
    pub to: &'a BigUint,
    pub matches: &'a [Match],
    pub candidates: u64,
    pub active: Duration,
    pub hardware: &'a str,
    pub backend: &'a str,
}

#[derive(Clone, Debug)]
pub struct SpaceCoverage {
    pub space: SearchSpace,
    pub family: Family,
    pub total: BigUint,
    pub covered: BigUint,
    pub intervals: Vec<(BigUint, BigUint)>,
}

#[derive(Clone, Debug)]
pub struct DiscoveryRow {
    pub decimal: String,
    pub hexadecimal: String,
    pub binary: String,
    pub nontrivial: bool,
    pub verification: String,
    pub chunk_id: String,
    pub candidate_index: String,
    pub discovered_at: i64,
}

#[derive(Clone, Debug)]
pub struct RunningChunk {
    pub id: String,
    pub next_index: BigUint,
    pub end_index: BigUint,
    pub backend: Option<String>,
    pub last_checkpoint_at: Option<i64>,
}

#[derive(Clone, Debug)]
pub struct WorkerRow {
    pub requested: String,
    pub state: String,
    pub reason: Option<String>,
    pub pid: Option<i64>,
    pub started_at: Option<i64>,
    pub heartbeat_at: Option<i64>,
    pub last_error: Option<String>,
}

#[derive(Clone, Debug, Default)]
pub struct Totals {
    pub candidates: u64,
    pub active_ms: i64,
    pub last_checkpoint_at: Option<i64>,
}

pub struct Store {
    conn: Connection,
}

impl Store {
    pub fn open(path: &Path) -> Result<Self> {
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).with_context(|| format!("cannot create {}", dir.display()))?;
        }
        let conn = Connection::open(path).with_context(|| format!("cannot open database {}", path.display()))?;
        conn.busy_timeout(Duration::from_secs(30))?;
        let mode: String = conn.pragma_update_and_check(None, "journal_mode", "WAL", |r| r.get(0))?;
        ensure!(
            mode.eq_ignore_ascii_case("wal"),
            "database refused WAL journaling (got {mode})"
        );
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.pragma_update(None, "foreign_keys", "ON")?;
        conn.execute_batch(SCHEMA)?;

        conn.execute(
            "INSERT OR IGNORE INTO meta (key, value) VALUES ('schema_version', ?1)",
            [STORE_SCHEMA_VERSION.to_string()],
        )?;
        let version: String =
            conn.query_row("SELECT value FROM meta WHERE key = 'schema_version'", [], |r| r.get(0))?;
        ensure!(
            version == STORE_SCHEMA_VERSION.to_string(),
            "database schema version {version} is not supported (this build uses {STORE_SCHEMA_VERSION})"
        );
        Ok(Self { conn })
    }

    /// Registers the historical search spaces the configuration selects.
    pub fn plan(&mut self, search: &Search) -> Result<usize> {
        let tx = self.conn.transaction()?;
        let mut added = 0;
        if search.mode.includes_historical() {
            for (leads, historical_max) in TRACKS {
                for n in search.minimum_hex_digits..=historical_max {
                    if search.allows_length(n) {
                        for &lead in leads {
                            added += insert_space(&tx, SearchSpace::new(n, lead)?)?;
                        }
                    }
                }
            }
        }
        ensure_frontier(&tx, search)?;
        tx.commit()?;
        Ok(added)
    }

    /// Returns chunks left `running` by a crash or forced exit to the pending pool.
    /// Their committed cursors are kept, so only uncommitted work is repeated.
    pub fn recover_interrupted(&self) -> Result<usize> {
        Ok(self
            .conn
            .execute("UPDATE work_chunks SET state = 'pending' WHERE state = 'running'", [])?)
    }

    /// Atomically takes the next chunk to work on: a previously interrupted chunk if
    /// any, otherwise a fresh allocation from the highest-priority space.
    pub fn claim(&mut self, search: &Search, chunk_candidates: u64, backend: &str) -> Result<Option<Claim>> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        ensure_frontier(&tx, search)?;
        let spaces = load_spaces(&tx)?;
        let eligible = |row: &&SpaceRow| row.family.included_by(search) && search.allows_length(row.space.hex_digits());

        let mut pending: Vec<(SpaceRow, Claim)> = Vec::new();
        {
            let mut stmt = tx.prepare(
                "SELECT hex_digits, leading_digit, start_index, end_index, next_index FROM work_chunks
                 WHERE state = 'pending' AND generator_version = ?1",
            )?;
            let rows = stmt.query_map([GENERATOR_VERSION], |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    r.get::<_, u8>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, String>(3)?,
                    r.get::<_, String>(4)?,
                ))
            })?;
            for row in rows {
                let (n, lead, start, end, next) = row?;
                let Some(space_row) = spaces
                    .iter()
                    .filter(eligible)
                    .find(|s| s.space.hex_digits() == n && s.space.leading() == lead)
                else {
                    continue;
                };
                let chunk = WorkChunk::new(space_row.space, parse_big(&start)?, parse_big(&end)?)?;
                pending.push((
                    space_row.clone(),
                    Claim {
                        chunk,
                        next_index: parse_big(&next)?,
                    },
                ));
            }
        }

        let claim = if let Some((_, claim)) = pending.into_iter().min_by(|(a, x), (b, y)| {
            a.order_key()
                .cmp(&b.order_key())
                .then(x.chunk.start_index.cmp(&y.chunk.start_index))
        }) {
            claim
        } else if let Some(row) = spaces
            .iter()
            .filter(eligible)
            .filter(|s| s.has_unallocated())
            .min_by(|a, b| a.order_key().cmp(&b.order_key()))
        {
            let start = row.next_unallocated.clone();
            let end = (&start + chunk_candidates).min(row.total.clone());
            let chunk = WorkChunk::new(row.space, start.clone(), end.clone())?;
            tx.execute(
                "INSERT INTO work_chunks
                 (id, generator_version, hex_digits, leading_digit, start_index, end_index, next_index, state, created_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?5, 'pending', ?7)",
                params![chunk.id(), GENERATOR_VERSION, row.space.hex_digits(), row.space.leading(), start.to_string(), end.to_string(), now_ms()],
            )?;
            tx.execute(
                "UPDATE search_spaces SET next_unallocated = ?1
                 WHERE generator_version = ?2 AND hex_digits = ?3 AND leading_digit = ?4",
                params![
                    end.to_string(),
                    GENERATOR_VERSION,
                    row.space.hex_digits(),
                    row.space.leading()
                ],
            )?;
            Claim {
                chunk,
                next_index: start,
            }
        } else {
            return Ok(None);
        };

        tx.execute(
            "UPDATE work_chunks SET state = 'running', attempts = attempts + 1, backend = ?2, last_checkpoint_at = ?3 WHERE id = ?1",
            params![claim.chunk.id(), backend, now_ms()],
        )?;
        tx.commit()?;
        Ok(Some(claim))
    }

    /// Records finished work and its discoveries in a single transaction.
    /// Returns whether the chunk is now complete.
    pub fn checkpoint(&mut self, cp: &Checkpoint) -> Result<bool> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let (state, n, lead, start, end, next): (String, u32, u8, String, String, String) = tx
            .query_row(
                "SELECT state, hex_digits, leading_digit, start_index, end_index, next_index FROM work_chunks WHERE id = ?1",
                [cp.chunk_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?, r.get(5)?)),
            )
            .optional()?
            .ok_or_else(|| anyhow!("checkpoint for unknown chunk {}", cp.chunk_id))?;
        let (start, end, next) = (parse_big(&start)?, parse_big(&end)?, parse_big(&next)?);
        ensure!(state == "running", "chunk {} is {state}, not running", cp.chunk_id);
        ensure!(
            next == *cp.from,
            "chunk {} cursor is {next}, but progress claims to continue from {}",
            cp.chunk_id,
            cp.from
        );
        ensure!(
            cp.from < cp.to && *cp.to <= end,
            "progress [{}, {}) is outside chunk {}",
            cp.from,
            cp.to,
            cp.chunk_id
        );
        ensure!(
            BigUint::from(cp.candidates) == cp.to - cp.from,
            "chunk {} progress covers {} candidates but {} were reported",
            cp.chunk_id,
            cp.to - cp.from,
            cp.candidates
        );

        let space = SearchSpace::new(n, lead)?;
        for m in cp.matches {
            ensure!(
                m.index >= *cp.from && m.index < *cp.to && m.index >= start,
                "match index {} outside progress",
                m.index
            );
            // Rebuild the candidate and recheck all three bases independently of the backend.
            let value = space.candidate(&m.index)?;
            let check = Verification::of(&value);
            ensure!(
                value == m.verification.value && check.is_triple_palindrome(),
                "backend reported index {} of {space:?} as a match but it does not verify",
                m.index
            );
            tx.execute(
                "INSERT OR IGNORE INTO discoveries
                 (decimal, hexadecimal, binary, nontrivial, verification, chunk_id, candidate_index, discovered_at)
                 VALUES (?1, ?2, ?3, ?4, 'cpu_verified', ?5, ?6, ?7)",
                params![
                    check.decimal,
                    check.hexadecimal,
                    check.binary,
                    check.is_nontrivial(),
                    cp.chunk_id,
                    m.index.to_string(),
                    now_ms()
                ],
            )?;
        }

        let complete = *cp.to == end;
        let now = now_ms();
        let active_ms = cp.active.as_millis() as i64;
        tx.execute(
            "UPDATE work_chunks SET next_index = ?2, active_ms = active_ms + ?3, last_checkpoint_at = ?4,
                 state = CASE WHEN ?5 THEN 'complete' ELSE state END,
                 completed_at = CASE WHEN ?5 THEN ?4 ELSE NULL END
             WHERE id = ?1",
            params![cp.chunk_id, cp.to.to_string(), active_ms, now, complete],
        )?;
        tx.execute(
            "INSERT INTO performance_samples
             (recorded_at, hardware, backend, integer_bits, hex_digits, leading_digit, candidates, active_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                now,
                cp.hardware,
                cp.backend,
                4 * n,
                n,
                lead,
                cp.candidates as i64,
                active_ms
            ],
        )?;
        tx.commit()?;
        Ok(complete)
    }

    /// Hands an unfinished chunk back to the pending pool.
    pub fn release(&self, chunk_id: &str) -> Result<()> {
        self.conn.execute(
            "UPDATE work_chunks SET state = 'pending' WHERE id = ?1 AND state = 'running'",
            [chunk_id],
        )?;
        Ok(())
    }

    pub fn requested(&self) -> Result<Requested> {
        let s: String = self
            .conn
            .query_row("SELECT requested FROM worker_state WHERE id = 1", [], |r| r.get(0))?;
        Ok(match s.as_str() {
            "pause" => Requested::Pause,
            "stop" => Requested::Stop,
            _ => Requested::Run,
        })
    }

    pub fn set_requested(&self, requested: Requested) -> Result<()> {
        self.conn.execute(
            "UPDATE worker_state SET requested = ?1 WHERE id = 1",
            [requested.as_str()],
        )?;
        Ok(())
    }

    pub fn worker_started(&self) -> Result<()> {
        self.conn.execute(
            "UPDATE worker_state SET state = 'starting', reason = NULL, pid = ?1, started_at = ?2, heartbeat_at = ?2,
                 requested = CASE WHEN requested = 'stop' THEN 'run' ELSE requested END
             WHERE id = 1",
            params![std::process::id(), now_ms()],
        )?;
        Ok(())
    }

    pub fn heartbeat(&self, state: &str, reason: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE worker_state SET state = ?1, reason = ?2, heartbeat_at = ?3 WHERE id = 1",
            params![state, reason, now_ms()],
        )?;
        Ok(())
    }

    pub fn worker_stopped(&self, reason: &str, error: Option<&str>) -> Result<()> {
        self.conn.execute(
            "UPDATE worker_state SET state = 'stopped', reason = ?1, heartbeat_at = ?2, pid = NULL,
                 last_error = COALESCE(?3, last_error),
                 requested = CASE WHEN requested = 'stop' THEN 'run' ELSE requested END
             WHERE id = 1",
            params![reason, now_ms(), error],
        )?;
        Ok(())
    }

    pub fn worker(&self) -> Result<WorkerRow> {
        Ok(self.conn.query_row(
            "SELECT requested, state, reason, pid, started_at, heartbeat_at, last_error FROM worker_state WHERE id = 1",
            [],
            |r| {
                Ok(WorkerRow {
                    requested: r.get(0)?,
                    state: r.get(1)?,
                    reason: r.get(2)?,
                    pid: r.get(3)?,
                    started_at: r.get(4)?,
                    heartbeat_at: r.get(5)?,
                    last_error: r.get(6)?,
                })
            },
        )?)
    }

    pub fn spaces(&self) -> Result<Vec<SpaceRow>> {
        load_spaces(&self.conn)
    }

    pub fn coverage(&self) -> Result<Vec<SpaceCoverage>> {
        let mut coverage: Vec<SpaceCoverage> = self
            .spaces()?
            .into_iter()
            .map(|row| SpaceCoverage {
                space: row.space,
                family: row.family,
                total: row.total,
                covered: BigUint::ZERO,
                intervals: Vec::new(),
            })
            .collect();
        for chunk in self.chunks()? {
            if chunk.next > chunk.start
                && let Some(c) = coverage.iter_mut().find(|c| c.space == chunk.space)
            {
                c.intervals.push((chunk.start, chunk.next));
            }
        }
        for c in &mut coverage {
            c.intervals = merge_intervals(std::mem::take(&mut c.intervals));
            c.covered = c.intervals.iter().map(|(s, e)| e - s).sum();
        }
        coverage.sort_by(|a, b| (a.family, &a.total, a.space).cmp(&(b.family, &b.total, b.space)));
        Ok(coverage)
    }

    pub fn running_chunks(&self) -> Result<Vec<RunningChunk>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, next_index, end_index, backend, last_checkpoint_at FROM work_chunks WHERE state = 'running' ORDER BY id",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, String>(1)?,
                r.get::<_, String>(2)?,
                r.get(3)?,
                r.get(4)?,
            ))
        })?;
        rows.map(|row| {
            let (id, next, end, backend, last_checkpoint_at) = row?;
            Ok(RunningChunk {
                id,
                next_index: parse_big(&next)?,
                end_index: parse_big(&end)?,
                backend,
                last_checkpoint_at,
            })
        })
        .collect()
    }

    pub fn discoveries(&self) -> Result<Vec<DiscoveryRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT decimal, hexadecimal, binary, nontrivial, verification, chunk_id, candidate_index, discovered_at
             FROM discoveries ORDER BY discovered_at, decimal",
        )?;
        let rows = stmt.query_map([], |r| {
            Ok(DiscoveryRow {
                decimal: r.get(0)?,
                hexadecimal: r.get(1)?,
                binary: r.get(2)?,
                nontrivial: r.get(3)?,
                verification: r.get(4)?,
                chunk_id: r.get(5)?,
                candidate_index: r.get(6)?,
                discovered_at: r.get(7)?,
            })
        })?;
        Ok(rows.collect::<rusqlite::Result<_>>()?)
    }

    pub fn totals(&self) -> Result<Totals> {
        Ok(self.conn.query_row(
            "SELECT COALESCE(SUM(candidates), 0), COALESCE(SUM(active_ms), 0), MAX(recorded_at) FROM performance_samples",
            [],
            |r| {
                Ok(Totals {
                    candidates: r.get::<_, i64>(0)? as u64,
                    active_ms: r.get(1)?,
                    last_checkpoint_at: r.get(2)?,
                })
            },
        )?)
    }

    /// Candidates committed per second over the trailing `window`.
    /// The window is shortened to begin no earlier than `not_before` (in ms), such
    /// as the worker's start time, so a freshly started worker is not under-reported.
    pub fn recent_rate(&self, window: Duration, not_before: Option<i64>) -> Result<f64> {
        let now = now_ms();
        let since = (now - window.as_millis() as i64).max(not_before.unwrap_or(i64::MIN));
        let candidates: i64 = self.conn.query_row(
            "SELECT COALESCE(SUM(candidates), 0) FROM performance_samples WHERE recorded_at >= ?1",
            [since],
            |r| r.get(0),
        )?;
        Ok(candidates as f64 / ((now - since) as f64 / 1000.0).max(1.0))
    }

    /// Checks the chunk ledger and discoveries for anything inconsistent.
    pub fn verify_integrity(&self) -> Result<Vec<String>> {
        let mut problems = Vec::new();
        let chunks = self.chunks()?;
        for row in self.spaces()? {
            let mut mine: Vec<&ChunkRow> = chunks.iter().filter(|c| c.space == row.space).collect();
            mine.sort_by(|a, b| a.start.cmp(&b.start));
            let mut cursor = BigUint::ZERO;
            for c in mine {
                if c.start != cursor {
                    problems.push(format!(
                        "{}: starts at {} but previous allocation ended at {cursor}",
                        c.id, c.start
                    ));
                }
                if !(c.start <= c.next && c.next <= c.end && c.start < c.end) {
                    problems.push(format!("{}: cursor {} outside [{}, {})", c.id, c.next, c.start, c.end));
                }
                if (c.state == "complete") != (c.next == c.end) {
                    problems.push(format!(
                        "{}: state {} disagrees with cursor {} of {}",
                        c.id, c.state, c.next, c.end
                    ));
                }
                match WorkChunk::new(row.space, c.start.clone(), c.end.clone()) {
                    Ok(chunk) if chunk.id() == c.id => {}
                    Ok(chunk) => problems.push(format!("{}: id should be {}", c.id, chunk.id())),
                    Err(e) => problems.push(format!("{}: {e}", c.id)),
                }
                cursor = c.end.clone();
            }
            if cursor != row.next_unallocated {
                problems.push(format!(
                    "{:?}: chunks end at {cursor} but allocation cursor is {}",
                    row.space, row.next_unallocated
                ));
            }
            if row.next_unallocated > row.total {
                problems.push(format!(
                    "{:?}: allocation cursor beyond {} candidates",
                    row.space, row.total
                ));
            }
        }

        for d in self.discoveries()? {
            let Some(c) = chunks.iter().find(|c| c.id == d.chunk_id) else {
                problems.push(format!("discovery {}: unknown chunk {}", d.decimal, d.chunk_id));
                continue;
            };
            let index = parse_big(&d.candidate_index)?;
            if index < c.start || index >= c.next {
                problems.push(format!(
                    "discovery {}: index {index} not in searched part of {}",
                    d.decimal, c.id
                ));
            }
            match c.space.candidate(&index) {
                Ok(value) => {
                    let check = Verification::of(&value);
                    if check.decimal != d.decimal || !check.is_triple_palindrome() {
                        problems.push(format!(
                            "discovery {}: does not reverify (index {index} gives {})",
                            d.decimal, check.decimal
                        ));
                    }
                }
                Err(e) => problems.push(format!("discovery {}: {e}", d.decimal)),
            }
        }
        Ok(problems)
    }

    fn chunks(&self) -> Result<Vec<ChunkRow>> {
        let mut stmt = self.conn.prepare(
            "SELECT id, hex_digits, leading_digit, start_index, end_index, next_index, state FROM work_chunks
             WHERE generator_version = ?1",
        )?;
        let rows = stmt.query_map([GENERATOR_VERSION], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, u32>(1)?,
                r.get::<_, u8>(2)?,
                r.get::<_, String>(3)?,
                r.get::<_, String>(4)?,
                r.get::<_, String>(5)?,
                r.get::<_, String>(6)?,
            ))
        })?;
        rows.map(|row| {
            let (id, n, lead, start, end, next, state) = row?;
            Ok(ChunkRow {
                id,
                space: SearchSpace::new(n, lead)?,
                start: parse_big(&start)?,
                end: parse_big(&end)?,
                next: parse_big(&next)?,
                state,
            })
        })
        .collect()
    }
}

struct ChunkRow {
    id: String,
    space: SearchSpace,
    start: BigUint,
    end: BigUint,
    next: BigUint,
    state: String,
}

fn insert_space(tx: &Transaction, space: SearchSpace) -> Result<usize> {
    Ok(tx.execute(
        "INSERT OR IGNORE INTO search_spaces
         (generator_version, hex_digits, leading_digit, family, total_candidates, next_unallocated, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, '0', ?6)",
        params![
            GENERATOR_VERSION,
            space.hex_digits(),
            space.leading(),
            Family::of(space).as_str(),
            space.candidate_count().to_string(),
            now_ms()
        ],
    )?)
}

fn load_spaces(conn: &Connection) -> Result<Vec<SpaceRow>> {
    let mut stmt = conn.prepare(
        "SELECT hex_digits, leading_digit, total_candidates, next_unallocated FROM search_spaces WHERE generator_version = ?1",
    )?;
    let rows = stmt.query_map([GENERATOR_VERSION], |r| {
        Ok((
            r.get::<_, u32>(0)?,
            r.get::<_, u8>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    rows.map(|row| {
        let (n, lead, total, next) = row?;
        let space = SearchSpace::new(n, lead)?;
        let total = parse_big(&total)?;
        if total != space.candidate_count() {
            bail!(
                "{space:?} recorded with {total} candidates, generator says {}",
                space.candidate_count()
            );
        }
        Ok(SpaceRow {
            space,
            family: Family::of(space),
            total,
            next_unallocated: parse_big(&next)?,
        })
    })
    .collect()
}

/// Makes sure each frontier track has a space with unallocated work, opening the
/// next hexadecimal length when the current ones are fully allocated.
fn ensure_frontier(tx: &Transaction, search: &Search) -> Result<()> {
    if !search.mode.includes_frontier() {
        return Ok(());
    }
    let spaces = load_spaces(tx)?;
    for (leads, historical_max) in TRACKS {
        let track: Vec<&SpaceRow> = spaces
            .iter()
            .filter(|s| s.family == Family::Frontier && leads.contains(&s.space.leading()))
            .collect();
        if track
            .iter()
            .any(|s| s.has_unallocated() && search.allows_length(s.space.hex_digits()))
        {
            continue;
        }
        let next = track
            .iter()
            .map(|s| s.space.hex_digits() + 1)
            .max()
            .unwrap_or(historical_max + 1)
            .max(search.minimum_hex_digits);
        if search.allows_length(next) {
            for &lead in leads {
                insert_space(tx, SearchSpace::new(next, lead)?)?;
            }
        }
    }
    Ok(())
}

/// Sorts and merges overlapping or adjacent half-open intervals.
pub fn merge_intervals(mut intervals: Vec<(BigUint, BigUint)>) -> Vec<(BigUint, BigUint)> {
    intervals.sort();
    let mut merged: Vec<(BigUint, BigUint)> = Vec::with_capacity(intervals.len());
    for (start, end) in intervals {
        match merged.last_mut() {
            Some(last) if start <= last.1 => last.1 = last.1.clone().max(end),
            _ => merged.push((start, end)),
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use super::*;

    fn b(v: u64) -> BigUint {
        v.into()
    }

    #[test]
    fn merging_handles_adjacent_overlapping_and_sparse() {
        let merged = merge_intervals(vec![
            (b(10), b(20)),
            (b(0), b(5)),
            (b(5), b(7)),
            (b(15), b(25)),
            (b(30), b(31)),
        ]);
        assert_eq!(merged, vec![(b(0), b(7)), (b(10), b(25)), (b(30), b(31))]);
        assert!(merge_intervals(Vec::new()).is_empty());
    }

    #[test]
    fn family_boundaries_match_regan() {
        let f = |n, l| Family::of(SearchSpace::new(n, l).unwrap());
        assert_eq!(f(64, 0x1), Family::Historical);
        assert_eq!(f(65, 0x3), Family::Frontier);
        assert_eq!(f(32, 0xF), Family::Historical);
        assert_eq!(f(33, 0x5), Family::Frontier);
    }
}
