//! The zero-dependency JSON reader that parses memtier's output
//! (F-L18-07): arbitrary text never panics and never overflows the stack.
//! The bin has no lib target, so the module is included by path.
#![no_main]
#![allow(dead_code)]

use libfuzzer_sys::fuzz_target;

#[path = "../../src/json.rs"]
mod json;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        if let Ok(doc) = json::Json::parse(text) {
            // Navigation is total on whatever parsed.
            let _ = doc.num_at(&["ALL STATS", "Totals", "Ops/sec"]);
            let _ = doc.get(&[]);
        }
    }
});
