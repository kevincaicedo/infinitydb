//! M1-S13: the published compat matrix is generated, never hand-edited.
//!
//! `generated_matrix_is_current` fails whenever `docs/compat-matrix.md`
//! diverges from the renderer — CI (and therefore the release pipeline)
//! refuses a stale matrix. Regenerate with
//! `INF_REGEN_MATRIX=1 cargo test -p compat --test matrix_artifact`.

use std::path::PathBuf;

use compat::matrixgen::{Status, render, rows};

fn artifact_path() -> PathBuf {
    // tests/compat → tests → repo root → docs/compat-matrix.md.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/compat-matrix.md")
}

/// The declaration table is mechanically consistent with the registry and
/// the corpus (`rows()` panics otherwise — see matrixgen.rs): every command
/// declared, every `full` claim backed by ≥ 1 byte-compared case, every
/// `partial` justified.
#[test]
fn declared_statuses_are_mechanically_enforced() {
    let rows = rows();
    assert!(rows.iter().any(|r| r.status == Status::Full), "a surface exists");
    // The M1-E5 surface is declared.
    for name in ["SUBSCRIBE", "UNSUBSCRIBE", "PSUBSCRIBE", "PUNSUBSCRIBE", "PUBLISH", "PUBSUB"] {
        assert!(rows.iter().any(|r| r.name == name), "{name} missing from the matrix");
    }
    // The M3 §10.1 `JSON.*` surface is declared (M3-S22), so the staleness
    // gate below demonstrably covers the JSON section: dropping a JSON row
    // from the registry or the declaration table fails here, and any status/
    // note/deviation drift fails `generated_matrix_is_current`.
    for name in [
        "JSON.SET",
        "JSON.GET",
        "JSON.MGET",
        "JSON.DEL",
        "JSON.FORGET",
        "JSON.TYPE",
        "JSON.NUMINCRBY",
        "JSON.NUMMULTBY",
        "JSON.STRAPPEND",
        "JSON.STRLEN",
        "JSON.TOGGLE",
        "JSON.CLEAR",
        "JSON.ARRAPPEND",
        "JSON.ARRINSERT",
        "JSON.ARRINDEX",
        "JSON.ARRLEN",
        "JSON.ARRPOP",
        "JSON.ARRTRIM",
        "JSON.OBJKEYS",
        "JSON.OBJLEN",
        "JSON.MERGE",
        "JSON.DEBUG",
    ] {
        assert!(rows.iter().any(|r| r.name == name), "{name} missing from the matrix");
    }
}

/// M3-S22: the rendered artifact carries the whole JSON section — the
/// RedisJSON oracle pin, per-command RedisJSON deviation entries, and the
/// `JSON.RESP` absent row (deprecated upstream — M3 plan anti-goals). Byte
/// equality in `generated_matrix_is_current` then extends the release
/// pipeline's staleness refusal to the JSON section as a whole.
#[test]
fn rendered_matrix_covers_the_json_section() {
    let rendered = render();
    for needle in [
        "redis/redis-stack-server:7.4.0-v8",
        "| `JSON.SET` |",
        "RedisJSON RESP2 `",
        "RedisJSON RESP3 `",
        "| `JSON.RESP` | Never — deprecated upstream; declared absent per the M3 plan anti-goals |",
    ] {
        assert!(rendered.contains(needle), "rendered matrix lost the JSON section: {needle:?}");
    }
}

#[test]
fn generated_matrix_is_current() {
    let want = render();
    let path = artifact_path();
    if std::env::var_os("INF_REGEN_MATRIX").is_some() {
        std::fs::write(&path, &want).expect("write docs/compat-matrix.md");
        println!("regenerated {}", path.display());
        return;
    }
    let got = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        got == want,
        "docs/compat-matrix.md is stale — regenerate with \
         `INF_REGEN_MATRIX=1 cargo test -p compat --test matrix_artifact`"
    );
}
