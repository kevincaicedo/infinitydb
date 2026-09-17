//! `INFO` — the section renderers. Scope trap (documented, measured):
//! the Memory section is a node fold, Tiering/Persistence/Tripwires are
//! cell-scope; a reader multiplies by cells before comparing to RSS.

use super::*;

// ---- INFO --------------------------------------------------------------------

const SECTIONS: &[&str] = &[
    "server",
    "clients",
    "memory",
    "persistence",
    "tiering",
    "stats",
    "replication",
    "cpu",
    "tripwires",
    "loophist",
    "keyspace",
];

pub(crate) fn info(
    argv: &(impl Argv + ?Sized),
    ks: &Keyspace,
    node: &NodeInfo,
    now: Nanos,
    w: &mut RespWriter<'_>,
) {
    // Redis's section set: a bare `INFO` (or `all`/`default`/`everything`
    // anywhere in argv) is every section; otherwise only the named ones,
    // and a name this build lacks selects nothing — `INFO nosuchsection`
    // is an empty body (F-L15-10: "nothing selected" once meant
    // "everything", so an argv of only unknown names rendered it all).
    let mut selected: Vec<&str> = Vec::new();
    let mut everything = argv.len() == 1;
    for i in 1..argv.len() {
        let arg = argv.arg(i).to_ascii_lowercase();
        match arg.as_slice() {
            b"all" | b"default" | b"everything" => everything = true,
            section => {
                if let Some(name) = SECTIONS.iter().find(|s| s.as_bytes() == section) {
                    selected.push(name);
                }
            }
        }
    }
    let wants = |name: &str| everything || selected.contains(&name);
    let mut text = String::new();

    if wants("server") {
        server_section(&mut text, node, now);
    }
    if wants("clients") {
        clients_section(&mut text, node);
    }
    #[cfg(feature = "doc")]
    let report = {
        let mut report = ks.report();
        node.add_cell_doc_memory(&mut report);
        report
    };
    #[cfg(not(feature = "doc"))]
    let report = ks.report();
    // M3-S25 attribution fix: `used_memory_rss` is process-wide, so the
    // byte gauges beside it must be node-wide too. The serving cell
    // publishes its fresh gauges and folds the board (peers lag their
    // MAINTAIN publish by at most one period); without a board (bare
    // harness) the sections render cell scope and say so. One fold serves
    // `# Memory` and `# Keyspace` (ADR-0122 A2).
    let fold = if wants("memory") || wants("keyspace") {
        let local = crate::exec::memory_gauges_of(&report, node, ks);
        Some(match node.publish_and_total_memory(local) {
            Some(totals) => ("node", totals),
            None => ("cell", local),
        })
    } else {
        None
    };
    if let Some((scope, g)) = fold.filter(|_| wants("memory")) {
        memory_section(&mut text, node, scope, &g);
    }
    if wants("persistence") {
        persistence_section(&mut text, node, now);
    }
    if wants("tiering") {
        tiering_section(ks, node, &mut text);
    }
    let stats = ks.stats();
    if wants("stats") {
        stats_section(&mut text, node, ks, &stats, now);
    }
    if wants("replication") {
        replication_section(&mut text, node);
    }
    if wants("cpu") {
        cpu_section(&mut text);
    }
    if wants("tripwires") {
        tripwires_section(&mut text, node, &report, &stats);
    }
    // Explicit only: ordinary INFO must not request or serialize 1920 buckets.
    if selected.contains(&"loophist") {
        node.loop_snapshot.append_info(&mut text);
    }
    if let Some((scope, g)) = fold.filter(|_| wants("keyspace")) {
        keyspace_section(&mut text, scope, &g);
    }
    // Redis ends INFO without the final blank line duplicated.
    while text.ends_with("\r\n\r\n") {
        text.truncate(text.len() - 2);
    }
    w.verbatim(b"txt", text.as_bytes());
}

/// Appends one `field:value` line with the RESP line terminator.
fn push(text: &mut String, line: &str) {
    text.push_str(line);
    text.push_str("\r\n");
}

/// `INFO` — the server lines.
fn server_section(text: &mut String, node: &NodeInfo, now: Nanos) {
    let uptime_secs = {
        let (internal_anchor, _) = node.wall_anchor.get();
        now.as_secs().saturating_sub(internal_anchor / 1000)
    };
    push(text, "# Server");
    push(text, "infinitydb_version:0.1.0-alpha.0");
    push(text, "redis_version:7.4.0-compat");
    push(text, "redis_git_sha1:00000000");
    push(text, "redis_git_dirty:0");
    push(text, "redis_mode:standalone");
    push(text, &format!("os:{}", std::env::consts::OS));
    push(text, "arch_bits:64");
    push(text, &format!("process_id:{}", node.process_id.get()));
    push(text, &format!("run_id:{}", render_run_id(node)));
    push(text, &format!("server_time_usec:{}", wall_ms(node, now) * 1000));
    push(text, &format!("uptime_in_seconds:{uptime_secs}"));
    push(text, &format!("uptime_in_days:{}", uptime_secs / 86_400));
    push(text, "config_file:");
    push(text, &format!("cell:{}", node.cell.get()));
    push(text, &format!("cells:{}", node.cells.get()));
    text.push_str("\r\n");
}

/// `INFO` — the clients lines.
fn clients_section(text: &mut String, node: &NodeInfo) {
    push(text, "# Clients");
    push(text, &format!("connected_clients:{}", node.connections.get()));
    push(text, "cluster_connections:0");
    let maxclients = node.config.borrow().get("maxclients").unwrap_or("10000").to_string();
    push(text, &format!("maxclients:{maxclients}"));
    push(text, "blocked_clients:0");
    push(text, "tracking_clients:0");
    text.push_str("\r\n");
}

/// `INFO` — the memory lines.
fn memory_section(
    text: &mut String,
    node: &NodeInfo,
    scope: &str,
    g: &crate::control::MemoryGauges,
) {
    let used = g.used_bytes;
    let rss = process_rss_bytes();
    push(text, "# Memory");
    push(text, &format!("used_memory:{used}"));
    push(text, &format!("used_memory_human:{}", human_bytes(used)));
    push(text, &format!("used_memory_rss:{rss}"));
    // The frozen L5 denominator, from the same read as `used_memory_rss`:
    // process-wide, so it lives here beside the node fold, never in the
    // cell-scope `# Tripwires` (ADR-0122 A1, F-L15-07).
    push(text, &format!("{}:{rss}", inf_foundation::tripwire::PROCESS_RSS));
    push(text, &format!("memory_scope:{scope}"));
    let cfg = node.config.borrow();
    push(text, &format!("maxmemory:{}", cfg.get("maxmemory").unwrap_or("0")));
    push(
        text,
        &format!("maxmemory_policy:{}", cfg.get("maxmemory-policy").unwrap_or("noeviction")),
    );
    drop(cfg);
    // The figure `maxmemory` compares against (ADR-0068 A2): the
    // pool's logical bytes — numbered dbs plus every namespace without
    // a budget of its own — folded like `used_memory`. `used_memory`
    // carries wire buffers, arena slack and budgeted namespaces, so it
    // never says how far the node is from eviction; this does.
    push(text, &format!("used_memory_pool:{}", g.pool_used_bytes));
    let frag = if used > 0 { rss as f64 / used as f64 } else { 0.0 };
    push(text, &format!("mem_fragmentation_ratio:{frag:.2}"));
    push(text, "mem_allocator:inf-arena");
    // The attribution fold under the `used_memory_*` family (ADR-0122
    // D3, F-L15-06): the frozen tripwire names stay cell-scope in
    // `# Tripwires`; one name never carries two scopes in one reply.
    push(text, &format!("used_memory_doc_tape:{}", g.doc_tape_bytes));
    push(text, &format!("used_memory_doc_arena:{}", g.doc_arena_bytes));
    push(text, &format!("used_memory_doc_resident:{}", g.doc_resident_bytes));
    push(text, &format!("used_memory_doc_intern:{}", g.doc_intern_bytes));
    push(text, &format!("used_memory_doc_slack:{}", g.doc_slack_bytes));
    push(text, &format!("used_memory_doc_scratch:{}", g.doc_scratch_bytes));
    push(text, &format!("used_memory_doc_path_cache:{}", g.doc_path_cache_bytes));
    push(text, &format!("docs_live:{}", g.docs_live));
    // Index-tree domains (M4.5-S03, ADR-0075 D6): counted in
    // used_memory and the namespace budgets — never a second ledger.
    push(text, &format!("used_memory_idx_tree:{}", g.idx_tree_bytes));
    push(text, &format!("used_memory_idx_slack:{}", g.idx_slack_bytes));
    text.push_str("\r\n");
}

/// `INFO` — the persistence lines.
fn persistence_section(text: &mut String, node: &NodeInfo, now: Nanos) {
    push(text, "# Persistence");
    // Boot-recovery fields (M2-S15): shapes mirror Redis (`loading:1`
    // plus `loading_*` while a load is in progress — capture artifact
    // `.artifacts/m2/loading-redis-capture-20260703/`); byte totals
    // are file extents including preallocated slack (upper bound).
    let loading = node.loading.get() != 0;
    push(text, &format!("loading:{}", u8::from(loading)));
    if loading {
        let (anchor_internal_ms, anchor_unix_ms) = node.wall_anchor.get();
        let wall_now_ms = anchor_unix_ms + now.as_millis().saturating_sub(anchor_internal_ms);
        let start_ms = node.loading_start_unix_ms.get();
        let done = node.loading_loaded_bytes.get();
        let total = node.loading_total_bytes.get();
        let elapsed_ms = wall_now_ms.saturating_sub(start_ms);
        let perc = if total > 0 { done as f64 * 100.0 / total as f64 } else { 0.0 };
        let eta_s = if done > 0 {
            let remaining = total.saturating_sub(done) as f64;
            (elapsed_ms as f64 / 1000.0 * remaining / done as f64).ceil() as u64
        } else {
            0
        };
        push(text, &format!("loading_start_time:{}", start_ms / 1000));
        push(text, &format!("loading_total_bytes:{total}"));
        push(text, &format!("loading_loaded_bytes:{done}"));
        push(text, &format!("loading_loaded_perc:{perc:.2}"));
        push(text, &format!("loading_eta_seconds:{eta_s}"));
        // Extension fields (per-cell recovery is an InfinityDB shape).
        push(text, &format!("loading_cells_ready:{}", node.loading_cells_ready.get()));
        push(text, &format!("loading_cells:{}", node.cells.get()));
    }
    push(text, "rdb_changes_since_last_save:0");
    // M2-S20: BGSAVE maps onto the fuzzy checkpoint (no fork); the
    // save time is the newest durable MANIFEST publication (board
    // max across cells, unix seconds — the LASTSAVE currency).
    push(text, &format!("rdb_bgsave_in_progress:{}", node.ckpt_in_progress.get()));
    push(text, &format!("rdb_last_save_time:{}", node.rdb_last_save_ms.get() / 1000));
    // ADR-0100 D7: durable namespaces dropped since every cell last
    // published a MANIFEST past the drop (node scope).
    push(text, &format!("ns_drop_tombstones:{}", node.ns_drop_tombstones.get()));
    push(text, "aof_enabled:0");
    push(text, "aof_rewrite_in_progress:0");
    log_gauge_lines(text, node);
    fsync_gauge_lines(text, node);
    barrier_gauge_lines(text, node);
    io_budget_lines(text, node);
    recycle_gauge_lines(text, node);
    recover_gauge_lines(text, node);
    ckpt_gauge_lines(text, node);
    text.push_str("\r\n");
}

/// `INFO` — the stats lines.
fn stats_section(
    text: &mut String,
    node: &NodeInfo,
    ks: &Keyspace,
    stats: &inf_store::StoreStats,
    now: Nanos,
) {
    let [_, _, _, _, commands, _] = node.raw_counters.get();
    push(text, "# Stats");
    push(text, &format!("total_connections_received:{}", node.total_connections.get()));
    push(text, &format!("total_commands_processed:{commands}"));
    push(text, "instantaneous_ops_per_sec:0");
    push(text, &format!("rejected_connections:{}", node.rejected_connections.get()));
    push(text, &format!("accept_errors:{}", node.accept_errors.get()));
    push(text, &format!("expired_keys:{}", stats.expired_lazy + stats.expired_active));
    push(text, &format!("expired_active:{}", stats.expired_active));
    push(text, &format!("expired_lazy:{}", stats.expired_lazy));
    // The M1-S05 `expiry_debt` backlog, cell scope like the counters
    // beside it: the worst wheel debt across every store, served by
    // the last slice or not (F-L05-03).
    push(text, &format!("expiry_debt_ms:{}", ks.expiry_lag_ms(now)));
    push(text, &format!("evicted_keys:{}", stats.evicted_keys));
    push(text, &format!("keyspace_hits:{}", stats.keyspace_hits));
    push(text, &format!("keyspace_misses:{}", stats.keyspace_misses));
    // M4.5-S40: stop-and-copy index grows on this cell's flat stores
    // (a foreground latency step per doubling; the S40 timeline's event).
    push(text, &format!("index_grows:{}", stats.index_grows));
    // M3-S10: per-cell path-program cache (extension fields).
    #[cfg(feature = "doc")]
    {
        let cache = node.path_cache.borrow();
        push(text, &format!("path_cache_hits:{}", cache.hits()));
        push(text, &format!("path_cache_misses:{}", cache.misses()));
        push(text, &format!("path_cache_evictions:{}", cache.evictions()));
    }
    index_stat_lines(text, ks);
    push(text, &format!("pubsub_channels:{}", node.pubsub_channels.get()));
    push(text, &format!("pubsub_patterns:{}", node.pubsub_patterns.get()));
    push(
        text,
        &format!("client_output_buffer_limit_disconnections:{}", node.cob_disconnections.get()),
    );
    // ADR-0123 D2: idle closes under `timeout` (cell scope, like the
    // output-cap kills above).
    push(text, &format!("idle_disconnections:{}", node.idle_disconnections.get()));
    push(text, "latest_fork_usec:0");
}

/// `INFO` — the replication lines.
fn replication_section(text: &mut String, node: &NodeInfo) {
    push(text, "# Replication");
    push(text, "role:master");
    push(text, "connected_slaves:0");
    push(text, "master_failover_state:no-failover");
    push(text, &format!("master_replid:{}", render_run_id(node)));
    push(text, "master_repl_offset:0");
    text.push_str("\r\n");
}

/// `INFO` — the cpu lines.
fn cpu_section(text: &mut String) {
    let (sys, user) = process_cpu_secs();
    push(text, "# CPU");
    push(text, &format!("used_cpu_sys:{sys:.6}"));
    push(text, &format!("used_cpu_user:{user:.6}"));
    text.push_str("\r\n");
}

/// `INFO` — the tripwires lines.
fn tripwires_section(
    text: &mut String,
    node: &NodeInfo,
    report: &inf_store::MemoryReport,
    stats: &inf_store::StoreStats,
) {
    use inf_foundation::tripwire as tw;
    let [sqes, cqes, cmds, fabric, p999] = node.tripwires.get();
    push(text, "# Tripwires");
    // Every gauge below is this cell's slice (`inf-bench` scrapes each
    // cell and sums); the process-wide `process_rss` renders in
    // `# Memory` with the node fold (ADR-0122 A1).
    push(text, "tripwire_scope:cell");
    push(text, &format!("{}:{sqes}", tw::SQES_PER_SUBMIT));
    push(text, &format!("{}:{cqes}", tw::CQES_PER_REAP));
    push(text, &format!("{}:{cmds}", tw::CMDS_PER_ITER));
    push(text, &format!("{}:{fabric}", tw::FABRIC_MSGS_PER_BATCH));
    push(text, &format!("{}:{p999}", tw::LOOP_ITER_P999_US));
    push(text, &format!("fabric_rtt_p50_ns:{}", node.fabric_rtt_p50_ns.get()));
    push(text, &format!("recv_dropped:{}", node.recv_dropped.get()));
    let [submits, raw_sqes, raw_cqes, iters, commands, fabric_msgs] = node.raw_counters.get();
    push(text, &format!("raw_submits:{submits}"));
    push(text, &format!("raw_sqes:{raw_sqes}"));
    push(text, &format!("raw_cqes:{raw_cqes}"));
    push(text, &format!("raw_iterations:{iters}"));
    push(text, &format!("raw_commands:{commands}"));
    push(text, &format!("raw_fabric_msgs:{fabric_msgs}"));
    push(text, &format!("{}:{}", tw::RECORDS_LIVE_BYTES, report.records_live_bytes));
    push(text, &format!("{}:{}", tw::RECORDS_SLACK_BYTES, report.records_slack_bytes));
    push(text, &format!("records_resident_bytes:{}", report.records_resident_bytes));
    push(text, &format!("{}:{}", tw::INDEX_BYTES, report.index_bytes));
    push(text, &format!("{}:{}", tw::WHEEL_BYTES, report.wheel_bytes));
    push(text, &format!("{}:{}", tw::EVICT_BYTES, report.evict_bytes));
    push(text, &format!("{}:{}", tw::DOC_TAPE_BYTES, report.doc_tape_bytes));
    push(text, &format!("{}:{}", tw::DOC_ARENA_BYTES, report.doc_arena_bytes));
    push(text, &format!("{}:{}", tw::DOC_RESIDENT_BYTES, report.doc_resident_bytes));
    push(text, &format!("{}:{}", tw::DOC_INTERN_BYTES, report.doc_intern_bytes));
    push(text, &format!("{}:{}", tw::DOC_SLACK_BYTES, report.doc_slack_bytes));
    push(text, &format!("{}:{}", tw::DOC_SCRATCH_BYTES, report.doc_scratch_bytes));
    push(text, &format!("{}:{}", tw::DOC_PATH_CACHE_BYTES, report.doc_path_cache_bytes));
    push(text, &format!("idx_tree_bytes:{}", report.idx_tree_bytes));
    push(text, &format!("idx_slack_bytes:{}", report.idx_slack_bytes));
    push(text, &format!("wheel_fallback:{}", stats.wheel_fallback));
    push(text, &format!("wheel_stale:{}", stats.wheel_stale));
    push(text, &format!("pubsub_fan_msgs:{}", node.pubsub_fan_msgs.get()));
    push(text, &format!("pubsub_delivered:{}", node.pubsub_delivered.get()));
    push(text, &format!("pubsub_state_bytes:{}", node.pubsub_state_bytes.get()));
    push(text, &format!("{}:{}", tw::WIRE_BUFFERS_BYTES, node.wire_buffers_bytes.get()));
    push(text, &format!("{}:{}", tw::CONN_STATE_BYTES, node.conn_state_bytes.get()));
    // Recycle-pool residency (v0.4.0-alpha RSS-attribution gauges):
    // the reply/command pools were the last unattributed malloc
    // consumers — the warm-up-grower hypothesis instrument, read
    // against `process_rss` over a soak.
    push(text, &format!("reply_pool_bytes:{}", node.reply_pool_bytes.get()));
    push(text, &format!("cmd_pool_bytes:{}", node.cmd_pool_bytes.get()));
    push(text, &format!("cold_pool_bytes:{}", node.cold_pool_bytes.get()));
    push(text, &format!("loop_snapshot_bytes:{}", node.loop_snapshot.reserved_bytes()));
    text.push_str("\r\n");
}

/// `INFO` — the keyspace lines.
fn keyspace_section(text: &mut String, scope: &str, g: &crate::control::MemoryGauges) {
    push(text, "# Keyspace");
    // The node fold, like `DBSIZE` (ADR-0122 A2, F-L15-03): one line
    // per db with a nonzero folded count (Redis shape), scope first.
    push(text, &format!("keyspace_scope:{scope}"));
    for (db, (keys, expires)) in g.db_keys.iter().zip(g.db_expires).enumerate() {
        if *keys != 0 {
            push(text, &format!("db{db}:keys={keys},expires={expires},avg_ttl=0"));
        }
    }
    text.push_str("\r\n");
}

/// `INFO` — the log gauge lines.
fn log_gauge_lines(text: &mut String, node: &NodeInfo) {
    // Durable-namespace gauges (M2-S08, this cell's slice — the S21
    // counter set; control-plane aggregation lands with S21).
    push(text, &format!("log_records_appended:{}", node.log_records_appended.get()));
    push(text, &format!("pending_log_bytes:{}", node.log_pending_bytes.get()));
    push(text, &format!("last_durable_lsn:{}", node.log_last_durable_lsn.get()));
    push(text, &format!("watermark_lag_lsn:{}", node.log_watermark_lag.get()));
    push(text, &format!("fsyncs_completed:{}", node.log_fsyncs_completed.get()));
    push(text, &format!("acks_gated:{}", node.log_acks_gated.get()));
    // M2-S22: frames queued (log_writes_per_iter numerator) + the
    // staging domain's resident bytes (attribution observable).
    push(text, &format!("log_frames_queued:{}", node.log_frames_queued.get()));
    push(text, &format!("log_staging_bytes:{}", node.log_staging_bytes.get()));
    // Typed `-BUSY` staging-admission refusals (v0.4.0-alpha
    // instrument fix; M4.5-S27 re-scoped it to client-visible
    // refusals only — the doc exact late admission is the one
    // remaining emitter, so a climbing rate here is a finding).
    push(text, &format!("log_admission_busy:{}", node.log_admission_busy.get()));
    // M4.5-S27 (ADR-0083 D2/D5): the pacing observables. Parks are
    // backpressure working as designed; `oversized` is the typed
    // never-fits refusal; the write-stall percentiles are the
    // staging drain's binding variable (frame-write submit →
    // LogWritten — under kernel writeback throttling this is what
    // starves staging, and fsync latency is the correlated symptom).
    push(text, &format!("log_admission_oversized:{}", node.log_admission_oversized.get()));
    push(text, &format!("log_admission_parked:{}", node.log_admission_parked.get()));
    push(text, &format!("log_admission_parked_total:{}", node.log_admission_parked_total.get()));
    push(text, &format!("log_staging_capacity_bytes:{}", node.log_staging_capacity.get()));
    push(text, &format!("log_write_stall_p50_us:{}", node.log_write_stall_p50_us.get()));
    push(text, &format!("log_write_stall_p99_us:{}", node.log_write_stall_p99_us.get()));
    push(text, &format!("log_write_stall_p999_us:{}", node.log_write_stall_p999_us.get()));
}

/// `INFO` — the fsync gauge lines.
fn fsync_gauge_lines(text: &mut String, node: &NodeInfo) {
    // M2-S21: windowed rates (previous everysec tick window, injected
    // clock) + fsync latency percentiles (HDR-class histogram, ~3%
    // quantization — the §8.2 storage-bound honesty fields).
    push(text, &format!("fsyncs_per_sec:{}", node.fsyncs_per_sec.get()));
    push(text, &format!("acks_per_sec:{}", node.acks_per_sec.get()));
    push(text, &format!("fsync_latency_p50_us:{}", node.fsync_p50_us.get()));
    push(text, &format!("fsync_latency_p99_us:{}", node.fsync_p99_us.get()));
    push(text, &format!("fsync_latency_p999_us:{}", node.fsync_p999_us.get()));
    // M2.5-S07: group formation — records covered per durability
    // fsync (the >= 0.8x available-in-flight-writes gate observable).
    push(text, &format!("fsync_group_p50:{}", node.fsync_group_p50.get()));
    push(text, &format!("fsync_group_p99:{}", node.fsync_group_p99.get()));
    // M4.5-S27 (ADR-0083 D5): per-reason durability-fsync counts —
    // the S29 named observability gap (`CommitStats` had them,
    // nothing exported them). Linked syncs' latency samples rebase
    // at their covering write's completion (ADR-0083 D4), so the
    // fsync percentiles above measure sync service time, never the
    // write+sync chain.
    push(text, &format!("fsyncs_linked:{}", node.fsyncs_linked.get()));
    push(text, &format!("fsyncs_seal:{}", node.fsyncs_seal.get()));
    push(text, &format!("fsyncs_standalone:{}", node.fsyncs_standalone.get()));
    push(text, &format!("fsyncs_completion:{}", node.fsyncs_completion.get()));
}

/// `INFO` — the barrier gauge lines.
fn barrier_gauge_lines(text: &mut String, node: &NodeInfo) {
    // M4.5-S34 (ADR-0086): the barrier class the active segment runs
    // (fua = write-through frames, flush = linked fdatasync), the
    // write-through latency the `always` client actually waits on,
    // the direct class's two write-amplification disclosures, and the
    // tripwire that says the device is not delivering the class it
    // was probed for (never an automatic flip — the operator decides).
    let class = if node.barrier_class_fua.get() == 1 { "fua" } else { "flush" };
    push(text, &format!("barrier_class:{class}"));
    // M4.5-S42 follow-up (campaign L's finding): `barrier_class` is
    // the *active segment's* class — a fresh cell says `flush` until
    // its class-upgrade rotation. This is the configured verdict.
    let configured = if node.io_class_configured_fua.get() == 1 { "fua" } else { "flush" };
    push(text, &format!("io_class_configured:{configured}"));
    push(text, &format!("fsyncs_fua:{}", node.fsyncs_fua.get()));
    push(text, &format!("fua_latency_p50_us:{}", node.fua_p50_us.get()));
    push(text, &format!("fua_latency_p99_us:{}", node.fua_p99_us.get()));
    push(text, &format!("log_padding_bytes:{}", node.log_padding_bytes.get()));
    push(text, &format!("zero_fill_bytes:{}", node.zero_fill_bytes.get()));
    push(text, &format!("rotations_unzeroed:{}", node.rotations_unzeroed.get()));
    push(text, &format!("rotations_upgrade:{}", node.rotations_upgrade.get()));
    push(text, &format!("reopened_packed_tails:{}", node.reopened_packed_tails.get()));
    push(text, &format!("barrier_class_degraded:{}", node.barrier_class_degraded.get()));
    // M4.5-S35 (ADR-0087 D5): the frame pipeline — configured depth,
    // the deepest it actually reached (a gate run proves it filled by
    // the second number), and the two bounded waits it introduces.
    push(text, &format!("frames_in_flight:{}", node.frames_in_flight.get()));
    push(text, &format!("frames_in_flight_max:{}", node.frames_in_flight_max.get()));
    push(text, &format!("frame_waits_barrier:{}", node.frame_waits_barrier.get()));
    push(text, &format!("frame_waits_rotation:{}", node.frame_waits_rotation.get()));
    push(text, &format!("frame_waits_reorder:{}", node.frame_waits_reorder.get()));
    // M4.5-S39a: the fill policy in force and its hold episodes.
    push(text, &format!("frame_waits_fill:{}", node.frame_waits_fill.get()));
    push(text, &format!("fill_window_us:{}", node.fill_window_us.get()));
    push(text, &format!("fill_target_bytes:{}", node.fill_target_bytes.get()));
    // M4.5-S43 (ADR-0092): the FLUSH-class group hold in force and
    // its hold episodes; the adaptive target beside them.
    push(text, &format!("frame_waits_group:{}", node.frame_waits_group.get()));
    push(text, &format!("flush_group_window_us:{}", node.flush_group_window_us.get()));
    push(text, &format!("frame_records_last:{}", node.frame_records_last.get()));
    push(text, &format!("group_round_target:{}", node.group_round_target.get()));
    // M4.5-S42 (ADR-0091 D5): the device model's provenance — read
    // beside `barrier_class` and `io_budget_model`, these three lines
    // say whether the node runs the product configuration.
    let provenance = node.io_provenance.get();
    push(text, &format!("io_properties_source:{}", provenance.source.as_str()));
    push(text, &format!("io_properties_schema:{}", provenance.schema));
    push(text, &format!("io_properties_identity:{}", provenance.identity_str()));
}

/// `INFO` — the io budget lines.
fn io_budget_lines(text: &mut String, node: &NodeInfo) {
    // M4.5-S36 (ADR-0088 D7): the device budget's ledger (names per
    // INFINITY_STYLE — units and qualifiers last), the seal pacer's
    // waits, the checkpoint domain's bytes, the derived trigger, and
    // the cell-scope write-amplification figure — undefined (0 with
    // the flag set) until the first checkpoint publishes.
    push(
        text,
        &format!(
            "io_budget_model:{}",
            if node.io_budget_model_absent.get() == 1 { "absent" } else { "probed" }
        ),
    );
    push(text, &format!("io_budget_write_bytes_per_s:{}", node.io_budget_write_bytes_per_s.get()));
    push(text, &format!("io_budget_read_bytes_per_s:{}", node.io_budget_read_bytes_per_s.get()));
    let budget = node.io_budget.get();
    for class in inf_runtime::IoClass::ALL {
        let at = 3 * class.index();
        push(text, &format!("io_budget_bytes_{}:{}", class.name(), budget[at]));
        push(text, &format!("io_budget_ops_{}:{}", class.name(), budget[at + 1]));
        push(text, &format!("io_budget_deferrals_{}:{}", class.name(), budget[at + 2]));
    }
    push(text, &format!("frame_waits_pace:{}", node.frame_waits_pace.get()));
    push(text, &format!("log_frame_bytes:{}", node.log_frame_bytes.get()));
    push(text, &format!("ckpt_bytes_total:{}", node.ckpt_bytes_total.get()));
    push(text, &format!("ckpt_bytes_last:{}", node.ckpt_bytes_last.get()));
    push(text, &format!("ckpt_padding_bytes:{}", node.ckpt_padding_bytes.get()));
    push(text, &format!("manifest_bytes_total:{}", node.manifest_bytes_total.get()));
    push(text, &format!("ckpt_interval_bytes:{}", node.ckpt_interval_bytes.get()));
    // ADR-0088 D4 as amended: the cap's replay term and the cap.
    push(text, &format!("ckpt_replay_bytes_per_s:{}", node.ckpt_replay_bytes_per_s.get()));
    push(text, &format!("ckpt_cap_bytes:{}", node.ckpt_cap_bytes.get()));
    push(
        text,
        &format!(
            "ckpt_io_mode:{}",
            if node.ckpt_io_mode_buffered.get() == 1 { "buffered" } else { "direct" }
        ),
    );
    push(text, &format!("ckpt_records_since_begin:{}", node.ckpt_records_since_begin.get()));
    push(
        text,
        &format!("write_amp_milli_log_checkpoint:{}", node.write_amp_milli_log_checkpoint.get()),
    );
    push(
        text,
        &format!(
            "write_amp_log_checkpoint_undefined:{}",
            node.write_amp_log_checkpoint_undefined.get()
        ),
    );
}

/// `INFO` — the recycle gauge lines.
fn recycle_gauge_lines(text: &mut String, node: &NodeInfo) {
    // M4.5-S39b (ADR-0090 D4 as amended): the host-write figure with
    // zero-fill inside it (what recycling removes), the pool's
    // counters, and what recovery proved about recycled residue.
    push(text, &format!("accounted_host_write_bytes:{}", node.accounted_host_write_bytes.get()));
    push(
        text,
        &format!("write_amp_milli_accounted_host:{}", node.write_amp_milli_accounted_host.get()),
    );
    push(text, &format!("segments_recycled:{}", node.segments_recycled.get()));
    push(text, &format!("recycle_misses:{}", node.recycle_misses.get()));
    push(text, &format!("recycle_fallbacks:{}", node.recycle_fallbacks.get()));
    push(text, &format!("recycle_pool_bytes:{}", node.recycle_pool_bytes.get()));
    push(text, &format!("recycle_pool_full:{}", node.recycle_pool_full.get()));
    push(text, &format!("recycle_sentinels:{}", node.recycle_sentinels.get()));
    push(text, &format!("segment_rotations:{}", node.segment_rotations.get()));
    push(text, &format!("segment_preallocs:{}", node.segment_preallocs.get()));
    push(text, &format!("segment_inline_preallocs:{}", node.segment_inline_preallocs.get()));
    push(text, &format!("segment_prealloc_failures:{}", node.segment_prealloc_failures.get()));
    push(text, &format!("recycle_waits_started:{}", node.recycle_waits_started.get()));
    push(text, &format!("recycle_waits_satisfied:{}", node.recycle_waits_satisfied.get()));
    push(text, &format!("recycle_waits_expired:{}", node.recycle_waits_expired.get()));
    push(
        text,
        &format!("recycle_wait_active_bytes_max:{}", node.recycle_wait_active_bytes_max.get()),
    );
    push(
        text,
        &format!("recover_segment_residue_stops:{}", node.recover_segment_residue_stops.get()),
    );
    push(
        text,
        &format!("recover_recycled_residue_slacks:{}", node.recover_recycled_residue_slacks.get()),
    );
    push(
        text,
        &format!("recover_stale_residue_slacks:{}", node.recover_stale_residue_slacks.get()),
    );
}

/// `INFO` — the recover gauge lines.
fn recover_gauge_lines(text: &mut String, node: &NodeInfo) {
    // M4.5-S39d: the boot's recovery decomposed by phase (bytes read
    // and loop-clock µs; the µs sum to `recover_total_us` exactly).
    let phases = node.recover_phases.get();
    for (field, value) in [
        ("recover_start_us", phases.start_ns / 1000),
        ("recover_ckpt_bytes", phases.ckpt_bytes),
        ("recover_ckpt_us", phases.ckpt_ns / 1000),
        ("recover_replay_bytes", phases.replay_bytes),
        ("recover_replay_frames", phases.replay_frames),
        ("recover_replay_us", phases.replay_ns / 1000),
        ("recover_audit_bytes", phases.audit_bytes),
        ("recover_audit_valid_frames", phases.audit_valid_frames),
        ("recover_audit_foreign_frames", phases.audit_foreign_frames),
        ("recover_audit_us", phases.audit_ns / 1000),
        ("recover_finish_us", phases.finish_ns / 1000),
        ("recover_stale_files_removed", node.recover_stale_files_removed.get()),
        ("recover_records", node.recover_records.get()),
        ("recover_replay_records", node.recover_replay_records.get()),
        ("recover_total_us", phases.total_ns / 1000),
    ] {
        push(text, &format!("{field}:{value}"));
    }
}

/// `INFO` — the ckpt gauge lines.
fn ckpt_gauge_lines(text: &mut String, node: &NodeInfo) {
    // Fuzzy-checkpoint gauges (M2-S10; `ckpt_age_s` derives at S21).
    push(text, &format!("ckpts_completed:{}", node.ckpts_completed.get()));
    push(text, &format!("ckpts_aborted:{}", node.ckpts_aborted.get()));
    // ADR-0117 D1/D2: sections sealed because the next image would
    // have breached the loader bound (the walk resumed at it).
    push(text, &format!("ckpt_bound_splits:{}", node.ckpt_bound_splits.get()));
    push(text, &format!("ckpt_last_unix_ms:{}", node.ckpt_last_unix_ms.get()));
    push(text, &format!("ckpt_last_begin_lsn:{}", node.ckpt_last_begin_lsn.get()));
    push(text, &format!("ckpt_buffer_bytes:{}", node.ckpt_buffer_bytes.get()));
    push(text, &format!("ckpt_age_s:{}", node.ckpt_age_s.get()));
    // MANIFEST + truncation gauges (M2-S11 — the reclamation-bound
    // observables: live segments stay bounded once truncation runs).
    push(text, &format!("manifests_published:{}", node.manifests_published.get()));
    push(text, &format!("manifests_aborted:{}", node.manifests_aborted.get()));
    push(text, &format!("segments_truncated:{}", node.segments_truncated.get()));
    push(text, &format!("log_segments_live:{}", node.log_segments_live.get()));
}

/// `INFO` — the index stat lines.
fn index_stat_lines(text: &mut String, ks: &Keyspace) {
    // M4.5-S04 (ADR-0076 D8): index-maintenance counters, cell-scope
    // fold (per-index detail rides `INF.IDX LIST` at S10). Nothing
    // skips, prunes, or degrades silently (L10). Cumulative per boot
    // — CONFIG RESETSTAT does not reset them (recorded deviation).
    {
        let idx = ks.idx_counters_total();
        push(text, &format!("idx_maint_inserts:{}", idx.maint_inserts));
        push(text, &format!("idx_maint_removes:{}", idx.maint_removes));
        push(text, &format!("idx_maint_prunes:{}", idx.maint_prunes));
        push(text, &format!("idx_skipped_sparse:{}", idx.skipped_sparse));
        push(text, &format!("idx_skipped_inexact:{}", idx.skipped_inexact));
        push(text, &format!("idx_skipped_nan:{}", idx.skipped_nan));
        push(text, &format!("idx_skipped_toolong:{}", idx.skipped_toolong));
        push(text, &format!("idx_degraded_trips:{}", idx.degraded_trips));
    }
    // M4.5-S05 (ADR-0077 D8): backfill progress, cell-scope fold —
    // phase counts plus cumulative walk totals (same per-boot
    // cumulative deviation as the idx_* lines above).
    {
        let backfill = ks.idx_backfill_info();
        push(text, &format!("idx_backfill_walking:{}", backfill.walking));
        push(text, &format!("idx_backfill_parked:{}", backfill.parked));
        push(text, &format!("idx_backfill_published:{}", backfill.published));
        push(text, &format!("idx_backfill_scanned:{}", backfill.docs_scanned_total));
        push(text, &format!("idx_backfill_inserted:{}", backfill.entries_inserted_total));
    }
    // M4.5-S06 (ADR-0078 D6): this boot's sidecar rebuild-vs-load
    // fold (per-index rows ride `INF.IDX LIST` at S10; damaged
    // sections are unattributable and counted here — L10).
    {
        let sidecar = ks.idx_sidecar_info();
        push(text, &format!("idx_sidecar_loaded:{}", sidecar.loaded));
        push(text, &format!("idx_sidecar_rebuilt:{}", sidecar.rebuilt));
        push(text, &format!("idx_sidecar_entries_loaded:{}", sidecar.entries_loaded));
        push(text, &format!("idx_sidecar_damaged:{}", sidecar.damaged_sections));
    }
}

/// `INFO` — the tiering read lines.
fn tiering_read_lines(ks: &Keyspace, node: &NodeInfo, text: &mut String) {
    let tiering = ks.tiering_counters();
    // M4-S26 (ADR-0064 D3): the pinned `SPLIT_FIELDS` contract — the
    // resolver-tagged service percentiles the S22 harness scrapes — plus
    // the five ADR-0055 cold-read counters. Flushed by the tiered
    // MAINTAIN; identically zero on nodes with no tiered namespace. The
    // ram-hit half renders absent while tiered is live — see the branch.
    let split = node.tiering_split.get();
    if ks.tiered_tables() == 0 {
        // Degenerate contract (§3.3): every field literal zero.
        push(text, &format!("tiering_ram_hit_p50_us:{}", split[0]));
        push(text, &format!("tiering_ram_hit_p99_us:{}", split[1]));
        push(text, &format!("tiering_ram_hit_p999_us:{}", split[2]));
    } else {
        // The ram-hit lane records on the loop clock, which is frozen
        // per reactor iteration — a command that never suspends reads
        // 0 µs whatever its true service time. Rendering those zeros
        // would let the M4 §7 hot-set gate "pass" on an instrument with
        // no discriminating power, so the percentile fields go absent
        // (refuse/absent over silent zero) and this named line keeps
        // the absence loud. The S22 harness refuses a tiered row that
        // misses a SPLIT_FIELDS entry — by design, until a finer
        // injected clock exists (v0.4.0-alpha instrument fix).
        push(text, "tiering_ram_hit_split:unmeasured-iteration-clock");
    }
    push(text, &format!("tiering_cold_p50_us:{}", split[3]));
    push(text, &format!("tiering_cold_p99_us:{}", split[4]));
    push(text, &format!("tiering_cold_p999_us:{}", split[5]));
    push(text, &format!("cold_read_qd_p99:{}", split[6]));
    push(text, &format!("coalesce_ratio_milli:{}", split[7]));
    push(text, &format!("cold_reads_inflight:{}", split[8]));
    push(text, &format!("cold_queue_depth:{}", split[9]));
    push(text, &format!("cold_read_p99_us:{}", split[10]));
    push(text, &format!("cold_reads_issued:{}", split[11]));
    push(text, &format!("cold_reads_enqueued:{}", split[12]));
    // Pool-sizing stalls + typed enqueue refusals (v0.4.0-alpha
    // instrument fix — invisible in soak artifacts until now).
    push(text, &format!("cold_pool_dry:{}", split[13]));
    push(text, &format!("cold_queue_full:{}", split[14]));
    push(text, &format!("tiering_tail_allocs:{}", tiering.tail_allocs));
    push(text, &format!("tiering_seal_holes:{}", tiering.seal_holes));
    push(text, &format!("tiering_seal_hole_bytes:{}", tiering.seal_hole_bytes));
    push(text, &format!("tiering_region_commit_pages:{}", tiering.region_commit_pages));
    push(text, &format!("tiering_region_decommit_pages:{}", tiering.region_decommit_pages));
    push(text, &format!("tiering_cold_resolves:{}", tiering.cold_resolves));
    // C2′ (review of 2026-08-30): typed cold-read failures served to
    // clients — zero in memory mode and in any healthy run; the paired
    // saturation cause is `cold_queue_full` above.
    push(text, &format!("tiering_cold_read_errors:{}", tiering.cold_read_errors));
    // F-L06-03 (review of 2026-08-30): writes that re-resolved because
    // the key moved while they were suspended on an extent read — a
    // legal interleaving, counted so the race is observable.
    push(text, &format!("tiering_write_replans:{}", tiering.write_replans));
    // F-L03-04 (review of 2026-08-30; ADR-0057 A3): publications whose
    // walk of some tiered table began under an older checkpoint id — a
    // walk that reused a leaked pin (never re-latched its watermark,
    // never advanced the retirement stamp). Sticky; zero in every
    // correct run; the `m4-tiered` DST's oracle.
    push(text, &format!("tiering_walk_behind:{}", node.ckpt_walks_behind.get()));
    // M4.5-S37 step 1: the ceiling arm's count — present only in a
    // `bench-diagnostics` build, so a shipping INFO cannot be mistaken
    // for one.
    #[cfg(feature = "bench-diagnostics")]
    push(text, &format!("blind_overwrites_ceiling:{}", node.blind_overwrites_ceiling.get()));
}

/// `INFO` — the tiering flush lines.
fn tiering_flush_lines(ks: &Keyspace, node: &NodeInfo, text: &mut String) {
    let tiering = ks.tiering_counters();
    // M4-S07: demotion + backpressure counters and the L5 usage
    // attribution — same zero-in-memory-mode contract as above.
    push(text, &format!("tiering_tail_alloc_stalls:{}", tiering.tail_alloc_stalls));
    push(text, &format!("tiering_demote_slices:{}", tiering.demote_slices));
    push(text, &format!("tiering_demote_sealed_bytes:{}", tiering.demote_sealed_bytes));
    // M4-S11: flush-pipeline counters — same zero contract.
    push(text, &format!("tiering_flush_slices:{}", tiering.flush_slices));
    push(text, &format!("tiering_flush_confirmed_bytes:{}", tiering.flush_confirmed_bytes));
    // M4.5-S31 (ADR-0084 D6): reactor-drive flush rounds — the sealing
    // path's visibility, cell scope, flushed by the tiered MAINTAIN.
    let flush = node.tier_flush.get();
    push(text, &format!("tiering_flush_rounds:{}", flush[0]));
    push(text, &format!("tiering_flush_write_retries:{}", flush[1]));
    push(text, &format!("tiering_flush_stale_completions:{}", flush[2]));
    push(text, &format!("tiering_flush_round_p50_us:{}", flush[3]));
    push(text, &format!("tiering_flush_round_p99_us:{}", flush[4]));
    push(text, &format!("tiering_flush_rounds_inflight:{}", flush[5]));
    push(text, &format!("tiering_files_sealed:{}", flush[6]));
    push(text, &format!("tiering_files_active:{}", flush[7]));
    // M4.5-S36 (ADR-0088 D5): rounds the device budget deferred.
    push(text, &format!("tiering_flush_rounds_deferred:{}", flush[8]));
    // M4-S15: copy-forward slices — same zero contract.
    push(text, &format!("tiering_compact_slices:{}", tiering.compact_slices));
}

/// `INFO` — the tiering promotion lines.
fn tiering_promotion_lines(ks: &Keyspace, text: &mut String) {
    // M4.5-S30 (ADR-0085 D6): read-driven promotion — engagement, the
    // counted skip reasons, and the filter's fixed L5 term. Same zero
    // contract; the A/B and the DST oracles read these.
    let promo = ks.tiering_promotion();
    push(text, &format!("tiering_promotions:{}", promo.promotions));
    push(text, &format!("tiering_promoted_bytes:{}", promo.promoted_bytes));
    push(text, &format!("tiering_promote_first_touch:{}", promo.first_touch));
    push(text, &format!("tiering_promote_skip_window:{}", promo.skip_window));
    push(text, &format!("tiering_promote_skip_pinned:{}", promo.skip_pinned));
    push(text, &format!("tiering_promote_skip_disk:{}", promo.skip_disk));
    push(text, &format!("tiering_promote_skip_stale:{}", promo.skip_stale));
    push(text, &format!("tiering_promote_skip_cap:{}", promo.skip_cap));
}

/// `INFO` — the tiering shadow lines.
fn tiering_shadow_lines(ks: &Keyspace, node: &NodeInfo, text: &mut String) {
    // M4.5-S37 (ADR-0093 D8): shadow-slot reconciliation — creation,
    // the verdicts, the reads, the gauges (open tickets, the pinned RAM
    // suffix and its cap), every bound's fallback, and the paths that
    // consult the ticket set. Same zero contract; the A/B and the DST
    // oracles read these.
    let shadow = ks.tiering_shadow();
    push(text, &format!("tiering_shadow_enabled:{}", shadow.enabled));
    push(text, &format!("tiering_shadow_reconcile_paused:{}", shadow.reconcile_paused));
    push(text, &format!("tiering_shadow_created:{}", shadow.created));
    push(text, &format!("tiering_shadow_resolved_same_key:{}", shadow.resolved_same_key));
    push(text, &format!("tiering_shadow_resolved_collision:{}", shadow.resolved_collision));
    push(text, &format!("tiering_shadow_verified:{}", shadow.verified));
    push(text, &format!("tiering_shadow_settled_without_read:{}", shadow.settled_without_read));
    push(text, &format!("tiering_shadow_verified_pending:{}", shadow.verified_pending));
    push(text, &format!("tiering_shadow_stale:{}", shadow.stale));
    push(text, &format!("tiering_shadow_read_errors:{}", shadow.read_errors));
    push(text, &format!("tiering_shadow_reads_issued:{}", shadow.reads_issued));
    push(text, &format!("tiering_shadow_reads_foreground:{}", shadow.reads_foreground));
    push(text, &format!("tiering_shadow_pending:{}", shadow.pending));
    push(text, &format!("tiering_shadow_pending_peak:{}", shadow.pending_peak));
    push(text, &format!("tiering_shadow_pinned_bytes:{}", shadow.pinned_bytes));
    push(text, &format!("tiering_shadow_pinned_bytes_peak:{}", shadow.pinned_bytes_peak));
    push(text, &format!("tiering_shadow_pin_cap_bytes:{}", shadow.pin_cap_bytes));
    push(text, &format!("tiering_shadow_fallback_off:{}", shadow.fallback_off));
    push(text, &format!("tiering_shadow_fallback_fence:{}", shadow.fallback_fence));
    push(text, &format!("tiering_shadow_fallback_multi:{}", shadow.fallback_multi));
    push(text, &format!("tiering_shadow_fallback_ticketed:{}", shadow.fallback_ticketed));
    push(text, &format!("tiering_shadow_fallback_tickets:{}", shadow.fallback_tickets));
    push(text, &format!("tiering_shadow_fallback_pin:{}", shadow.fallback_pin));
    push(text, &format!("tiering_shadow_fallback_origin:{}", shadow.fallback_origin));
    push(text, &format!("tiering_shadow_fallback_staging:{}", node.shadow_fallback_staging.get()));
    push(text, &format!("tiering_shadow_exact_miss_inserts:{}", shadow.exact_miss_inserts));
    push(text, &format!("tiering_shadow_compaction_deferred:{}", shadow.compaction_deferred));
    push(text, &format!("tiering_shadow_promote_skip:{}", shadow.promote_skip));
    push(text, &format!("tiering_shadow_scan_twins_emitted:{}", shadow.scan_twins_emitted));
    push(text, &format!("tiering_shadow_forced_by_delete:{}", shadow.forced_by_delete));
    push(text, &format!("tiering_shadow_delete_run_refused:{}", shadow.delete_run_refused));
    push(text, &format!("tiering_shadow_retargeted:{}", shadow.retargeted));
    push(text, &format!("tiering_shadow_dropped_by_removal:{}", shadow.dropped_by_removal));
    push(text, &format!("tiering_shadow_deferred_walk:{}", shadow.deferred_walk));
    push(text, &format!("tiering_shadow_deferred_origin:{}", shadow.deferred_origin));
    push(text, &format!("tiering_shadow_dbsize_drains:{}", shadow.dbsize_drains));
    push(text, &format!("tiering_shadow_dbsize_reads:{}", shadow.dbsize_reads));
    push(text, &format!("tiering_shadow_dbsize_fence:{}", shadow.dbsize_fence));
    push(text, &format!("tiering_shadow_rebuild_reads:{}", shadow.rebuild_reads));
    push(
        text,
        &format!("tiering_shadow_rebuild_settled_same_key:{}", shadow.rebuild_settled_same_key),
    );
    push(
        text,
        &format!("tiering_shadow_rebuild_settled_distinct:{}", shadow.rebuild_settled_distinct),
    );
    push(text, &format!("tiering_shadow_rebuild_over_cap:{}", shadow.rebuild_over_cap));
    push(text, &format!("tiering_shadow_bytes:{}", shadow.bytes));
    push(
        text,
        &format!(
            "tiering_promote_filter_bytes:{}",
            ks.tiered_tables() as u64 * inf_store::TieredTable::promote_filter_bytes()
        ),
    );
}

/// `INFO` — the tiering write lines.
fn tiering_write_lines(ks: &Keyspace, text: &mut String) {
    let usage = ks.tiering_usage();
    push(text, &format!("tiering_reserved_bytes:{}", usage.reserved_bytes));
    push(text, &format!("tiering_committed_bytes:{}", usage.committed_bytes));
    push(text, &format!("tiering_allocated_bytes:{}", usage.allocated_bytes));
    push(text, &format!("tiering_dead_bytes:{}", usage.dead_bytes));
    push(text, &format!("tiering_live_bytes:{}", usage.live_bytes));
    push(text, &format!("tiering_index_bytes:{}", usage.index_bytes));
    // M4-S13 write-path accounting: cell totals, then the per-namespace
    // lines they are the exact field-wise sum of. `written_bytes` is the
    // write-amp numerator (WAL + flush — M4-S16/ADR-0060 D2: the
    // relocation volume in `compaction_bytes` reaches the device through
    // the flush leg and is not added again).
    let write = ks.tiering_write_accounting();
    push(text, &format!("tiering_user_bytes:{}", write.user_bytes));
    push(text, &format!("tiering_wal_bytes:{}", write.wal_bytes));
    push(text, &format!("tiering_flush_bytes:{}", write.flush_bytes));
    push(text, &format!("tiering_compaction_bytes:{}", write.compaction_bytes));
    push(text, &format!("tiering_written_bytes:{}", write.written_bytes()));
    // M4-S16 write amplification: the **worst** namespace, plus the count
    // of namespaces that wrote bytes while admitting none (unbounded — a
    // gate must not read those as a pass, and no maximum over the others
    // describes them). Never a blended cell-wide ratio: that is the shape
    // that hides one runaway namespace behind a quiet one.
    let amp = ks.tiering_write_amp();
    push(text, &format!("tiering_write_amp_milli_max:{}", amp.milli_max));
    push(text, &format!("tiering_write_amp_undefined_ns:{}", amp.unbounded_namespaces));
}

/// `INFO` — the tiering extent lines.
fn tiering_extent_lines(ks: &Keyspace, text: &mut String) {
    let write = ks.tiering_write_accounting();
    // M4-S17 blob extents (ADR-0061 D8): the disjoint device leg and the
    // extent lifecycle observables — same zero contract.
    push(text, &format!("tiering_blob_user_bytes:{}", write.blob_user_bytes));
    push(text, &format!("tiering_blob_bytes:{}", write.blob_bytes));
    // M4-S18: the blob leg's own worst-namespace ratio — never blended
    // into the record ratio above (a byte is written once and counted in
    // exactly one leg), and never blended across namespaces either.
    let blob_amp = ks.tiering_blob_write_amp();
    push(text, &format!("tiering_blob_write_amp_milli_max:{}", blob_amp.milli_max));
    push(text, &format!("tiering_blob_write_amp_undefined_ns:{}", blob_amp.unbounded_namespaces));
    let extents = ks.tiering_extent_stats();
    push(text, &format!("tiering_blob_extents_live:{}", extents.live));
    push(text, &format!("tiering_blob_extent_bytes_live:{}", extents.live_bytes));
    push(text, &format!("tiering_blob_extents_created:{}", extents.created));
    push(text, &format!("tiering_blob_extents_reclaimed:{}", extents.reclaimed));
    // M4-S18 reclaim visibility: the standing backlog (parked + stamped +
    // handed out) and the non-fatal unlink deferrals — both zero at
    // quiescence, which is exactly what the leak test asserts.
    push(text, &format!("tiering_blob_reclaimable:{}", extents.reclaimable));
    push(text, &format!("tiering_blob_reclaim_deferred:{}", extents.reclaim_deferred));
    push(text, &format!("tiering_blob_reclaim_slices:{}", extents.reclaim_slices));
    // ADR-0096: boot orphans renamed to their quarantine twin instead of
    // unlinked, and quarantined extents revived because the replayed map
    // references them — the latter nonzero is the upstream-accounting
    // falsifier signal (a wrong orphan verdict healed).
    push(text, &format!("tiering_blob_quarantined:{}", extents.quarantined));
    push(text, &format!("tiering_blob_quarantine_revived:{}", extents.quarantine_revived));
    push(text, &format!("tiering_blob_rmw_ops:{}", extents.rmw_ops));
    // M4-S19 (ADR-0062 D5): extent device bytes on disk right now — the
    // blob half of every namespace's disk usage (the tier-file half is
    // plane state and joins with the wiring).
    push(text, &format!("tiering_blob_disk_bytes:{}", extents.disk_bytes));
    // M4-S21 (ADR-0063 D5): disk-admission observables — namespaces
    // currently refusing, typed refusals issued, the
    // nothing-compactable-under-pressure alarm, and the enforced
    // `disk_used` snapshots. Same zero contract.
    let disk = ks.tiering_disk_admission();
    push(text, &format!("tiering_diskfull_ns:{}", disk.full_namespaces));
    push(text, &format!("tiering_diskfull_refusals:{}", disk.refusals));
    push(text, &format!("tiering_compact_idle_pressure:{}", disk.compact_idle_pressure));
    push(text, &format!("tiering_disk_used_bytes:{}", disk.used_bytes));
}

/// `INFO` — the tiering namespace lines.
fn tiering_namespace_lines(ks: &Keyspace, text: &mut String) {
    for (ns, table) in ks.tiered_namespaces() {
        let space = table.space();
        let report = space.report();
        let write = table.write_accounting();
        push(
            text,
            &format!(
                "tiering_ns{}:head={},flushed={},ro_boundary={},tail={},committed_bytes={},\
                 budget_bytes={},disk_budget_bytes={},mutable_permille={},live_bytes={},\
                 dead_bytes={},user_bytes={},wal_bytes={},flush_bytes={},compaction_bytes={},\
                 write_amp_milli={},blob_user_bytes={},blob_bytes={},blob_write_amp_milli={},\
                 blob_extents_live={},blob_disk_bytes={},disk_used_bytes={},disk_full={},\
                 diskfull_refusals={},compact_idle_pressure={},promotions={},\
                 promoted_bytes={},shadow_pending={},shadow_pinned_bytes={}",
                ns.0,
                space.head().to_raw(),
                space.flushed().to_raw(),
                space.ro_boundary().to_raw(),
                space.tail().to_raw(),
                report.committed_bytes,
                table.demotion().mem_budget_bytes,
                table.disk_budget(),
                table.demotion().mutable_permille,
                table.live_bytes(),
                report.dead_bytes,
                write.user_bytes,
                write.wal_bytes,
                write.flush_bytes,
                write.compaction_bytes,
                write.write_amplification(),
                write.blob_user_bytes,
                write.blob_bytes,
                write.blob_write_amplification(),
                table.extent_stats().live,
                table.extent_stats().disk_bytes,
                table.disk_admission_used(),
                // M4-S21 (ADR-0063 D5): which admission leg is refusing.
                match table.disk_full() {
                    None => "none",
                    Some(inf_store::DiskFullCause::Budget { .. }) => "budget",
                    Some(inf_store::DiskFullCause::Device) => "device",
                },
                table.diskfull_refusals(),
                table.compact_idle_pressure(),
                table.promotion_counters().promotions,
                table.promotion_counters().promoted_bytes,
                table.shadow_pending(),
                table.shadow_pinned_bytes(),
            ),
        );
    }
    text.push_str("\r\n");
}

/// `INFO tiering` — this cell's slice of the M4 tiered-storage surface.
///
/// Two shapes, deliberately: cell-aggregate `tiering_*` fields (the
/// §3.3 degenerate-case contract — on a node with no durable-tiered
/// namespace **every one of them is identically zero**, which the
/// `inf-bench` m4 rows assert as a release blocker), and one
/// `tiering_ns<id>:` line per tiered namespace carrying the watermarks,
/// the budget, the M4-S13 write counters, and the M4-S16 write
/// amplification. Per-namespace is not a nicety: a blended node-wide
/// number hides a runaway tiered namespace behind a quiet one, which is
/// why the ratio is per namespace and the only aggregate of it is a
/// maximum.
///
/// The operator's reading of every field is
/// `infinitydb/docs/ops-tiered-storage.md` — that chapter and this
/// function are edited together.
fn tiering_section(ks: &Keyspace, node: &NodeInfo, text: &mut String) {
    // M4-S03: tiering code-path counters (this cell's slice), one group
    // of lines per story; each helper reads the counters it renders.
    push(text, "# Tiering");
    push(text, &format!("tiering_tables:{}", ks.tiered_tables()));
    tiering_read_lines(ks, node, text);
    tiering_flush_lines(ks, node, text);
    tiering_promotion_lines(ks, text);
    tiering_shadow_lines(ks, node, text);
    tiering_write_lines(ks, text);
    tiering_extent_lines(ks, text);
    tiering_namespace_lines(ks, text);
}

/// VmRSS from procfs (Linux); 0 where unavailable.
/// The 40-hex node identity (ADR-0124 D5) — `run_id` and, until M9
/// brings a replication history, `master_replid`.
fn render_run_id(node: &NodeInfo) -> String {
    let [a, b, c] = node.run_id.get();
    format!("{a:016x}{b:016x}{:08x}", c as u32)
}

/// The process RSS via the runtime's reader (Linux `/proc`, macOS
/// `proc_pidinfo`); 0 only where no reader exists (lane L11 N19).
fn process_rss_bytes() -> u64 {
    inf_runtime::net::process_rss_bytes().unwrap_or(0)
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
