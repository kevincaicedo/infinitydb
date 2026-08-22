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

use crate::json_oracle::{
    DEVIATIONS as JSON_DEVIATIONS, JSON_CASES, Protocol as JsonProtocol, REDIS_STACK_DIGEST,
    REDIS_STACK_IMAGE, REDISJSON_MODULE_VERSION,
};
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
    d(
        "QUIT",
        Status::Partial,
        "M1",
        "replies +OK and closes the connection (Redis-equivalent); not in the byte-diff corpus because closing tears down the shared oracle connection — covered by a unit test and the client-smoke suite",
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
    d(
        "INF.NS",
        Status::Extension,
        "M1",
        "namespace registry (M2 durability seam; M4-S19 adds SET + the ADR-0062 tiering keys; \
         M4-S26 lifts the D8 `USE` refusal — the string family, `SCAN`, and `DBSIZE` serve \
         tiered namespaces; two extension error classes are live on their writes: `DISKFULL …` \
         (ADR-0063 — disk budget or device full; new-tier-byte placements only) and \
         `STALLED tiered write timed out waiting for flush progress (TAIL-STALL-TIMEOUT)` \
         (ADR-0053 D4 — retryable). Deviations on tiered namespaces: no expiry (the `EXPIRE` \
         family + `SET` expiry options refuse typed; `TTL` = -1 for live keys), non-string \
         families refuse typed, multi-key ops resolve sequentially. M4-S27 (ADR-0068): \
         `MAXMEMORY`/`EVICTION` on named *memory* namespaces are enforced and Hot via `SET` \
         (`inherit`/`0` reset them); a namespace with its own budget answers the Redis-exact \
         OOM error scoped to that namespace and reclaims only its own keys; durable and \
         tiered namespaces refuse both keys typed (tiered budgets belong to `MEM-BUDGET`)",
    ),
    d(
        "INF.CKPT",
        Status::Extension,
        "M2",
        "checkpoint operator surface (M2-S20): [CELL k] [WAIT]; WAIT returns after the new \
         MANIFEST is durable — no fork, per-cell timing (ADR-0021)",
    ),
    d(
        "BGSAVE",
        Status::Partial,
        "M2",
        "maps onto INF.CKPT (fuzzy checkpoint, no fork, no RDB file); SCHEDULE accepted and \
         moot; reply byte-identical; memory-only nodes answer a documented error",
    ),
    d(
        "LASTSAVE",
        Status::Partial,
        "M2",
        "unix seconds of the newest durable MANIFEST publication; 0 before the first \
         (Redis reports process-start time); loading flag docs-derived, not capture-verified",
    ),
    d("INF.TAKE", Status::Internal, "M1", "cross-cell RENAME/COPY program primitive"),
    d("INF.PEEK", Status::Internal, "M1", "cross-cell COPY program primitive"),
    // ---- M3-S11/S12 · `JSON.*` (ADR-0041). S21 supplies the pinned
    // RedisJSON RESP2/RESP3 byte corpus and explicit deviation allowlist;
    // S22 (2026-07-16) audited every row: `full` requires byte-compared
    // cases under both protocols, zero attributable allowlist entries, and
    // no semantic difference (error-text wording is not representational —
    // module vocabulary above). Notes whose RedisJSON arm the corpus left
    // unpinned were settled against the pinned oracle:
    // `.artifacts/m3/s22-20260716/oracle-probes.txt`.
    d(
        "JSON.SET",
        Status::Partial,
        "M3",
        "S21 corpus exact except parser-specific malformed-input text; root sets preserve TTL \
         (as RedisJSON — S22 probe); durable writes use M3-S17 document records",
    ),
    d(
        "JSON.GET",
        Status::Partial,
        "M3",
        "S21 corpus exact except documented large-exponent f64 text and module-specific \
         WRONGTYPE wording; INDENT/NEWLINE/SPACE covered; path match sets capped by \
         doc-max-path-matches",
    ),
    d(
        "JSON.MGET",
        Status::Partial,
        "M3",
        "S21 corpus exact; per-key atomicity only — no cross-cell snapshot (each cell serves \
         its key at its own serve time)",
    ),
    d(
        "JSON.DEL",
        Status::Partial,
        "M3",
        "recursive-overlap result count differs from RedisJSON while post-state is identical",
    ),
    d(
        "JSON.FORGET",
        Status::Partial,
        "M3",
        "alias of JSON.DEL — inherits its recursive-overlap count difference (S22 probe: the \
         oracle reports 2 where InfinityDB reports 3 raw matches); own S21 corpus case exact",
    ),
    d(
        "JSON.TYPE",
        Status::Full,
        "M3",
        "RESP2/RESP3 type-name vocabulary and frames are exact in the S21 corpus",
    ),
    d(
        "JSON.NUMINCRBY",
        Status::Partial,
        "M3",
        "i64 preserved exactly; i64 overflow errors atomically where the pinned RedisJSON \
         wraps to i64::MIN (S22 probe); non-finite results error on both; value echoes share \
         JSON.GET's large-exponent f64 deviation",
    ),
    d(
        "JSON.NUMMULTBY",
        Status::Partial,
        "M3",
        "same numeric model and deviation classes as JSON.NUMINCRBY; S21 RESP2/RESP3 corpus \
         exact",
    ),
    d(
        "JSON.STRAPPEND",
        Status::Full,
        "M3",
        "lengths reported in bytes and the implicit legacy root path match the pinned oracle \
         (S21 corpus + S22 probes)",
    ),
    d(
        "JSON.STRLEN",
        Status::Full,
        "M3",
        "lengths reported in bytes, matching the pinned oracle (S21 corpus + S22 multibyte \
         probe)",
    ),
    d(
        "JSON.TOGGLE",
        Status::Full,
        "M3",
        "S21 RESP2/RESP3 corpus exact; non-boolean skip (modern) / error (legacy) split \
         matches the pinned oracle (S22 probe)",
    ),
    d(
        "JSON.CLEAR",
        Status::Full,
        "M3",
        "already-empty containers and zero numbers skip (uncounted), matching the pinned \
         oracle (S21 corpus + S22 probe)",
    ),
    // ---- M3-S13/S14 · array + object ops, MERGE (ADR-0042). The same S21
    // corpus/allowlist rule applies.
    d(
        "JSON.ARRAPPEND",
        Status::Partial,
        "M3",
        "S21 corpus exact; three-argument form appends one value at the legacy root, a form \
         the pinned RedisJSON rejects with an arity error (S22 probe)",
    ),
    d(
        "JSON.ARRINSERT",
        Status::Partial,
        "M3",
        "resolved index outside 0..=len aborts the whole command atomically (§3.4 R4); \
         RedisJSON can mutate an earlier match before a later index error",
    ),
    d(
        "JSON.ARRINDEX",
        Status::Partial,
        "M3",
        "scalar needles only (container needles rejected — ADR-0042 D3); mixed-width numbers \
         compare numerically; S21 corpus exact",
    ),
    d(
        "JSON.ARRLEN",
        Status::Partial,
        "M3",
        "S21 corpus exact except module-specific WRONGTYPE error text",
    ),
    d(
        "JSON.ARRPOP",
        Status::Partial,
        "M3",
        "out-of-range clamps and empty-array null match the pinned oracle (S22 probes); the \
         popped-value text shares JSON.GET's large-exponent f64 deviation (the oracle echoes \
         a 3e72 literal as 2.9999999999999996e72); S21 corpus exact",
    ),
    d(
        "JSON.ARRTRIM",
        Status::Partial,
        "M3",
        "inclusive window and out-of-range clamps; overlapping mixed-type reply/error shape \
         differs from RedisJSON with the same post-state",
    ),
    d(
        "JSON.OBJKEYS",
        Status::Full,
        "M3",
        "keys in insertion order, as the pinned RedisJSON returns them (the only order the \
         format has — ADR-0036); S21 corpus exact",
    ),
    d("JSON.OBJLEN", Status::Full, "M3", "S21 RESP2/RESP3 corpus exact"),
    d(
        "JSON.MERGE",
        Status::Partial,
        "M3",
        "RFC 7386 at the selected value; null members inside object patches delete keys, while \
         a path-targeted null is literal (ADR-0042 D6); retaining overlaps use one immutable \
         snapshot rather than RedisJSON cascade semantics; missing keys create at the root only",
    ),
    d(
        "JSON.DEBUG",
        Status::Partial,
        "M3",
        "MEMORY reports InfinityDB-attributed record + external document bytes; missing-key \
         and allocator-specific RedisJSON parity are intentionally not claimed",
    ),
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
    pub deviations: Vec<String>,
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
        let mut deviations: Vec<String> = Vec::new();
        for case in MATRIX {
            if !case.argv[0].eq_ignore_ascii_case(meta.name) {
                continue;
            }
            if case.check.compared() {
                compared_cases += 1;
            } else if let Check::SkipDiff(why) = case.check
                && !deviations.iter().any(|deviation| deviation == why)
            {
                deviations.push(why.to_string());
            }
        }
        compared_cases +=
            JSON_CASES.iter().filter(|case| case.argv[0].eq_ignore_ascii_case(meta.name)).count()
                * JsonProtocol::ALL.len();
        deviations.extend(
            JSON_DEVIATIONS
                .iter()
                .filter(|deviation| deviation.command.eq_ignore_ascii_case(meta.name))
                .map(|deviation| {
                    format!(
                        "RedisJSON {} `{}`: {}",
                        deviation.protocol.name(),
                        deviation.case_id,
                        deviation.justification
                    )
                }),
        );
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
// Owners follow the ADR-0023 documents-first train (master plan §21):
// the pre-reorder numbering this table carried went stale when `JSON.*`
// (old M6) shipped at M3 — caught by the M3-S13/S15 review.
pub static ABSENT: &[(&str, &str)] = &[
    ("Persistence admin (SAVE, …)", "M9 — RDB import/export"),
    ("Hashes, lists, sets, zsets, bitmaps, bitfield, HyperLogLog", "M5 — data types"),
    ("Keyspace notifications, SLOWLOG, MONITOR, sharded pub/sub (SSUBSCRIBE/SPUBLISH)", "M5"),
    ("Connection control (RESET)", "M6 (RESET pairs with transaction state)"),
    ("MULTI / EXEC / WATCH / DISCARD, EVAL / Lua, FUNCTION, WAIT", "M6 — transactions"),
    ("Streams (X*), AUTH / TLS / ACL, CLIENT TRACKING", "M7"),
    ("JSONPath filter expressions `?(@…)`, secondary indexes, query engine", "M4.5 — ADR-0024"),
    ("`JSON.RESP`", "Never — deprecated upstream; declared absent per the M3 plan anti-goals"),
    ("Vector sets", "M8"),
    ("Replication / cluster admin", "M9+"),
];

/// Renders the full `docs/compat-matrix.md` artifact.
pub fn render() -> String {
    let rows = rows();
    let core_skipped = MATRIX.iter().filter(|c| !c.check.compared()).count();
    let compared = MATRIX.len() - core_skipped + JSON_CASES.len() * JsonProtocol::ALL.len();
    let deviations = core_skipped + JSON_DEVIATIONS.len();
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
    push("Oracles: **Redis 8.0.5** for the core surface; RedisJSON uses");
    push(&format!(
        "**{REDIS_STACK_IMAGE}@{REDIS_STACK_DIGEST}** with ReJSON/{REDISJSON_MODULE_VERSION}."
    ));
    push("Every covered behavior is byte-diffed under its declared protocol; any new or");
    push("stale deviation fails CI (L8 — honesty is total).");
    push("");
    push(&format!(
        "**Corpus:** {compared} byte-compared executions · {deviations} documented deviations · 0 tolerated failures.",
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
    push("Entries come verbatim from the core `SkipDiff` corpus or the protocol-keyed");
    push("RedisJSON allowlist. The candidate still produces well-formed RESP, but the");
    push("bytes or post-state differ by an understood, reviewed design decision.");
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
    push("## Durable write backpressure (extension surface, L8 note — M4.5-S27, ADR-0083)");
    push("");
    push("Durable namespaces under log-staging pressure **pace** (the reply is");
    push("delayed while the command stays suspended) instead of erroring — every");
    push("path, local and fabric-routed (ADR-0083 D1). Redis has no equivalent");
    push("surface (no durable log). Mainstream Redis clients do not auto-retry");
    push("`-BUSY`, which is why refusal is not the design response to pressure;");
    push("the remaining typed `-BUSY` emitters are the document exact late");
    push("admission and the tiered cold-read queue cap, both counted");
    push("(`log_admission_busy`) and expected ≈ 0 — a climbing rate is a finding,");
    push("not designed behaviour. A durable write whose record can never fit the");
    push("staging domain refuses up front with typed");
    push("`ERR write exceeds durable log staging capacity` — non-retryable by");
    push("design (ADR-0083 D2; retrying it is a livelock). That bound is the");
    push("staging buffer minus the frame framing: **4 MiB − 56 B per record at the");
    push("default `--log-staging-mib 4`** (2 MiB − 56 B under the measured");
    push("`--frames-in-flight 3 --log-staging-mib 2` arm, ADR-0087 D1) — below the");
    push("16 MiB − 1 record-format cap memory namespaces honour in full; tiered");
    push("namespaces route values at or above `BLOB-THRESHOLD` out of line and are");
    push("not bound by it. The barrier class (`--barrier-class`, ADR-0086) and the");
    push("frame pipeline depth do not move the bound; only the staging buffer size does.");
    push("");
    push("`FSYNC everysec` namespaces ack on apply and fsync on the 1 s tick — the");
    push("`appendfsync everysec` loss window (≤ 1 s on power loss). With the frame-fill");
    push("policy on (`--fill-window-us N`, M4.5-S39a; off by default until its A/B), a");
    push("barrier-less frame on an aligned segment may hold un-sealed for up to `N` µs");
    push("(design point 1 000) before it reaches the device: the **process-crash**");
    push("exposure of `everysec` records is then ≤ `N` µs of writes per cell, where");
    push("Redis's AOF buffer reaches the page cache every event loop. The power-loss");
    push("window is unchanged; `always` acks are never held (their frames carry the");
    push("barrier the ack waits on).");
    push("");
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
