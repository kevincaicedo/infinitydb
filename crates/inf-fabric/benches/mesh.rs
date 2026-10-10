//! FABRIC-OUT/FABRIC-IN pair cost on a 2-cell mesh, single thread: `send`
//! × N, `flush`, peer `drain`, replies back. Dev-tier A/B instrument for
//! the L12 performance rows (batch 48); not a gate.

use std::hint::black_box;

use criterion::{Criterion, Throughput, criterion_group, criterion_main};
use inf_fabric::{CellFabric, Mesh, MeshConfig, Op, Outcome};
use inf_foundation::{CellId, KeySlot};

const OPS_PER_FLUSH: usize = 16;
const ROUNDS: usize = 1_000;

fn round_trip(a: &mut CellFabric, b: &mut CellFabric) {
    for _ in 0..OPS_PER_FLUSH {
        let token = a.next_token();
        a.send(CellId(1), &Op::Read { token, slot: KeySlot::of_key(b"k"), key: b"k" })
            .expect("credits");
    }
    a.flush();
    let mut owed = Vec::with_capacity(OPS_PER_FLUSH);
    b.drain(1024, |_, op| {
        if let Op::Read { token, .. } = op {
            owed.push(token);
        }
    });
    for token in owed {
        b.reply(CellId(0), token, &Outcome::Nil);
    }
    b.flush();
    a.drain(1024, |_, op| {
        black_box(op);
    });
}

fn bench_mesh(c: &mut Criterion) {
    let mut group = c.benchmark_group("mesh_2cells_1thread");
    group.throughput(Throughput::Elements((OPS_PER_FLUSH * ROUNDS) as u64));
    group.sample_size(20);
    group.bench_function("send16_flush_drain_reply", |b| {
        b.iter(|| {
            let mut cells = Mesh::new(2, MeshConfig { ring_capacity: 4096, data_credits: 1024 });
            let mut cell_b = cells.pop().expect("b");
            let mut cell_a = cells.pop().expect("a");
            for _ in 0..ROUNDS {
                round_trip(&mut cell_a, &mut cell_b);
            }
            black_box(cell_a.stats());
        });
    });
    group.finish();
}

criterion_group!(benches, bench_mesh);
criterion_main!(benches);
