//! Command execution v0 (M0-S15): the ~26-command string surface, mapping
//! parsed argv → `inf-store` ops → RESP2/RESP3 reply bytes.
//!
//! Every command enters through the `inf-wire` registry (lookup → arity →
//! key spec); no handler bypasses the metadata path (kernel rule). Reply
//! bytes aim to be byte-identical to Redis 7.x/8.x — the compat-diff
//! harness (`tests/compat`) is the oracle; documented deviations:
//! `HELLO`/`INFO`/`COMMAND` payloads (identity fields, registry size),
//! `SET ... EXAT/PXAT` (wall-clock timebase lands with the node clock),
//! and keys > 255 B / values > 16 MiB − 1 (record format v0 bounds).

use core::cell::Cell;
use std::rc::Rc;

use inf_foundation::time::Nanos;
use inf_store::{CellStore, ExpireCond, OpError, SetCond, SetExpire, SetOptions, SetOutcome, Ttl};
use inf_wire::{
    ArgvRef, CommandId, ConnParser, Parsed, ParserLimits, Protocol, RespWriter, arity_ok, lookup,
};

/// Node-level stats surfaced through `INFO` (M0-S19): the frozen tripwire
/// snapshot plus the memory-attribution domains the store can't see. The
/// node assembly (or sim harness) refreshes these; a default-constructed
/// `ConnCx` carries an all-zero instance, so tests and the compat candidate
/// need no wiring.
#[derive(Default, Debug)]
pub struct NodeInfo {
    /// Frozen order: sqes_per_submit, cqes_per_reap, cmds_per_iter,
    /// fabric_msgs_per_batch (each ×1000), loop_iter_p999_us.
    pub tripwires: Cell<[u64; 5]>,
    /// Raw lifetime counters (submits, sqes, cqes, iterations, commands,
    /// fabric_msgs) — scrapers diff two snapshots for under-load ratios.
    pub raw_counters: Cell<[u64; 6]>,
    pub wire_buffers_bytes: Cell<u64>,
    pub conn_state_bytes: Cell<u64>,
    pub connections: Cell<u64>,
    pub recv_dropped: Cell<u64>,
    pub fabric_rtt_p50_ns: Cell<u64>,
    pub cell: Cell<u16>,
    pub cells: Cell<u16>,
}

/// Per-connection execution state (protocol negotiated via `HELLO`).
#[derive(Debug)]
pub struct ConnCx {
    pub proto: Protocol,
    pub id: u64,
    /// Shared node stats for `INFO` (zeroed default outside a node).
    pub node: Rc<NodeInfo>,
}

impl Default for ConnCx {
    fn default() -> ConnCx {
        ConnCx { proto: Protocol::Resp2, id: 1, node: Rc::new(NodeInfo::default()) }
    }
}

/// Executes one parsed command against `store`, appending the reply to
/// `out`. `now` is injected (L7) — same clock the store's TTLs live on.
pub fn execute(
    argv: &ArgvRef<'_>,
    store: &mut CellStore,
    cx: &mut ConnCx,
    now: Nanos,
    out: &mut Vec<u8>,
) {
    let mut w = RespWriter::new(out, cx.proto);
    let Some(meta) = lookup(argv.arg(0)) else {
        return unknown_command(argv, &mut w);
    };
    if !arity_ok(meta, argv.len()) {
        return w.error(&format!(
            "ERR wrong number of arguments for '{}' command",
            meta.name.to_ascii_lowercase()
        ));
    }
    match meta.id {
        CommandId::Ping => {
            if argv.len() == 1 {
                w.simple("PONG");
            } else if argv.len() == 2 {
                w.bulk(argv.arg(1));
            } else {
                w.error("ERR wrong number of arguments for 'ping' command");
            }
        }
        CommandId::Echo => w.bulk(argv.arg(1)),
        CommandId::Hello => hello(argv, cx, out),
        CommandId::Get => match store.get(argv.arg(1), now) {
            Some(value) => w.bulk(value),
            None => w.null(),
        },
        CommandId::Set => set(argv, store, now, &mut w),
        CommandId::Setnx => {
            let opts = SetOptions { cond: SetCond::IfAbsent, ..Default::default() };
            match store.set(argv.arg(1), argv.arg(2), opts, now) {
                Ok(SetOutcome::Applied { .. }) => w.int(1),
                Ok(SetOutcome::Skipped { .. }) => w.int(0),
                Err(e) => op_error(e, &mut w),
            }
        }
        CommandId::Setex | CommandId::Psetex => {
            let unit_ms = if meta.id == CommandId::Setex { 1000 } else { 1 };
            let Ok(ttl) = parse_i64(argv.arg(2)) else {
                return w.error("ERR value is not an integer or out of range");
            };
            let Some(at) = expire_deadline(now, ttl, unit_ms) else {
                return w.error(&format!(
                    "ERR invalid expire time in '{}' command",
                    meta.name.to_ascii_lowercase()
                ));
            };
            let opts = SetOptions { expire: SetExpire::At(at), ..Default::default() };
            match store.set(argv.arg(1), argv.arg(3), opts, now) {
                Ok(_) => w.simple("OK"),
                Err(e) => op_error(e, &mut w),
            }
        }
        CommandId::Getset => {
            let opts = SetOptions { get_old: true, ..Default::default() };
            match store.set(argv.arg(1), argv.arg(2), opts, now) {
                Ok(SetOutcome::Applied { old } | SetOutcome::Skipped { old }) => match old {
                    Some(value) => w.bulk(&value),
                    None => w.null(),
                },
                Err(e) => op_error(e, &mut w),
            }
        }
        CommandId::Getdel => match store.getdel(argv.arg(1), now) {
            Some(value) => w.bulk(&value),
            None => w.null(),
        },
        CommandId::Del => {
            let mut removed = 0;
            for i in 1..argv.len() {
                removed += i64::from(store.del(argv.arg(i), now));
            }
            w.int(removed);
        }
        CommandId::Exists => {
            let mut found = 0;
            for i in 1..argv.len() {
                found += i64::from(store.exists(argv.arg(i), now));
            }
            w.int(found);
        }
        CommandId::Type => match store.type_of(argv.arg(1), now) {
            Some(_) => w.simple("string"),
            None => w.simple("none"),
        },
        CommandId::Incr => incr(store, argv.arg(1), 1, now, &mut w),
        CommandId::Decr => incr(store, argv.arg(1), -1, now, &mut w),
        CommandId::IncrBy => {
            let Ok(delta) = parse_i64(argv.arg(2)) else {
                return w.error("ERR value is not an integer or out of range");
            };
            incr(store, argv.arg(1), delta, now, &mut w);
        }
        CommandId::DecrBy => {
            let Ok(delta) = parse_i64(argv.arg(2)) else {
                return w.error("ERR value is not an integer or out of range");
            };
            let Some(delta) = delta.checked_neg() else {
                return w.error("ERR decrement would overflow");
            };
            incr(store, argv.arg(1), delta, now, &mut w);
        }
        CommandId::Append => match store.append(argv.arg(1), argv.arg(2), now) {
            Ok(len) => w.int(len as i64),
            Err(e) => op_error(e, &mut w),
        },
        CommandId::Strlen => w.int(store.strlen(argv.arg(1), now) as i64),
        CommandId::Expire | CommandId::Pexpire => {
            let unit_ms = if meta.id == CommandId::Expire { 1000 } else { 1 };
            expire(argv, store, now, unit_ms, meta.name, &mut w);
        }
        CommandId::Ttl | CommandId::Pttl => {
            let value = match store.ttl(argv.arg(1), now) {
                Ttl::Missing => -2,
                Ttl::NoExpiry => -1,
                // Redis rounds TTL to the nearest second ((ms + 500) / 1000).
                Ttl::Ms(ms) => {
                    if meta.id == CommandId::Ttl {
                        ((ms + 500) / 1000) as i64
                    } else {
                        ms as i64
                    }
                }
            };
            w.int(value);
        }
        CommandId::Persist => {
            let removed = store.expire(argv.arg(1), None, ExpireCond::Always, now);
            w.int(i64::from(removed));
        }
        CommandId::Info => info(store, &cx.node, &mut w),
        CommandId::Command => command_introspection(argv, &mut w),
    }
}

/// Executes a command from owned argument slices — the queued-behind-async
/// and remote-`Apply` paths. Re-encodes to RESP and re-parses: `ArgvRef` is
/// parser-internal by design (borrowed offsets over the wire buffer), so the
/// slow paths pay one copy instead of widening the fast-path type. M1 may
/// add an owned-argv entry point if this ever shows up in a profile.
pub fn execute_slices(
    argv: &[&[u8]],
    store: &mut CellStore,
    cx: &mut ConnCx,
    now: Nanos,
    out: &mut Vec<u8>,
) {
    debug_assert!(!argv.is_empty(), "empty argv is a caller bug");
    let mut wire = Vec::with_capacity(32 + argv.iter().map(|a| a.len() + 16).sum::<usize>());
    wire.extend_from_slice(format!("*{}\r\n", argv.len()).as_bytes());
    for arg in argv {
        wire.extend_from_slice(format!("${}\r\n", arg.len()).as_bytes());
        wire.extend_from_slice(arg);
        wire.extend_from_slice(b"\r\n");
    }
    let mut parser = ConnParser::new(ParserLimits::default());
    let mut iter = parser.feed(&wire);
    match iter.next() {
        Some(Parsed::Command(parsed)) => execute(&parsed, store, cx, now, out),
        other => {
            // Only reachable if an argument exceeds the parser caps.
            let _ = other;
            RespWriter::new(out, cx.proto).error("ERR argument exceeds protocol limits");
        }
    }
}

// ---- SET ---------------------------------------------------------------------

fn set(argv: &ArgvRef<'_>, store: &mut CellStore, now: Nanos, w: &mut RespWriter<'_>) {
    let mut opts = SetOptions::default();
    let mut have_cond = false;
    let mut have_expire = false;
    let mut i = 3;
    while i < argv.len() {
        let opt = argv.arg(i);
        if opt.eq_ignore_ascii_case(b"NX") || opt.eq_ignore_ascii_case(b"XX") {
            if have_cond {
                return w.error("ERR syntax error");
            }
            have_cond = true;
            opts.cond = if opt.eq_ignore_ascii_case(b"NX") {
                SetCond::IfAbsent
            } else {
                SetCond::IfPresent
            };
        } else if opt.eq_ignore_ascii_case(b"GET") {
            opts.get_old = true;
        } else if opt.eq_ignore_ascii_case(b"KEEPTTL") {
            if have_expire {
                return w.error("ERR syntax error");
            }
            have_expire = true;
            opts.expire = SetExpire::Keep;
        } else if opt.eq_ignore_ascii_case(b"EX") || opt.eq_ignore_ascii_case(b"PX") {
            if have_expire || i + 1 >= argv.len() {
                return w.error("ERR syntax error");
            }
            have_expire = true;
            let Ok(ttl) = parse_i64(argv.arg(i + 1)) else {
                return w.error("ERR value is not an integer or out of range");
            };
            let unit_ms = if opt.eq_ignore_ascii_case(b"EX") { 1000 } else { 1 };
            let Some(at) = expire_deadline(now, ttl, unit_ms) else {
                return w.error("ERR invalid expire time in 'set' command");
            };
            opts.expire = SetExpire::At(at);
            i += 1;
        } else {
            // Includes EXAT/PXAT — recorded deviation (wall-clock timebase).
            return w.error("ERR syntax error");
        }
        i += 1;
    }
    match store.set(argv.arg(1), argv.arg(2), opts, now) {
        Ok(outcome) => {
            let (applied, old) = match outcome {
                SetOutcome::Applied { old } => (true, old),
                SetOutcome::Skipped { old } => (false, old),
            };
            if opts.get_old {
                match old {
                    Some(value) => w.bulk(&value),
                    None => w.null(),
                }
            } else if applied {
                w.simple("OK");
            } else {
                w.null();
            }
        }
        Err(e) => op_error(e, w),
    }
}

// ---- EXPIRE / PEXPIRE ----------------------------------------------------------

fn expire(
    argv: &ArgvRef<'_>,
    store: &mut CellStore,
    now: Nanos,
    unit_ms: i64,
    name: &str,
    w: &mut RespWriter<'_>,
) {
    let Ok(ttl) = parse_i64(argv.arg(2)) else {
        return w.error("ERR value is not an integer or out of range");
    };
    let (mut nx, mut xx, mut gt, mut lt) = (false, false, false, false);
    for i in 3..argv.len() {
        match argv.arg(i) {
            f if f.eq_ignore_ascii_case(b"NX") => nx = true,
            f if f.eq_ignore_ascii_case(b"XX") => xx = true,
            f if f.eq_ignore_ascii_case(b"GT") => gt = true,
            f if f.eq_ignore_ascii_case(b"LT") => lt = true,
            other => {
                return w
                    .error(&format!("ERR Unsupported option {}", String::from_utf8_lossy(other)));
            }
        }
    }
    if nx && (xx || gt || lt) {
        return w.error("ERR NX and XX, GT or LT options at the same time are not compatible");
    }
    if gt && lt {
        return w.error("ERR GT and LT options at the same time are not compatible");
    }
    // XX composes with GT/LT (the store cond is single-valued; an XX
    // pre-check on the TTL state reproduces the conjunction exactly).
    if xx && !matches!(store.ttl(argv.arg(1), now), Ttl::Ms(_)) {
        return w.int(0);
    }
    let cond = if nx {
        ExpireCond::IfNoExpiry
    } else if gt {
        ExpireCond::IfGreater
    } else if lt {
        ExpireCond::IfLess
    } else {
        ExpireCond::Always
    };
    // Deadline overflow is "invalid expire time".
    let Some(at) = expire_deadline_signed(now, ttl, unit_ms) else {
        return w
            .error(&format!("ERR invalid expire time in '{}' command", name.to_ascii_lowercase()));
    };
    let applied = store.expire(argv.arg(1), Some(at), cond, now);
    w.int(i64::from(applied));
}

// ---- INCR family ----------------------------------------------------------------

fn incr(store: &mut CellStore, key: &[u8], delta: i64, now: Nanos, w: &mut RespWriter<'_>) {
    match store.incr_by(key, delta, now) {
        Ok(value) => w.int(value),
        Err(e) => op_error(e, w),
    }
}

// ---- HELLO / INFO / COMMAND -------------------------------------------------------

fn hello(argv: &ArgvRef<'_>, cx: &mut ConnCx, out: &mut Vec<u8>) {
    let mut requested = cx.proto;
    if argv.len() >= 2 {
        match parse_i64(argv.arg(1)) {
            Ok(2) => requested = Protocol::Resp2,
            Ok(3) => requested = Protocol::Resp3,
            _ => {
                let mut w = RespWriter::new(out, cx.proto);
                w.error("NOPROTO unsupported protocol version");
                return;
            }
        }
    }
    if argv.len() > 2 {
        let mut w = RespWriter::new(out, cx.proto);
        w.error("ERR syntax error in HELLO");
        return;
    }
    cx.proto = requested;
    let mut w = RespWriter::new(out, cx.proto);
    w.map_header(7);
    w.bulk(b"server");
    w.bulk(b"infinitydb");
    w.bulk(b"version");
    w.bulk(b"0.0.0-m0");
    w.bulk(b"proto");
    w.int(if cx.proto == Protocol::Resp3 { 3 } else { 2 });
    w.bulk(b"id");
    w.int(cx.id as i64);
    w.bulk(b"mode");
    w.bulk(b"standalone");
    w.bulk(b"role");
    w.bulk(b"master");
    w.bulk(b"modules");
    w.array_header(0);
}

fn info(store: &CellStore, node: &NodeInfo, w: &mut RespWriter<'_>) {
    use inf_foundation::tripwire as tw;
    let report = store.report();
    let [sqes, cqes, cmds, fabric, p999] = node.tripwires.get();
    let mut text = String::new();
    text.push_str("# Server\r\n");
    text.push_str("infinitydb_version:0.0.0-m0\r\n");
    text.push_str("redis_version:7.4.0-compat\r\n");
    text.push_str("redis_mode:standalone\r\n");
    text.push_str("arch_bits:64\r\n");
    text.push_str(&format!("cell:{}\r\ncells:{}\r\n", node.cell.get(), node.cells.get()));
    text.push_str(&format!("connections:{}\r\n", node.connections.get()));
    text.push_str("\r\n# Tripwires\r\n");
    text.push_str(&format!("{}:{}\r\n", tw::SQES_PER_SUBMIT, sqes));
    text.push_str(&format!("{}:{}\r\n", tw::CQES_PER_REAP, cqes));
    text.push_str(&format!("{}:{}\r\n", tw::CMDS_PER_ITER, cmds));
    text.push_str(&format!("{}:{}\r\n", tw::FABRIC_MSGS_PER_BATCH, fabric));
    text.push_str(&format!("{}:{}\r\n", tw::LOOP_ITER_P999_US, p999));
    text.push_str(&format!("fabric_rtt_p50_ns:{}\r\n", node.fabric_rtt_p50_ns.get()));
    text.push_str(&format!("recv_dropped:{}\r\n", node.recv_dropped.get()));
    let [submits, raw_sqes, raw_cqes, iters, commands, fabric_msgs] = node.raw_counters.get();
    text.push_str(&format!("raw_submits:{submits}\r\n"));
    text.push_str(&format!("raw_sqes:{raw_sqes}\r\n"));
    text.push_str(&format!("raw_cqes:{raw_cqes}\r\n"));
    text.push_str(&format!("raw_iterations:{iters}\r\n"));
    text.push_str(&format!("raw_commands:{commands}\r\n"));
    text.push_str(&format!("raw_fabric_msgs:{fabric_msgs}\r\n"));
    text.push_str("\r\n# Memory\r\n");
    text.push_str(&format!("{}:{}\r\n", tw::RECORDS_LIVE_BYTES, report.records_live_bytes));
    text.push_str(&format!("{}:{}\r\n", tw::RECORDS_SLACK_BYTES, report.records_slack_bytes));
    text.push_str(&format!("records_resident_bytes:{}\r\n", report.records_resident_bytes));
    text.push_str(&format!("{}:{}\r\n", tw::INDEX_BYTES, report.index_bytes));
    text.push_str(&format!("{}:{}\r\n", tw::WIRE_BUFFERS_BYTES, node.wire_buffers_bytes.get()));
    text.push_str(&format!("{}:{}\r\n", tw::CONN_STATE_BYTES, node.conn_state_bytes.get()));
    text.push_str(&format!("{}:{}\r\n", tw::PROCESS_RSS, process_rss_bytes()));
    text.push_str("\r\n# Keyspace\r\n");
    if !store.is_empty() {
        text.push_str(&format!("db0:keys={},expires=0,avg_ttl=0\r\n", store.len()));
    }
    w.verbatim(b"txt", text.as_bytes());
}

/// VmRSS from procfs (Linux); 0 where unavailable. INFO is a cold admin
/// path — reading procfs here is fine.
fn process_rss_bytes() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|s| {
            s.lines()
                .find(|l| l.starts_with("VmRSS:"))
                .and_then(|l| l.split_whitespace().nth(1).and_then(|kb| kb.parse::<u64>().ok()))
        })
        .map_or(0, |kb| kb * 1024)
}

fn command_introspection(argv: &ArgvRef<'_>, w: &mut RespWriter<'_>) {
    if argv.len() >= 2 && argv.arg(1).eq_ignore_ascii_case(b"COUNT") {
        w.int(inf_wire::COMMANDS.len() as i64);
        return;
    }
    if argv.len() > 1 {
        return w.error(&format!(
            "ERR Unknown subcommand or wrong number of arguments for '{}'. Try COMMAND HELP.",
            String::from_utf8_lossy(argv.arg(1))
        ));
    }
    w.array_header(inf_wire::COMMANDS.len());
    for meta in &inf_wire::COMMANDS {
        w.array_header(10);
        w.bulk(meta.name.to_ascii_lowercase().as_bytes());
        w.int(i64::from(meta.arity));
        w.array_header(0); // flags (minimal at M0)
        w.int(i64::from(meta.keys.first));
        w.int(i64::from(meta.keys.last));
        w.int(i64::from(meta.keys.step));
        w.array_header(0); // acl categories
        w.array_header(0); // tips
        w.array_header(0); // key specs
        w.array_header(0); // subcommands
    }
}

// ---- shared helpers ---------------------------------------------------------------

// Format pinned byte-exact against Redis 8.0.5 by the compat harness:
// `'arg1' 'arg2' ` — space-separated, trailing space, no parentheses.
fn unknown_command(argv: &ArgvRef<'_>, w: &mut RespWriter<'_>) {
    let mut text = format!(
        "ERR unknown command '{}', with args beginning with: ",
        String::from_utf8_lossy(argv.arg(0))
    );
    for i in 1..argv.len().min(21) {
        text.push_str(&format!("'{}' ", String::from_utf8_lossy(argv.arg(i))));
    }
    w.error(&text);
}

fn op_error(e: OpError, w: &mut RespWriter<'_>) {
    match e {
        OpError::NotInt => w.error("ERR value is not an integer or out of range"),
        OpError::Overflow => w.error("ERR increment or decrement would overflow"),
        OpError::OutOfMemory => w.error("OOM command not allowed when used memory > 'maxmemory'."),
        OpError::TooLarge => w.error("ERR key or value exceeds InfinityDB M0 record bounds"),
    }
}

/// Positive-TTL deadline for SET EX/PX and SETEX (must be > 0).
fn expire_deadline(now: Nanos, ttl: i64, unit_ms: i64) -> Option<Nanos> {
    if ttl <= 0 {
        return None;
    }
    let ms = ttl.checked_mul(unit_ms)?;
    let at = (now.0 / 1_000_000).checked_add_signed(ms)?;
    Some(Nanos(at.checked_mul(1_000_000)?))
}

/// EXPIRE deadline — negative TTLs are legal (delete-on-apply).
fn expire_deadline_signed(now: Nanos, ttl: i64, unit_ms: i64) -> Option<Nanos> {
    let ms = ttl.checked_mul(unit_ms)?;
    let at = (now.0 / 1_000_000).saturating_add_signed(ms);
    Some(Nanos(at.checked_mul(1_000_000)?))
}

/// Redis `string2ll`: optional sign, no leading zeros (and no `-0` —
/// oracle-pinned against Redis 8.0.5), no '+', i64 range.
fn parse_i64(bytes: &[u8]) -> Result<i64, ()> {
    if bytes.is_empty() || bytes.len() > 21 {
        return Err(());
    }
    let (neg, digits) = match bytes[0] {
        b'-' => (true, &bytes[1..]),
        _ => (false, bytes),
    };
    if digits.is_empty()
        || !digits.iter().all(u8::is_ascii_digit)
        || (digits[0] == b'0' && (digits.len() > 1 || neg))
    {
        return Err(());
    }
    let mut acc: i64 = 0;
    for &d in digits {
        acc = acc
            .checked_mul(10)
            .and_then(|a| {
                let v = i64::from(d - b'0');
                if neg { a.checked_sub(v) } else { a.checked_add(v) }
            })
            .ok_or(())?;
    }
    Ok(acc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use inf_store::StoreConfig;
    use inf_wire::{ConnParser, Parsed, ParserLimits};

    fn run(cx: &mut ConnCx, store: &mut CellStore, parts: &[&[u8]]) -> Vec<u8> {
        let mut wire = format!("*{}\r\n", parts.len()).into_bytes();
        for p in parts {
            wire.extend_from_slice(format!("${}\r\n", p.len()).as_bytes());
            wire.extend_from_slice(p);
            wire.extend_from_slice(b"\r\n");
        }
        let mut parser = ConnParser::new(ParserLimits::default());
        let mut iter = parser.feed(&wire);
        let Some(Parsed::Command(argv)) = iter.next() else { panic!("one command") };
        let mut out = Vec::new();
        execute(&argv, store, cx, Nanos(1), &mut out);
        out
    }

    #[test]
    fn hello_switches_protocol_and_rejects_unknown_versions() {
        let mut cx = ConnCx::default();
        let mut store = CellStore::new(StoreConfig::default());
        // RESP2 null is the bulk form.
        assert_eq!(run(&mut cx, &mut store, &[b"GET", b"missing"]), b"$-1\r\n");
        // Bad versions: NOPROTO, protocol unchanged.
        let reply = run(&mut cx, &mut store, &[b"HELLO", b"9"]);
        assert_eq!(reply, b"-NOPROTO unsupported protocol version\r\n");
        assert_eq!(cx.proto, Protocol::Resp2);
        let reply = run(&mut cx, &mut store, &[b"HELLO", b"abc"]);
        assert_eq!(reply, b"-NOPROTO unsupported protocol version\r\n");
        // HELLO 3 switches: the reply itself is a RESP3 map, and nulls
        // become the RESP3 `_` form afterwards.
        let reply = run(&mut cx, &mut store, &[b"HELLO", b"3"]);
        assert_eq!(cx.proto, Protocol::Resp3);
        assert!(reply.starts_with(b"%7\r\n"), "RESP3 map: {reply:?}");
        assert_eq!(run(&mut cx, &mut store, &[b"GET", b"missing"]), b"_\r\n");
        // And back down to RESP2.
        let reply = run(&mut cx, &mut store, &[b"HELLO", b"2"]);
        assert!(reply.starts_with(b"*14\r\n"), "RESP2 flat array: {reply:?}");
        assert_eq!(run(&mut cx, &mut store, &[b"GET", b"missing"]), b"$-1\r\n");
    }

    #[test]
    fn record_bound_deviation_is_a_typed_error() {
        let mut cx = ConnCx::default();
        let mut store = CellStore::new(StoreConfig::default());
        let long_key = vec![b'k'; 256];
        let reply = run(&mut cx, &mut store, &[b"SET", &long_key, b"v"]);
        assert!(reply.starts_with(b"-ERR key or value exceeds"), "{reply:?}");
    }
}
