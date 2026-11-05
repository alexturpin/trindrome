// Secrets, which `wrangler types` does not know about. Set them with
// `wrangler secret put NAME`, or in .dev.vars for `wrangler dev`.
interface Env {
  /** Comma-separated base64 Ed25519 public keys allowed to import coverage. */
  OWNER_PUBLIC_KEYS?: string;
}
