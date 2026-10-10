#![allow(
    clippy::disallowed_methods,
    clippy::disallowed_types,
    reason = "benchmark: fixture files outside cell code (ADR-0144 D5)"
)]
//! M3-S10/S11 command-level rows (ADR-0041): the e2e-minus-reactor
//! denominators the S05 throughput decision has been waiting on —
//! `JSON.SET` vs `SET`, `JSON.GET $.path` vs `GET`, and S16's
//! `JSON.NUMINCRBY $.n` vs `INCR` through the real `execute` path
//! (registry → handlers → store → RESP bytes) on the 1 KiB gate shape —
//! plus the S10 program-cache row. The S25 campaign re-runs the true
//! wire-level gates on the reference box.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use inf_foundation::time::Nanos;
use inf_server::{ConnCx, execute_slices};
use inf_store::{Keyspace, StoreConfig};

#[allow(dead_code, unused_imports)] // shared generator also contains its CLI and witness tests
#[path = "../../../bins/inf-bench/src/doc_corpus.rs"]
mod doc_corpus;

struct Harness {
    ks: Keyspace,
    cx: ConnCx,
    clock: u64,
    out: Vec<u8>,
}

impl Harness {
    fn new() -> Harness {
        Harness::with_config(StoreConfig::default())
    }

    fn with_config(config: StoreConfig) -> Harness {
        Harness {
            ks: Keyspace::new(config),
            cx: ConnCx::try_default().expect("fixture cache allocation"),
            clock: 0,
            out: Vec::with_capacity(4096),
        }
    }

    #[inline]
    fn run(&mut self, argv: &[&[u8]]) -> &[u8] {
        self.clock += 1;
        self.out.clear();
        execute_slices(argv, &mut self.ks, &mut self.cx, Nanos(self.clock), &mut self.out);
        &self.out
    }
}

fn bulk(bytes: &[u8]) -> Vec<u8> {
    let mut reply = format!("${}\r\n", bytes.len()).into_bytes();
    reply.extend_from_slice(bytes);
    reply.extend_from_slice(b"\r\n");
    reply
}

fn verify_reply(case: &str, actual: &[u8], expected: &[u8]) {
    let mut actual = actual.to_vec();
    if std::env::var("INF_BENCH_JSON_CMD_CANARY").as_deref() == Ok(case) {
        actual.push(b'!');
    }
    assert_eq!(actual, expected, "{case} command answer");
}

/// The corpus emits decimal, nonnegative ids without escapes or padding.
/// Read the first/root or last/leaf occurrence independently of the engine.
fn fixture_id(text: &str, leaf: bool) -> u64 {
    let marker = "\"id\":";
    let at = if leaf { text.rfind(marker) } else { text.find(marker) }.expect("fixture id");
    let digits: String =
        text[at + marker.len()..].chars().take_while(char::is_ascii_digit).collect();
    digits.parse().expect("fixture integer")
}

fn verify_commands(text: &str, plain: &[u8]) {
    let mut h = Harness::new();
    verify_reply("set_plain", h.run(&[b"SET", b"k:plain", plain]), b"+OK\r\n");
    verify_reply(
        "set_json_root",
        h.run(&[b"JSON.SET", b"k:doc", b"$", text.as_bytes()]),
        b"+OK\r\n",
    );
    verify_reply("get_plain", h.run(&[b"GET", b"k:plain"]), &bulk(plain));
    let leaf = format!("[{}]", fixture_id(text, true));
    verify_reply(
        "get_json_path",
        h.run(&[b"JSON.GET", b"k:doc", b"$.child.child.child.child.id"]),
        &bulk(leaf.as_bytes()),
    );
    verify_reply("get_json_root", h.run(&[b"JSON.GET", b"k:doc"]), &bulk(text.as_bytes()));
    assert_eq!(h.run(&[b"SET", b"k:counter", b"482190"]), b"+OK\r\n");
    verify_reply("incr_plain", h.run(&[b"INCR", b"k:counter"]), b":482191\r\n");
    let incremented = format!("[{}]", fixture_id(text, false) + 1);
    verify_reply(
        "numincrby_json",
        h.run(&[b"JSON.NUMINCRBY", b"k:doc", b"$.id", b"1"]),
        &bulk(incremented.as_bytes()),
    );
    let mut tree =
        Harness::with_config(StoreConfig { doc_morph_bytes_min: 0, ..StoreConfig::default() });
    assert_eq!(tree.run(&[b"JSON.SET", b"k:doc", b"$", text.as_bytes()]), b"+OK\r\n");
    verify_reply(
        "numincrby_json_forced_tree",
        tree.run(&[b"JSON.NUMINCRBY", b"k:doc", b"$.id", b"1"]),
        &bulk(incremented.as_bytes()),
    );
}

fn bench_json_cmd(c: &mut Criterion) {
    let text = doc_corpus::shape(doc_corpus::CANONICAL_SEED, "gate-1KiB").json;
    let plain = vec![0xABu8; text.len()];
    verify_commands(&text, &plain);
    let mut h = Harness::new();

    let mut group = c.benchmark_group("json_cmd_1kib");
    group.bench_function("set_plain", |b| {
        b.iter(|| {
            let reply = h.run(&[b"SET", b"k:plain", &plain]);
            black_box(reply.len())
        })
    });
    group.bench_function("set_json_root", |b| {
        b.iter(|| {
            let reply = h.run(&[b"JSON.SET", b"k:doc", b"$", text.as_bytes()]);
            black_box(reply.len())
        })
    });
    h.run(&[b"SET", b"k:plain", &plain]);
    h.run(&[b"JSON.SET", b"k:doc", b"$", text.as_bytes()]);
    group.bench_function("get_plain", |b| {
        b.iter(|| {
            let reply = h.run(&[b"GET", b"k:plain"]);
            black_box(reply.len())
        })
    });
    // Depth-4 leaf path — the S02 traversal budget's shape, now paying
    // dispatch + cache hit + eval + resolve + serialize + RESP.
    group.bench_function("get_json_path", |b| {
        b.iter(|| {
            let reply = h.run(&[b"JSON.GET", b"k:doc", b"$.child.child.child.child.id"]);
            black_box(reply.len())
        })
    });
    // Root read: full-document serialization through bulk_patched.
    group.bench_function("get_json_root", |b| {
        b.iter(|| {
            let reply = h.run(&[b"JSON.GET", b"k:doc"]);
            black_box(reply.len())
        })
    });
    h.run(&[b"SET", b"k:counter", b"482190"]);
    group.bench_function("incr_plain", |b| {
        b.iter(|| {
            let reply = h.run(&[b"INCR", b"k:counter"]);
            black_box(reply.len())
        })
    });
    // S16's binding ≤ 1.3× INCR row. `$.id` is a simple child program;
    // the same-width lane patches the stored scalar and bumps once.
    group.bench_function("numincrby_json", |b| {
        b.iter(|| {
            let reply = h.run(&[b"JSON.NUMINCRBY", b"k:doc", b"$.id", b"1"]);
            black_box(reply.len())
        })
    });
    let mut tree =
        Harness::with_config(StoreConfig { doc_morph_bytes_min: 0, ..StoreConfig::default() });
    tree.run(&[b"JSON.SET", b"k:doc", b"$", text.as_bytes()]);
    group.bench_function("numincrby_json_forced_tree", |b| {
        b.iter(|| {
            let reply = tree.run(&[b"JSON.NUMINCRBY", b"k:doc", b"$.id", b"1"]);
            black_box(reply.len())
        })
    });
    group.finish();

    // S10 AC evidence: the whole run above compiled each distinct path
    // once — everything else hit.
    let cache = h.cx.node.path_cache.borrow();
    let total = cache.hits() + cache.misses();
    eprintln!(
        "path_cache: hits={} misses={} evictions={} bytes={} hit_rate={:.4}",
        cache.hits(),
        cache.misses(),
        cache.evictions(),
        cache.bytes(),
        cache.hits() as f64 / total.max(1) as f64,
    );
}

criterion_group!(benches, bench_json_cmd);
criterion_main!(benches);
