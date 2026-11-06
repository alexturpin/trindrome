//! Distributed mode: the coordinator client and the sync loop.
//!
//! Search threads lease chunks through [`Client::acquire`] and search them
//! exactly as local chunks, except that every checkpoint also stores the
//! segment's survivor set. The sync thread ([`sync_loop`]) uploads those
//! segments, renews leases while there is nothing new to report, and hands back
//! leases the worker has paused on. The local database stays the journal, so a
//! crash or a lost connection loses nothing: unreported segments wait until the
//! coordinator can be reached. See docs/distributed.md.

use std::path::Path;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use num_bigint::BigUint;
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use tracing::{debug, info, warn};
use trindrome_core::proof::encode_survivors;
use trindrome_core::{FILTER_VERSION, GENERATOR_VERSION, WorkChunk};

use crate::identity::Identity;
use crate::persistence::{LeaseGrant, LeaseRow, Store, now_ms};

const AGENT_VERSION: &str = env!("CARGO_PKG_VERSION");
const TIMEOUT: Duration = Duration::from_secs(60);

/// Largest hex length any of our engines accepts (the 512-bit GPU kernels).
pub const MAX_HEX_DIGITS: u32 = 128;

/// A reply with a status code the protocol defines.
struct Reply {
    status: u16,
    body: Value,
}

impl Reply {
    fn error_code(&self) -> &str {
        self.body["error"].as_str().unwrap_or("")
    }

    fn describe(&self) -> String {
        let message = self.body["message"]
            .as_str()
            .or(self.body["reason"].as_str())
            .unwrap_or("");
        format!("HTTP {} {} {message}", self.status, self.error_code())
    }
}

/// What the coordinator said about a submitted segment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SegmentOutcome {
    Accepted { expires_at: Option<i64> },
    Superseded,
    Rejected(String),
}

pub enum Acquired {
    Lease(LeaseGrant),
    NoWork { retry_after: Duration },
}

pub struct Client {
    url: String,
    agent: ureq::Agent,
    identity: Identity,
    worker_id: Mutex<Option<String>>,
    hardware: String,
    /// This computer's name, recorded privately by the coordinator so the
    /// owner can find a discoverer whose display name doesn't say who they are.
    hostname: Option<String>,
}

impl Client {
    /// `identity_dir` holds the key and registration (next to the database).
    pub fn new(url: &str, identity_dir: &Path, hardware: &str) -> Result<Self> {
        let identity = Identity::load_or_create(identity_dir)?;
        let url = url.trim_end_matches('/').to_string();
        let worker_id = identity.worker_id(&url);
        Ok(Self {
            agent: ureq::AgentBuilder::new().timeout(TIMEOUT).build(),
            worker_id: Mutex::new(worker_id),
            hardware: hardware.to_string(),
            hostname: hostname(),
            identity,
            url,
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }

    pub fn worker_id(&self) -> Option<String> {
        self.worker_id.lock().unwrap().clone()
    }

    /// Sends one signed request; transport failures are errors, HTTP statuses are replies.
    fn send(&self, path: &str, body: &Value, worker_id: Option<&str>) -> Result<Reply> {
        let bytes = serde_json::to_vec(body)?;
        let timestamp = now_ms().to_string();
        let digest: String = Sha256::digest(&bytes).iter().map(|b| format!("{b:02x}")).collect();
        let signature = self
            .identity
            .sign_base64(format!("{timestamp}\nPOST\n{path}\n{digest}").as_bytes());
        let mut request = self
            .agent
            .post(&format!("{}{path}", self.url))
            .set("Content-Type", "application/json")
            .set("Trindrome-Timestamp", &timestamp)
            .set("Trindrome-Signature", &signature);
        if let Some(id) = worker_id {
            request = request.set("Trindrome-Worker", id);
        }
        let response = match request.send_bytes(&bytes) {
            Ok(response) => response,
            Err(ureq::Error::Status(_, response)) => response,
            Err(e) => return Err(anyhow!("cannot reach the coordinator at {}: {e}", self.url)),
        };
        let status = response.status();
        let text = response.into_string().context("reading the coordinator's reply")?;
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(Reply { status, body })
    }

    fn register(&self) -> Result<String> {
        let body = json!({
            "public_key": self.identity.public_key_base64(),
            "agent_version": AGENT_VERSION,
            "gpu": self.hardware,
            "hostname": self.hostname,
        });
        let reply = self.send("/v1/register", &body, None)?;
        if reply.status != 200 {
            bail!("registration with {} failed: {}", self.url, reply.describe());
        }
        let id = reply.body["worker_id"]
            .as_str()
            .ok_or_else(|| anyhow!("registration reply has no worker_id"))?
            .to_string();
        self.identity.save_worker_id(&self.url, &id)?;
        info!(worker_id = %id, coordinator = %self.url, "registered with the coordinator");
        Ok(id)
    }

    /// Sends an authenticated request, registering first (or again, if the
    /// coordinator no longer knows this worker).
    fn call(&self, path: &str, body: &Value) -> Result<Reply> {
        let mut worker_id = self.worker_id.lock().unwrap();
        if worker_id.is_none() {
            *worker_id = Some(self.register()?);
        }
        let reply = self.send(path, body, worker_id.as_deref())?;
        if reply.status == 401 && reply.error_code() == "unknown_worker" {
            *worker_id = Some(self.register()?);
            return self.send(path, body, worker_id.as_deref());
        }
        if reply.status == 429 {
            bail!("the coordinator is rate limiting this worker: {}", reply.describe());
        }
        Ok(reply)
    }

    /// Asks for work, given the worker's measured rate in candidates per
    /// second. `name` is the display name currently configured; sending it
    /// every time lets a changed name show up without a restart.
    pub fn acquire(&self, rate: f64, name: Option<&str>) -> Result<Acquired> {
        let body = json!({
            "generator_version": GENERATOR_VERSION,
            "filter_version": FILTER_VERSION,
            "max_hex_digits": MAX_HEX_DIGITS,
            "rate": rate,
            "name": name,
            "hostname": self.hostname,
        });
        let reply = self.call("/v1/leases", &body)?;
        if reply.status != 200 {
            bail!("the coordinator refused to lease work: {}", reply.describe());
        }
        let lease = &reply.body["lease"];
        if lease.is_null() {
            let seconds = reply.body["retry_after_seconds"].as_u64().unwrap_or(300);
            return Ok(Acquired::NoWork {
                retry_after: Duration::from_secs(seconds),
            });
        }
        let chunk: WorkChunk = serde_json::from_value(lease["chunk"].clone()).context("invalid chunk in lease")?;
        chunk.validate()?;
        let field = |name: &str| lease[name].clone();
        Ok(Acquired::Lease(LeaseGrant {
            lease_id: field("lease_id")
                .as_str()
                .ok_or_else(|| anyhow!("lease has no id"))?
                .to_string(),
            epoch: field("epoch").as_i64().ok_or_else(|| anyhow!("lease has no epoch"))?,
            cursor: field("cursor")
                .as_str()
                .and_then(|c| BigUint::parse_bytes(c.as_bytes(), 10))
                .ok_or_else(|| anyhow!("lease has no cursor"))?,
            expires_at: field("expires_at").as_i64().unwrap_or(0),
            report_interval_seconds: field("report_interval_seconds").as_u64().unwrap_or(300),
            chunk,
        }))
    }

    pub fn submit_segment(
        &self,
        lease: &LeaseRow,
        start: &BigUint,
        end: &BigUint,
        survivors: &[u64],
        backend: &str,
    ) -> Result<SegmentOutcome> {
        let (start64, end64) = (u64::try_from(start)?, u64::try_from(end)?);
        let encoded = encode_survivors(start64, end64, survivors)?;
        let body = json!({
            "epoch": lease.epoch,
            "chunk": lease.chunk,
            "start": start.to_string(),
            "end": end.to_string(),
            "survivors": BASE64.encode(encoded),
            "backend": backend,
        });
        let reply = self.call(&format!("/v1/leases/{}/segments", lease.lease_id), &body)?;
        Ok(match (reply.status, reply.body["status"].as_str()) {
            (200, Some("accepted" | "duplicate")) => SegmentOutcome::Accepted {
                expires_at: reply.body["expires_at"].as_i64(),
            },
            (409, Some("superseded")) => SegmentOutcome::Superseded,
            (422, _) => SegmentOutcome::Rejected(reply.describe()),
            _ => bail!("unexpected reply to a segment: {}", reply.describe()),
        })
    }

    /// Extends a lease; `None` if it was reassigned.
    pub fn renew(&self, lease_id: &str) -> Result<Option<i64>> {
        let reply = self.call(&format!("/v1/leases/{lease_id}/renew"), &json!({}))?;
        match reply.status {
            200 => Ok(reply.body["expires_at"].as_i64()),
            409 => Ok(None),
            _ => bail!("unexpected reply to a renewal: {}", reply.describe()),
        }
    }

    pub fn release(&self, lease_id: &str) -> Result<()> {
        let reply = self.call(&format!("/v1/leases/{lease_id}/release"), &json!({}))?;
        match reply.status {
            // 409: no longer ours anyway.
            200 | 409 => Ok(()),
            _ => bail!("unexpected reply to a release: {}", reply.describe()),
        }
    }

    pub fn public_key(&self) -> String {
        self.identity.public_key_base64()
    }

    /// Records local search as done on the coordinator (owner keys only).
    /// Returns the number of spaces it newly recorded and their candidates.
    pub fn import_coverage(&self, spaces: &[ImportedSpace], name: Option<&str>) -> Result<(u64, String)> {
        let spaces: Vec<Value> = spaces
            .iter()
            .map(|s| {
                json!({
                    "hex_digits": s.hex_digits,
                    "leading_hex_digit": s.leading_hex_digit,
                    "searched": s.searched.to_string(),
                })
            })
            .collect();
        let body = json!({ "generator_version": GENERATOR_VERSION, "spaces": spaces, "name": name });
        let reply = self.call("/v1/import", &body)?;
        if reply.status != 200 {
            bail!("the coordinator refused the import: {}", reply.describe());
        }
        Ok((
            reply.body["spaces"].as_u64().unwrap_or(0),
            reply.body["candidates"].as_str().unwrap_or("0").to_string(),
        ))
    }
    /// Every registered worker, with private details (owner keys only).
    pub fn workers(&self) -> Result<Vec<WorkerInfo>> {
        let reply = self.call("/v1/workers", &json!({}))?;
        if reply.status != 200 {
            bail!("the coordinator refused to list workers: {}", reply.describe());
        }
        serde_json::from_value(reply.body["workers"].clone()).context("invalid worker list")
    }

    /// Sets a worker's trust (owner keys only); returns how many queued
    /// replicas of its results were dropped.
    pub fn set_trust(&self, worker_id: &str, trust: &str) -> Result<u64> {
        let reply = self.call("/v1/trust", &json!({ "worker_id": worker_id, "trust": trust }))?;
        if reply.status != 200 {
            bail!("the coordinator refused to set trust: {}", reply.describe());
        }
        Ok(reply.body["cancelled_replicas"].as_u64().unwrap_or(0))
    }
}

/// What the coordinator knows about a worker, for its owner.
#[derive(Clone, Debug, Deserialize)]
pub struct WorkerInfo {
    pub worker_id: String,
    pub name: Option<String>,
    pub hostname: Option<String>,
    pub gpu: Option<String>,
    pub network: String,
    #[serde(default)]
    pub trust: String,
    #[serde(default)]
    pub matched: u64,
    pub candidates: f64,
    pub discoveries: u64,
    pub registered_at: i64,
    pub last_seen: i64,
}

fn hostname() -> Option<String> {
    std::env::var("COMPUTERNAME")
        .or_else(|_| std::env::var("HOSTNAME"))
        .ok()
        .map(|h| h.trim().to_string())
        .filter(|h| !h.is_empty())
}

/// A space whose indices `[0, searched)` were searched locally.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImportedSpace {
    pub hex_digits: u32,
    pub leading_hex_digit: u32,
    pub searched: BigUint,
}

/// The coverage of a local database that can be imported: per started space,
/// the searched prefix from index 0. Coverage past a gap is left out, since the
/// coordinator would hand it out again.
pub fn importable_coverage(store: &Store) -> Result<Vec<ImportedSpace>> {
    Ok(store
        .coverage()?
        .into_iter()
        .filter_map(|c| match c.intervals.first() {
            Some((start, end)) if *start == BigUint::ZERO => Some(ImportedSpace {
                hex_digits: c.space.hex_digits(),
                leading_hex_digit: c.space.leading().into(),
                searched: end.clone(),
            }),
            _ => None,
        })
        .collect())
}

/// The coordinator rejected our proof of search. That means this machine's
/// results disagree with the reference filter, so the worker must stop.
#[derive(Debug)]
pub struct Rejected(pub String);

impl std::fmt::Display for Rejected {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "the coordinator rejected this worker's results: {}", self.0)
    }
}

impl std::error::Error for Rejected {}

const SYNC_TICK: Duration = Duration::from_millis(250);
const RETRY_AFTER_ERROR: Duration = Duration::from_secs(30);
/// How long a stopping worker keeps trying to hand everything over.
const FINAL_FLUSH: Duration = Duration::from_secs(15);

/// Reports leased work until `stopping` returns true, then makes a final attempt
/// to upload what is left and hand back unfinished leases. Returns an error
/// only for [`Rejected`] results.
pub fn sync_loop(database: &Path, client: &Client, release_after: Duration, stopping: &dyn Fn() -> bool) -> Result<()> {
    let store = Store::open(database)?;
    let mut retry_at: Option<Instant> = None;
    loop {
        let stop = stopping();
        if retry_at.is_none_or(|at| Instant::now() >= at) {
            match sync_once(&store, client, release_after, stop) {
                Ok(_) => retry_at = None,
                Err(e) if e.is::<Rejected>() => return Err(e),
                Err(e) => {
                    warn!("coordinator sync failed, retrying in {RETRY_AFTER_ERROR:?}: {e:#}");
                    store.record_error(&format!("coordinator sync: {e:#}"))?;
                    retry_at = Some(Instant::now() + RETRY_AFTER_ERROR);
                }
            }
        }
        if stop {
            break;
        }
        std::thread::sleep(SYNC_TICK);
    }
    let deadline = Instant::now() + FINAL_FLUSH;
    while Instant::now() < deadline {
        match sync_once(&store, client, release_after, true) {
            Ok(true) => return Ok(()),
            Ok(false) => std::thread::sleep(SYNC_TICK),
            Err(e) if e.is::<Rejected>() => return Err(e),
            Err(e) => {
                warn!("could not hand leased work back before stopping; it will be reported next time: {e:#}");
                return Ok(());
            }
        }
    }
    Ok(())
}

/// One pass over the active leases. Returns true once nothing is left to report
/// or release (only meaningful when `stopping`).
fn sync_once(store: &Store, client: &Client, release_after: Duration, stopping: bool) -> Result<bool> {
    let now = now_ms();
    let mut settled = true;
    for lease in store.leases("active")? {
        let chunk_id = lease.chunk.id();
        let paused = lease.chunk_state == "pending";
        let interval_ms = lease.report_interval_seconds as i64 * 1000;
        let due = now - lease.last_contact_at >= interval_ms || paused || stopping;
        let segments = store.pending_segments(&chunk_id)?;

        if !segments.is_empty() {
            if due || lease.chunk_state == "complete" {
                report(store, client, &lease, &segments)?;
            }
            settled = false;
            continue;
        }
        if lease.chunk_state == "complete" {
            // Everything was reported, and the coordinator saw the chunk end.
            store.update_lease(&chunk_id, "complete", None, None, None)?;
            continue;
        }
        let paused_for = lease.paused_since.map_or(0, |since| now - since);
        if paused && (stopping || paused_for >= release_after.as_millis() as i64) {
            client.release(&lease.lease_id)?;
            store.update_lease(&chunk_id, "released", None, None, None)?;
            info!(chunk = %chunk_id, at = %lease.next_index, "handed the lease back to the coordinator");
            continue;
        }
        if paused {
            settled = false;
        }
        if due && !paused {
            match client.renew(&lease.lease_id)? {
                Some(expires_at) => store.update_lease(&chunk_id, "active", None, Some(expires_at), None)?,
                None => superseded(store, &chunk_id)?,
            }
        }
    }
    Ok(settled)
}

/// Uploads the unreported segments of a lease as one segment.
fn report(store: &Store, client: &Client, lease: &LeaseRow, segments: &[crate::persistence::SegmentRow]) -> Result<()> {
    let chunk_id = lease.chunk.id();
    // Segments continue from what the coordinator accepted; anything else is
    // left over from an earlier lease and can no longer be reported.
    let mut expected = lease.reported_index.clone();
    let mut batch = Vec::new();
    let mut stale = Vec::new();
    for segment in segments {
        if segment.start == expected {
            expected = segment.end.clone();
            batch.push(segment);
        } else if batch.is_empty() {
            stale.push(segment.id);
        }
    }
    if !stale.is_empty() {
        warn!(chunk = %chunk_id, count = stale.len(), "discarding segments that do not continue the accepted work");
        store.settle_segments(&stale, "discarded")?;
    }
    let Some(first) = batch.first() else {
        return Ok(());
    };
    let start = first.start.clone();
    let survivors: Vec<u64> = batch.iter().flat_map(|s| s.survivors.iter().copied()).collect();
    let mut backends: Vec<&str> = batch.iter().map(|s| s.backend.as_str()).collect();
    backends.dedup();
    let ids: Vec<i64> = batch.iter().map(|s| s.id).collect();

    match client.submit_segment(lease, &start, &expected, &survivors, &backends.join("+"))? {
        SegmentOutcome::Accepted { expires_at } => {
            store.settle_segments(&ids, "accepted")?;
            store.update_lease(&chunk_id, "active", Some(&expected), expires_at, None)?;
            debug!(chunk = %chunk_id, %start, end = %expected, survivors = survivors.len(), "segment accepted");
        }
        SegmentOutcome::Superseded => {
            store.settle_segments(&ids, "discarded")?;
            superseded(store, &chunk_id)?;
        }
        SegmentOutcome::Rejected(reason) => {
            store.settle_segments(&ids, "discarded")?;
            store.update_lease(&chunk_id, "rejected", None, None, Some(&reason))?;
            return Err(Rejected(format!("segment of {chunk_id}: {reason}")).into());
        }
    }
    Ok(())
}

fn superseded(store: &Store, chunk_id: &str) -> Result<()> {
    warn!(chunk = %chunk_id, "lease was reassigned while this worker was away; dropping it");
    store.update_lease(chunk_id, "superseded", None, None, None)
}
