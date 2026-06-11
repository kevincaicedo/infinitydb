//! M0-S08 AC: 2-core SPSC throughput — gate: ≥ 8M msgs/s/ring-pair,
//! amortized enqueue+dequeue < 40 ns/msg.
//!
//! The gate artifact requires the Linux reference box with pinned cores
//! (`taskset`/`isolcpus`); macOS has no public thread-affinity API, so local
//! runs are dev-tier sanity numbers (scheduler may co-locate or migrate the
//! threads). Measures the batch paths (`publish_batch`/`consume_batch`) —
//! the paths FABRIC-OUT/FABRIC-IN actually use — over 64-byte-class slots.

use std::hint::black_box;
use std::thread;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use inf_fabric::{FabricMsg, ring};

const MESSAGES: u64 = 1_000_000;
const RING_CAPACITY: usize = 1024;
const CONSUME_QUOTA: usize = 256;

/// Round-trip MESSAGES 64-byte-class slots through a fresh ring pair with a
/// dedicated producer thread; returns once every message is consumed.
fn pump_messages(frame: &[u8]) -> u64 {
    let (mut producer, mut consumer) = ring::<FabricMsg>(RING_CAPACITY);
    let template = FabricMsg::from_frame(frame);
    let join = thread::spawn(move || {
        let mut published = 0u64;
        while published < MESSAGES {
            let want = (MESSAGES - published).min(CONSUME_QUOTA as u64);
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
        let n = consumer.consume_batch(CONSUME_QUOTA, |msg| {
            black_box(msg.frame().len());
        }) as u64;
        consumed += n;
        if n == 0 {
            std::hint::spin_loop();
        }
    }
    join.join().expect("producer thread");
    consumed
}

fn bench_ring(c: &mut Criterion) {
    let mut group = c.benchmark_group("spsc_ring_2threads");
    group.throughput(Throughput::Elements(MESSAGES));
    group.sample_size(10);

    // Inline-slot frame (the Read/Reply common case).
    let inline_frame = vec![0xA5u8; 48];
    group.bench_function("msgs_64B_inline", |b| {
        b.iter(|| pump_messages(black_box(&inline_frame)));
    });

    group.finish();
}

criterion_group!(benches, bench_ring);
criterion_main!(benches);
