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
//!   loss against `head_8_1t` in the same binary is not judging;
//! - `retained_8`, `retained_8_1t`: the retained ring (ADR-0157) on the
//!   `head_8` and `head_8_1t` shapes, its values through reservations, its
//!   backing built, joined and retired in every iteration as HEAD's is
//!   allocated and freed.

use std::hint::black_box;
use std::thread;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use inf_fabric::{FabricMsg, RetainedRing, RingPhase, RingSteps, ring};

const MESSAGES: u64 = 1_000_000;

/// The planted leg's added work a message: iterations of a loop the
/// optimizer cannot remove; one reads ≈ 5 % above `head_8_1t` here.
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

/// A retained ring of `FabricMsg`, built to Ready.
fn retained_ready() -> RetainedRing<FabricMsg> {
    let mut ring = RetainedRing::admit().expect("FabricMsg is within the block bound");
    let mut steps = RingSteps::new(3);
    for _ in 0..3 {
        ring.build_step(&mut steps);
    }
    assert_eq!(ring.phase(), RingPhase::Ready, "a ring builds in three steps");
    ring
}

/// Joins a ring whose endpoints were released and retires it: no value is
/// left, so two steps free its blocks.
fn retained_retire(mut ring: RetainedRing<FabricMsg>) {
    assert!(matches!(ring.join(), RingPhase::Retiring(_)), "both endpoints released");
    let mut steps = RingSteps::new(2);
    while matches!(ring.phase(), RingPhase::Retiring(_)) && steps.left() > 0 {
        ring.retire_step(&mut steps, drop);
    }
    assert!(matches!(ring.phase(), RingPhase::Terminal(_)), "an empty ring retires in two steps");
}

/// Writes up to `want` clones of `template` through one reservation, as
/// `publish_batch` writes up to the room it finds; returns the count.
fn retained_publish(
    producer: &mut inf_fabric::RetainedProducer<FabricMsg>,
    template: &FabricMsg,
    want: u64,
) -> usize {
    let mut reservation = producer.reserve(usize::MAX);
    let mut written = 0u64;
    while written < want {
        let Some(permit) = reservation.permit() else { break };
        permit.write(template.clone());
        written += 1;
    }
    reservation.publish()
}

/// `pump_messages` on the retained ring: 8 slots, batches of 8, two threads.
fn pump_retained(frame: &[u8]) -> u64 {
    let mut ring = retained_ready();
    let (mut producer, mut consumer) = ring.split().expect("a ready ring splits");
    let template = FabricMsg::from_frame(frame);
    let join = thread::spawn(move || {
        let mut published = 0u64;
        while published < MESSAGES {
            let want = (MESSAGES - published).min(8);
            let n = retained_publish(&mut producer, &template, want) as u64;
            published += n;
            if n == 0 {
                std::hint::spin_loop();
            }
        }
    });
    let mut consumed = 0u64;
    while consumed < MESSAGES {
        let n = consumer.consume_batch(8, |msg| {
            black_box(msg.frame().len());
        }) as u64;
        consumed += n;
        if n == 0 {
            std::hint::spin_loop();
        }
    }
    join.join().expect("producer thread");
    drop(consumer);
    retained_retire(ring);
    assert_eq!(consumed, MESSAGES, "every message consumed");
    consumed
}

/// `pump_one_thread` on the retained ring: 8 slots, batches of 8.
fn pump_retained_one_thread(frame: &[u8]) -> u64 {
    let mut ring = retained_ready();
    let (mut producer, mut consumer) = ring.split().expect("a ready ring splits");
    let template = FabricMsg::from_frame(frame);
    let mut consumed = 0u64;
    while consumed < MESSAGES {
        let want = (MESSAGES - consumed).min(8);
        let published = retained_publish(&mut producer, &template, want);
        let n = consumer.consume_batch(published, |msg| {
            black_box(msg.frame().len());
        });
        assert_eq!(n, published, "a published batch is consumed whole");
        consumed += n as u64;
    }
    drop((producer, consumer));
    retained_retire(ring);
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
    group.bench_function("retained_8", |b| {
        b.iter(|| pump_retained(black_box(&inline_frame)));
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
    group.bench_function("retained_8_1t", |b| {
        b.iter(|| pump_retained_one_thread(black_box(&inline_frame)));
    });
    group.finish();
}

criterion_group!(benches, bench_ring);
criterion_main!(benches);
