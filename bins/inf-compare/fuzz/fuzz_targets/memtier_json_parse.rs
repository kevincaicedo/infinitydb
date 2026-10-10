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
            assert!(data.len() <= json::MAX_BYTES);
            let mut pending = vec![(&doc, 0)];
            let mut nodes = 0;
            while let Some((value, depth)) = pending.pop() {
                nodes += 1;
                assert!(depth <= json::MAX_DEPTH);
                match value {
                    json::Json::Arr(items) => {
                        assert!(depth < json::MAX_DEPTH);
                        pending.extend(items.iter().map(|item| (item, depth + 1)));
                    }
                    json::Json::Obj(fields) => {
                        assert!(depth < json::MAX_DEPTH);
                        nodes += fields.len();
                        pending.extend(fields.iter().map(|(_, item)| (item, depth + 1)));
                    }
                    json::Json::Num(number) => assert!(number.is_finite()),
                    _ => {}
                }
            }
            assert!(nodes <= json::MAX_ITEMS);
            // Navigation is total on whatever parsed.
            let _ = doc.num_at(&["ALL STATS", "Totals", "Ops/sec"]);
            let _ = doc.get(&[]);
        }
    }
});
