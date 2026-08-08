//! M3-S15 (ADR-0042 D8): the published reply-shape matrix is generated,
//! never hand-edited — the M1-S13 staleness rule applied verbatim.
//!
//! `generated_reply_shapes_are_current` fails whenever
//! `docs/json-reply-shapes.md` diverges from the renderer — CI (and
//! therefore the release pipeline) refuses a stale matrix. Regenerate
//! with `INF_REGEN_REPLY_SHAPES=1 cargo test -p compat --test
//! reply_shapes_artifact`.

use std::path::PathBuf;

use compat::replyshapes::{render, rows};

fn artifact_path() -> PathBuf {
    // tests/compat → tests → repo root → docs/json-reply-shapes.md.
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../docs/json-reply-shapes.md")
}

/// The shape table is mechanically consistent with the registry
/// (`rows()` panics otherwise): every `JSON.*` command declared once,
/// write bits agreeing with the registry flags.
#[test]
fn shape_table_is_mechanically_enforced() {
    let rows = rows();
    assert!(rows.len() >= 21, "the full §10.1 surface declares shapes");
    for name in ["JSON.ARRAPPEND", "JSON.ARRPOP", "JSON.OBJKEYS", "JSON.MERGE"] {
        assert!(rows.iter().any(|r| r.name == name), "{name} missing from the shape table");
    }
}

#[test]
fn generated_reply_shapes_are_current() {
    let want = render();
    let path = artifact_path();
    if std::env::var_os("INF_REGEN_REPLY_SHAPES").is_some() {
        std::fs::write(&path, &want).expect("write docs/json-reply-shapes.md");
        println!("regenerated {}", path.display());
        return;
    }
    let got = std::fs::read_to_string(&path).unwrap_or_default();
    assert!(
        got == want,
        "docs/json-reply-shapes.md is stale — regenerate with \
         `INF_REGEN_REPLY_SHAPES=1 cargo test -p compat --test reply_shapes_artifact`"
    );
}
