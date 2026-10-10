//! PartiQL statement fuzz (M4.5-S09, L9 — the same-PR rule for the new
//! parser): arbitrary bytes never panic the lexer/parser/compiler;
//! rejection is deterministic (same bytes, same documented error);
//! accepted statements compile byte-identically twice (L7), their
//! access programs survive the `from_bytes` trust boundary unchanged,
//! and EXPLAIN renders total and deterministic. The fixture catalog
//! covers every key type, a multi-valued path, a not-ready index, and
//! a duplicate-path pair, so every resolution branch is reachable.

#![no_main]

use std::sync::OnceLock;

use inf_query::access::AccessProgram;
use inf_query::partiql::{CatalogView, compile};
use inf_store::{IndexId, IndexKeyType, IndexSpec, IndexState, NsId};
use libfuzzer_sys::fuzz_target;

struct Catalog {
    specs: Vec<IndexSpec>,
}

impl CatalogView for Catalog {
    fn resolve_ns(&self, name: &[u8]) -> Option<NsId> {
        match name {
            b"ns" => Some(NsId(1)),
            b"dup" => Some(NsId(2)),
            _ => None,
        }
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

fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| {
        let spec = |id: u32, ns: u32, name: &str, path: &str, key_type, state| IndexSpec {
            id: IndexId(id),
            generation: u64::from(id),
            ns: NsId(ns),
            name: name.as_bytes().to_vec(),
            program: inf_doc::path::compile(path.as_bytes())
                .expect("fixture path")
                .as_bytes()
                .to_vec(),
            key_type,
            state,
        };
        Catalog {
            specs: vec![
                spec(1, 1, "i", "$.i", IndexKeyType::I64, IndexState::Ready),
                spec(2, 1, "f", "$.f", IndexKeyType::F64, IndexState::Ready),
                spec(3, 1, "s", "$.s", IndexKeyType::Utf8, IndexState::Ready),
                spec(4, 1, "b", "$.b", IndexKeyType::Bool, IndexState::Ready),
                spec(5, 1, "m", "$.m[*]", IndexKeyType::Utf8, IndexState::Ready),
                spec(6, 1, "p", "$.p", IndexKeyType::I64, IndexState::Backfilling),
                spec(7, 2, "d1", "$.d", IndexKeyType::I64, IndexState::Ready),
                spec(8, 2, "d2", "$.d", IndexKeyType::I64, IndexState::Ready),
            ],
        }
    })
}

fuzz_target!(|data: &[u8]| {
    let catalog = catalog();
    match compile(data, catalog) {
        Err(first) => {
            let again = compile(data, catalog).expect_err("rejection is deterministic");
            assert_eq!(first, again, "one statement, one documented error");
            // Display is the compat contract — it must render total.
            let _ = first.to_string();
        }
        Ok(compiled) => {
            let again = compile(data, catalog).expect("acceptance is deterministic");
            assert_eq!(
                compiled.program.as_bytes(),
                again.program.as_bytes(),
                "recompilation is byte-identical (L7)"
            );
            let revalidated = AccessProgram::from_bytes(compiled.program.as_bytes())
                .expect("compiler output survives its own trust boundary");
            assert_eq!(revalidated.as_bytes(), compiled.program.as_bytes());
            assert_eq!(revalidated.decode(), compiled.access, "decode round-trip");
            let explained = compiled.program.explain();
            assert_eq!(explained, revalidated.explain(), "EXPLAIN is deterministic");
        }
    }
});
