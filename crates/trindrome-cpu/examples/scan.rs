//! Exhaustively scans every search space up to a hexadecimal length on the CPU.
//!
//!     cargo run --release -p trindrome-cpu --example scan -- 24

use std::time::Duration;

use num_bigint::BigUint;
use trindrome_core::{LEADING_DIGITS, SearchSpace, WorkChunk};
use trindrome_cpu::search_chunk;

fn main() {
    let max_hex_digits: u32 = match std::env::args().nth(1) {
        Some(arg) => arg.parse().expect("usage: scan [max_hex_digits]"),
        None => 16,
    };

    let mut total = BigUint::ZERO;
    let mut elapsed = Duration::ZERO;
    let mut nontrivial = 0usize;

    for n in 1..=max_hex_digits {
        for leading in LEADING_DIGITS {
            let space = SearchSpace::new(n, leading).unwrap();
            let result = search_chunk(&WorkChunk::whole(space)).unwrap();
            total += result.candidates_checked;
            elapsed += result.elapsed;
            for m in &result.matches {
                let v = &m.verification;
                let kind = if v.is_nontrivial() { "NONTRIVIAL" } else { "trivial" };
                nontrivial += v.is_nontrivial() as usize;
                println!("  {kind} match: {} = 0x{} = 0b{}", v.decimal, v.hexadecimal, v.binary);
            }
        }
        let rate = total.to_string().parse::<f64>().unwrap() / elapsed.as_secs_f64().max(1e-9);
        println!("length {n:>3} done: {total} candidates cumulative, {rate:.0}/s");
    }
    println!("nontrivial triple palindromes found: {nontrivial}");
}
