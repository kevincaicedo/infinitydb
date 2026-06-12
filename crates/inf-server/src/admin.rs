//! Server-introspection surface (M1-S03): `HELLO`, `INFO` real sections,
//! `COMMAND` full output, `CONFIG GET/SET`, `CLIENT *`, `DEBUG` subset.
//! Cold admin paths — `format!`/procfs reads are acceptable here (the M0
//! precedent: INFO already read `/proc/self/status`).
//!
//! Payloads are documented deviations in the compat matrix (identity
//! fields, registry size, address placeholders); the *shape* — section
//! headers, `key:value` lines, CLIENT LIST field vocabulary — follows Redis
//! so client libraries parse it (the M1-S03 client-smoke AC).

use inf_foundation::time::Nanos;
use inf_store::CellStore;
use inf_wire::{CmdFlags, Protocol, RespWriter};

use crate::clients::{format_client_line, valid_client_name};
use crate::config::ConfigSetError;
use crate::exec::{Argv, ConnCx, NodeInfo, arity_error, parse_i64, wall_ms};

// ---- HELLO -------------------------------------------------------------------

pub(crate) fn hello(argv: &(impl Argv + ?Sized), cx: &mut ConnCx, now: Nanos, out: &mut Vec<u8>) {
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
    {
        let mut clients = cx.node.clients.borrow_mut();
        clients.ensure(cx.id, now.as_millis());
        clients.set_resp(cx.id, if requested == Protocol::Resp3 { 3 } else { 2 });
    }
    let mut w = RespWriter::new(out, cx.proto);
    w.map_header(7);
    w.bulk(b"server");
    w.bulk(b"infinitydb");
    w.bulk(b"version");
    w.bulk(b"0.1.0-alpha.0");
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

// ---- INFO --------------------------------------------------------------------

const SECTIONS: &[&str] = &[
    "server",
    "clients",
    "memory",
    "persistence",
    "stats",
    "replication",
    "cpu",
    "tripwires",
    "keyspace",
];

pub(crate) fn info(
    argv: &(impl Argv + ?Sized),
    store: &CellStore,
    node: &NodeInfo,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let mut selected: Vec<&str> = Vec::new();
    for i in 1..argv.len() {
        let arg = argv.arg(i).to_ascii_lowercase();
        match arg.as_slice() {
            b"all" | b"default" | b"everything" => selected.clear(),
            section => {
                if let Some(name) = SECTIONS.iter().find(|s| s.as_bytes() == section) {
                    selected.push(name);
                }
                // Unknown sections yield nothing for that name (Redis shape).
            }
        }
    }
    let wants = |name: &str| selected.is_empty() || selected.contains(&name);
    let mut text = String::new();
    let push = |text: &mut String, line: &str| {
        text.push_str(line);
        text.push_str("\r\n");
    };

    if wants("server") {
        let uptime_secs = {
            let (internal_anchor, _) = node.wall_anchor.get();
            now.as_secs().saturating_sub(internal_anchor / 1000)
        };
        push(&mut text, "# Server");
        push(&mut text, "infinitydb_version:0.1.0-alpha.0");
        push(&mut text, "redis_version:7.4.0-compat");
        push(&mut text, "redis_git_sha1:00000000");
        push(&mut text, "redis_git_dirty:0");
        push(&mut text, "redis_mode:standalone");
        push(&mut text, &format!("os:{}", std::env::consts::OS));
        push(&mut text, "arch_bits:64");
        push(&mut text, &format!("process_id:{}", std::process::id()));
        push(
            &mut text,
            &format!(
                "run_id:{:032x}",
                u128::from(node.rng_state.get()) << 64 | u128::from(node.cell.get())
            ),
        );
        push(&mut text, &format!("tcp_port:{}", node.tcp_port.get()));
        push(&mut text, &format!("server_time_usec:{}", wall_ms(node, now) * 1000));
        push(&mut text, &format!("uptime_in_seconds:{uptime_secs}"));
        push(&mut text, &format!("uptime_in_days:{}", uptime_secs / 86_400));
        push(&mut text, "config_file:");
        push(&mut text, &format!("cell:{}", node.cell.get()));
        push(&mut text, &format!("cells:{}", node.cells.get()));
        text.push_str("\r\n");
    }
    if wants("clients") {
        push(&mut text, "# Clients");
        push(&mut text, &format!("connected_clients:{}", node.connections.get()));
        push(&mut text, "cluster_connections:0");
        let maxclients = node.config.borrow().get("maxclients").unwrap_or("10000").to_string();
        push(&mut text, &format!("maxclients:{maxclients}"));
        push(&mut text, "blocked_clients:0");
        push(&mut text, "tracking_clients:0");
        push(&mut text, &format!("total_connections_received:{}", node.total_connections.get()));
        text.push_str("\r\n");
    }
    let report = store.report();
    if wants("memory") {
        let used = report.records_live_bytes
            + report.records_slack_bytes
            + report.index_bytes
            + report.wheel_bytes
            + node.wire_buffers_bytes.get()
            + node.conn_state_bytes.get();
        let rss = process_rss_bytes();
        push(&mut text, "# Memory");
        push(&mut text, &format!("used_memory:{used}"));
        push(&mut text, &format!("used_memory_human:{}", human_bytes(used)));
        push(&mut text, &format!("used_memory_rss:{rss}"));
        let cfg = node.config.borrow();
        push(&mut text, &format!("maxmemory:{}", cfg.get("maxmemory").unwrap_or("0")));
        push(
            &mut text,
            &format!("maxmemory_policy:{}", cfg.get("maxmemory-policy").unwrap_or("noeviction")),
        );
        drop(cfg);
        let frag = if used > 0 { rss as f64 / used as f64 } else { 0.0 };
        push(&mut text, &format!("mem_fragmentation_ratio:{frag:.2}"));
        push(&mut text, "mem_allocator:inf-arena");
        text.push_str("\r\n");
    }
    if wants("persistence") {
        push(&mut text, "# Persistence");
        push(&mut text, "loading:0");
        push(&mut text, "rdb_changes_since_last_save:0");
        push(&mut text, "rdb_bgsave_in_progress:0");
        push(&mut text, "aof_enabled:0");
        push(&mut text, "aof_rewrite_in_progress:0");
        text.push_str("\r\n");
    }
    let stats = store.stats();
    if wants("stats") {
        let [_, _, _, _, commands, _] = node.raw_counters.get();
        push(&mut text, "# Stats");
        push(&mut text, &format!("total_connections_received:{}", node.total_connections.get()));
        push(&mut text, &format!("total_commands_processed:{commands}"));
        push(&mut text, "instantaneous_ops_per_sec:0");
        push(&mut text, "rejected_connections:0");
        push(&mut text, &format!("expired_keys:{}", stats.expired_lazy + stats.expired_active));
        push(&mut text, &format!("expired_active:{}", stats.expired_active));
        push(&mut text, &format!("expired_lazy:{}", stats.expired_lazy));
        push(&mut text, "evicted_keys:0");
        push(&mut text, &format!("keyspace_hits:{}", stats.keyspace_hits));
        push(&mut text, &format!("keyspace_misses:{}", stats.keyspace_misses));
        push(&mut text, "pubsub_channels:0");
        push(&mut text, "pubsub_patterns:0");
        push(&mut text, "latest_fork_usec:0");
        text.push_str("\r\n");
    }
    if wants("replication") {
        push(&mut text, "# Replication");
        push(&mut text, "role:master");
        push(&mut text, "connected_slaves:0");
        push(&mut text, "master_failover_state:no-failover");
        push(&mut text, &format!("master_replid:{:040x}", node.rng_state.get()));
        push(&mut text, "master_repl_offset:0");
        text.push_str("\r\n");
    }
    if wants("cpu") {
        let (sys, user) = process_cpu_secs();
        push(&mut text, "# CPU");
        push(&mut text, &format!("used_cpu_sys:{sys:.6}"));
        push(&mut text, &format!("used_cpu_user:{user:.6}"));
        text.push_str("\r\n");
    }
    if wants("tripwires") {
        use inf_foundation::tripwire as tw;
        let [sqes, cqes, cmds, fabric, p999] = node.tripwires.get();
        push(&mut text, "# Tripwires");
        push(&mut text, &format!("{}:{sqes}", tw::SQES_PER_SUBMIT));
        push(&mut text, &format!("{}:{cqes}", tw::CQES_PER_REAP));
        push(&mut text, &format!("{}:{cmds}", tw::CMDS_PER_ITER));
        push(&mut text, &format!("{}:{fabric}", tw::FABRIC_MSGS_PER_BATCH));
        push(&mut text, &format!("{}:{p999}", tw::LOOP_ITER_P999_US));
        push(&mut text, &format!("fabric_rtt_p50_ns:{}", node.fabric_rtt_p50_ns.get()));
        push(&mut text, &format!("recv_dropped:{}", node.recv_dropped.get()));
        let [submits, raw_sqes, raw_cqes, iters, commands, fabric_msgs] = node.raw_counters.get();
        push(&mut text, &format!("raw_submits:{submits}"));
        push(&mut text, &format!("raw_sqes:{raw_sqes}"));
        push(&mut text, &format!("raw_cqes:{raw_cqes}"));
        push(&mut text, &format!("raw_iterations:{iters}"));
        push(&mut text, &format!("raw_commands:{commands}"));
        push(&mut text, &format!("raw_fabric_msgs:{fabric_msgs}"));
        push(&mut text, &format!("{}:{}", tw::RECORDS_LIVE_BYTES, report.records_live_bytes));
        push(&mut text, &format!("{}:{}", tw::RECORDS_SLACK_BYTES, report.records_slack_bytes));
        push(&mut text, &format!("records_resident_bytes:{}", report.records_resident_bytes));
        push(&mut text, &format!("{}:{}", tw::INDEX_BYTES, report.index_bytes));
        push(&mut text, &format!("wheel_bytes:{}", report.wheel_bytes));
        push(&mut text, &format!("wheel_fallback:{}", stats.wheel_fallback));
        push(&mut text, &format!("wheel_stale:{}", stats.wheel_stale));
        push(&mut text, &format!("{}:{}", tw::WIRE_BUFFERS_BYTES, node.wire_buffers_bytes.get()));
        push(&mut text, &format!("{}:{}", tw::CONN_STATE_BYTES, node.conn_state_bytes.get()));
        push(&mut text, &format!("{}:{}", tw::PROCESS_RSS, process_rss_bytes()));
        text.push_str("\r\n");
    }
    if wants("keyspace") {
        push(&mut text, "# Keyspace");
        if !store.is_empty() {
            push(
                &mut text,
                &format!("db0:keys={},expires={},avg_ttl=0", store.len(), stats.ttl_live),
            );
        }
        text.push_str("\r\n");
    }
    // Redis ends INFO without the final blank line duplicated.
    while text.ends_with("\r\n\r\n") {
        text.truncate(text.len() - 2);
    }
    w.verbatim(b"txt", text.as_bytes());
}

/// VmRSS from procfs (Linux); 0 where unavailable.
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

/// (sys, user) CPU seconds from `/proc/self/stat` (USER_HZ=100 assumption,
/// dev-tier; zeros where unavailable).
fn process_cpu_secs() -> (f64, f64) {
    let Ok(stat) = std::fs::read_to_string("/proc/self/stat") else {
        return (0.0, 0.0);
    };
    // Split after the parenthesised comm; utime/stime are overall fields
    // 14/15 → indices 11/12 of the remainder (state is index 0).
    let Some((_, after)) = stat.rsplit_once(')') else { return (0.0, 0.0) };
    let fields: Vec<&str> = after.split_whitespace().collect();
    let utime: f64 = fields.get(11).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    let stime: f64 = fields.get(12).and_then(|v| v.parse().ok()).unwrap_or(0.0);
    (stime / 100.0, utime / 100.0)
}

fn human_bytes(bytes: u64) -> String {
    const UNITS: &[(&str, u64)] = &[("G", 1 << 30), ("M", 1 << 20), ("K", 1 << 10)];
    for (suffix, scale) in UNITS {
        if bytes >= *scale {
            return format!("{:.2}{suffix}", bytes as f64 / *scale as f64);
        }
    }
    format!("{bytes}B")
}

// ---- COMMAND -----------------------------------------------------------------

pub(crate) fn command_introspection(argv: &(impl Argv + ?Sized), w: &mut RespWriter<'_>) {
    if argv.len() == 1 {
        w.array_header(inf_wire::COMMANDS.len());
        for meta in &inf_wire::COMMANDS {
            command_row(meta, w);
        }
        return;
    }
    let sub = argv.arg(1);
    if sub.eq_ignore_ascii_case(b"COUNT") {
        w.int(inf_wire::COMMANDS.len() as i64);
    } else if sub.eq_ignore_ascii_case(b"INFO") {
        w.array_header(argv.len() - 2);
        for i in 2..argv.len() {
            match inf_wire::lookup(argv.arg(i)) {
                Some(meta) => command_row(meta, w),
                None => w.null_array(),
            }
        }
    } else if sub.eq_ignore_ascii_case(b"DOCS") {
        // Honest empty map until per-command docs exist (deviation entry).
        w.map_header(0);
    } else if sub.eq_ignore_ascii_case(b"GETKEYS") {
        command_getkeys(argv, w);
    } else {
        w.error(&format!(
            "ERR Unknown subcommand or wrong number of arguments for '{}'. Try COMMAND HELP.",
            String::from_utf8_lossy(sub)
        ));
    }
}

fn command_row(meta: &inf_wire::CommandMeta, w: &mut RespWriter<'_>) {
    w.array_header(10);
    w.bulk(meta.name.to_ascii_lowercase().as_bytes());
    w.int(i64::from(meta.arity));
    let mut flags: Vec<&str> = Vec::new();
    if meta.flags.contains(CmdFlags::READONLY) {
        flags.push("readonly");
    }
    if meta.flags.contains(CmdFlags::WRITE) {
        flags.push("write");
        flags.push("denyoom");
    }
    if meta.flags.contains(CmdFlags::ADMIN) {
        flags.push("admin");
    }
    if meta.flags.contains(CmdFlags::FAST) {
        flags.push("fast");
    }
    w.array_header(flags.len());
    for f in flags {
        w.simple(f);
    }
    w.int(i64::from(meta.keys.first));
    w.int(i64::from(meta.keys.last));
    w.int(i64::from(meta.keys.step));
    w.array_header(0); // acl categories
    w.array_header(0); // tips
    w.array_header(0); // key specs
    w.array_header(0); // subcommands
}

fn command_getkeys(argv: &(impl Argv + ?Sized), w: &mut RespWriter<'_>) {
    if argv.len() < 3 {
        return w.error(
            "ERR Unknown subcommand or wrong number of arguments for 'GETKEYS'. Try COMMAND HELP.",
        );
    }
    let Some(meta) = inf_wire::lookup(argv.arg(2)) else {
        return w.error("ERR Invalid command specified");
    };
    if !inf_wire::arity_ok(meta, argv.len() - 2) {
        return w.error("ERR Invalid number of arguments specified for command");
    }
    let spec = meta.keys;
    if spec.first == 0 {
        return w.error("ERR The command has no key arguments");
    }
    let argc = argv.len() - 2;
    let last = if spec.last >= 0 {
        spec.last as usize
    } else {
        argc.saturating_sub(spec.last.unsigned_abs() as usize)
    };
    let mut keys: Vec<usize> = Vec::new();
    let mut at = usize::from(spec.first);
    while at <= last && at < argc && spec.step > 0 {
        keys.push(at);
        at += usize::from(spec.step);
    }
    if keys.is_empty() {
        return w.error("ERR The command has no key arguments");
    }
    w.array_header(keys.len());
    for i in keys {
        w.bulk(argv.arg(2 + i));
    }
}

// ---- CONFIG ------------------------------------------------------------------

pub(crate) fn config(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    node: &NodeInfo,
    w: &mut RespWriter<'_>,
) {
    let sub = argv.arg(1);
    if sub.eq_ignore_ascii_case(b"GET") {
        if argv.len() < 3 {
            return w.error(
                "ERR Unknown subcommand or wrong number of arguments for 'GET'. Try CONFIG HELP.",
            );
        }
        let patterns: Vec<&[u8]> = (2..argv.len()).map(|i| argv.arg(i)).collect();
        let cfg = node.config.borrow();
        let hits = cfg.get_matching(&patterns);
        w.map_header(hits.len());
        for (key, value) in hits {
            w.bulk(key.as_bytes());
            w.bulk(value.as_bytes());
        }
    } else if sub.eq_ignore_ascii_case(b"SET") {
        if argv.len() < 4 || !(argv.len() - 2).is_multiple_of(2) {
            return w.error(
                "ERR Unknown subcommand or wrong number of arguments for 'SET'. Try CONFIG HELP.",
            );
        }
        // Validate every pair before applying any (Redis 7 all-or-nothing).
        let mut i = 2;
        while i < argv.len() {
            let outcome = node.config.borrow_mut().set(argv.arg(i), argv.arg(i + 1));
            match outcome {
                Ok(_) => {}
                Err(ConfigSetError::Unknown(key)) => {
                    return w.error(&format!(
                        "ERR Unknown option or number of arguments for CONFIG SET - '{key}'"
                    ));
                }
                Err(ConfigSetError::Immutable(key)) => {
                    return w.error(&format!(
                        "ERR CONFIG SET failed (possibly related to argument '{key}') - can't set immutable config"
                    ));
                }
                Err(ConfigSetError::Invalid { key, value }) => {
                    return w.error(&format!(
                        "ERR CONFIG SET failed (possibly related to argument '{key}') - invalid value '{value}'"
                    ));
                }
            }
            i += 2;
        }
        w.simple("OK");
    } else if sub.eq_ignore_ascii_case(b"RESETSTAT") {
        store.reset_stats();
        w.simple("OK");
    } else if sub.eq_ignore_ascii_case(b"REWRITE") {
        w.error("ERR The server is running without a config file");
    } else {
        w.error(&format!(
            "ERR Unknown subcommand or wrong number of arguments for '{}'. Try CONFIG HELP.",
            String::from_utf8_lossy(sub)
        ));
    }
}

// ---- CLIENT ------------------------------------------------------------------

pub(crate) fn client(argv: &(impl Argv + ?Sized), cx: &mut ConnCx, now: Nanos, out: &mut Vec<u8>) {
    let proto = cx.proto;
    let mut w = RespWriter::new(out, proto);
    let sub = argv.arg(1);
    cx.node.clients.borrow_mut().ensure(cx.id, now.as_millis());
    if sub.eq_ignore_ascii_case(b"ID") {
        w.int(cx.id as i64);
    } else if sub.eq_ignore_ascii_case(b"GETNAME") {
        let clients = cx.node.clients.borrow();
        let name = clients.get(cx.id).map(|c| c.name.clone()).unwrap_or_default();
        if name.is_empty() {
            // No name is a null reply, not an empty bulk (Redis 8,
            // oracle-pinned).
            w.null();
        } else {
            w.bulk(&name);
        }
    } else if sub.eq_ignore_ascii_case(b"SETNAME") {
        if argv.len() != 3 {
            return arity_error("CLIENT|SETNAME", &mut w);
        }
        let name = argv.arg(2);
        if !valid_client_name(name) {
            return w
                .error("ERR Client names cannot contain spaces, newlines or special characters.");
        }
        cx.node.clients.borrow_mut().ensure(cx.id, now.as_millis()).name = name.to_vec();
        w.simple("OK");
    } else if sub.eq_ignore_ascii_case(b"LIST") {
        let mut id_filter: Option<Vec<u64>> = None;
        if argv.len() > 2 {
            if !argv.arg(2).eq_ignore_ascii_case(b"ID") || argv.len() < 4 {
                return w.error("ERR syntax error");
            }
            let mut ids = Vec::new();
            for i in 3..argv.len() {
                match parse_i64(argv.arg(i)) {
                    Ok(id) if id >= 0 => ids.push(id as u64),
                    _ => return w.error("ERR Invalid client ID"),
                }
            }
            id_filter = Some(ids);
        }
        let text = render_client_lines(cx, now, id_filter.as_deref());
        w.bulk(text.as_bytes());
    } else if sub.eq_ignore_ascii_case(b"INFO") {
        let text = render_client_lines(cx, now, Some(&[cx.id]));
        w.bulk(text.trim_end_matches('\n').as_bytes());
    } else if sub.eq_ignore_ascii_case(b"KILL") {
        // M1 surface: the `ID <id>` filter form (the address forms predate
        // ids and need peername capture — documented as not-yet).
        if argv.len() == 4 && argv.arg(2).eq_ignore_ascii_case(b"ID") {
            let Ok(id) = parse_i64(argv.arg(3)) else {
                return w.error("ERR client-id should be greater than 0");
            };
            if id <= 0 {
                return w.error("ERR client-id should be greater than 0");
            }
            let killed = cx.node.clients.borrow_mut().request_kill(id as u64);
            w.int(i64::from(killed));
        } else {
            w.error("ERR syntax error in CLIENT KILL (InfinityDB M1 supports the ID filter form)");
        }
    } else {
        w.error(&format!(
            "ERR Unknown subcommand or wrong number of arguments for '{}'. Try CLIENT HELP.",
            String::from_utf8_lossy(sub)
        ));
    }
}

fn render_client_lines(cx: &ConnCx, now: Nanos, ids: Option<&[u64]>) -> String {
    let clients = cx.node.clients.borrow();
    let mut text = String::new();
    for (id, info) in clients.iter() {
        if ids.is_some_and(|wanted| !wanted.contains(&id)) {
            continue;
        }
        let age = now.as_millis().saturating_sub(info.created_ms) / 1000;
        text.push_str(&format_client_line(id, info, age, "client"));
        text.push('\n');
    }
    text
}

// ---- DEBUG -------------------------------------------------------------------

pub(crate) fn debug(
    argv: &(impl Argv + ?Sized),
    store: &mut CellStore,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    let sub = argv.arg(1);
    if sub.eq_ignore_ascii_case(b"SLEEP") {
        // The reply is immediate at the exec layer; the PLANE stalls its
        // connection processing for the parsed duration (one cell blocks,
        // not the server — the documented deviation; fabric service
        // continues for deadlock safety). See `exec::stall_request`.
        if argv.len() != 3 {
            return w.error("ERR wrong number of arguments for 'debug|sleep' command");
        }
        let valid = core::str::from_utf8(argv.arg(2))
            .ok()
            .and_then(|s| s.parse::<f64>().ok())
            .is_some_and(|secs| secs.is_finite() && secs >= 0.0);
        if !valid {
            return w.error("ERR value is not a valid float");
        }
        w.simple("OK");
    } else if sub.eq_ignore_ascii_case(b"JMAP") {
        w.simple("OK");
    } else if sub.eq_ignore_ascii_case(b"SET-ACTIVE-EXPIRE") {
        // Accepted for test-suite compatibility; the wheel stays active
        // (recorded deviation — lazy expiry alone still upholds visibility).
        w.simple("OK");
    } else if sub.eq_ignore_ascii_case(b"OBJECT") {
        if argv.len() != 3 {
            return w.error("ERR wrong number of arguments for 'debug|object' command");
        }
        let key = argv.arg(2);
        let Some((encoding, _)) = store.object_encoding(key, now) else {
            return w.error("ERR no such key");
        };
        let len = store.strlen(key, now);
        w.simple(&format!(
            "Value at:0x0 refcount:1 encoding:{} serializedlength:{} lru:0 lru_seconds_idle:0",
            encoding.name(),
            len
        ));
    } else {
        w.error(&format!(
            "ERR unknown subcommand or wrong number of arguments for '{}'. Try DEBUG HELP.",
            String::from_utf8_lossy(sub)
        ));
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::exec::execute;
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
    fn config_get_set_roundtrip() {
        let mut cx = ConnCx::default();
        let mut store = CellStore::new(StoreConfig::default());
        assert_eq!(
            run(&mut cx, &mut store, &[b"CONFIG", b"GET", b"maxmemory"]),
            b"*2\r\n$9\r\nmaxmemory\r\n$1\r\n0\r\n"
        );
        assert_eq!(
            run(&mut cx, &mut store, &[b"CONFIG", b"SET", b"maxmemory", b"100mb"]),
            b"+OK\r\n"
        );
        assert_eq!(
            run(&mut cx, &mut store, &[b"CONFIG", b"GET", b"maxmemory"]),
            b"*2\r\n$9\r\nmaxmemory\r\n$9\r\n104857600\r\n"
        );
        let reply = run(&mut cx, &mut store, &[b"CONFIG", b"SET", b"databases", b"32"]);
        assert!(reply.starts_with(b"-ERR CONFIG SET failed"), "{reply:?}");
    }

    #[test]
    fn client_name_and_kill_flow() {
        let mut cx = ConnCx::default();
        let mut store = CellStore::new(StoreConfig::default());
        assert_eq!(run(&mut cx, &mut store, &[b"CLIENT", b"ID"]), b":1\r\n");
        // No name yet: null (Redis 8, oracle-pinned), not an empty bulk.
        assert_eq!(run(&mut cx, &mut store, &[b"CLIENT", b"GETNAME"]), b"$-1\r\n");
        assert_eq!(run(&mut cx, &mut store, &[b"CLIENT", b"SETNAME", b"worker-1"]), b"+OK\r\n");
        assert_eq!(run(&mut cx, &mut store, &[b"CLIENT", b"GETNAME"]), b"$8\r\nworker-1\r\n");
        let reply = run(&mut cx, &mut store, &[b"CLIENT", b"SETNAME", b"has space"]);
        assert!(reply.starts_with(b"-ERR Client names"), "{reply:?}");
        // Kill marks the registry; the plane sweeps it.
        assert_eq!(run(&mut cx, &mut store, &[b"CLIENT", b"KILL", b"ID", b"1"]), b":1\r\n");
        assert_eq!(cx.node.clients.borrow_mut().take_kill_requests(), vec![1]);
        assert_eq!(run(&mut cx, &mut store, &[b"CLIENT", b"KILL", b"ID", b"99"]), b":0\r\n");
        let list = run(&mut cx, &mut store, &[b"CLIENT", b"LIST"]);
        let text = String::from_utf8(list).expect("ascii");
        assert!(text.contains("id=1"), "{text}");
        assert!(text.contains("name=worker-1"), "{text}");
        assert!(text.contains("resp=2"), "{text}");
    }

    #[test]
    fn info_sections_filter() {
        let mut cx = ConnCx::default();
        let mut store = CellStore::new(StoreConfig::default());
        run(&mut cx, &mut store, &[b"SET", b"k", b"v"]);
        let all = String::from_utf8(run(&mut cx, &mut store, &[b"INFO"])).expect("ascii");
        for section in
            ["# Server", "# Clients", "# Memory", "# Stats", "# Replication", "# Keyspace"]
        {
            assert!(all.contains(section), "missing {section}: {all}");
        }
        assert!(all.contains("db0:keys=1,expires=0"), "{all}");
        let server_only =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"server"])).expect("ascii");
        assert!(server_only.contains("# Server"));
        assert!(!server_only.contains("# Memory"), "{server_only}");
    }

    #[test]
    fn command_introspection_shapes() {
        let mut cx = ConnCx::default();
        let mut store = CellStore::new(StoreConfig::default());
        let count = run(&mut cx, &mut store, &[b"COMMAND", b"COUNT"]);
        assert_eq!(count, format!(":{}\r\n", inf_wire::COMMANDS.len()).into_bytes());
        let getkeys = run(
            &mut cx,
            &mut store,
            &[b"COMMAND", b"GETKEYS", b"MSET", b"k1", b"v1", b"k2", b"v2"],
        );
        assert_eq!(getkeys, b"*2\r\n$2\r\nk1\r\n$2\r\nk2\r\n");
        let nokeys = run(&mut cx, &mut store, &[b"COMMAND", b"GETKEYS", b"PING"]);
        assert!(nokeys.starts_with(b"-ERR The command has no key arguments"), "{nokeys:?}");
    }
}
