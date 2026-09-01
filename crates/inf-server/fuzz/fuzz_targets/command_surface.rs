//! Command-surface totality (Group 0 item 3, review of 2026-08-30 §5.5 —
//! the C3 lesson): the full command registry driven with adversarial argv
//! shapes — arbitrary argument counts, key/value lengths across the
//! `MAX_KEY_LEN` edge, and raw bytes — must never panic a cell. Replies
//! are RESP frames or typed errors; correctness is the compat oracle's
//! job, totality is this target's. C3 (`INCR <256-byte key>` → cell
//! abort) is exactly the class this target exists to catch before a
//! client does.
#![no_main]

use inf_foundation::time::Nanos;
use inf_server::{ConnCx, execute_slices};
use inf_store::{Keyspace, StoreConfig};
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let mut store = Keyspace::new(StoreConfig::default());
    let mut cx = ConnCx::default();
    let mut rest = data;
    let mut out = Vec::new();
    // Up to 8 commands per input. Each: [selector][argc][per-arg: u16-le
    // length + bytes, clamped to what remains]. Even selectors pick a
    // registry command name (the biased half); odd selectors use the raw
    // next bytes as the name (the unknown-command path).
    for _ in 0..8 {
        let [selector, argc, tail @ ..] = rest else { break };
        let (selector, argc) = (*selector, usize::from(*argc) % 8);
        rest = tail;
        let mut owned: Vec<Vec<u8>> = Vec::with_capacity(argc + 1);
        if selector % 2 == 0 {
            let commands = &inf_wire::COMMANDS;
            let name = commands[usize::from(selector / 2) % commands.len()].name;
            owned.push(name.as_bytes().to_vec());
        } else {
            let take = usize::from(selector).min(rest.len());
            owned.push(rest[..take].to_vec());
            rest = &rest[take..];
        }
        for _ in 0..argc {
            let [a, b, tail @ ..] = rest else { break };
            let len = usize::from(u16::from_le_bytes([*a, *b])).min(tail.len());
            owned.push(tail[..len].to_vec());
            rest = &tail[len..];
        }
        let argv: Vec<&[u8]> = owned.iter().map(Vec::as_slice).collect();
        out.clear();
        execute_slices(&argv, &mut store, &mut cx, Nanos(1), &mut out);
    }
});
