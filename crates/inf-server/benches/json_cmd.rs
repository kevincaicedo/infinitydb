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
            cx: ConnCx::default(),
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

fn bench_json_cmd(c: &mut Criterion) {
    let text = doc_corpus::shape(doc_corpus::CANONICAL_SEED, "gate-1KiB").json;
    let plain = vec![0xABu8; text.len()];
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
