//! Totality and metric-width checks for the exact control-owner stat parser.
#![no_main]

#[path = "../../src/limits.rs"]
mod limits;
#[path = "../../src/process_info/stat.rs"]
mod stat;

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if let Ok(text) = std::str::from_utf8(data) {
        let parsed = stat::cpu_microseconds(text);
        if text.len() as u64 > limits::PROCESS_STAT_BYTES_MAX {
            assert_eq!(parsed, None);
        }
    }
    if data.len() >= 16 {
        let user = u64::from_le_bytes(data[..8].try_into().unwrap());
        let sys = u64::from_le_bytes(data[8..16].try_into().unwrap());
        let text = format!("1 (fuzz ) comm) R 0 0 0 0 0 0 0 0 0 0 {user} {sys}");
        let expected = |ticks| (u128::from(ticks) * 10_000).min(u128::from(u64::MAX)) as u64;
        assert_eq!(stat::cpu_microseconds(&text), Some((expected(sys), expected(user))));
    }
});
