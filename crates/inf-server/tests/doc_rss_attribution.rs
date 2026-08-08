//! M3-S19 Linux CI divergence check. A document-heavy command workload
//! is loaded through the real JSON parser/cache/store path, then the
//! incremental RSS window is reconciled against the same disjoint
//! tripwire domains production INFO exports. The baseline snapshot makes
//! executable text, test-harness state, and the pre-sized index cancel.
#![cfg(all(feature = "doc", target_os = "linux"))]

use std::collections::BTreeMap;

use inf_foundation::time::Nanos;
use inf_server::{ConnCx, execute_slices};
use inf_store::{ArenaConfig, Keyspace, StoreConfig};

const DOCUMENTS: usize = 65_536;

fn key_of(mut value: usize) -> [u8; 12] {
    let mut key = *b"d:0000000000";
    for byte in key[2..].iter_mut().rev() {
        *byte = b'0' + (value % 10) as u8;
        value /= 10;
    }
    key
}

fn snapshot(ks: &mut Keyspace, cx: &mut ConnCx, clock: &mut u64) -> BTreeMap<String, u64> {
    *clock += 1;
    let mut out = Vec::new();
    execute_slices(&[b"INFO", b"tripwires"], ks, cx, Nanos(*clock), &mut out);
    String::from_utf8(out)
        .expect("INFO is UTF-8")
        .lines()
        .filter_map(|line| {
            let (name, value) = line.trim_end_matches('\r').split_once(':')?;
            Some((name.to_owned(), value.parse().ok()?))
        })
        .collect()
}

fn field(snapshot: &BTreeMap<String, u64>, name: &str) -> u64 {
    snapshot.get(name).copied().unwrap_or_else(|| panic!("INFO omitted {name}"))
}

#[test]
fn document_heavy_domains_track_incremental_rss_within_ten_percent() {
    let arena = ArenaConfig { chunk_size: 64 << 10, max_resident: None };
    let cfg =
        StoreConfig { arena, doc_arena: arena, initial_keys: DOCUMENTS, ..StoreConfig::default() };
    let mut ks = Keyspace::new(cfg);
    let mut cx = ConnCx::default();
    let mut clock = 0u64;
    // One baseline INFO also warms its formatter before the measured
    // interval; the fixed input stays live on both sides.
    let json = format!(r#"{{"pad":"{}"}}"#, "x".repeat(1_000));
    let before = snapshot(&mut ks, &mut cx, &mut clock);

    let mut out = Vec::with_capacity(8);
    for index in 0..DOCUMENTS {
        let key = key_of(index);
        out.clear();
        clock += 1;
        execute_slices(
            &[b"JSON.SET", &key, b"$", json.as_bytes()],
            &mut ks,
            &mut cx,
            Nanos(clock),
            &mut out,
        );
        assert_eq!(out, b"+OK\r\n");
    }
    let after = snapshot(&mut ks, &mut cx, &mut clock);

    const DOMAINS: &[&str] = &[
        "records_resident_bytes",
        "index_bytes",
        "wheel_bytes",
        "evict_bytes",
        "doc_resident_bytes",
        "doc_scratch_bytes",
        "doc_path_cache_bytes",
        "wire_buffers_bytes",
        "conn_state_bytes",
        "pubsub_state_bytes",
    ];
    let domain_delta: u64 =
        DOMAINS.iter().map(|name| field(&after, name).saturating_sub(field(&before, name))).sum();
    let rss_delta = field(&after, "process_rss").saturating_sub(field(&before, "process_rss"));
    assert!(rss_delta > 0, "document fill must raise VmRSS");
    let divergence = domain_delta.abs_diff(rss_delta) as f64 / rss_delta as f64 * 100.0;
    eprintln!(
        "document RSS attribution: documents={DOCUMENTS} domains={domain_delta} rss={rss_delta} divergence={divergence:.3}%"
    );
    assert!(
        divergence <= 10.0,
        "document domain delta {domain_delta} vs RSS delta {rss_delta}: {divergence:.3}% > 10%"
    );
}
