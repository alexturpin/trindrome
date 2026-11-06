// trindrome-core, compiled to wasm (crates/trindrome-wasm). The coordinator uses
// it for everything mathematical, so it cannot disagree with the workers.

import wasmModule from "./wasm/trindrome_wasm_bg.wasm";
import * as core from "./wasm/trindrome_wasm.js";

core.initSync({ module: wasmModule });

export interface Versions {
  generator: number;
  filter: number;
  work_schema: number;
}

export interface SegmentCheck {
  survivors: number;
  digest: string;
  /** survivor_fingerprint as a decimal u64: additive over disjoint ranges. */
  fingerprint: string;
  matches: { index: string; decimal: string; hexadecimal: string; nontrivial: boolean }[];
}

export const versions: Versions = JSON.parse(core.versions());

export function candidateCount(hexDigits: number, leading: number): bigint {
  return BigInt(core.candidate_count(hexDigits, leading));
}

/** The canonical chunk ID; throws if the chunk is invalid. */
export function chunkId(hexDigits: number, leading: number, start: bigint, end: bigint): string {
  return core.chunk_id(hexDigits, leading, start.toString(), end.toString());
}

/** Checks a segment's encoded survivor set; throws with the reason if it is invalid. */
export function checkSurvivors(
  hexDigits: number,
  leading: number,
  start: bigint,
  end: bigint,
  encoded: Uint8Array,
): SegmentCheck {
  return JSON.parse(core.check_survivors(hexDigits, leading, start.toString(), end.toString(), encoded));
}
