//! M1-S13: the published compat matrix is generated, never hand-edited.
//!
//! `generated_matrix_is_current` fails whenever `docs/compat-matrix.md`
//! diverges from the renderer — CI (and therefore the release pipeline)
//! refuses a stale matrix. Regenerate with
//! `INF_REGEN_MATRIX=1 cargo test -p compat --test matrix_artifact`.

use std::path::PathBuf;

use compat::matrixgen::{Status, render, rows};

fn artifact_path() -> PathBuf {
    // tests/compat → infinitydb → repo root → docs/compat-matrix.md.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../docs/compat-matrix.md")
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
