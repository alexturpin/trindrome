// The public HTTPS API (docs/distributed.md#api). It checks signatures, rate
// limits and survivor sets, stores survivor lists in R2, and forwards small,
// already-checked requests to the coordinator.

import { checkSurvivors, chunkId, versions } from "./core";
import { renderIndex } from "./page";
import type { Capabilities, Coordinator, WorkChunkJson } from "./coordinator";

export { Coordinator } from "./coordinator";

/** Accepted clock difference between a worker and Cloudflare. */
const MAX_SKEW_MS = 5 * 60_000;
const MAX_BODY_BYTES = 8 << 20;
const CANONICAL_INDEX = /^(0|[1-9][0-9]*)$/;

class HttpError extends Error {
  constructor(
    readonly status: number,
    readonly code: string,
    message?: string,
  ) {
    super(message ?? code);
  }
}

function json(body: unknown, status = 200): Response {
  return Response.json(body, { status });
}

function coordinator(env: Env): DurableObjectStub<Coordinator> {
  return env.COORDINATOR.get(env.COORDINATOR.idFromName("global"));
}

function fromBase64(s: string): Uint8Array {
  try {
    return Uint8Array.from(atob(s), (c) => c.charCodeAt(0));
  } catch {
    throw new HttpError(400, "bad_request", "invalid base64");
  }
}

function hex(bytes: ArrayBuffer): string {
  return [...new Uint8Array(bytes)].map((b) => b.toString(16).padStart(2, "0")).join("");
}

/** A display name: 1 to 40 printable characters, or null. */
function displayName(value: unknown): string | null {
  if (typeof value !== "string") {
    return null;
  }
  const name = value.replace(/\p{C}/gu, "").trim();
  return name ? [...name].slice(0, 40).join("") : null;
}

function index(value: unknown, name: string): bigint {
  if (typeof value !== "string" || !CANONICAL_INDEX.test(value)) {
    throw new HttpError(400, "bad_request", `${name} must be a canonical decimal string`);
  }
  return BigInt(value);
}

/** The client's network: /24 for IPv4, /48 for IPv6. */
function networkOf(request: Request): string {
  const ip = request.headers.get("CF-Connecting-IP") ?? "unknown";
  if (ip.includes(".")) {
    return ip.split(".").slice(0, 3).join(".") + ".0/24";
  }
  if (ip.includes(":")) {
    const [head, tail = ""] = ip.split("::");
    const left = head ? head.split(":") : [];
    const right = tail ? tail.split(":") : [];
    const groups = [...left, ...Array(8 - left.length - right.length).fill("0"), ...right];
    return groups.slice(0, 3).join(":") + "::/48";
  }
  return ip;
}

/**
 * Verifies an Ed25519 signature over `timestamp \n method \n path \n sha256(body)`.
 * Replays within the skew window are harmless: every request is idempotent.
 */
async function verifySignature(request: Request, body: ArrayBuffer, publicKey: string): Promise<void> {
  const timestamp = request.headers.get("Trindrome-Timestamp") ?? "";
  const signature = request.headers.get("Trindrome-Signature") ?? "";
  if (!/^[0-9]+$/.test(timestamp) || Math.abs(Date.now() - Number(timestamp)) > MAX_SKEW_MS) {
    throw new HttpError(401, "bad_timestamp", "timestamp missing or more than 5 minutes off");
  }
  const url = new URL(request.url);
  const message = `${timestamp}\n${request.method}\n${url.pathname}\n${hex(await crypto.subtle.digest("SHA-256", body))}`;
  let valid = false;
  try {
    const key = await crypto.subtle.importKey("raw", fromBase64(publicKey), { name: "Ed25519" }, false, ["verify"]);
    valid = await crypto.subtle.verify({ name: "Ed25519" }, key, fromBase64(signature), new TextEncoder().encode(message));
  } catch {
    valid = false;
  }
  if (!valid) {
    throw new HttpError(401, "bad_signature");
  }
}

async function readBody(request: Request): Promise<{ raw: ArrayBuffer; value: any }> {
  if (Number(request.headers.get("Content-Length") ?? 0) > MAX_BODY_BYTES) {
    throw new HttpError(413, "too_large");
  }
  const raw = await request.arrayBuffer();
  if (raw.byteLength > MAX_BODY_BYTES) {
    throw new HttpError(413, "too_large");
  }
  try {
    return { raw, value: raw.byteLength ? JSON.parse(new TextDecoder().decode(raw)) : {} };
  } catch {
    throw new HttpError(400, "bad_request", "body is not JSON");
  }
}

/** Authenticates a registered worker; returns its ID and public key. */
async function authenticate(request: Request, env: Env, raw: ArrayBuffer): Promise<{ workerId: string; publicKey: string }> {
  const workerId = request.headers.get("Trindrome-Worker") ?? "";
  if (!workerId) {
    throw new HttpError(401, "unknown_worker", "Trindrome-Worker header missing");
  }
  if (!(await env.API_LIMITER.limit({ key: workerId })).success) {
    throw new HttpError(429, "rate_limited");
  }
  const publicKey = await coordinator(env).publicKey(workerId);
  if (!publicKey) {
    throw new HttpError(401, "unknown_worker");
  }
  await verifySignature(request, raw, publicKey);
  return { workerId, publicKey };
}

/** Owners are the keys listed in the OWNER_PUBLIC_KEYS secret, comma-separated. */
function isOwner(env: Env, publicKey: string): boolean {
  return (env.OWNER_PUBLIC_KEYS ?? "")
    .split(",")
    .map((k) => k.trim())
    .includes(publicKey);
}

async function importCoverage(env: Env, workerId: string, publicKey: string, body: any): Promise<Response> {
  if (!isOwner(env, publicKey)) {
    throw new HttpError(403, "forbidden", "only an owner key can import coverage");
  }
  if (body.generator_version !== versions.generator || !Array.isArray(body.spaces)) {
    throw new HttpError(400, "bad_request", `spaces from generator ${versions.generator} are required`);
  }
  const spaces = body.spaces.map((s: any) => {
    if (!Number.isInteger(s?.hex_digits) || !Number.isInteger(s?.leading_hex_digit)) {
      throw new HttpError(400, "bad_request", "each space needs hex_digits and leading_hex_digit");
    }
    return { hex_digits: s.hex_digits, leading_hex_digit: s.leading_hex_digit, searched: index(s.searched, "searched") };
  });
  const result = await coordinator(env).importCoverage(workerId, spaces, displayName(body.name));
  return json(result, result.status === "imported" ? 200 : 409);
}

async function register(request: Request, env: Env): Promise<Response> {
  const network = networkOf(request);
  if (!(await env.REGISTER_LIMITER.limit({ key: network })).success) {
    throw new HttpError(429, "rate_limited");
  }
  const { raw, value } = await readBody(request);
  const publicKey = value.public_key;
  if (typeof publicKey !== "string" || fromBase64(publicKey).length !== 32) {
    throw new HttpError(400, "bad_request", "public_key must be a base64 Ed25519 key");
  }
  // Proof of possession: the request is signed with the key being registered.
  await verifySignature(request, raw, publicKey);
  const text = (v: unknown) => (typeof v === "string" ? v.slice(0, 200) : null);
  const result = await coordinator(env).register(
    publicKey,
    network,
    text(value.agent_version) ?? "unknown",
    text(value.gpu),
    text(value.gpu_hash),
  );
  if ("error" in result) {
    throw new HttpError(429, result.error, "too many new workers from this network or overall; try again later");
  }
  return json(result);
}

async function acquire(env: Env, workerId: string, body: any): Promise<Response> {
  if (body.generator_version !== versions.generator || body.filter_version !== versions.filter) {
    throw new HttpError(
      409,
      "unsupported_version",
      `this coordinator uses generator ${versions.generator} and filter ${versions.filter}`,
    );
  }
  const caps: Capabilities = {
    max_hex_digits: Number.isInteger(body.max_hex_digits) ? body.max_hex_digits : 0,
    rate: typeof body.rate === "number" && Number.isFinite(body.rate) ? body.rate : 0,
    name: displayName(body.name),
  };
  const lease = await coordinator(env).acquire(workerId, caps);
  const retry = Number(env.REPORT_INTERVAL_SECONDS) || 300;
  return json(lease ? { lease } : { lease: null, retry_after_seconds: retry });
}

async function submitSegment(env: Env, workerId: string, leaseId: string, body: any): Promise<Response> {
  const chunk = body.chunk as WorkChunkJson;
  if (!chunk || typeof chunk !== "object" || !Number.isInteger(body.epoch)) {
    throw new HttpError(400, "bad_request", "chunk and epoch are required");
  }
  const [chunkStart, chunkEnd] = [index(chunk.start_index, "chunk.start_index"), index(chunk.end_index, "chunk.end_index")];
  const [start, end] = [index(body.start, "start"), index(body.end, "end")];
  let id: string;
  let check;
  const survivors = fromBase64(typeof body.survivors === "string" ? body.survivors : "");
  try {
    id = chunkId(chunk.hex_digits, chunk.leading_hex_digit, chunkStart, chunkEnd);
    if (start < chunkStart || end > chunkEnd) {
      throw new Error(`segment [${start}, ${end}) is outside the chunk`);
    }
    check = checkSurvivors(chunk.hex_digits, chunk.leading_hex_digit, start, end, survivors);
  } catch (e) {
    return json({ status: "rejected", reason: e instanceof Error ? e.message : String(e) }, 422);
  }

  const r2Key = `survivors/${id}/${start}-${end}/${workerId}-${body.epoch}.bin`;
  await env.SURVIVORS.put(r2Key, survivors, {
    customMetadata: { digest: check.digest, survivors: String(check.survivors) },
  });
  const result = await coordinator(env).submitSegment({
    workerId,
    leaseId,
    epoch: body.epoch,
    chunkId: id,
    start: start.toString(),
    end: end.toString(),
    survivors: check.survivors,
    digest: check.digest,
    r2Key,
    backend: typeof body.backend === "string" ? body.backend.slice(0, 100) : null,
    matches: check.matches,
  });
  for (const m of check.matches.filter((m) => m.nontrivial)) {
    console.log(`VERIFIED NONTRIVIAL TRIPLE PALINDROME ${m.decimal} in ${id} from ${workerId}`);
  }
  const status = { accepted: 200, duplicate: 200, superseded: 409, rejected: 422 }[result.status];
  return json(result, status);
}

async function route(request: Request, env: Env): Promise<Response> {
  const url = new URL(request.url);
  const path = url.pathname;
  if (request.method === "GET" && path === "/v1/coverage") {
    return json(await coordinator(env).coverage());
  }
  if (request.method === "GET" && path === "/") {
    return new Response(renderIndex(await coordinator(env).coverage()), {
      headers: { "Content-Type": "text/html; charset=utf-8", "Cache-Control": "public, max-age=60" },
    });
  }
  if (request.method !== "POST") {
    throw new HttpError(404, "not_found");
  }
  if (path === "/v1/register") {
    return register(request, env);
  }
  const { raw, value } = await readBody(request);
  const { workerId, publicKey } = await authenticate(request, env, raw);
  if (path === "/v1/import") {
    return importCoverage(env, workerId, publicKey, value);
  }
  if (path === "/v1/leases") {
    return acquire(env, workerId, value);
  }
  const lease = path.match(/^\/v1\/leases\/([0-9a-f-]{36})\/(segments|renew|release)$/);
  if (!lease) {
    throw new HttpError(404, "not_found");
  }
  const [, leaseId, action] = lease;
  switch (action) {
    case "segments":
      return submitSegment(env, workerId, leaseId, value);
    case "renew": {
      const result = await coordinator(env).renew(workerId, leaseId);
      return json(result, result.status === "ok" ? 200 : 409);
    }
    default: {
      const result = await coordinator(env).release(workerId, leaseId);
      return json(result, result.status === "released" ? 200 : 409);
    }
  }
}

export default {
  async fetch(request, env): Promise<Response> {
    try {
      return await route(request, env);
    } catch (e) {
      if (e instanceof HttpError) {
        return json({ error: e.code, message: e.message }, e.status);
      }
      console.error(e);
      return json({ error: "internal" }, 500);
    }
  },
} satisfies ExportedHandler<Env>;
