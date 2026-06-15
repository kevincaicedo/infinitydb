//! The benchmark workload catalog — gated to the M1 command surface.
//!
//! Load-bearing correctness constraint: redis-benchmark's default `-t` set and
//! a naive command list would fire `lpush/sadd/hset/zadd`, none of which M1
//! implements (collections are M3); that would benchmark error replies. Every
//! workload here names string-family commands only — expressed as a memtier
//! SET:GET ratio, an arbitrary memtier `--command`, or a special memory pass.

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
            in_all: true,
            redisbench_test: Some("set"),
            about: "write-only SET storm",
        },
        Workload {
            name: "mixed",
            kind: Ratio { ratio: "1:10", expiry: None },
            needs_fill: false,
            in_all: true,
            redisbench_test: None, // redis-benchmark has no SET:GET ratio mode
            about: "canonical cache mix, 1 SET per 10 GET",
        },
        Workload {
            name: "get",
            kind: Ratio { ratio: "0:1", expiry: None },
            needs_fill: true,
            in_all: true,
            redisbench_test: Some("get"),
            about: "read-only GET after a populate pass",
        },
        Workload {
            name: "incr",
            kind: Command { command: "INCR k:__key__" },
            needs_fill: false,
            in_all: true,
            redisbench_test: Some("incr"),
            about: "counter INCR storm",
        },
        Workload {
            name: "mset",
            kind: Command { command: "MSET k:__key__ __data__" },
            needs_fill: false,
            in_all: true,
            redisbench_test: None, // redis-benchmark MSET writes 10 keys/op — not comparable
            about: "MSET single-pair writes",
        },
        Workload {
            name: "ttl",
            kind: Ratio { ratio: "1:10", expiry: Some("1-5") },
            needs_fill: false,
            in_all: true,
            redisbench_test: None, // redis-benchmark cannot attach a TTL
            about: "TTL-heavy mix (every SET carries a 1-5s expiry)",
        },
        Workload {
            name: "memory",
            kind: Memory,
            needs_fill: false,
            in_all: true,
            redisbench_test: None,
            about: "bytes/key attribution: fill, DBSIZE, RSS delta",
        },
        Workload {
            name: "eviction",
            kind: Ratio { ratio: "1:0", expiry: None },
            needs_fill: false,
            in_all: false, // opt-in: only meaningful with --maxmemory-mb
            redisbench_test: Some("set"),
            about: "write storm vs --maxmemory-mb (allkeys-lru); pass --maxmemory-mb",
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
        let w = *catalog().iter().find(|w| w.name == name).ok_or_else(|| {
            let known: Vec<&str> = catalog().iter().map(|w| w.name).collect();
            format!("unknown workload `{name}` (known: {}, all)", known.join(", "))
        })?;
        push(w, &mut out);
    }
    if out.is_empty() {
        return Err("no workloads selected".into());
    }
    Ok(out)
}
