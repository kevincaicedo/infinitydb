//! Reply-shape matrix generator (M3-S15; ADR-0042 D8):
//! `docs/json-reply-shapes.md` is rendered from
//! `inf_server::JSON_REPLY_SHAPES` — the typed table that lives beside
//! the handlers it describes — **generated, never hand-edited** (the
//! milestone §3.2 freeze row: command × path mode × protocol is the
//! compat contract, an artifact rather than tribal knowledge, L8).
//!
//! The staleness test (`tests/reply_shapes_artifact.rs`) fails CI
//! whenever the committed artifact diverges from this render — the
//! release pipeline inherits that refusal (the M1-S13 rule, applied
//! verbatim). [`rows`] mechanically enforces the table against the
//! `inf-wire` registry: every `JSON.*` registry command has exactly one
//! shape row, and the row's write bit agrees with the registry flags.

use inf_server::{JSON_REPLY_SHAPES, ReplyShape};
use inf_wire::{COMMANDS, CmdFlags};

/// The shape table, registry-enforced: panics (failing the test that
/// calls it) when a `JSON.*` command lacks a row, a row names an unknown
/// command, or a row's write bit disagrees with the registry.
pub fn rows() -> &'static [ReplyShape] {
    let json_commands: Vec<_> =
        COMMANDS.iter().filter(|meta| meta.name.starts_with("JSON.")).collect();
    assert_eq!(
        json_commands.len(),
        JSON_REPLY_SHAPES.len(),
        "every JSON.* registry command declares exactly one reply-shape row"
    );
    for meta in json_commands {
        let row = JSON_REPLY_SHAPES
            .iter()
            .find(|row| row.name == meta.name)
            .unwrap_or_else(|| panic!("{} missing from JSON_REPLY_SHAPES", meta.name));
        assert_eq!(
            row.write,
            meta.flags.contains(CmdFlags::WRITE),
            "{}: shape-table write bit disagrees with the registry",
            meta.name
        );
    }
    JSON_REPLY_SHAPES
}

/// Renders the full `docs/json-reply-shapes.md` artifact.
pub fn render() -> String {
    let rows = rows();
    let mut out = String::new();
    let mut push = |line: &str| {
        out.push_str(line);
        out.push('\n');
    };
    push("# InfinityDB `JSON.*` Reply Shapes");
    push("");
    push("> **GENERATED — do not edit.** Rendered by `tests/compat/src/replyshapes.rs`");
    push("> from `inf_server::JSON_REPLY_SHAPES` (the table beside the handlers).");
    push(
        "> Regenerate: `INF_REGEN_REPLY_SHAPES=1 cargo test -p compat --test reply_shapes_artifact`",
    );
    push("> (CI fails when this file is stale — the release pipeline inherits that refusal).");
    push("");
    push("The RedisJSON reply contract differs by **path mode**: `$` paths answer");
    push("match *sets*, legacy (non-`$`) paths answer single values — the first match");
    push("for reads, the last applied match for mutations (ADR-0041 D7). Most RESP3");
    push("shapes differ only by protocol-level nulls (`_` for `$-1`/`*-1`); the");
    push("RedisJSON-native TYPE and number frames are declared explicitly below. M3-S21");
    push("byte-diffs both protocols against the pinned container, and every accepted");
    push("divergence is generated into `docs/compat-matrix.md` (L8).");
    push("");
    push("Errors shared across the family: missing keys on mutations answer");
    push("`ERR could not perform this operation on a key that doesn't exist`; legacy");
    push("paths with zero matches answer `ERR Path '<path>' does not exist`; legacy");
    push("paths whose matches are all type-inapplicable answer");
    push("`ERR Path '<path>' does not contain a <type>`; size/depth limits answer the");
    push("ADR-0039 D5 pinned lines. Durable namespaces accept `JSON.*` writes through");
    push("M3-S17's `DocDelta`/`DocFull` path (ADR-0043).");
    push("");
    push("| Command | Kind | `$` path | Legacy path | RESP3 delta | Notes |");
    push("|---|---|---|---|---|---|");
    for row in rows {
        push(&format!(
            "| `{}` | {} | {} | {} | {} | {} |",
            row.name,
            if row.write { "write" } else { "read" },
            row.dollar,
            row.legacy,
            row.resp3,
            row.notes,
        ));
    }
    push("");
    push("---");
    push("");
    push("Compatibility status per command lives in `docs/compat-matrix.md`; this");
    push("artifact pins the *shapes* the corpus executes under both protocols");
    push("(`inf-server/tests/json_commands.rs`). Performance claims live in the");
    push("claim ledger, never here (L10).");
    out
}
