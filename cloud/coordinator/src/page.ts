// The public index page: the collective search at a glance, rendered from
// coverage() on every request (cached for a minute at the edge).

import type { Coordinator } from "./coordinator";

type Stats = ReturnType<Coordinator["coverage"]>;

const TRACKS: [string, number[]][] = [
  ["1 or 3", [0x1, 0x3]],
  ["5, 7, 9 or F", [0x5, 0x7, 0x9, 0xf]],
];

function escape(s: string): string {
  return s.replace(/[&<>"']/g, (c) => `&#${c.charCodeAt(0)};`);
}

function count(n: bigint | number): string {
  const v = Number(n);
  for (const [scale, suffix] of [
    [1e15, " quadrillion"],
    [1e12, " trillion"],
    [1e9, " billion"],
    [1e6, " million"],
  ] as const) {
    if (v >= scale) {
      return `${(v / scale).toFixed(2)}${suffix}`;
    }
  }
  return Math.round(v).toLocaleString("en-US");
}

function ago(ms: number): string {
  const s = Math.max(0, Math.round((Date.now() - ms) / 1000));
  if (s < 120) return "just now";
  if (s < 7200) return `${Math.round(s / 60)} min ago`;
  if (s < 172800) return `${Math.round(s / 3600)} h ago`;
  return `${Math.round(s / 86400)} days ago`;
}

function percent(part: bigint, whole: bigint): string {
  return whole === 0n ? "0%" : `${(Number((part * 1_000_000n) / whole) / 10_000).toFixed(2)}%`;
}

function trackRows(stats: Stats): string {
  return TRACKS.map(([label, leads]) => {
    const spaces = stats.spaces.filter((s) => leads.includes(s.leading_hex_digit));
    const lengths = [...new Set(spaces.map((s) => s.hex_digits))].sort((a, b) => a - b);
    let complete = 0;
    let current = "";
    for (const n of lengths) {
      const group = spaces.filter((s) => s.hex_digits === n);
      const searched = group.reduce((t, s) => t + BigInt(s.searched), 0n);
      const total = group.reduce((t, s) => t + BigInt(s.total), 0n);
      if (searched === total && group.length === leads.length) {
        complete = n;
      } else if (!current) {
        current = `${n} digits: ${percent(searched, total)}`;
      }
    }
    return `<tr><td>Starting with ${label}</td><td>${complete ? `${complete} hex digits` : "—"}</td><td>${current || "—"}</td></tr>`;
  }).join("");
}

export function renderIndex(stats: Stats): string {
  const searched = stats.spaces.reduce((t, s) => t + BigInt(s.searched), 0n);
  const nontrivial = stats.discoveries.filter((d) => d.nontrivial);
  const contributors = stats.contributors
    .map(
      (c, i) =>
        `<tr><td>${i + 1}</td><td>${escape(c.name ?? c.worker)}</td><td class="num">${count(c.candidates)}</td><td>${ago(c.last_seen)}</td></tr>`,
    )
    .join("");
  const discoveries = stats.discoveries
    .map(
      (d) =>
        `<li${d.nontrivial ? ' class="found"' : ""}><code>${escape(String(d.decimal))}</code> found by ${escape(String(d.worker_name ?? d.worker_id))}, ${new Date(Number(d.found_at)).toISOString().slice(0, 10)}</li>`,
    )
    .join("");
  const rate = stats.candidates_per_second_last_hour;

  return `<!doctype html>
<html lang="en">
<head>
<meta charset="utf-8">
<meta name="viewport" content="width=device-width, initial-scale=1">
<title>trindrome</title>
<style>
  :root { --bg: #fafaf8; --fg: #1c1c1a; --muted: #6b6b66; --line: #e2e2dc; --accent: #b4461f; }
  @media (prefers-color-scheme: dark) {
    :root { --bg: #161615; --fg: #ecece6; --muted: #9a9a92; --line: #2e2e2b; --accent: #f08a5d; }
  }
  * { box-sizing: border-box; }
  body { margin: 0; background: var(--bg); color: var(--fg); font: 16px/1.55 system-ui, sans-serif; }
  main { max-width: 760px; margin: 0 auto; padding: 40px 16px 64px; }
  h1 { font-size: 28px; margin: 0 0 4px; }
  h2 { font-size: 17px; margin: 40px 0 8px; }
  p { margin: 8px 0; }
  .muted { color: var(--muted); }
  .stats { display: grid; grid-template-columns: repeat(auto-fit, minmax(170px, 1fr)); gap: 12px; margin: 24px 0; }
  .stat { border: 1px solid var(--line); border-radius: 8px; padding: 12px 14px; }
  .stat b { display: block; font-size: 20px; white-space: nowrap; font-variant-numeric: tabular-nums; }
  .stat span { color: var(--muted); font-size: 13px; }
  table { width: 100%; border-collapse: collapse; font-size: 15px; }
  td, th { text-align: left; padding: 6px 8px; border-bottom: 1px solid var(--line); }
  th { color: var(--muted); font-weight: 500; font-size: 13px; }
  .num { text-align: right; font-variant-numeric: tabular-nums; }
  .table { overflow-x: auto; }
  code { font-size: 13px; word-break: break-all; }
  .found { color: var(--accent); font-weight: 600; }
</style>
</head>
<body>
<main>
<h1>trindrome</h1>
<p class="muted">A shared search for numbers that are palindromes in binary, decimal and hexadecimal at the same time.</p>

<div class="stats">
  <div class="stat"><b>${count(searched)}</b><span>candidates searched</span></div>
  <div class="stat"><b>${rate > 0 ? `${count(rate)}/s` : "idle"}</b><span>over the last hour</span></div>
  <div class="stat"><b>${stats.workers_last_hour} / ${stats.workers_registered}</b><span>machines active / joined</span></div>
  <div class="stat"><b>${nontrivial.length}</b><span>new triple palindromes</span></div>
</div>

<h2>Progress</h2>
<p class="muted">Lengths in hex digits. Everything here lies beyond Rick Regan's 2010 search, which covered up to 64 digits for numbers starting with 1 or 3 and 32 digits for the rest.</p>
<div class="table"><table>
<tr><th>Numbers</th><th>Complete through</th><th>In progress</th></tr>
${trackRows(stats)}
</table></div>

<h2>Discoveries</h2>
${discoveries ? `<ul>${discoveries}</ul>` : '<p class="muted">None yet. Every candidate that passes the GPU filter is checked exactly before it counts.</p>'}

<h2>Contributors</h2>
${contributors ? `<div class="table"><table><tr><th>#</th><th>Machine</th><th class="num">Searched</th><th>Last seen</th></tr>${contributors}</table></div>` : '<p class="muted">Nobody yet.</p>'}

<p class="muted" style="margin-top:40px">Raw numbers: <a href="/v1/coverage" style="color:inherit">/v1/coverage</a></p>
</main>
</body>
</html>`;
}
