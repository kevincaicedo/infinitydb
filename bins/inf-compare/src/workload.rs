//! The benchmark workload catalog — the M1 string surface plus the M3
//! `json` group (ADR-0025 D3: `JSON.SET`/`JSON.GET` memtier `--command`
//! lanes vs the pinned redis-stack image).
//!
//! Load-bearing correctness constraint: redis-benchmark's default `-t` set and
//! a naive command list would fire `lpush/sadd/hset/zadd`, none of which M1
//! implements (collections are M3); that would benchmark error replies. Every
//! workload here names string-family commands only — expressed as a memtier
//! SET:GET ratio, an arbitrary memtier `--command`, or a special memory pass —
//! except the opt-in `json` group, which is skipped (with a report note) on
//! engines without a `JSON.*` surface.

/// The JSON lane document: a gate-shape-like ~1 KiB minified object with a
/// depth-4 child chain for the path-read lane. A `macro_rules` literal so
/// it composes into the memtier `--command` string with `concat!`. No
/// spaces (memtier splits its command string on them) and no escapes.
macro_rules! json_doc_1k {
    () => {
        concat!(
            "{\"kind\":\"gate\",\"id\":428711,\"score\":0.815,",
            "\"child\":{\"id\":1,\"name\":\"alpha\",\"active\":true,\"score\":0.815,",
            "\"child\":{\"id\":2,\"name\":\"vector\",\"active\":false,\"score\":0.815,",
            "\"child\":{\"id\":3,\"name\":\"engine\",\"active\":true,\"score\":0.815,",
            "\"child\":{\"id\":4,\"name\":\"stream\",\"active\":true,\"score\":0.815,",
            "\"note\":\"leaf\"}}}},\"tags\":[\"optics\",\"catalog\",\"durable\"],\"pad\":\"",
            "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789",
            "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789",
            "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789",
            "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789",
            "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789",
            "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789",
            "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789",
            "abcdefghijklmnopqrstuvwxyz0123456789abcdefghijklmnopqrstuvwxyz0123456789",
            "abcdefghijklmnopqrstuvwxyz0123456789\"}"
        )
    };
}

/// The lane document, standalone (the JSON preload writes it per key).
pub const JSON_DOC_1K: &str = json_doc_1k!();

/// How a workload is driven.
#[derive(Clone, Copy, Debug)]
pub enum Kind {
    /// memtier `--ratio` (SET:GET), with an optional `--expiry-range` (seconds).
    Ratio { ratio: &'static str, expiry: Option<&'static str> },
    /// memtier `--command` with `__key__`/`__data__` placeholders.
    Command { command: &'static str },
    /// Special: bytes/key attribution via fill + DBSIZE + RSS delta (no latency).
    Memory,
}

#[derive(Clone, Copy, Debug)]
pub struct Workload {
    pub name: &'static str,
    pub kind: Kind,
    /// Populate the keyspace before the timed run (GET-heavy rows).
    pub needs_fill: bool,
    /// Lane drives `JSON.*`: the fill is a document preload, and engines
    /// without a JSON surface skip the lane with a report note.
    pub requires_json: bool,
    /// Included when `--workload all` is selected.
    pub in_all: bool,
    /// redis-benchmark `-t` test for the cross-check, if one is apples-to-apples.
    pub redisbench_test: Option<&'static str>,
    pub about: &'static str,
}

pub fn catalog() -> &'static [Workload] {
    use Kind::{Command, Memory, Ratio};
    &[
        Workload {
            name: "set",
            kind: Ratio { ratio: "1:0", expiry: None },
            needs_fill: false,
            requires_json: false,
            in_all: true,
            redisbench_test: Some("set"),
            about: "write-only SET storm",
        },
        Workload {
            name: "mixed",
            kind: Ratio { ratio: "1:10", expiry: None },
            needs_fill: false,
            requires_json: false,
            in_all: true,
            redisbench_test: None, // redis-benchmark has no SET:GET ratio mode
            about: "canonical cache mix, 1 SET per 10 GET",
        },
        Workload {
            name: "get",
            kind: Ratio { ratio: "0:1", expiry: None },
            needs_fill: true,
            requires_json: false,
            in_all: true,
            redisbench_test: Some("get"),
            about: "read-only GET after a populate pass",
        },
        Workload {
            name: "incr",
            kind: Command { command: "INCR k:__key__" },
            needs_fill: false,
            requires_json: false,
            in_all: true,
            redisbench_test: Some("incr"),
            about: "counter INCR storm",
        },
        Workload {
            name: "mset",
            kind: Command { command: "MSET k:__key__ __data__" },
            needs_fill: false,
            requires_json: false,
            in_all: true,
            redisbench_test: None, // redis-benchmark MSET writes 10 keys/op — not comparable
            about: "MSET single-pair writes",
        },
        Workload {
            name: "ttl",
            kind: Ratio { ratio: "1:10", expiry: Some("1-5") },
            needs_fill: false,
            requires_json: false,
            in_all: true,
            redisbench_test: None, // redis-benchmark cannot attach a TTL
            about: "TTL-heavy mix (every SET carries a 1-5s expiry)",
        },
        Workload {
            name: "memory",
            kind: Memory,
            needs_fill: false,
            requires_json: false,
            in_all: true,
            redisbench_test: None,
            about: "bytes/key attribution: fill, DBSIZE, RSS delta",
        },
        Workload {
            name: "eviction",
            kind: Ratio { ratio: "1:0", expiry: None },
            needs_fill: false,
            requires_json: false,
            in_all: false, // opt-in: only meaningful with --maxmemory-mb
            redisbench_test: Some("set"),
            about: "write storm vs --maxmemory-mb (allkeys-lru); pass --maxmemory-mb",
        },
        Workload {
            name: "json-set",
            // The document rides single-quoted: memtier's arbitrary-command
            // parser treats a quoted region as one token (the JSON braces
            // otherwise fail its parse).
            kind: Command { command: concat!("JSON.SET k:__key__ $ '", json_doc_1k!(), "'") },
            needs_fill: false,
            requires_json: true,
            in_all: false, // opt-in via `json` (M3 surface; skipped on plain redis)
            redisbench_test: None, // redis-benchmark has no JSON tests
            about: "JSON.SET root writes, ~1 KiB gate-shape document (M3 write gate's cross-check)",
        },
        Workload {
            name: "json-get",
            kind: Command { command: "JSON.GET k:__key__ $.child.child.child.child.id" },
            needs_fill: true,
            requires_json: true,
            in_all: false, // opt-in via `json`
            redisbench_test: None,
            about: "JSON.GET depth-4 path reads after a document preload (M3 read gate's cross-check)",
        },
    ]
}

/// Resolve a `--workload` value: a single name, `all`, or a comma list like
/// `set,get,memory`. `all` expands to every `in_all` workload. Duplicates are
/// dropped, order preserved.
pub fn select(spec: &str) -> Result<Vec<Workload>, String> {
    let mut out: Vec<Workload> = Vec::new();
    let push = |w: Workload, out: &mut Vec<Workload>| {
        if !out.iter().any(|x| x.name == w.name) {
            out.push(w);
        }
    };
    for name in spec.split(',').map(str::trim).filter(|s| !s.is_empty()) {
        if name == "all" {
            for w in catalog().iter().copied().filter(|w| w.in_all) {
                push(w, &mut out);
            }
            continue;
        }
        if name == "json" {
            for w in catalog().iter().copied().filter(|w| w.requires_json) {
                push(w, &mut out);
            }
            continue;
        }
        let w = *catalog().iter().find(|w| w.name == name).ok_or_else(|| {
            let known: Vec<&str> = catalog().iter().map(|w| w.name).collect();
            format!("unknown workload `{name}` (known: {}, all, json)", known.join(", "))
        })?;
        push(w, &mut out);
    }
    if out.is_empty() {
        return Err("no workloads selected".into());
    }
    Ok(out)
}
