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
//!
//! The binary also counts the funnel row of the reply-outcome A/B
//! (ADR-0190 D1's cost row): one `JSON.GET` of the depth-4 leaf path per
//! op through `execute_slices`, the reply opened over the harness's
//! long-lived buffer as a connection's is. The verdict rule, the
//! instrument and its controls are the writer rows' (`inf-wire`'s `wire`
//! bench): B − A ≤ 3 instructions per reply; `(I(n) − I(0)) / n` at
//! `n` = 10⁷ by `perf stat -e instructions:u` on one pinned core, legs
//! A B B A twice; A/A and B/B within 0.05; `json-get-plus-one` reads its
//! build + 1.00 ± 0.05; `json-get-plus-four` on B is the rule's own red.
//!
//!   cargo bench -p inf-server --bench json_cmd --no-run
//!   taskset -c <core> perf stat -e instructions:u -x, -- <binary> \
//!       --variant <v> --ops <n>
//!
//! Without `--variant` the binary runs the criterion group below.

use criterion::{Criterion, criterion_group};
use std::hint::black_box;
use std::process::ExitCode;

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
        execute_slices(
            argv,
            &mut self.ks,
            &mut self.cx,
            Nanos(self.clock),
            &mut inf_wire::ReplyBuf::over(&mut self.out),
        );
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

fn main() -> ExitCode {
    if std::env::args().any(|arg| arg == "--variant") {
        return reply_cost::main();
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
    ExitCode::SUCCESS
}

/// The funnel row of the reply-outcome A/B, by instructions per reply.
mod reply_cost {
    use std::hint::black_box;
    use std::process::ExitCode;

    use super::{Harness, bulk, doc_corpus, fixture_id};

    const PATH: &[u8] = b"$.child.child.child.child.id";

    /// The one table of leg names: `--variant` is parsed against it; the
    /// count is the `nop`s planted per reply.
    const LEGS: [(&str, u8); 3] =
        [("json-get", 0), ("json-get-plus-one", 1), ("json-get-plus-four", 4)];

    fn parse() -> Result<(&'static str, u8, u64), String> {
        let mut leg = None;
        let mut ops = None;
        let mut args = std::env::args().skip(1);
        while let Some(flag) = args.next() {
            if flag == "--bench" {
                continue; // cargo bench passes it to a harness-free binary
            }
            let value = args.next().ok_or_else(|| format!("{flag} needs a value"))?;
            match flag.as_str() {
                "--variant" => {
                    let Some(&found) = LEGS.iter().find(|(name, _)| *name == value) else {
                        let names: Vec<&str> = LEGS.iter().map(|(name, _)| *name).collect();
                        return Err(format!("--variant {value}: one of {}", names.join(", ")));
                    };
                    leg = Some(found);
                }
                "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
                other => return Err(format!("unknown flag {other}")),
            }
        }
        let (name, nops) = leg.ok_or("--variant is required")?;
        Ok((name, nops, ops.ok_or("--ops is required")?))
    }

    /// The plants: `NOPS` instructions per reply at the top of the loop
    /// body, each a `nop` with no operand, so the count reads exactly
    /// `NOPS` more.
    #[inline(always)]
    fn plant<const NOPS: u8>() {
        // With no `nop` the leg emits no `asm!` at all.
        if NOPS > 0 {
            // SAFETY: a `nop` touches no register, memory or flag.
            unsafe {
                std::arch::asm!(
                    ".rept {count}",
                    "nop",
                    ".endr",
                    count = const NOPS,
                    options(nomem, nostack, preserves_flags)
                );
            }
        }
    }

    // One command per call through `&mut`: the step is never inlined into
    // the loop.
    #[inline(never)]
    fn json_get(h: &mut Harness, key: &[u8], path: &[u8]) -> usize {
        h.run(&[b"JSON.GET", key, path]).len()
    }

    fn drive<const NOPS: u8>(h: &mut Harness, ops: u64) -> u64 {
        let mut bytes = 0u64;
        for _ in 0..ops {
            plant::<NOPS>();
            let len = json_get(black_box(&mut *h), black_box(b"k:doc"), black_box(PATH));
            bytes = bytes.wrapping_add(len as u64);
        }
        bytes
    }

    pub(super) fn main() -> ExitCode {
        let (name, nops, ops) = match parse() {
            Ok(args) => args,
            Err(error) => {
                eprintln!("json_cmd reply_cost: {error}");
                return ExitCode::from(2);
            }
        };
        let text = doc_corpus::shape(doc_corpus::CANONICAL_SEED, "gate-1KiB").json;
        let mut h = Harness::new();
        assert_eq!(h.run(&[b"JSON.SET", b"k:doc", b"$", text.as_bytes()]), b"+OK\r\n");
        let expected = bulk(format!("[{}]", fixture_id(&text, true)).as_bytes());
        assert_eq!(h.run(&[b"JSON.GET", b"k:doc", PATH]), expected.as_slice());
        let bytes = match nops {
            0 => drive::<0>(&mut h, ops),
            1 => drive::<1>(&mut h, ops),
            4 => drive::<4>(&mut h, ops),
            other => {
                eprintln!("json_cmd reply_cost: {name}: no plant of {other} nops");
                return ExitCode::from(2);
            }
        };
        // The oracle that every op answered the leaf: a leg that answered
        // less would count a cheaper command, not a cheaper reply.
        let want = ops.wrapping_mul(expected.len() as u64);
        assert_eq!(black_box(bytes), want, "{name}: every op answers the leaf");
        println!("{name} ops={ops} bytes={bytes}");
        ExitCode::SUCCESS
    }
}
