//! Command execution (M0-S15 base + M1-E1 surface): the ~55-command
//! string/key/expiry/server surface, mapping parsed argv → `inf-store` ops →
//! RESP2/RESP3 reply bytes.
//!
//! Every command enters through the `inf-wire` registry (lookup → arity →
//! key spec); no handler bypasses the metadata path (kernel rule). Reply
//! bytes aim to be byte-identical to Redis 7.x/8.x — the compat-diff
//! harness (`tests/compat`) is the oracle; documented deviations:
//! `HELLO`/`INFO`/`COMMAND`/`CLIENT LIST`/`LOLWUT`/`DEBUG OBJECT` payloads,
//! keys > 255 B / values > 16 MiB − 1 / TTLs > ~34.8 y (record format v0
//! bounds), `SELECT` limited to db 0 until namespaces v1 (M1-E4),
//! `RANDOMKEY` two-level random, and `INCRBYFLOAT` f64 (Redis: long double)
//! precision tails.
//!
//! Wall-clock commands (`EXPIREAT`/`EXAT`/`EXPIRETIME` …) convert through
//! the node's injected wall anchor ([`NodeInfo::wall_anchor`]) — internal
//! time stays the monotonic injected `Nanos` (L7); only the anchor knows the
//! Unix epoch, so DST can fabricate any wall time deterministically.

use core::cell::{Cell, RefCell};
use std::rc::Rc;

use inf_foundation::time::Nanos;
use inf_store::{
    CellStore, CopyResult, ExpireCond, Keyspace, OpError, SetCond, SetExpire, SetOptions,
    SetOutcome, Ttl, TtlUpdate,
};
use inf_wire::{ArgvRef, CmdFlags, CommandId, Protocol, RespWriter, arity_ok, lookup};

use crate::admin;
use crate::clients::ClientRegistry;
use crate::config::ConfigStore;
use crate::glob::glob_match;
use crate::pubsub;

/// Node-level state surfaced through the command layer (M0-S19 + M1-S03):
/// the frozen tripwire snapshot, the memory-attribution domains the store
/// can't see, and the M1 cell-local registries (clients, config), the
/// injected wall-clock anchor, and the injected RNG state. The node assembly
/// (or sim harness) wires these; a default-constructed `ConnCx` carries an
/// all-zero instance, so tests and the compat candidate need no wiring.
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
    /// Wall-clock anchor `(internal_ms, unix_ms)` taken at one instant by
    /// the assembly layer (bins read the system clock ONCE at boot; the sim
    /// injects any anchor). `(0, 0)` ⇒ wall time == internal time.
    pub wall_anchor: Cell<(u64, u64)>,
    /// Injected RNG state (SplitMix64 stream; RANDOMKEY) — seeded by the
    /// assembly layer, deterministic under DST (L7).
    pub rng_state: Cell<u64>,
    pub tcp_port: Cell<u16>,
    pub total_connections: Cell<u64>,
    /// Pub/sub gauges + counters (M1-S10/S11), flushed by the plane's
    /// MAINTAIN: channels this cell owns with live subscribers, live
    /// patterns (node-wide — the index is replicated), estimated registry
    /// bytes, fan-out messages sent, deliveries appended, and
    /// output-buffer-cap disconnections.
    pub pubsub_channels: Cell<u64>,
    pub pubsub_patterns: Cell<u64>,
    pub pubsub_state_bytes: Cell<u64>,
    pub pubsub_fan_msgs: Cell<u64>,
    pub pubsub_delivered: Cell<u64>,
    pub cob_disconnections: Cell<u64>,
    /// CLIENT registry for this cell's connections (single-threaded).
    pub clients: RefCell<ClientRegistry>,
    /// Typed CONFIG store (M1-S03 freeze: keys + hot-reload classes).
    pub config: RefCell<ConfigStore>,
}

/// Per-connection execution state (protocol negotiated via `HELLO`,
/// database selected via `SELECT` — M1-S08, subscriptions via
/// `(P)SUBSCRIBE` — M1-S10).
#[derive(Debug)]
pub struct ConnCx {
    pub proto: Protocol,
    pub id: u64,
    /// Selected default namespace (`SELECT 0..15`); 0 unless SELECTed.
    pub db: u16,
    /// Subscribed channels, in subscription order (M1-S10). Empty vectors
    /// never allocate, so a non-subscriber connection pays two length
    /// loads at most.
    pub sub_channels: Vec<Vec<u8>>,
    /// Subscribed glob patterns, in subscription order.
    pub sub_patterns: Vec<Vec<u8>>,
    /// Shared node stats for `INFO` (zeroed default outside a node).
    pub node: Rc<NodeInfo>,
}

impl Default for ConnCx {
    fn default() -> ConnCx {
        ConnCx {
            proto: Protocol::Resp2,
            id: 1,
            db: 0,
            sub_channels: Vec::new(),
            sub_patterns: Vec::new(),
            node: Rc::new(NodeInfo::default()),
        }
    }
}

/// Argument-vector view: the parser's borrowed [`ArgvRef`] on the fast path,
/// plain owned slices on the queued/remote paths. Monomorphized per caller —
/// the owned path previously re-encoded to RESP and re-parsed per command,
/// which the M0-E8 cross-cell profile flagged exactly as the reserved note
/// predicted (allocator + `format!` machinery outweighing store work).
pub trait Argv {
    fn arg(&self, i: usize) -> &[u8];
    fn len(&self) -> usize;
    fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

impl Argv for ArgvRef<'_> {
    #[inline]
    fn arg(&self, i: usize) -> &[u8] {
        ArgvRef::arg(self, i)
    }

    #[inline]
    fn len(&self) -> usize {
        ArgvRef::len(self)
    }
}

impl Argv for [&[u8]] {
    #[inline]
    fn arg(&self, i: usize) -> &[u8] {
        self[i]
    }

    #[inline]
    fn len(&self) -> usize {
        <[&[u8]]>::len(self)
    }
}

/// Executes one parsed command against the cell's keyspace, appending the
/// reply to `out`. `now` is injected (L7) — same clock the store's TTLs
/// live on. Keyspace-level commands (SELECT, FLUSHALL, cross-db COPY,
/// INF.NS, INFO, CONFIG) dispatch here; everything else runs against the
/// connection's selected database.
pub fn execute(
    argv: &(impl Argv + ?Sized),
    ks: &mut Keyspace,
    cx: &mut ConnCx,
    now: Nanos,
    out: &mut Vec<u8>,
) {
    let Some(meta) = lookup(argv.arg(0)) else {
        let mut w = RespWriter::new(out, cx.proto);
        return unknown_command(argv, &mut w);
    };
    if !arity_ok(meta, argv.len()) {
        let mut w = RespWriter::new(out, cx.proto);
        return arity_error(meta.name, &mut w);
    }
    // M1-S07 OOM gate: DENYOOM commands enter through metadata, never
    // per-handler checks (kernel rule). The first test is one branch on the
    // keyspace's cached flag; only genuine pressure pays the inline
    // eviction escalation, and `noeviction`/unfreeable pressure answers
    // the Redis-exact OOM error.
    if meta.flags.contains(CmdFlags::DENYOOM) && ks.over_limit() && ks.free_for_write(now).is_err()
    {
        let mut w = RespWriter::new(out, cx.proto);
        return w.error("OOM command not allowed when used memory > 'maxmemory'.");
    }
    // M1-S10: a RESP2 subscriber may only run the subscribe family + PING
    // (Redis `processCommand` order: after the OOM gate). RESP3 lifts this.
    if pubsub::subscriber_restricted(cx) && !pubsub::allowed_in_subscriber_mode(meta.id) {
        let sub = (argv.len() > 1).then(|| argv.arg(1));
        let mut w = RespWriter::new(out, cx.proto);
        return pubsub::restricted_error(meta.id, meta.name, sub, &mut w);
    }
    match meta.id {
        // ---- keyspace-level commands (M1-E3/E4) ----
        CommandId::Select => {
            let mut w = RespWriter::new(out, cx.proto);
            select(argv, ks, cx, &mut w);
        }
        CommandId::Flushall => {
            let mut w = RespWriter::new(out, cx.proto);
            flush(argv, ks, None, now, &mut w);
        }
        CommandId::Flushdb => {
            let db = cx.db;
            let mut w = RespWriter::new(out, cx.proto);
            flush(argv, ks, Some(db), now, &mut w);
        }
        CommandId::Copy => {
            let db = cx.db;
            let mut w = RespWriter::new(out, cx.proto);
            copy(argv, ks, db, now, &mut w);
        }
        CommandId::Info => {
            let mut w = RespWriter::new(out, cx.proto);
            admin::info(argv, ks, &cx.node, now, &mut w);
        }
        CommandId::Config => {
            let mut w = RespWriter::new(out, cx.proto);
            admin::config(argv, ks, &cx.node, &mut w);
        }
        CommandId::InfNs => {
            let mut w = RespWriter::new(out, cx.proto);
            admin::inf_ns(argv, ks, &cx.node, &mut w);
        }
        // ---- pub/sub (M1-S10): conn-state ops here; registries, delivery,
        // and fan-out are plane state, so inside a node the plane intercepts
        // these before `execute` and this path is the planeless fallback
        // (compat candidate, embedded) — the single-connection view, which
        // is byte-exact Redis behavior for one client on one server.
        CommandId::Subscribe | CommandId::Psubscribe => {
            let kind = if meta.id == CommandId::Subscribe {
                pubsub::SubKind::Channel
            } else {
                pubsub::SubKind::Pattern
            };
            let names: Vec<&[u8]> = (1..argv.len()).map(|i| argv.arg(i)).collect();
            pubsub::apply_subscribe(&names, kind, cx, out);
        }
        CommandId::Unsubscribe | CommandId::Punsubscribe => {
            let kind = if meta.id == CommandId::Unsubscribe {
                pubsub::SubKind::Channel
            } else {
                pubsub::SubKind::Pattern
            };
            let names: Vec<&[u8]> = (1..argv.len()).map(|i| argv.arg(i)).collect();
            let names = if names.is_empty() { None } else { Some(names.as_slice()) };
            pubsub::apply_unsubscribe(names, kind, cx, out);
        }
        CommandId::Publish => pubsub::publish_fallback(argv.arg(1), argv.arg(2), cx, out),
        CommandId::Pubsub => {
            let args: Vec<&[u8]> = (1..argv.len()).map(|i| argv.arg(i)).collect();
            pubsub::pubsub_fallback(&args, cx, out);
        }
        _ => {
            let db = usize::from(cx.db);
            execute_db(meta, argv, ks.db_mut(db), cx, now, out);
        }
    }
    // Mutations refresh the cached pressure flag (no-op without a limit).
    if meta.flags.contains(CmdFlags::WRITE) {
        ks.refresh_pressure();
    }
}

/// Executes one command against the selected database's store (the M0-S15
/// body, unchanged in shape — keyspace-level commands never reach here).
fn execute_db(
    meta: &'static inf_wire::CommandMeta,
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    cx: &mut ConnCx,
    now: Nanos,
    out: &mut Vec<u8>,
) {
    let mut w = RespWriter::new(out, cx.proto);
    match meta.id {
        CommandId::Ping => {
            if argv.len() > 2 {
                w.error("ERR wrong number of arguments for 'ping' command");
            } else if pubsub::subscriber_restricted(cx) {
                // RESP2 subscriber mode: `[pong, <arg|"">]` (Redis shape).
                let arg = (argv.len() == 2).then(|| argv.arg(1));
                pubsub::subscriber_ping(arg, cx.proto, out);
            } else if argv.len() == 2 {
                w.bulk(argv.arg(1));
            } else {
                w.simple("PONG");
            }
        }
        CommandId::Echo => w.bulk(argv.arg(1)),
        CommandId::Hello => admin::hello(argv, cx, now, out),
        CommandId::Get => match store.get(argv.arg(1), now) {
            Some(value) => w.bulk(value),
            None => w.null(),
        },
        CommandId::Set => set(argv, store, &cx.node, now, &mut w),
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
        CommandId::Getex => getex(argv, store, &cx.node, now, &mut w),
        CommandId::Del | CommandId::Unlink => {
            let mut removed = 0;
            for i in 1..argv.len() {
                removed += i64::from(store.del(argv.arg(i), now));
            }
            w.int(removed);
        }
        CommandId::Exists | CommandId::Touch => {
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
        CommandId::IncrByFloat => {
            let Some(delta) = parse_f64(argv.arg(2)) else {
                return w.error("ERR value is not a valid float");
            };
            match store.incr_by_float(argv.arg(1), delta, now) {
                Ok(text) => w.bulk(&text),
                Err(e) => op_error(e, &mut w),
            }
        }
        CommandId::Append => match store.append(argv.arg(1), argv.arg(2), now) {
            Ok(len) => w.int(len as i64),
            Err(e) => op_error(e, &mut w),
        },
        CommandId::Strlen => w.int(store.strlen(argv.arg(1), now) as i64),
        // ---- M1-S01 · string family ----
        CommandId::Mget => {
            let keys: Vec<&[u8]> = (1..argv.len()).map(|i| argv.arg(i)).collect();
            w.array_header(keys.len());
            store.get_many(&keys, now, |_, value| match value {
                Some(v) => w.bulk(v),
                None => w.null(),
            });
        }
        CommandId::Mset => mset(argv, store, now, &mut w),
        CommandId::Msetnx => msetnx(argv, store, now, &mut w),
        CommandId::Getrange | CommandId::Substr => {
            let (Ok(start), Ok(end)) = (parse_i64(argv.arg(2)), parse_i64(argv.arg(3))) else {
                return w.error("ERR value is not an integer or out of range");
            };
            let slice = store.get_range(argv.arg(1), start, end, now);
            w.bulk(slice);
        }
        CommandId::Setrange => {
            let Ok(offset) = parse_i64(argv.arg(2)) else {
                return w.error("ERR value is not an integer or out of range");
            };
            if offset < 0 {
                return w.error("ERR offset is out of range");
            }
            match store.set_range(argv.arg(1), offset as usize, argv.arg(3), now) {
                Ok(len) => w.int(len as i64),
                Err(e) => op_error(e, &mut w),
            }
        }
        // ---- M1-S02 · key management ----
        CommandId::Rename => match store.rename(argv.arg(1), argv.arg(2), now) {
            Ok(true) => w.simple("OK"),
            Ok(false) => w.error("ERR no such key"),
            Err(e) => op_error(e, &mut w),
        },
        CommandId::Renamenx => {
            if !store.exists(argv.arg(1), now) {
                return w.error("ERR no such key");
            }
            if store.exists(argv.arg(2), now) && argv.arg(1) != argv.arg(2) {
                return w.int(0);
            }
            match store.rename(argv.arg(1), argv.arg(2), now) {
                Ok(true) => w.int(1),
                Ok(false) => w.error("ERR no such key"),
                Err(e) => op_error(e, &mut w),
            }
        }
        CommandId::Dbsize => w.int(store.len() as i64),
        CommandId::Keys => keys(argv.arg(1), store, now, cx.proto, out),
        CommandId::Randomkey => {
            let roll = next_rand(&cx.node);
            match store.random_key(roll, now) {
                Some(key) => w.bulk(&key),
                None => w.null(),
            }
        }
        CommandId::Scan => scan(argv, store, now, cx.proto, out),
        CommandId::Object => object(argv, store, &cx.node, now, &mut w),
        CommandId::Debug => admin::debug(argv, store, now, &mut w),
        // ---- M1-S03 · expiry completion + introspection ----
        CommandId::Expire | CommandId::Pexpire => {
            let unit_ms = if meta.id == CommandId::Expire { 1000 } else { 1 };
            expire(argv, store, now, Deadline::Relative { unit_ms }, meta.name, &cx.node, &mut w);
        }
        CommandId::Expireat | CommandId::Pexpireat => {
            let unit_ms = if meta.id == CommandId::Expireat { 1000 } else { 1 };
            expire(
                argv,
                store,
                now,
                Deadline::AbsoluteUnix { unit_ms },
                meta.name,
                &cx.node,
                &mut w,
            );
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
        CommandId::Expiretime | CommandId::Pexpiretime => {
            let value = match store.expire_at(argv.arg(1), now) {
                Ttl::Missing => -2,
                Ttl::NoExpiry => -1,
                Ttl::Ms(at_internal_ms) => {
                    let unix_ms = unix_from_internal_ms(&cx.node, at_internal_ms);
                    if meta.id == CommandId::Expiretime { unix_ms / 1000 } else { unix_ms }
                }
            };
            w.int(value);
        }
        CommandId::Persist => {
            let removed = store.expire(argv.arg(1), None, ExpireCond::Always, now);
            w.int(i64::from(removed));
        }
        CommandId::Command => admin::command_introspection(argv, &mut w),
        CommandId::Client => admin::client(argv, cx, now, out),
        CommandId::Lolwut => w.bulk(b"InfinityDB ver. 0.1.0-alpha.0\n"),
        // ---- internal fabric-program ops ----
        CommandId::InfTake | CommandId::InfPeek => {
            inf_take_peek(argv, store, meta.id == CommandId::InfTake, now, &mut w);
        }
        CommandId::Select
        | CommandId::Flushdb
        | CommandId::Flushall
        | CommandId::Copy
        | CommandId::Info
        | CommandId::Config
        | CommandId::InfNs
        | CommandId::Subscribe
        | CommandId::Unsubscribe
        | CommandId::Psubscribe
        | CommandId::Punsubscribe
        | CommandId::Publish
        | CommandId::Pubsub => {
            unreachable!("keyspace-level and pub/sub commands dispatch in execute()")
        }
    }
}

/// Executes a command from owned argument slices — the queued-behind-async
/// and remote-`Apply` paths — through the same [`Argv`]-generic body as the
/// fast path. Arguments arriving here were already parsed under the
/// originating connection's `ParserLimits`; the store enforces its own
/// record bounds, so no protocol-side recheck is needed.
pub fn execute_slices(
    argv: &[&[u8]],
    ks: &mut Keyspace,
    cx: &mut ConnCx,
    now: Nanos,
    out: &mut Vec<u8>,
) {
    debug_assert!(!argv.is_empty(), "empty argv is a caller bug");
    execute(argv, ks, cx, now, out);
}

// ---- wall clock (M1-S03) -----------------------------------------------------

/// Current wall-clock milliseconds (Unix epoch) through the injected anchor.
pub(crate) fn wall_ms(node: &NodeInfo, now: Nanos) -> u64 {
    let (internal_anchor, unix_anchor) = node.wall_anchor.get();
    now.as_millis().saturating_sub(internal_anchor).saturating_add(unix_anchor)
}

/// Internal (injected-clock) milliseconds for a Unix-epoch deadline. Past
/// deadlines clamp to 0 (already expired); `None` = arithmetic overflow.
fn internal_from_unix_ms(node: &NodeInfo, unix_ms: i64) -> Option<u64> {
    let (internal_anchor, unix_anchor) = node.wall_anchor.get();
    let delta = unix_ms.checked_sub(i64::try_from(unix_anchor).ok()?)?;
    let internal = i64::try_from(internal_anchor).ok()?.checked_add(delta)?;
    Some(internal.max(0) as u64)
}

/// Unix-epoch milliseconds for an internal deadline (EXPIRETIME family).
fn unix_from_internal_ms(node: &NodeInfo, internal_ms: u64) -> i64 {
    let (internal_anchor, unix_anchor) = node.wall_anchor.get();
    internal_ms as i64 - internal_anchor as i64 + unix_anchor as i64
}

/// SplitMix64 step over the node's injected RNG state (L7: the seed is
/// injected; the stream is deterministic).
pub(crate) fn next_rand(node: &NodeInfo) -> u64 {
    let s = node.rng_state.get().wrapping_add(0x9E37_79B9_7F4A_7C15);
    node.rng_state.set(s);
    let mut z = s;
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

// ---- SET ---------------------------------------------------------------------

fn set(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    node: &NodeInfo,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
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
        } else if opt.eq_ignore_ascii_case(b"EX")
            || opt.eq_ignore_ascii_case(b"PX")
            || opt.eq_ignore_ascii_case(b"EXAT")
            || opt.eq_ignore_ascii_case(b"PXAT")
        {
            if have_expire || i + 1 >= argv.len() {
                return w.error("ERR syntax error");
            }
            have_expire = true;
            let Ok(value) = parse_i64(argv.arg(i + 1)) else {
                return w.error("ERR value is not an integer or out of range");
            };
            let unit_ms: i64 =
                if opt.eq_ignore_ascii_case(b"EX") || opt.eq_ignore_ascii_case(b"EXAT") {
                    1000
                } else {
                    1
                };
            let absolute = opt.eq_ignore_ascii_case(b"EXAT") || opt.eq_ignore_ascii_case(b"PXAT");
            let at = if absolute {
                // Past EXAT/PXAT is legal: SET applies, the key is born
                // expired (Redis semantics).
                value
                    .checked_mul(unit_ms)
                    .and_then(|unix| internal_from_unix_ms(node, unix))
                    .and_then(ms_to_nanos)
            } else {
                expire_deadline(now, value, unit_ms)
            };
            let Some(at) = at else {
                return w.error("ERR invalid expire time in 'set' command");
            };
            opts.expire = SetExpire::At(at);
            i += 1;
        } else {
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

// ---- GETEX -------------------------------------------------------------------

fn getex(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    node: &NodeInfo,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let mut update = TtlUpdate::Keep;
    let mut have = false;
    let mut i = 2;
    while i < argv.len() {
        let opt = argv.arg(i);
        if have {
            return w.error("ERR syntax error");
        }
        if opt.eq_ignore_ascii_case(b"PERSIST") {
            have = true;
            update = TtlUpdate::Persist;
        } else if opt.eq_ignore_ascii_case(b"EX")
            || opt.eq_ignore_ascii_case(b"PX")
            || opt.eq_ignore_ascii_case(b"EXAT")
            || opt.eq_ignore_ascii_case(b"PXAT")
        {
            if i + 1 >= argv.len() {
                return w.error("ERR syntax error");
            }
            have = true;
            let Ok(value) = parse_i64(argv.arg(i + 1)) else {
                return w.error("ERR value is not an integer or out of range");
            };
            let unit_ms: i64 =
                if opt.eq_ignore_ascii_case(b"EX") || opt.eq_ignore_ascii_case(b"EXAT") {
                    1000
                } else {
                    1
                };
            let absolute = opt.eq_ignore_ascii_case(b"EXAT") || opt.eq_ignore_ascii_case(b"PXAT");
            let at = if absolute {
                value
                    .checked_mul(unit_ms)
                    .and_then(|unix| internal_from_unix_ms(node, unix))
                    .and_then(ms_to_nanos)
            } else {
                expire_deadline(now, value, unit_ms)
            };
            let Some(at) = at else {
                return w.error("ERR invalid expire time in 'getex' command");
            };
            update = TtlUpdate::At(at);
            i += 1;
        } else {
            return w.error("ERR syntax error");
        }
        i += 1;
    }
    match store.get_ex(argv.arg(1), update, now) {
        Some(value) => w.bulk(&value),
        None => w.null(),
    }
}

// ---- MSET / MSETNX -----------------------------------------------------------

fn mset(argv: &(impl Argv + ?Sized), store: &mut CellStore, now: Nanos, w: &mut RespWriter<'_>) {
    if argv.len().is_multiple_of(2) {
        return arity_error("MSET", w);
    }
    let mut i = 1;
    while i < argv.len() {
        // Single-cell MSET is atomic by single-threadedness; an OOM mid-way
        // surfaces as the error (partial application — Redis can't hit this
        // shape; recorded with the OOM backpressure semantics).
        if let Err(e) = store.set(argv.arg(i), argv.arg(i + 1), SetOptions::default(), now) {
            return op_error(e, w);
        }
        i += 2;
    }
    w.simple("OK");
}

fn msetnx(argv: &(impl Argv + ?Sized), store: &mut CellStore, now: Nanos, w: &mut RespWriter<'_>) {
    if argv.len().is_multiple_of(2) {
        return arity_error("MSETNX", w);
    }
    let mut i = 1;
    while i < argv.len() {
        if store.exists(argv.arg(i), now) {
            return w.int(0);
        }
        i += 2;
    }
    let mut i = 1;
    while i < argv.len() {
        if let Err(e) = store.set(argv.arg(i), argv.arg(i + 1), SetOptions::default(), now) {
            return op_error(e, w);
        }
        i += 2;
    }
    w.int(1);
}

// ---- COPY ----------------------------------------------------------------------

/// `COPY src dst [DB n] [REPLACE]` — cross-db is real with namespaces v1
/// (M1-S08); the source database is the connection's selected db.
fn copy(
    argv: &(impl Argv + ?Sized),
    ks: &mut Keyspace,
    src_db: u16,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let mut replace = false;
    let mut dst_db = src_db;
    let mut i = 3;
    while i < argv.len() {
        let opt = argv.arg(i);
        if opt.eq_ignore_ascii_case(b"REPLACE") {
            replace = true;
        } else if opt.eq_ignore_ascii_case(b"DB") {
            if i + 1 >= argv.len() {
                return w.error("ERR syntax error");
            }
            match parse_i64(argv.arg(i + 1)) {
                Ok(n @ 0..=15) => dst_db = n as u16,
                Ok(_) => return w.error("ERR DB index is out of range"),
                Err(()) => return w.error("ERR value is not an integer or out of range"),
            }
            i += 1;
        } else {
            return w.error("ERR syntax error");
        }
        i += 1;
    }
    // Same key is only an error within one database (Redis: cross-db
    // self-copy is legal).
    if argv.arg(1) == argv.arg(2) && src_db == dst_db {
        return w.error("ERR source and destination objects are the same");
    }
    let (src_db, dst_db) = (usize::from(src_db), usize::from(dst_db));
    match ks.copy_between(src_db, argv.arg(1), dst_db, argv.arg(2), replace, now) {
        Ok(CopyResult::Copied) => w.int(1),
        Ok(CopyResult::SourceMissing | CopyResult::DestinationExists) => w.int(0),
        Err(e) => op_error(e, w),
    }
}

/// `FLUSHDB` (the selected db) / `FLUSHALL` (every db) — this cell's slice.
fn flush(
    argv: &(impl Argv + ?Sized),
    ks: &mut Keyspace,
    db: Option<u16>,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    for i in 1..argv.len() {
        let opt = argv.arg(i);
        if !opt.eq_ignore_ascii_case(b"ASYNC") && !opt.eq_ignore_ascii_case(b"SYNC") {
            return w.error("ERR syntax error");
        }
    }
    match db {
        Some(db) => ks.db_mut(usize::from(db)).flush(now),
        None => ks.flush_all(now),
    }
    w.simple("OK");
}

// ---- KEYS / SCAN ---------------------------------------------------------------

fn keys(pattern: &[u8], store: &mut CellStore, now: Nanos, proto: Protocol, out: &mut Vec<u8>) {
    // Bounded per-cell slice semantics arrive with the plane's scatter
    // (M1-S02); locally this is one full home-group sweep.
    let mut hits: Vec<Vec<u8>> = Vec::new();
    let mut cursor = 0u64;
    loop {
        cursor = store.scan(cursor, usize::MAX, now, |key| {
            if glob_match(pattern, key, false) {
                hits.push(key.to_vec());
            }
        });
        if cursor == 0 {
            break;
        }
    }
    let mut w = RespWriter::new(out, proto);
    w.array_header(hits.len());
    for key in &hits {
        w.bulk(key);
    }
}

fn scan(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    now: Nanos,
    proto: Protocol,
    out: &mut Vec<u8>,
) {
    let mut w = RespWriter::new(out, proto);
    let Some(cursor) = parse_cursor(argv.arg(1)) else {
        return w.error("ERR invalid cursor");
    };
    let mut pattern: Option<&[u8]> = None;
    let mut count: usize = 10;
    let mut type_filter: Option<&[u8]> = None;
    let mut i = 2;
    while i < argv.len() {
        let opt = argv.arg(i);
        if opt.eq_ignore_ascii_case(b"MATCH") && i + 1 < argv.len() {
            pattern = Some(argv.arg(i + 1));
            i += 2;
        } else if opt.eq_ignore_ascii_case(b"COUNT") && i + 1 < argv.len() {
            match parse_i64(argv.arg(i + 1)) {
                Ok(n) if n >= 1 => count = n as usize,
                Ok(_) => return w.error("ERR syntax error"),
                Err(()) => return w.error("ERR value is not an integer or out of range"),
            }
            i += 2;
        } else if opt.eq_ignore_ascii_case(b"TYPE") && i + 1 < argv.len() {
            type_filter = Some(argv.arg(i + 1));
            i += 2;
        } else {
            return w.error("ERR syntax error");
        }
    }
    // Only strings exist until M3: a non-string TYPE filter yields nothing
    // (cursor still advances — Redis shape).
    let type_excludes = type_filter.is_some_and(|t| !t.eq_ignore_ascii_case(b"string"));
    let mut hits: Vec<Vec<u8>> = Vec::new();
    let next = store.scan(cursor, count, now, |key| {
        if type_excludes {
            return;
        }
        if pattern.is_none_or(|p| glob_match(p, key, false)) {
            hits.push(key.to_vec());
        }
    });
    w.array_header(2);
    let mut cursor_text = [0u8; 20];
    w.bulk(fmt_u64(&mut cursor_text, next));
    w.array_header(hits.len());
    for key in &hits {
        w.bulk(key);
    }
}

/// SCAN cursors are decimal u64 (Redis `strtoull` shape).
pub(crate) fn parse_cursor(bytes: &[u8]) -> Option<u64> {
    if bytes.is_empty() || bytes.len() > 20 || !bytes.iter().all(u8::is_ascii_digit) {
        return None;
    }
    core::str::from_utf8(bytes).ok()?.parse().ok()
}

// ---- OBJECT --------------------------------------------------------------------

fn object(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    node: &NodeInfo,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let sub = argv.arg(1);
    if sub.eq_ignore_ascii_case(b"HELP") {
        w.array_header(2);
        w.bulk(b"OBJECT <subcommand> [<arg> [value] [opt] ...]. Subcommands are:");
        w.bulk(b"ENCODING <key> | REFCOUNT <key> | IDLETIME <key> | FREQ <key>");
        return;
    }
    let known = [&b"ENCODING"[..], b"REFCOUNT", b"IDLETIME", b"FREQ"]
        .iter()
        .any(|s| sub.eq_ignore_ascii_case(s));
    if !known || argv.len() != 3 {
        return object_subcommand_error(sub, w);
    }
    let key = argv.arg(2);
    // Missing key is a null reply, not an error (Redis 8, oracle-pinned).
    let Some((encoding, int_value)) = store.object_encoding(key, now) else {
        return w.null();
    };
    if sub.eq_ignore_ascii_case(b"ENCODING") {
        w.bulk(encoding.name().as_bytes());
    } else if sub.eq_ignore_ascii_case(b"REFCOUNT") {
        // Redis shared integers (0..10000) report INT_MAX.
        let shared = matches!(int_value, Some(v) if (0..10_000).contains(&v));
        w.int(if shared { 2_147_483_647 } else { 1 });
    } else if sub.eq_ignore_ascii_case(b"IDLETIME") {
        // No LRU clock yet (eviction engine is M1-E3) — honest zero,
        // recorded deviation.
        w.int(0);
    } else {
        // FREQ requires an LFU policy, exactly like Redis. The value is the
        // CMS Morris estimate (recorded deviation: Redis's log-counter
        // scale differs; both are opaque popularity scores).
        let lfu = node.config.borrow().get("maxmemory-policy").is_some_and(|p| p.contains("lfu"));
        if lfu {
            w.int(i64::from(store.object_freq(key, now).unwrap_or(0)));
        } else {
            w.error(
                "ERR An LFU maxmemory policy is not selected, access frequency not tracked. Please note that when switching between policies at runtime LRU and LFU data will take some time to adjust.",
            );
        }
    }
}

// Redis 8 unknown-subcommand format (oracle-pinned).
fn object_subcommand_error(sub: &[u8], w: &mut RespWriter<'_>) {
    w.error(&format!(
        "ERR unknown subcommand '{}'. Try OBJECT HELP.",
        String::from_utf8_lossy(sub)
    ));
}

// ---- SELECT --------------------------------------------------------------------

/// `SELECT 0..15` maps to the default namespaces (M1-S08). The selection
/// is connection state — the plane serializes it in pipeline order exactly
/// like HELLO's protocol switch (a conn-state barrier).
fn select(argv: &(impl Argv + ?Sized), ks: &mut Keyspace, cx: &mut ConnCx, w: &mut RespWriter<'_>) {
    match parse_i64(argv.arg(1)) {
        Ok(n @ 0..=15) => {
            cx.db = n as u16;
            // Materialize eagerly: a SELECTed db is about to be used.
            let _ = ks.db_mut(n as usize);
            w.simple("OK");
        }
        Ok(_) => w.error("ERR DB index is out of range"),
        Err(()) => w.error("ERR value is not an integer or out of range"),
    }
}

// ---- EXPIRE family -------------------------------------------------------------

/// How the third argument converts to an absolute internal deadline.
#[derive(Copy, Clone)]
enum Deadline {
    /// EXPIRE/PEXPIRE: `now + value × unit`.
    Relative { unit_ms: i64 },
    /// EXPIREAT/PEXPIREAT: Unix `value × unit` through the wall anchor.
    AbsoluteUnix { unit_ms: i64 },
}

fn expire(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    now: Nanos,
    deadline: Deadline,
    name: &str,
    node: &NodeInfo,
    w: &mut RespWriter<'_>,
) {
    let Ok(value) = parse_i64(argv.arg(2)) else {
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
    let at = match deadline {
        Deadline::Relative { unit_ms } => expire_deadline_signed(now, value, unit_ms),
        Deadline::AbsoluteUnix { unit_ms } => value
            .checked_mul(unit_ms)
            .and_then(|unix| internal_from_unix_ms(node, unix))
            .and_then(ms_to_nanos),
    };
    let Some(at) = at else {
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

// ---- INF.TAKE / INF.PEEK (fabric-program primitives) ------------------------------

/// `INF.TAKE key` → `[value, pttl_ms]` removing the key; `INF.PEEK` reads
/// without removal. `pttl_ms = -1` ⇒ no TTL. Missing key ⇒ null array.
/// Atomic at the owning cell by single-threadedness — the cross-cell
/// RENAME/COPY program rides these (M1-S02).
fn inf_take_peek(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    take: bool,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let key = argv.arg(1);
    let pttl: i64 = match store.ttl(key, now) {
        Ttl::Missing => return w.null_array(),
        Ttl::NoExpiry => -1,
        Ttl::Ms(ms) => ms as i64,
    };
    w.array_header(2);
    if take {
        match store.getdel(key, now) {
            Some(value) => w.bulk(&value),
            None => w.null(), // unreachable: resolved above at the same `now`
        }
    } else {
        match store.get(key, now) {
            Some(value) => w.bulk(value),
            None => w.null(),
        }
    }
    w.int(pttl);
}

// ---- shared helpers ---------------------------------------------------------------

// Format pinned byte-exact against Redis 8.0.5 by the compat harness:
// `'arg1' 'arg2' ` — space-separated, trailing space, no parentheses.
fn unknown_command(argv: &(impl Argv + ?Sized), w: &mut RespWriter<'_>) {
    let mut text = format!(
        "ERR unknown command '{}', with args beginning with: ",
        String::from_utf8_lossy(argv.arg(0))
    );
    for i in 1..argv.len().min(21) {
        text.push_str(&format!("'{}' ", String::from_utf8_lossy(argv.arg(i))));
    }
    w.error(&text);
}

pub(crate) fn arity_error(name: &str, w: &mut RespWriter<'_>) {
    w.error(&format!("ERR wrong number of arguments for '{}' command", name.to_ascii_lowercase()));
}

fn op_error(e: OpError, w: &mut RespWriter<'_>) {
    match e {
        OpError::NotInt => w.error("ERR value is not an integer or out of range"),
        OpError::Overflow => w.error("ERR increment or decrement would overflow"),
        OpError::NotFloat => w.error("ERR value is not a valid float"),
        OpError::NanOrInf => w.error("ERR increment would produce NaN or Infinity"),
        OpError::OutOfMemory => w.error("OOM command not allowed when used memory > 'maxmemory'."),
        OpError::TooLarge => w.error("ERR key or value exceeds InfinityDB M0 record bounds"),
    }
}

/// Positive-TTL deadline for SET EX/PX, SETEX, and GETEX EX/PX (must be > 0).
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

fn ms_to_nanos(ms: u64) -> Option<Nanos> {
    Some(Nanos(ms.checked_mul(1_000_000)?))
}

/// Redis `string2ll`: optional sign, no leading zeros (and no `-0` —
/// oracle-pinned against Redis 8.0.5), no '+', i64 range.
pub(crate) fn parse_i64(bytes: &[u8]) -> Result<i64, ()> {
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

/// Redis `strtold`-shape float argument parse (INCRBYFLOAT delta): strict
/// full-string, no whitespace, NaN rejected.
fn parse_f64(bytes: &[u8]) -> Option<f64> {
    let s = core::str::from_utf8(bytes).ok()?;
    if s.is_empty() || s.starts_with(char::is_whitespace) || s.ends_with(char::is_whitespace) {
        return None;
    }
    let v: f64 = s.parse().ok()?;
    if v.is_nan() { None } else { Some(v) }
}

/// u64 → ASCII into a caller stack buffer (cursor formatting).
pub(crate) fn fmt_u64(buf: &mut [u8; 20], mut v: u64) -> &[u8] {
    let mut at = buf.len();
    loop {
        at -= 1;
        buf[at] = b'0' + (v % 10) as u8;
        v /= 10;
        if v == 0 {
            break;
        }
    }
    &buf[at..]
}

/// Plane hook (M1-S02 `DEBUG SLEEP`): when `argv` is a well-formed
/// `DEBUG SLEEP <seconds>`, the duration the executing cell should stall its
/// connection plane (fabric service continues — deadlock safety; recorded
/// deviation: Redis blocks the whole server).
pub fn stall_request(argv: &[&[u8]]) -> Option<Nanos> {
    if argv.len() != 3
        || !argv[0].eq_ignore_ascii_case(b"DEBUG")
        || !argv[1].eq_ignore_ascii_case(b"SLEEP")
    {
        return None;
    }
    let secs: f64 = core::str::from_utf8(argv[2]).ok()?.parse().ok()?;
    if !secs.is_finite() || secs <= 0.0 {
        return None;
    }
    Some(Nanos((secs * 1e9) as u64))
}

#[cfg(test)]
mod tests {
    use super::*;
    use inf_store::StoreConfig;
    use inf_wire::{ConnParser, Parsed, ParserLimits};

    fn run_at(cx: &mut ConnCx, store: &mut Keyspace, now: Nanos, parts: &[&[u8]]) -> Vec<u8> {
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
        execute(&argv, store, cx, now, &mut out);
        out
    }

    fn run(cx: &mut ConnCx, store: &mut Keyspace, parts: &[&[u8]]) -> Vec<u8> {
        run_at(cx, store, Nanos(1), parts)
    }

    #[test]
    fn hello_switches_protocol_and_rejects_unknown_versions() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
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
        let mut store = Keyspace::new(StoreConfig::default());
        let long_key = vec![b'k'; 256];
        let reply = run(&mut cx, &mut store, &[b"SET", &long_key, b"v"]);
        assert!(reply.starts_with(b"-ERR key or value exceeds"), "{reply:?}");
    }

    #[test]
    fn mget_mset_roundtrip() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        assert_eq!(run(&mut cx, &mut store, &[b"MSET", b"a", b"1", b"b", b"2"]), b"+OK\r\n");
        assert_eq!(
            run(&mut cx, &mut store, &[b"MGET", b"a", b"nope", b"b"]),
            b"*3\r\n$1\r\n1\r\n$-1\r\n$1\r\n2\r\n"
        );
        // Odd pair count is the arity error.
        assert_eq!(
            run(&mut cx, &mut store, &[b"MSET", b"a", b"1", b"b"]),
            b"-ERR wrong number of arguments for 'mset' command\r\n".to_vec()
        );
        // MSETNX all-or-nothing.
        assert_eq!(run(&mut cx, &mut store, &[b"MSETNX", b"a", b"9", b"new", b"x"]), b":0\r\n");
        assert_eq!(run(&mut cx, &mut store, &[b"GET", b"new"]), b"$-1\r\n");
        assert_eq!(run(&mut cx, &mut store, &[b"MSETNX", b"n1", b"1", b"n2", b"2"]), b":1\r\n");
    }

    #[test]
    fn expireat_family_uses_the_wall_anchor() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        // Anchor: internal 1000 ms == unix 5_000_000 ms.
        cx.node.wall_anchor.set((1_000, 5_000_000));
        let now = Nanos::from_millis(1_000);
        assert_eq!(run_at(&mut cx, &mut store, now, &[b"SET", b"k", b"v"]), b"+OK\r\n");
        // EXPIREAT unix 5_100 s = unix ms 5_100_000 → internal 101_000 ms.
        assert_eq!(run_at(&mut cx, &mut store, now, &[b"EXPIREAT", b"k", b"5100"]), b":1\r\n");
        assert_eq!(run_at(&mut cx, &mut store, now, &[b"TTL", b"k"]), b":100\r\n");
        assert_eq!(run_at(&mut cx, &mut store, now, &[b"EXPIRETIME", b"k"]), b":5100\r\n");
        assert_eq!(run_at(&mut cx, &mut store, now, &[b"PEXPIRETIME", b"k"]), b":5100000\r\n");
        // Past EXPIREAT deletes.
        assert_eq!(run_at(&mut cx, &mut store, now, &[b"EXPIREAT", b"k", b"4999"]), b":1\r\n");
        assert_eq!(run_at(&mut cx, &mut store, now, &[b"EXISTS", b"k"]), b":0\r\n");
        // SET ... EXAT in the past: applied, born expired.
        assert_eq!(
            run_at(&mut cx, &mut store, now, &[b"SET", b"p", b"v", b"EXAT", b"1"]),
            b"+OK\r\n"
        );
        assert_eq!(run_at(&mut cx, &mut store, now, &[b"GET", b"p"]), b"$-1\r\n");
    }

    #[test]
    fn object_encoding_tracks_int_embstr_raw() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        run(&mut cx, &mut store, &[b"SET", b"n", b"123"]);
        assert_eq!(run(&mut cx, &mut store, &[b"OBJECT", b"ENCODING", b"n"]), b"$3\r\nint\r\n");
        assert_eq!(run(&mut cx, &mut store, &[b"OBJECT", b"REFCOUNT", b"n"]), b":2147483647\r\n");
        run(&mut cx, &mut store, &[b"SET", b"s", b"short"]);
        assert_eq!(run(&mut cx, &mut store, &[b"OBJECT", b"ENCODING", b"s"]), b"$6\r\nembstr\r\n");
        let long = vec![b'x'; 45];
        run(&mut cx, &mut store, &[b"SET", b"l", &long]);
        assert_eq!(run(&mut cx, &mut store, &[b"OBJECT", b"ENCODING", b"l"]), b"$3\r\nraw\r\n");
        // APPEND forces raw even when short and numeric.
        run(&mut cx, &mut store, &[b"SET", b"a", b"12"]);
        run(&mut cx, &mut store, &[b"APPEND", b"a", b"3"]);
        assert_eq!(run(&mut cx, &mut store, &[b"OBJECT", b"ENCODING", b"a"]), b"$3\r\nraw\r\n");
        assert_eq!(run(&mut cx, &mut store, &[b"OBJECT", b"REFCOUNT", b"a"]), b":1\r\n");
    }

    #[test]
    fn scan_walks_the_whole_keyspace() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        for i in 0..500 {
            let key = format!("k:{i}");
            run(&mut cx, &mut store, &[b"SET", key.as_bytes(), b"v"]);
        }
        let mut seen = std::collections::HashSet::new();
        let mut cursor: Vec<u8> = b"0".to_vec();
        loop {
            let reply = run(&mut cx, &mut store, &[b"SCAN", &cursor, b"COUNT", b"17"]);
            let text = String::from_utf8(reply).expect("ascii");
            let mut lines = text.split("\r\n");
            assert_eq!(lines.next(), Some("*2"));
            let _len = lines.next().expect("cursor len");
            cursor = lines.next().expect("cursor").as_bytes().to_vec();
            let rest: Vec<&str> = lines.collect();
            assert!(rest[0].starts_with('*'), "inner array header: {rest:?}");
            for chunk in rest[1..].chunks(2) {
                if chunk.len() == 2 && chunk[0].starts_with('$') && !chunk[1].is_empty() {
                    seen.insert(chunk[1].to_string());
                }
            }
            if cursor == b"0" {
                break;
            }
        }
        assert_eq!(seen.len(), 500, "every key emitted at least once");
    }

    #[test]
    fn stall_request_parses_debug_sleep_only() {
        assert_eq!(stall_request(&[b"DEBUG", b"SLEEP", b"0.5"]), Some(Nanos(500_000_000)));
        assert_eq!(stall_request(&[b"DEBUG", b"SLEEP", b"0"]), None);
        assert_eq!(stall_request(&[b"DEBUG", b"JMAP"]), None);
        assert_eq!(stall_request(&[b"GET", b"k", b"x"]), None);
    }

    // ---- M1-E4 · namespaces v1 -------------------------------------------------

    #[test]
    fn select_isolates_databases_and_flushdb_scopes() {
        let mut cx = ConnCx::default();
        let mut ks = Keyspace::new(StoreConfig::default());
        assert_eq!(run(&mut cx, &mut ks, &[b"SET", b"k", b"zero"]), b"+OK\r\n");
        assert_eq!(run(&mut cx, &mut ks, &[b"SELECT", b"1"]), b"+OK\r\n");
        assert_eq!(cx.db, 1);
        // Identical key, different namespace: never aliases (M1-S08 AC).
        assert_eq!(run(&mut cx, &mut ks, &[b"GET", b"k"]), b"$-1\r\n");
        assert_eq!(run(&mut cx, &mut ks, &[b"SET", b"k", b"one"]), b"+OK\r\n");
        assert_eq!(run(&mut cx, &mut ks, &[b"DBSIZE"]), b":1\r\n");
        assert_eq!(run(&mut cx, &mut ks, &[b"FLUSHDB"]), b"+OK\r\n");
        assert_eq!(run(&mut cx, &mut ks, &[b"DBSIZE"]), b":0\r\n");
        assert_eq!(run(&mut cx, &mut ks, &[b"SELECT", b"0"]), b"+OK\r\n");
        assert_eq!(run(&mut cx, &mut ks, &[b"GET", b"k"]), b"$4\r\nzero\r\n");
        // FLUSHALL clears every db.
        run(&mut cx, &mut ks, &[b"FLUSHALL"]);
        assert_eq!(run(&mut cx, &mut ks, &[b"DBSIZE"]), b":0\r\n");
        // Bounds + error wording.
        assert_eq!(
            run(&mut cx, &mut ks, &[b"SELECT", b"16"]),
            b"-ERR DB index is out of range\r\n"
        );
        assert_eq!(
            run(&mut cx, &mut ks, &[b"SELECT", b"abc"]),
            b"-ERR value is not an integer or out of range\r\n"
        );
    }

    #[test]
    fn copy_crosses_databases_with_ttl_and_encoding() {
        let mut cx = ConnCx::default();
        let mut ks = Keyspace::new(StoreConfig::default());
        let now = Nanos::from_millis(1_000);
        run_at(&mut cx, &mut ks, now, &[b"SET", b"src", b"v", b"PX", b"5000"]);
        run_at(&mut cx, &mut ks, now, &[b"APPEND", b"src", b"!"]); // raw encoding
        assert_eq!(
            run_at(&mut cx, &mut ks, now, &[b"COPY", b"src", b"src", b"DB", b"3"]),
            b":1\r\n"
        );
        run_at(&mut cx, &mut ks, now, &[b"SELECT", b"3"]);
        assert_eq!(run_at(&mut cx, &mut ks, now, &[b"GET", b"src"]), b"$2\r\nv!\r\n");
        assert_eq!(run_at(&mut cx, &mut ks, now, &[b"PTTL", b"src"]), b":5000\r\n");
        assert_eq!(
            run_at(&mut cx, &mut ks, now, &[b"OBJECT", b"ENCODING", b"src"]),
            b"$3\r\nraw\r\n"
        );
        // Destination exists without REPLACE: 0.
        assert_eq!(run_at(&mut cx, &mut ks, now, &[b"SELECT", b"0"]), b"+OK\r\n");
        assert_eq!(
            run_at(&mut cx, &mut ks, now, &[b"COPY", b"src", b"src", b"DB", b"3"]),
            b":0\r\n"
        );
        // Same key + same db is the Redis error; cross-db self-copy is legal.
        assert_eq!(
            run_at(&mut cx, &mut ks, now, &[b"COPY", b"src", b"src"]),
            b"-ERR source and destination objects are the same\r\n"
        );
    }

    #[test]
    fn inf_ns_registry_surface() {
        let mut cx = ConnCx::default();
        let mut ks = Keyspace::new(StoreConfig::default());
        assert_eq!(
            run(
                &mut cx,
                &mut ks,
                &[
                    b"INF.NS",
                    b"CREATE",
                    b"cache",
                    b"EVICTION",
                    b"allkeys-lfu",
                    b"MAXMEMORY",
                    b"16mb"
                ]
            ),
            b"+OK\r\n"
        );
        assert_eq!(
            run(&mut cx, &mut ks, &[b"INF.NS", b"CREATE", b"cache"]),
            b"-ERR namespace already exists\r\n"
        );
        // The M1-S08 honesty AC: durable mode is a documented not-yet error.
        let reply = run(&mut cx, &mut ks, &[b"INF.NS", b"CREATE", b"ledger", b"MODE", b"durable"]);
        assert!(
            reply.starts_with(b"-ERR namespace mode 'durable' is not yet supported"),
            "{reply:?}"
        );
        let list = run(&mut cx, &mut ks, &[b"INF.NS", b"LIST"]);
        let text = String::from_utf8(list).expect("ascii");
        assert!(text.starts_with("*17\r\n"), "16 defaults + 1 named: {text}");
        assert!(text.contains("cache"), "{text}");
        let info = String::from_utf8(run(&mut cx, &mut ks, &[b"INF.NS", b"INFO", b"cache"]))
            .expect("ascii");
        assert!(info.contains("allkeys-lfu"), "{info}");
        assert!(info.contains("16777216"), "{info}");
        let db_info =
            String::from_utf8(run(&mut cx, &mut ks, &[b"INF.NS", b"INFO", b"db0"])).expect("ascii");
        assert!(db_info.contains("memory"), "{db_info}");
        assert_eq!(run(&mut cx, &mut ks, &[b"INF.NS", b"DROP", b"cache"]), b"+OK\r\n");
        assert_eq!(
            run(&mut cx, &mut ks, &[b"INF.NS", b"DROP", b"db0"]),
            b"-ERR db0..db15 are reserved default namespaces (SELECT)\r\n"
        );
    }

    // ---- M1-E5 · pub/sub (exec-layer fallback + subscriber mode) -----------------

    #[test]
    fn resp2_subscriber_mode_restricts_and_reshapes_ping() {
        let mut cx = ConnCx::default();
        let mut ks = Keyspace::new(StoreConfig::default());
        run(&mut cx, &mut ks, &[b"SET", b"k", b"v"]);
        assert_eq!(
            run(&mut cx, &mut ks, &[b"SUBSCRIBE", b"news"]),
            b"*3\r\n$9\r\nsubscribe\r\n$4\r\nnews\r\n:1\r\n"
        );
        // Disallowed commands answer the Redis-exact context error.
        assert_eq!(
            run(&mut cx, &mut ks, &[b"GET", b"k"]),
            b"-ERR Can't execute 'get': only (P|S)SUBSCRIBE / (P|S)UNSUBSCRIBE / PING / QUIT / RESET are allowed in this context\r\n".to_vec()
        );
        // PING reshapes to [pong, <arg|"">] in RESP2 subscriber mode.
        assert_eq!(run(&mut cx, &mut ks, &[b"PING"]), b"*2\r\n$4\r\npong\r\n$0\r\n\r\n");
        assert_eq!(run(&mut cx, &mut ks, &[b"PING", b"hi"]), b"*2\r\n$4\r\npong\r\n$2\r\nhi\r\n");
        // Bare UNSUBSCRIBE drops everything and lifts the restriction.
        assert_eq!(
            run(&mut cx, &mut ks, &[b"UNSUBSCRIBE"]),
            b"*3\r\n$11\r\nunsubscribe\r\n$4\r\nnews\r\n:0\r\n"
        );
        assert_eq!(run(&mut cx, &mut ks, &[b"GET", b"k"]), b"$1\r\nv\r\n");
        // With no subscriptions, bare UNSUBSCRIBE is the single nil frame.
        assert_eq!(
            run(&mut cx, &mut ks, &[b"UNSUBSCRIBE"]),
            b"*3\r\n$11\r\nunsubscribe\r\n$-1\r\n:0\r\n"
        );
    }

    #[test]
    fn resp3_lifts_the_restriction_and_self_delivers() {
        let mut cx = ConnCx::default();
        let mut ks = Keyspace::new(StoreConfig::default());
        run(&mut cx, &mut ks, &[b"HELLO", b"3"]);
        assert_eq!(
            run(&mut cx, &mut ks, &[b"SUBSCRIBE", b"alpha"]),
            b">3\r\n$9\r\nsubscribe\r\n$5\r\nalpha\r\n:1\r\n"
        );
        // RESP3: other commands keep working while subscribed.
        assert_eq!(run(&mut cx, &mut ks, &[b"SET", b"k", b"v"]), b"+OK\r\n");
        // Self-delivery: the receiver count precedes the push frame
        // (oracle-pinned Redis order).
        assert_eq!(
            run(&mut cx, &mut ks, &[b"PUBLISH", b"alpha", b"msg"]),
            b":1\r\n>3\r\n$7\r\nmessage\r\n$5\r\nalpha\r\n$3\r\nmsg\r\n"
        );
        // Single-connection PUBSUB views.
        assert_eq!(run(&mut cx, &mut ks, &[b"PUBSUB", b"CHANNELS"]), b"*1\r\n$5\r\nalpha\r\n");
        assert_eq!(
            run(&mut cx, &mut ks, &[b"PUBSUB", b"NUMSUB", b"alpha", b"none"]),
            b"*4\r\n$5\r\nalpha\r\n:1\r\n$4\r\nnone\r\n:0\r\n"
        );
        assert_eq!(run(&mut cx, &mut ks, &[b"PUBSUB", b"NUMPAT"]), b":0\r\n");
        // Pattern subscription joins the count + pmessage self-delivery.
        run(&mut cx, &mut ks, &[b"PSUBSCRIBE", b"al*"]);
        assert_eq!(run(&mut cx, &mut ks, &[b"PUBSUB", b"NUMPAT"]), b":1\r\n");
        assert_eq!(
            run(&mut cx, &mut ks, &[b"PUBLISH", b"alpha", b"x"]),
            b":2\r\n>3\r\n$7\r\nmessage\r\n$5\r\nalpha\r\n$1\r\nx\r\n\
              >4\r\n$8\r\npmessage\r\n$3\r\nal*\r\n$5\r\nalpha\r\n$1\r\nx\r\n"
                .to_vec()
        );
        assert_eq!(
            run(&mut cx, &mut ks, &[b"PUBSUB", b"BOGUS"]),
            b"-ERR Unknown PUBSUB subcommand or wrong number of arguments for 'BOGUS'\r\n".to_vec()
        );
    }

    // ---- M1-E3 · eviction + OOM honesty ------------------------------------------

    #[test]
    fn oom_gate_denies_writes_allows_reads_and_recovers() {
        let mut cx = ConnCx::default();
        let mut ks = Keyspace::new(StoreConfig::default());
        run(&mut cx, &mut ks, &[b"SET", b"k", b"v"]);
        // maxmemory 1 byte: below the fixed floor, unfreeable — every
        // DENYOOM command answers the Redis-exact OOM error under
        // noeviction AND under volatile policies with nothing volatile.
        for policy in [
            &b"noeviction"[..],
            b"volatile-lru",
            b"volatile-random",
            b"volatile-ttl",
            b"volatile-lfu",
        ] {
            run(
                &mut cx,
                &mut ks,
                &[b"CONFIG", b"SET", b"maxmemory", b"1", b"maxmemory-policy", policy],
            );
            assert_eq!(
                run(&mut cx, &mut ks, &[b"SET", b"x", b"y"]),
                b"-OOM command not allowed when used memory > 'maxmemory'.\r\n",
                "policy {}",
                String::from_utf8_lossy(policy)
            );
            assert_eq!(
                run(&mut cx, &mut ks, &[b"INCR", b"ctr"]),
                b"-OOM command not allowed when used memory > 'maxmemory'.\r\n"
            );
            // Reads and freeing writes stay allowed (DENYOOM membership).
            assert_eq!(run(&mut cx, &mut ks, &[b"GET", b"k"]), b"$1\r\nv\r\n");
            assert_eq!(run(&mut cx, &mut ks, &[b"DEL", b"nope"]), b":0\r\n");
        }
        // allkeys-* with an unreachable floor: evicts what it can, then the
        // honest OOM (Redis behaves identically with maxmemory=1).
        run(&mut cx, &mut ks, &[b"CONFIG", b"SET", b"maxmemory-policy", b"allkeys-lru"]);
        assert_eq!(
            run(&mut cx, &mut ks, &[b"SET", b"x", b"y"]),
            b"-OOM command not allowed when used memory > 'maxmemory'.\r\n"
        );
        // Lifting the limit recovers immediately (hot-per-cell push at SET).
        run(&mut cx, &mut ks, &[b"CONFIG", b"SET", b"maxmemory", b"0"]);
        assert_eq!(run(&mut cx, &mut ks, &[b"SET", b"x", b"y"]), b"+OK\r\n");
    }

    #[test]
    fn config_maxmemory_has_observable_effect_within_one_round() {
        // The M1-S03 AC unblocked by E3: CONFIG SET maxmemory + an eviction
        // policy makes pressure observable without any plane in the loop —
        // the exec-layer push applies it to this cell immediately, and the
        // eviction MAINTAIN slice (driven here directly) frees to the
        // watermark.
        let mut cx = ConnCx::default();
        let mut ks = Keyspace::new(StoreConfig::default());
        for i in 0..500 {
            let key = format!("fill:{i}");
            run(&mut cx, &mut ks, &[b"SET", key.as_bytes(), &[0x61; 200]]);
        }
        let live = ks.report().records_live_bytes;
        let limit = ks.used_bytes() - live + live / 2;
        let limit_text = limit.to_string();
        run(&mut cx, &mut ks, &[b"CONFIG", b"SET", b"maxmemory-policy", b"allkeys-random"]);
        run(&mut cx, &mut ks, &[b"CONFIG", b"SET", b"maxmemory", limit_text.as_bytes()]);
        assert!(ks.over_limit(), "pressure visible immediately after CONFIG SET");
        let mut rounds = 0;
        while ks.over_limit() && rounds < 10_000 {
            ks.evict_tick(Nanos(1), inf_store::EvictBudget::default());
            rounds += 1;
        }
        assert!(ks.used_bytes() <= limit, "MAINTAIN slices must reach the new budget");
        let info = String::from_utf8(run(&mut cx, &mut ks, &[b"INFO", b"stats"])).expect("ascii");
        assert!(!info.contains("evicted_keys:0"), "evicted_keys must be real: {info}");
    }
}
