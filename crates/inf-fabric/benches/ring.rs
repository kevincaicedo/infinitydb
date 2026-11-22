//! M0-S08 AC: 2-core SPSC throughput — gate: ≥ 8M msgs/s/ring-pair,
//! amortized enqueue+dequeue < 40 ns/msg.
//!
//! The gate artifact requires the Linux reference box with pinned cores
//! (`taskset`/`isolcpus`); macOS has no public thread-affinity API, so local
//! runs are dev-tier sanity numbers (scheduler may co-locate or migrate the
//! threads). Measures the batch paths (`publish_batch`/`consume_batch`) —
//! the paths FABRIC-OUT/FABRIC-IN actually use — over 64-byte-class slots.
//!
//! The legs, each its own bench id so a run reads one leg at a time
//! (`--bench <id>`):
//! - `msgs_64B_inline`: the fabric's shape, 1024 slots, batches of 256;
//! - `head_8`: the same ring at the cleanup rings' shape, 8 slots, batches
//!   of 8 (ADR-0157 D1's slot count), two threads;
//! - `head_8_1t`: that shape on one thread, publish then consume: no line
//!   crosses a core, so the leg reads the algorithm's own cost a message;
//! - `head_8_1t_planted`: `head_8_1t` with a fixed cost on every consumed
//!   message, the A/B's planted loss: a judge that does not read it as a
//!   loss against `head_8_1t` in the same binary is not judging.

use std::hint::black_box;
use std::thread;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use inf_fabric::{FabricMsg, ring};

const MESSAGES: u64 = 1_000_000;

/// The planted leg's added work a message: iterations of a loop the
/// optimizer cannot remove; one reads ≈ 15 % above `head_8_1t` here.
const PLANTED_SPINS: u64 = 1;

/// Round-trip MESSAGES 64-byte-class slots through a fresh ring of
/// `capacity` slots with a dedicated producer thread, `batch` a call on
/// each side, `spins` planted iterations a consumed message; returns once
/// every message is consumed, in order.
fn pump_messages(frame: &[u8], capacity: usize, batch: usize, spins: u64) -> u64 {
    let (mut producer, mut consumer) = ring::<FabricMsg>(capacity);
    let template = FabricMsg::from_frame(frame);
    let join = thread::spawn(move || {
        let mut published = 0u64;
        while published < MESSAGES {
            let want = (MESSAGES - published).min(batch as u64);
            let batch = (0..want).map(|_| template.clone());
            let n = producer.publish_batch(batch) as u64;
            published += n;
            if n == 0 {
                std::hint::spin_loop();
            }
        }
    });
    let mut consumed = 0u64;
    while consumed < MESSAGES {
        let n = consumer.consume_batch(batch, |msg| {
            black_box(msg.frame().len());
            let mut planted = 0u64;
            for spin in 0..spins {
                planted = black_box(planted ^ spin);
            }
        }) as u64;
        consumed += n;
        if n == 0 {
            std::hint::spin_loop();
        }
    }
    join.join().expect("producer thread");
    assert_eq!(consumed, MESSAGES, "every message consumed");
    consumed
}

/// MESSAGES slots through a fresh ring on one thread: publish `batch`, then
/// consume them, until done. No cross-core line moves, so the leg reads the
/// algorithm's own cost a message (the A/B's explanatory leg).
fn pump_one_thread(frame: &[u8], capacity: usize, batch: usize, spins: u64) -> u64 {
    let (mut producer, mut consumer) = ring::<FabricMsg>(capacity);
    let template = FabricMsg::from_frame(frame);
    let mut consumed = 0u64;
    while consumed < MESSAGES {
        let want = (MESSAGES - consumed).min(batch as u64);
        let published = producer.publish_batch((0..want).map(|_| template.clone()));
        let n = consumer.consume_batch(published, |msg| {
            black_box(msg.frame().len());
            let mut planted = 0u64;
            for spin in 0..spins {
                planted = black_box(planted ^ spin);
            }
        });
        assert_eq!(n, published, "a published batch is consumed whole");
        consumed += n as u64;
    }
    consumed
}

fn bench_ring(c: &mut Criterion) {
    let mut group = c.benchmark_group("spsc_ring_2threads");
    group.throughput(Throughput::Elements(MESSAGES));
    group.sample_size(20);

    // Inline-slot frame (the Read/Reply common case).
    let inline_frame = vec![0xA5u8; 48];
    group.bench_function("msgs_64B_inline", |b| {
        b.iter(|| pump_messages(black_box(&inline_frame), 1024, 256, 0));
    });
    group.bench_function("head_8", |b| {
        b.iter(|| pump_messages(black_box(&inline_frame), 8, 8, 0));
    });
    group.finish();

    let mut group = c.benchmark_group("spsc_ring_1thread");
    group.throughput(Throughput::Elements(MESSAGES));
    group.sample_size(20);
    group.bench_function("head_8_1t", |b| {
        b.iter(|| pump_one_thread(black_box(&inline_frame), 8, 8, 0));
    });
    group.bench_function("head_8_1t_planted", |b| {
        b.iter(|| pump_one_thread(black_box(&inline_frame), 8, 8, black_box(PLANTED_SPINS)));
    });
    group.finish();
}

criterion_group!(benches, bench_ring);
criterion_main!(benches);
