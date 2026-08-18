//! M4.5-S09 §4.1 row (dev-tier on this box; campaign re-runs are
//! gate-grade): **parse+compile ≤ 20 μs typical statement; statement
//! cache ≥ 99% hit on the hot query mix** (the hit-*rate* proof is the
//! `cache_hot_mix_hits_over_99_percent` test — a rate is a property,
//! not a latency; this bench prices the paths).
//!
//! - `compile_typical`: the budget row — a two-conjunct SELECT with an
//!   index range and one residual conjunct, compiled cold.
//! - `compile_point`: the steel-thread point statement (`$key = …`).
//! - `compile_reject`: a documented rejection (rejections are not
//!   cached, so their cost is the slow path's floor).
//! - `cache_hit`: the ≥ 99% case — hash + memcmp + epoch check.
//! - `cache_hot_mix`: 20 hot statements round-robin through the cache.

use criterion::{Criterion, criterion_group, criterion_main};
use std::hint::black_box;

use inf_query::partiql::{CatalogView, StatementCache, compile};
use inf_store::{IndexId, IndexKeyType, IndexSpec, IndexState, NsId};

struct Catalog {
    specs: Vec<IndexSpec>,
}

impl CatalogView for Catalog {
    fn resolve_ns(&self, name: &[u8]) -> Option<NsId> {
        (name == b"orders").then_some(NsId(1))
    }

    fn index_by_name(&self, ns: NsId, name: &[u8]) -> Option<&IndexSpec> {
        self.specs.iter().find(|s| s.ns == ns && s.name == name)
    }

    fn indexes(&self, ns: NsId) -> impl Iterator<Item = &IndexSpec> {
        self.specs.iter().filter(move |s| s.ns == ns)
    }

    fn catalog_epoch(&self) -> u64 {
        1
    }
}

fn catalog() -> Catalog {
    let spec = |id: u32, name: &str, path: &str, key_type| IndexSpec {
        id: IndexId(id),
        generation: 1,
        ns: NsId(1),
        name: name.as_bytes().to_vec(),
        program: inf_doc::path::compile(path.as_bytes()).expect("path").as_bytes().to_vec(),
        key_type,
        state: IndexState::Ready,
    };
    Catalog {
        specs: vec![
            spec(1, "price_idx", "$.price", IndexKeyType::I64),
            spec(2, "score_idx", "$.score", IndexKeyType::F64),
            spec(3, "name_idx", "$.name", IndexKeyType::Utf8),
            spec(4, "tags_idx", "$.tags[*]", IndexKeyType::Utf8),
        ],
    }
}

const TYPICAL: &[u8] = b"SELECT * FROM orders WHERE price > 10 AND status = 'open' LIMIT 100";

fn bench_partiql_compile(c: &mut Criterion) {
    let catalog = catalog();

    c.bench_function("compile_typical", |b| {
        b.iter(|| compile(black_box(TYPICAL), &catalog).expect("compiles"))
    });

    c.bench_function("compile_point", |b| {
        b.iter(|| {
            compile(black_box(b"SELECT * FROM orders WHERE $key = 'user:12345'"), &catalog)
                .expect("compiles")
        })
    });

    c.bench_function("compile_reject", |b| {
        b.iter(|| {
            compile(black_box(b"SELECT * FROM orders ORDER BY price"), &catalog)
                .expect_err("documented rejection")
        })
    });

    c.bench_function("cache_hit", |b| {
        let mut cache = StatementCache::default();
        cache.get_or_compile(TYPICAL, &catalog, 8192).expect("warm");
        b.iter(|| cache.get_or_compile(black_box(TYPICAL), &catalog, 8192).expect("hit"))
    });

    c.bench_function("cache_hot_mix", |b| {
        let mut cache = StatementCache::default();
        let mix: Vec<Vec<u8>> = (0..20)
            .map(|i| {
                format!("SELECT * FROM orders WHERE price > {i} AND status = 'open' LIMIT 100")
                    .into_bytes()
            })
            .collect();
        for statement in &mix {
            cache.get_or_compile(statement, &catalog, 8192).expect("warm");
        }
        let mut at = 0usize;
        b.iter(|| {
            let statement = &mix[at % mix.len()];
            at += 1;
            cache.get_or_compile(black_box(statement), &catalog, 8192).expect("hit")
        })
    });
}

criterion_group!(benches, bench_partiql_compile);
criterion_main!(benches);
