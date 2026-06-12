//! M0-S11/S12 ACs: parse throughput (gate: ≥ 2 GB/s/core on the inline
//! command mix) and dispatch cost (gate: ≤ 15 cycles ≈ ~4 ns at reference
//! clocks). Gate artifacts come from the Linux reference box; local runs are
//! dev-tier numbers.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use inf_wire::{ConnParser, Parsed, ParserLimits, lookup};

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

criterion_group!(benches, bench_parse, bench_dispatch);
criterion_main!(benches);
