//! The parts of `trindrome-core` the Cloudflare coordinator needs, exported to
//! JavaScript. The coordinator checks submissions with this exact code, so it
//! can never disagree with the workers about candidates, the filter or chunk
//! identities.
//!
//! Indices cross the boundary as canonical decimal strings, and structured
//! results as JSON strings. Invalid input throws.

use num_bigint::BigUint;
use serde_json::json;
use trindrome_core::proof::{check_segment, encode_survivors};
use trindrome_core::{FILTER_VERSION, GENERATOR_VERSION, SearchSpace, WORK_SCHEMA_VERSION, WorkChunk};
use wasm_bindgen::prelude::*;

fn space(hex_digits: u32, leading: u8) -> Result<SearchSpace, JsError> {
    Ok(SearchSpace::new(hex_digits, leading)?)
}

fn index(s: &str) -> Result<BigUint, JsError> {
    let canonical = !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit()) && (s == "0" || !s.starts_with('0'));
    if !canonical {
        return Err(JsError::new(&format!("{s:?} is not a canonical decimal index")));
    }
    Ok(BigUint::parse_bytes(s.as_bytes(), 10).expect("checked digits"))
}

fn index64(s: &str) -> Result<u64, JsError> {
    u64::try_from(index(s)?).map_err(|_| JsError::new(&format!("index {s} exceeds 64 bits")))
}

/// `{"generator": 1, "filter": 1, "work_schema": 1}`
#[wasm_bindgen]
pub fn versions() -> String {
    json!({"generator": GENERATOR_VERSION, "filter": FILTER_VERSION, "work_schema": WORK_SCHEMA_VERSION}).to_string()
}

/// Candidates in a search space, as a decimal string.
#[wasm_bindgen]
pub fn candidate_count(hex_digits: u32, leading: u8) -> Result<String, JsError> {
    Ok(space(hex_digits, leading)?.candidate_count().to_string())
}

/// The canonical ID of chunk `[start, end)`, after validating it.
#[wasm_bindgen]
pub fn chunk_id(hex_digits: u32, leading: u8, start: &str, end: &str) -> Result<String, JsError> {
    Ok(WorkChunk::new(space(hex_digits, leading)?, index(start)?, index(end)?)?.id())
}

/// Checks a segment's encoded survivor set. Returns
/// `{"survivors": n, "digest": "<hex>", "matches": [{"index", "decimal", "hexadecimal", "nontrivial"}]}`.
#[wasm_bindgen]
pub fn check_survivors(
    hex_digits: u32,
    leading: u8,
    start: &str,
    end: &str,
    encoded: &[u8],
) -> Result<String, JsError> {
    let check = check_segment(space(hex_digits, leading)?, index64(start)?, index64(end)?, encoded)?;
    let digest: String = check.digest.iter().map(|b| format!("{b:02x}")).collect();
    let matches: Vec<_> = check
        .matches
        .iter()
        .map(|(index, v)| {
            json!({
                "index": index.to_string(),
                "decimal": v.decimal,
                "hexadecimal": v.hexadecimal,
                "nontrivial": v.is_nontrivial(),
            })
        })
        .collect();
    Ok(json!({"survivors": check.survivors, "digest": digest, "fingerprint": check.fingerprint.to_string(), "matches": matches}).to_string())
}

/// Encodes absolute survivor indices (decimal strings) of `[start, end)`; for tests.
#[wasm_bindgen]
pub fn encode_survivor_list(start: &str, end: &str, survivors: Vec<String>) -> Result<Vec<u8>, JsError> {
    let survivors = survivors.iter().map(|s| index64(s)).collect::<Result<Vec<_>, _>>()?;
    Ok(encode_survivors(index64(start)?, index64(end)?, &survivors)?)
}
