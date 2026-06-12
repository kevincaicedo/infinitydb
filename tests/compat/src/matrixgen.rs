//! Compat-matrix generator (M1-S13): `docs/compat-matrix.md` is rendered
//! from the command registry (`inf-wire`) plus the oracle-diff corpus
//! ([`MATRIX`]) — **generated, never hand-edited** (the milestone §3.2
//! freeze: `command → {status, since, deviations[], tests[]}`).
//!
//! The per-command status *declaration* lives in [`DECLARED`] and is the L8
//! compatibility claim; [`rows`] mechanically enforces it against the
//! corpus: a `full` command must have at least one byte-compared case, and
//! the registry and the declaration table must agree exactly. The staleness
//! test (`tests/matrix_artifact.rs`) fails CI whenever the committed
//! artifact diverges from this render — the release pipeline inherits that
//! refusal (M1-S13 AC).
//!
//! Status vocabulary (the decision rule, applied per command):
//! - `full` — behavior-contract equivalent to Redis 8; any recorded
//!   deviations are representational (ordering, identity payloads, opaque
//!   cursors/art).
//! - `partial` — a semantic difference exists (atomicity windows, precision,
//!   missing subcommands or filter forms) and is documented.
//! - `stub` — accepted but intentionally inert (none in the M1 surface).
//! - `extension` — InfinityDB `INF.*` surface, unknown to Redis.
//! - `internal` — fabric program primitives, not a client surface.

use inf_wire::{COMMANDS, CmdFlags};

use crate::matrix::{Check, MATRIX};

/// Declared compatibility level (see the module-level decision rule).
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Status {
    Full,
    Partial,
    Stub,
    Extension,
    Internal,
}

impl Status {
    pub fn name(self) -> &'static str {
        match self {
            Status::Full => "full",
            Status::Partial => "partial",
            Status::Stub => "stub",
            Status::Extension => "extension",
            Status::Internal => "internal",
        }
    }
}

/// One declared command: the human judgment the generator enforces.
pub struct Declared {
    pub name: &'static str,
    pub status: Status,
    pub since: &'static str,
    pub note: &'static str,
}

const fn d(
    name: &'static str,
    status: Status,
    since: &'static str,
    note: &'static str,
) -> Declared {
    Declared { name, status, since, note }
}

/// The compatibility declaration, one row per registry command (enforced
/// 1:1 against `inf_wire::COMMANDS` by [`rows`]).
pub static DECLARED: &[Declared] = &[
    d("PING", Status::Full, "M0", ""),
    d("ECHO", Status::Full, "M0", ""),
    d(
        "HELLO",
        Status::Full,
        "M0",
        "identity fields (server/version) are InfinityDB's own, as for any non-Redis server",
    ),
    d("GET", Status::Full, "M0", ""),
    d("SET", Status::Full, "M0", ""),
    d("SETNX", Status::Full, "M0", ""),
    d("SETEX", Status::Full, "M0", ""),
    d("PSETEX", Status::Full, "M0", ""),
    d("GETSET", Status::Full, "M0", ""),
    d("GETDEL", Status::Full, "M0", ""),
    d("DEL", Status::Full, "M0", ""),
    d("EXISTS", Status::Full, "M0", ""),
    d("TYPE", Status::Full, "M0", "only the string type exists until M3"),
    d("INCR", Status::Full, "M0", ""),
    d("DECR", Status::Full, "M0", ""),
    d("INCRBY", Status::Full, "M0", ""),
    d("DECRBY", Status::Full, "M0", ""),
    d("APPEND", Status::Full, "M0", ""),
    d("STRLEN", Status::Full, "M0", ""),
    d("EXPIRE", Status::Full, "M0", "TTLs ≥ ~34.8 years clamp to the u40 record bound"),
    d("PEXPIRE", Status::Full, "M0", "same u40 clamp"),
    d("TTL", Status::Full, "M0", ""),
    d("PTTL", Status::Full, "M0", ""),
    d("PERSIST", Status::Full, "M0", ""),
    d(
        "INFO",
        Status::Partial,
        "M0",
        "sections + field vocabulary present; gauges are this cell's slice until the control plane aggregates (client-smoke CI is the open M1-S14 AC)",
    ),
    d(
        "COMMAND",
        Status::Partial,
        "M0",
        "COMMAND DOCS is an honest empty map; the registry covers the implemented surface only",
    ),
    d("MGET", Status::Full, "M1", ""),
    d("MSET", Status::Full, "M1", ""),
    d(
        "MSETNX",
        Status::Partial,
        "M1",
        "cross-cell keys are check-then-set until M4 transactions; single-cell exact",
    ),
    d("GETRANGE", Status::Full, "M1", ""),
    d("SETRANGE", Status::Full, "M1", "values bound at 16 MiB − 1 (record format v0)"),
    d("GETEX", Status::Full, "M1", ""),
    d(
        "INCRBYFLOAT",
        Status::Partial,
        "M1",
        "computes in f64 (Redis: long double); formatting matches on the pinned corpus, precision tails may differ",
    ),
    d("SUBSTR", Status::Full, "M1", ""),
    d(
        "RENAME",
        Status::Partial,
        "M1",
        "cross-owner pairs run as a two-cell fabric program — atomic per cell, not across cells until M4; same-owner pairs exact",
    ),
    d("RENAMENX", Status::Partial, "M1", "same cross-owner window as RENAME"),
    d(
        "COPY",
        Status::Partial,
        "M1",
        "same cross-owner window as RENAME; TTL transfers as relative ms across cells",
    ),
    d("TOUCH", Status::Full, "M1", ""),
    d("UNLINK", Status::Full, "M1", ""),
    d("DBSIZE", Status::Full, "M1", ""),
    d("KEYS", Status::Full, "M1", "result ordering is engine-defined (set equality holds)"),
    d("RANDOMKEY", Status::Full, "M1", "two-level random: cell, then key"),
    d(
        "SCAN",
        Status::Full,
        "M1",
        "cursor values are engine-internal; the every-resident-key-≥-once guarantee is proptested",
    ),
    d("FLUSHDB", Status::Full, "M1", ""),
    d(
        "FLUSHALL",
        Status::Partial,
        "M1",
        "atomic per cell, eventually complete across cells within one scatter round (no global pause)",
    ),
    d(
        "OBJECT",
        Status::Partial,
        "M1",
        "IDLETIME is an honest 0 (CLOCK recency, no LRU clock); FREQ is the CMS Morris estimate",
    ),
    d(
        "DEBUG",
        Status::Partial,
        "M1",
        "subset: SLEEP / JMAP / OBJECT / SET-ACTIVE-EXPIRE; SLEEP stalls one cell, never the node",
    ),
    d("EXPIREAT", Status::Full, "M1", ""),
    d("PEXPIREAT", Status::Full, "M1", ""),
    d("EXPIRETIME", Status::Full, "M1", ""),
    d("PEXPIRETIME", Status::Full, "M1", ""),
    d("SELECT", Status::Full, "M1", ""),
    d("CONFIG", Status::Partial, "M1", "typed M1 key subset with frozen hot-reload classes"),
    d(
        "CLIENT",
        Status::Partial,
        "M1",
        "KILL supports the ID filter form; LIST addr/fd are placeholders until peername capture",
    ),
    d(
        "LOLWUT",
        Status::Partial,
        "M1",
        "the whole reply is version art (nothing byte-comparable by design)",
    ),
    d("SUBSCRIBE", Status::Full, "M1", ""),
    d(
        "UNSUBSCRIBE",
        Status::Full,
        "M1",
        "bare-form confirmations emit in subscription order (Redis: dict order)",
    ),
    d("PSUBSCRIBE", Status::Full, "M1", ""),
    d("PUNSUBSCRIBE", Status::Full, "M1", "same bare-form ordering note as UNSUBSCRIBE"),
    d(
        "PUBLISH",
        Status::Full,
        "M1",
        "a publisher subscribed to its own channel via a remote owner cell may receive its frame before the publish reply (local owners match Redis order)",
    ),
    d(
        "PUBSUB",
        Status::Partial,
        "M1",
        "SHARDCHANNELS / SHARDNUMSUB arrive with sharded pub/sub (M3 cut line)",
    ),
    d("INF.NS", Status::Extension, "M1", "namespace registry v1 — the M2 durability seam"),
    d("INF.TAKE", Status::Internal, "M1", "cross-cell RENAME/COPY program primitive"),
    d("INF.PEEK", Status::Internal, "M1", "cross-cell COPY program primitive"),
];

/// One rendered matrix row: declaration + mechanically-derived corpus data.
pub struct CommandRow {
    pub name: &'static str,
    pub status: Status,
    pub since: &'static str,
    pub note: &'static str,
    pub arity: i8,
    pub flags: String,
    pub compared_cases: usize,
    pub deviations: Vec<&'static str>,
}

/// Joins the registry, the declaration, and the corpus — panicking on any
/// inconsistency (these panics are the M1-S12/S13 CI enforcement: a new
/// command without a declaration, or a `full` claim without byte-compared
/// evidence, fails the build's test run).
pub fn rows() -> Vec<CommandRow> {
    assert_eq!(
        COMMANDS.len(),
        DECLARED.len(),
        "every registry command needs a compat declaration (and vice versa)"
    );
    let mut rows = Vec::with_capacity(COMMANDS.len());
    for meta in &COMMANDS {
        let declared = DECLARED
            .iter()
            .find(|d| d.name == meta.name)
            .unwrap_or_else(|| panic!("{} has no compat declaration", meta.name));
        let mut compared_cases = 0;
        let mut deviations: Vec<&'static str> = Vec::new();
        for case in MATRIX {
            if !case.argv[0].eq_ignore_ascii_case(meta.name) {
                continue;
            }
            if case.check.compared() {
                compared_cases += 1;
            } else if let Check::SkipDiff(why) = case.check
                && !deviations.contains(&why)
            {
                deviations.push(why);
            }
        }
        if declared.status == Status::Full {
            assert!(
                compared_cases > 0,
                "{} is declared full but has no byte-compared corpus case",
                meta.name
            );
        }
        if matches!(declared.status, Status::Partial | Status::Stub) {
            assert!(
                !declared.note.is_empty(),
                "{} is declared {} without a justification note",
                meta.name,
                declared.status.name()
            );
        }
        let mut flags = Vec::new();
        for (flag, name) in [
            (CmdFlags::READONLY, "readonly"),
            (CmdFlags::WRITE, "write"),
            (CmdFlags::DENYOOM, "denyoom"),
            (CmdFlags::ADMIN, "admin"),
            (CmdFlags::FAST, "fast"),
        ] {
            if meta.flags.contains(flag) {
                flags.push(name);
            }
        }
        rows.push(CommandRow {
            name: meta.name,
            status: declared.status,
            since: declared.since,
            note: declared.note,
            arity: meta.arity,
            flags: flags.join(" "),
            compared_cases,
            deviations,
        });
    }
    rows
}

/// Command families not yet implemented, with their owning milestone (the
/// `absent` half of the matrix — a static table in generator code, still
/// never hand-edited in the artifact).
pub static ABSENT: &[(&str, &str)] = &[
    ("Persistence admin (SAVE, BGSAVE, INF.CKPT, …)", "M2 — durability"),
    ("Hashes, lists, sets, zsets, bitmaps, bitfield, HyperLogLog", "M3 — data types"),
    ("Keyspace notifications, SLOWLOG, MONITOR, sharded pub/sub (SSUBSCRIBE/SPUBLISH)", "M3"),
    ("Connection control (QUIT, RESET)", "M3 (RESET pairs with transaction state)"),
    ("MULTI / EXEC / WATCH / DISCARD, EVAL / Lua, FUNCTION, WAIT", "M4 — transactions"),
    ("Streams (X*), AUTH / TLS / ACL, CLIENT TRACKING", "M5"),
    ("JSON.* documents", "M6"),
    ("Vector sets", "M8"),
    ("Replication / cluster admin", "M9+"),
];

/// Renders the full `docs/compat-matrix.md` artifact.
pub fn render() -> String {
    let rows = rows();
    let total = MATRIX.len();
    let skipped = MATRIX.iter().filter(|c| !c.check.compared()).count();
    let compared = total - skipped;
    let count = |status: Status| rows.iter().filter(|r| r.status == status).count();

    let mut out = String::new();
    let mut push = |line: &str| {
        out.push_str(line);
        out.push('\n');
    };
    push("# InfinityDB Redis Compatibility Matrix");
    push("");
    push("> **GENERATED — do not edit.** Rendered by `tests/compat/src/matrixgen.rs`");
    push("> from the `inf-wire` command registry and the oracle-diff corpus.");
    push("> Regenerate: `INF_REGEN_MATRIX=1 cargo test -p compat --test matrix_artifact`");
    push("> (CI fails when this file is stale — the release pipeline inherits that refusal).");
    push("");
    push("Oracle: **Redis 8.0.5** (local oracle on the dev box; the dockerized CI oracle");
    push("pin lands with the M1-S14 release pipeline). Every declared-`full` behavior is");
    push("byte-diffed against the oracle on every test run; any new deviation fails CI");
    push("until it is allowlisted with a justification (L8 — honesty is total).");
    push("");
    push(&format!(
        "**Corpus:** {compared} byte-compared cases · {skipped} documented deviations · 0 tolerated failures.",
    ));
    push(&format!(
        "**Surface:** {} commands — {} full · {} partial · {} stub · {} extension · {} internal.",
        rows.len(),
        count(Status::Full),
        count(Status::Partial),
        count(Status::Stub),
        count(Status::Extension),
        count(Status::Internal),
    ));
    push("");
    push("Status vocabulary: `full` = behavior-contract equivalent (recorded deviations");
    push("are representational: ordering, identity payloads, opaque cursors/art);");
    push("`partial` = a documented semantic difference exists; `stub` = accepted but");
    push("inert; `extension` = `INF.*` surface unknown to Redis; `internal` = fabric");
    push("program primitives, not a client surface.");
    push("");
    push("## Commands");
    push("");
    push("| Command | Status | Since | Flags | Arity | Cases | Notes |");
    push("|---|---|---|---|---|---|---|");
    for row in &rows {
        push(&format!(
            "| `{}` | {} | {} | {} | {} | {} | {} |",
            row.name,
            row.status.name(),
            row.since,
            row.flags,
            row.arity,
            row.compared_cases,
            row.note,
        ));
    }
    push("");
    push("## Documented deviations (the allowlist, verbatim)");
    push("");
    push("Each entry is a `SkipDiff` justification from the corpus: the candidate must");
    push("still produce well-formed RESP for these cases, but the bytes differ from the");
    push("oracle by design.");
    push("");
    for row in &rows {
        if row.deviations.is_empty() {
            continue;
        }
        push(&format!("### `{}`", row.name));
        push("");
        for why in &row.deviations {
            push(&format!("- {why}"));
        }
        push("");
    }
    push("## Absent (owner milestone)");
    push("");
    push("| Family | Arrives |");
    push("|---|---|");
    for (family, owner) in ABSENT {
        push(&format!("| {family} | {owner} |"));
    }
    push("");
    push("---");
    push("");
    push("Master plan §14 owns the staging policy; milestone plans own acceptance");
    push("criteria. Performance claims live in the claim ledger, never here (L10).");
    out
}
