//! M0-S11/S12 ACs: parse throughput (gate: ≥ 2 GB/s/core on the inline
//! command mix) and dispatch cost (gate: ≤ 15 cycles ≈ ~4 ns at reference
//! clocks). Gate artifacts come from the Linux reference box; local runs are
//! dev-tier numbers.
//!
//! The binary also counts the writer rows of the reply-outcome A/B
//! (ADR-0190 D1's cost row): one reply per op, at the `GET` shape (one
//! 64 B bulk) and the `+OK` shape, written as a command's reply is — a
//! reply opened over a long-lived buffer, a writer built over it, one frame.
//! The verdict rule: B − A ≤ 3 instructions per reply on each shape. The
//! instrument: exact user-space instructions per reply, `(I(n) − I(0)) / n`
//! at `n` = 10⁷, read outside this binary (`perf stat -e instructions:u`)
//! on one pinned core, legs A B B A twice per shape, A and B two builds of
//! this file's legs. A run judges only when its controls hold:
//!
//! - the same binary against itself: A against A and B against B within
//!   0.05 per reply, at a resolution of 0.01 per reply at `n` = 10⁷;
//! - liveness: `<shape>-plus-one`, one `nop` per reply at the top of the
//!   loop body, reads its build + 1.00, ± 0.05;
//! - the rule's own red: `<shape>-plus-four` on B reads B − A + 4, over
//!   the bound while B − A ≥ 0. A cheaper B leaves it at the bound or under,
//!   and the run is invalid until the red is repaired.
//!
//! One leg, then one leg counted: `perf` wraps the binary that `--no-run`
//! prints, never `cargo`, whose own instructions would join the count:
//!
//!   cargo bench -p inf-wire --bench wire --no-run
//!   taskset -c <core> perf stat -e instructions:u -x, -- <binary> \
//!       --variant <v> --ops <n>
//!
//! `v` is a leg of `LEGS`; `n` ≥ 0 (0 is the I(0) leg). Without
//! `--variant` the binary runs the criterion groups below.

use std::hint::black_box;
use std::process::ExitCode;

use criterion::{Criterion, Throughput, criterion_group};
use inf_wire::{ConnParser, Parsed, ParserLimits, Protocol, ReplyBuf, RespWriter, lookup};

/// Pipelined GET/SET mix, RESP-encoded — the regime the node throughput
/// gate runs (memtier P=16 shape).
fn pipeline(commands: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(commands * 48);
    for i in 0..commands {
        if i % 2 == 0 {
            let key = format!("key:{:08}", i % 10_000);
            bytes.extend_from_slice(
                format!(
                    "*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n$8\r\nvalue{:03}\r\n",
                    key.len(),
                    i % 1000
                )
                .as_bytes(),
            );
        } else {
            let key = format!("key:{:08}", i % 10_000);
            bytes.extend_from_slice(
                format!("*2\r\n$3\r\nGET\r\n${}\r\n{key}\r\n", key.len()).as_bytes(),
            );
        }
    }
    bytes
}

fn bench_parse(c: &mut Criterion) {
    let stream = pipeline(4096);
    let mut group = c.benchmark_group("resp_parse");
    group.throughput(Throughput::Bytes(stream.len() as u64));

    group.bench_function("pipelined_get_set_mix", |b| {
        let mut parser = ConnParser::new(ParserLimits::default());
        b.iter(|| {
            let mut commands = 0u64;
            let mut iter = parser.feed(black_box(&stream));
            while let Some(parsed) = iter.next() {
                match parsed {
                    Parsed::Command(argv) => {
                        commands += 1;
                        black_box(argv.arg(0));
                    }
                    other => panic!("unexpected: {other:?}"),
                }
            }
            assert_eq!(commands, 4096);
        });
    });
    group.finish();

    // Bulk-heavy regime: 512 B values — bytes/s here measures header
    // processing against realistic payload freight (payload bytes are never
    // read by the parser).
    let mut bulk_stream = Vec::new();
    let value = vec![0xABu8; 512];
    for i in 0..1024 {
        let key = format!("key:{i:08}");
        bulk_stream.extend_from_slice(
            format!("*3\r\n$3\r\nSET\r\n${}\r\n{key}\r\n$512\r\n", key.len()).as_bytes(),
        );
        bulk_stream.extend_from_slice(&value);
        bulk_stream.extend_from_slice(b"\r\n");
    }
    let mut group = c.benchmark_group("resp_parse_bulk");
    group.throughput(Throughput::Bytes(bulk_stream.len() as u64));
    group.bench_function("pipelined_set_512b", |b| {
        let mut parser = ConnParser::new(ParserLimits::default());
        b.iter(|| {
            let mut commands = 0u64;
            let mut iter = parser.feed(black_box(&bulk_stream));
            while let Some(parsed) = iter.next() {
                match parsed {
                    Parsed::Command(argv) => {
                        commands += 1;
                        black_box(argv.arg(2).len());
                    }
                    other => panic!("unexpected: {other:?}"),
                }
            }
            assert_eq!(commands, 1024);
        });
    });
    group.finish();
}

fn bench_dispatch(c: &mut Criterion) {
    let mut group = c.benchmark_group("command_dispatch");
    // Realistic case mix: clients send both cases.
    let names: Vec<&[u8]> =
        vec![b"GET", b"set", b"INCRBY", b"pexpire", b"DEL", b"strlen", b"PING", b"hello"];
    // Same closure WITHOUT the lookup: the dispatch cycle cost is
    // (hit_mix − baseline) × measured GHz, not the raw closure time.
    group.bench_function("harness_baseline", |b| {
        let mut i = 0;
        b.iter(|| {
            i = (i + 1) % names.len();
            black_box(names[i])
        });
    });
    group.bench_function("lookup_hit_mix", |b| {
        let mut i = 0;
        b.iter(|| {
            i = (i + 1) % names.len();
            black_box(lookup(black_box(names[i]))).expect("known command")
        });
    });
    group.bench_function("lookup_miss", |b| {
        b.iter(|| black_box(lookup(black_box(b"NOSUCHC"))));
    });
    // Throughput shape: EXECUTE dispatches a whole parse batch, so
    // independent lookups overlap in the pipeline — this amortized cost is
    // what the §18.1 cycle budget (10–20 cyc dispatch) means. Divide by 64.
    group.bench_function("lookup_hit_batch64", |b| {
        let batch: Vec<&[u8]> = (0..64).map(|i| names[i % names.len()]).collect();
        b.iter(|| {
            // Results feed an accumulator (dispatch always reads the meta),
            // black_box only at the end: per-element black_box would force
            // every result through memory and serialize the pipeline.
            let mut acc = 0u64;
            for name in &batch {
                if let Some(meta) = lookup(name) {
                    acc = acc.wrapping_add(meta.arity.unsigned_abs() as u64);
                }
            }
            black_box(acc)
        });
    });
    group.finish();
}

/// Reply-write cost (M0-S11 serializer). `simple("OK")` is the reply every
/// `SET` pays, so the C6 line sanitization (CR/LF mapped on the way out —
/// ADR-0097) lands directly on the write path: this group is the A/B for it.
/// Dev-tier numbers; the gate rows come from the reference box.
fn bench_write(c: &mut Criterion) {
    let mut group = c.benchmark_group("resp_write");
    let value = vec![0xABu8; 512];

    // Literal text is what every hot reply actually passes (`w.simple("OK")`,
    // `w.error("ERR value is not an integer or out of range")`), so the C6
    // line scan const-folds there; the `black_box`ed variants below are the
    // worst case, where the text is opaque to the compiler.
    group.bench_function("simple_ok_literal_x64", |b| {
        let mut out = Vec::with_capacity(1024);
        b.iter(|| {
            out.clear();
            let mut out_reply = ReplyBuf::over(&mut out);
            let mut w = RespWriter::new(&mut out_reply, Protocol::Resp2);
            for _ in 0..64 {
                w.simple("OK");
            }
            black_box(out.len())
        });
    });
    group.bench_function("error_literal_x64", |b| {
        let mut out = Vec::with_capacity(8192);
        b.iter(|| {
            out.clear();
            let mut out_reply = ReplyBuf::over(&mut out);
            let mut w = RespWriter::new(&mut out_reply, Protocol::Resp2);
            for _ in 0..64 {
                w.error("ERR value is not an integer or out of range");
            }
            black_box(out.len())
        });
    });
    group.bench_function("simple_ok_x64", |b| {
        let mut out = Vec::with_capacity(1024);
        b.iter(|| {
            out.clear();
            let mut out_reply = ReplyBuf::over(&mut out);
            let mut w = RespWriter::new(&mut out_reply, Protocol::Resp2);
            for _ in 0..64 {
                w.simple(black_box("OK"));
            }
            black_box(out.len())
        });
    });
    group.bench_function("bulk_512b_x64", |b| {
        let mut out = Vec::with_capacity(64 * 1024);
        b.iter(|| {
            out.clear();
            let mut out_reply = ReplyBuf::over(&mut out);
            let mut w = RespWriter::new(&mut out_reply, Protocol::Resp2);
            for _ in 0..64 {
                w.bulk(black_box(&value));
            }
            black_box(out.len())
        });
    });
    group.bench_function("error_x64", |b| {
        let mut out = Vec::with_capacity(8192);
        b.iter(|| {
            out.clear();
            let mut out_reply = ReplyBuf::over(&mut out);
            let mut w = RespWriter::new(&mut out_reply, Protocol::Resp2);
            for _ in 0..64 {
                w.error(black_box("ERR value is not an integer or out of range"));
            }
            black_box(out.len())
        });
    });
    group.finish();
}

criterion_group!(benches, bench_parse, bench_dispatch, bench_write);

fn main() -> ExitCode {
    if std::env::args().any(|arg| arg == "--variant") {
        return reply_cost::main();
    }
    benches();
    Criterion::default().configure_from_args().final_summary();
    ExitCode::SUCCESS
}

/// The writer rows of the reply-outcome A/B, by instructions per reply.
mod reply_cost {
    use std::hint::black_box;
    use std::process::ExitCode;

    use inf_wire::{Protocol, ReplyBuf, RespWriter};

    /// The reply a leg writes.
    #[derive(Clone, Copy)]
    enum Shape {
        /// `GET`'s hit: one 64 B bulk.
        Get,
        /// `SET`'s answer: `+OK`.
        Ok,
    }

    /// The one table of leg names: `--variant` is parsed against it; the
    /// count is the `nop`s planted per reply.
    const LEGS: [(&str, Shape, u8); 6] = [
        ("get", Shape::Get, 0),
        ("get-plus-one", Shape::Get, 1),
        ("get-plus-four", Shape::Get, 4),
        ("ok", Shape::Ok, 0),
        ("ok-plus-one", Shape::Ok, 1),
        ("ok-plus-four", Shape::Ok, 4),
    ];

    fn parse() -> Result<(&'static str, Shape, u8, u64), String> {
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
                    let Some(&found) = LEGS.iter().find(|(name, ..)| *name == value) else {
                        let names: Vec<&str> = LEGS.iter().map(|(name, ..)| *name).collect();
                        return Err(format!("--variant {value}: one of {}", names.join(", ")));
                    };
                    leg = Some(found);
                }
                "--ops" => ops = Some(value.parse::<u64>().map_err(|e| format!("--ops: {e}"))?),
                other => return Err(format!("unknown flag {other}")),
            }
        }
        let (name, shape, nops) = leg.ok_or("--variant is required")?;
        Ok((name, shape, nops, ops.ok_or("--ops is required")?))
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

    // One reply per call through `&mut`, as a connection's buffer is
    // reached: the step is never inlined into the loop.
    #[inline(never)]
    fn reply_get(out: &mut Vec<u8>, value: &[u8]) -> usize {
        out.clear();
        RespWriter::new(&mut ReplyBuf::over(out), Protocol::Resp2).bulk(value);
        out.len()
    }

    #[inline(never)]
    fn reply_ok(out: &mut Vec<u8>) -> usize {
        out.clear();
        RespWriter::new(&mut ReplyBuf::over(out), Protocol::Resp2).simple("OK");
        out.len()
    }

    fn drive<const NOPS: u8>(shape: Shape, ops: u64) -> u64 {
        let value = [0xABu8; 64];
        let mut out = Vec::with_capacity(256);
        let mut bytes = 0u64;
        match shape {
            Shape::Get => {
                for _ in 0..ops {
                    plant::<NOPS>();
                    let len = reply_get(black_box(&mut out), black_box(&value));
                    bytes = bytes.wrapping_add(len as u64);
                }
            }
            Shape::Ok => {
                for _ in 0..ops {
                    plant::<NOPS>();
                    bytes = bytes.wrapping_add(reply_ok(black_box(&mut out)) as u64);
                }
            }
        }
        bytes
    }

    pub(super) fn main() -> ExitCode {
        let (name, shape, nops, ops) = match parse() {
            Ok(args) => args,
            Err(error) => {
                eprintln!("wire reply_cost: {error}");
                return ExitCode::from(2);
            }
        };
        let bytes = match nops {
            0 => drive::<0>(shape, ops),
            1 => drive::<1>(shape, ops),
            4 => drive::<4>(shape, ops),
            other => {
                eprintln!("wire reply_cost: {name}: no plant of {other} nops");
                return ExitCode::from(2);
            }
        };
        // `$64\r\n`, the value, `\r\n`; `+OK\r\n`.
        let per_reply: u64 = match shape {
            Shape::Get => 71,
            Shape::Ok => 5,
        };
        // The oracle that every op wrote its whole reply: a leg that wrote
        // nothing would count a cheaper loop, not a cheaper reply.
        let expected = ops.wrapping_mul(per_reply);
        assert_eq!(black_box(bytes), expected, "{name}: every op writes one reply");
        println!("{name} ops={ops} bytes={bytes}");
        ExitCode::SUCCESS
    }
}
