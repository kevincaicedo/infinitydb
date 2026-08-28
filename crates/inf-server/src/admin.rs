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
use inf_store::{
    CellStore, EvictionPolicy, Keyspace, NsError, NsMode, NsSpec, PressureConfig, TierSpec,
};
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
    "tiering",
    "stats",
    "replication",
    "cpu",
    "tripwires",
    "keyspace",
];

pub(crate) fn info(
    argv: &(impl Argv + ?Sized),
    ks: &Keyspace,
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
    #[cfg(feature = "doc")]
    let report = {
        let mut report = ks.report();
        node.add_cell_doc_memory(&mut report);
        report
    };
    #[cfg(not(feature = "doc"))]
    let report = ks.report();
    if wants("memory") {
        // M3-S25 attribution fix: `used_memory_rss` is process-wide, so
        // the byte gauges beside it must be node-wide too. The serving
        // cell publishes its fresh gauges and folds the board (peers lag
        // their MAINTAIN publish by at most one period); without a board
        // (bare harness) the section renders cell scope and says so.
        let local = crate::exec::memory_gauges_of(&report, node);
        let (scope, g) = match node.publish_and_total_memory(local) {
            Some(totals) => ("node", totals),
            None => ("cell", local),
        };
        let used = g.used_bytes;
        let rss = process_rss_bytes();
        push(&mut text, "# Memory");
        push(&mut text, &format!("used_memory:{used}"));
        push(&mut text, &format!("used_memory_human:{}", human_bytes(used)));
        push(&mut text, &format!("used_memory_rss:{rss}"));
        push(&mut text, &format!("memory_scope:{scope}"));
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
        push(&mut text, &format!("doc_tape_bytes:{}", g.doc_tape_bytes));
        push(&mut text, &format!("doc_arena_bytes:{}", g.doc_arena_bytes));
        push(&mut text, &format!("doc_resident_bytes:{}", g.doc_resident_bytes));
        push(&mut text, &format!("doc_intern_bytes:{}", g.doc_intern_bytes));
        push(&mut text, &format!("doc_slack_bytes:{}", g.doc_slack_bytes));
        push(&mut text, &format!("doc_scratch_bytes:{}", g.doc_scratch_bytes));
        push(&mut text, &format!("doc_path_cache_bytes:{}", g.doc_path_cache_bytes));
        push(&mut text, &format!("docs_live:{}", g.docs_live));
        // Index-tree domains (M4.5-S03, ADR-0075 D6): counted in
        // used_memory and the namespace budgets — never a second ledger.
        push(&mut text, &format!("idx_tree_bytes:{}", g.idx_tree_bytes));
        push(&mut text, &format!("idx_slack_bytes:{}", g.idx_slack_bytes));
        text.push_str("\r\n");
    }
    if wants("persistence") {
        push(&mut text, "# Persistence");
        // Boot-recovery fields (M2-S15): shapes mirror Redis (`loading:1`
        // plus `loading_*` while a load is in progress — capture artifact
        // `.artifacts/m2/loading-redis-capture-20260703/`); byte totals
        // are file extents including preallocated slack (upper bound).
        let loading = node.loading.get() != 0;
        push(&mut text, &format!("loading:{}", u8::from(loading)));
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
            push(&mut text, &format!("loading_start_time:{}", start_ms / 1000));
            push(&mut text, &format!("loading_total_bytes:{total}"));
            push(&mut text, &format!("loading_loaded_bytes:{done}"));
            push(&mut text, &format!("loading_loaded_perc:{perc:.2}"));
            push(&mut text, &format!("loading_eta_seconds:{eta_s}"));
            // Extension fields (per-cell recovery is an InfinityDB shape).
            push(&mut text, &format!("loading_cells_ready:{}", node.loading_cells_ready.get()));
            push(&mut text, &format!("loading_cells:{}", node.cells.get()));
        }
        push(&mut text, "rdb_changes_since_last_save:0");
        // M2-S20: BGSAVE maps onto the fuzzy checkpoint (no fork); the
        // save time is the newest durable MANIFEST publication (board
        // max across cells, unix seconds — the LASTSAVE currency).
        push(&mut text, &format!("rdb_bgsave_in_progress:{}", node.ckpt_in_progress.get()));
        push(&mut text, &format!("rdb_last_save_time:{}", node.rdb_last_save_ms.get() / 1000));
        push(&mut text, "aof_enabled:0");
        push(&mut text, "aof_rewrite_in_progress:0");
        // Durable-namespace gauges (M2-S08, this cell's slice — the S21
        // counter set; control-plane aggregation lands with S21).
        push(&mut text, &format!("log_records_appended:{}", node.log_records_appended.get()));
        push(&mut text, &format!("pending_log_bytes:{}", node.log_pending_bytes.get()));
        push(&mut text, &format!("last_durable_lsn:{}", node.log_last_durable_lsn.get()));
        push(&mut text, &format!("watermark_lag_lsn:{}", node.log_watermark_lag.get()));
        push(&mut text, &format!("fsyncs_completed:{}", node.log_fsyncs_completed.get()));
        push(&mut text, &format!("acks_gated:{}", node.log_acks_gated.get()));
        // M2-S22: frames queued (log_writes_per_iter numerator) + the
        // staging domain's resident bytes (attribution observable).
        push(&mut text, &format!("log_frames_queued:{}", node.log_frames_queued.get()));
        push(&mut text, &format!("log_staging_bytes:{}", node.log_staging_bytes.get()));
        // Typed `-BUSY` staging-admission refusals (v0.4.0-alpha
        // instrument fix; M4.5-S27 re-scoped it to client-visible
        // refusals only — the doc exact late admission is the one
        // remaining emitter, so a climbing rate here is a finding).
        push(&mut text, &format!("log_admission_busy:{}", node.log_admission_busy.get()));
        // M4.5-S27 (ADR-0083 D2/D5): the pacing observables. Parks are
        // backpressure working as designed; `oversized` is the typed
        // never-fits refusal; the write-stall percentiles are the
        // staging drain's binding variable (frame-write submit →
        // LogWritten — under kernel writeback throttling this is what
        // starves staging, and fsync latency is the correlated symptom).
        push(&mut text, &format!("log_admission_oversized:{}", node.log_admission_oversized.get()));
        push(&mut text, &format!("log_admission_parked:{}", node.log_admission_parked.get()));
        push(
            &mut text,
            &format!("log_admission_parked_total:{}", node.log_admission_parked_total.get()),
        );
        push(&mut text, &format!("log_staging_capacity_bytes:{}", node.log_staging_capacity.get()));
        push(&mut text, &format!("log_write_stall_p50_us:{}", node.log_write_stall_p50_us.get()));
        push(&mut text, &format!("log_write_stall_p99_us:{}", node.log_write_stall_p99_us.get()));
        push(&mut text, &format!("log_write_stall_p999_us:{}", node.log_write_stall_p999_us.get()));
        // M2-S21: windowed rates (previous everysec tick window, injected
        // clock) + fsync latency percentiles (HDR-class histogram, ~3%
        // quantization — the §8.2 storage-bound honesty fields).
        push(&mut text, &format!("fsyncs_per_sec:{}", node.fsyncs_per_sec.get()));
        push(&mut text, &format!("acks_per_sec:{}", node.acks_per_sec.get()));
        push(&mut text, &format!("fsync_latency_p50_us:{}", node.fsync_p50_us.get()));
        push(&mut text, &format!("fsync_latency_p99_us:{}", node.fsync_p99_us.get()));
        push(&mut text, &format!("fsync_latency_p999_us:{}", node.fsync_p999_us.get()));
        // M2.5-S07: group formation — records covered per durability
        // fsync (the >= 0.8x available-in-flight-writes gate observable).
        push(&mut text, &format!("fsync_group_p50:{}", node.fsync_group_p50.get()));
        push(&mut text, &format!("fsync_group_p99:{}", node.fsync_group_p99.get()));
        // M4.5-S27 (ADR-0083 D5): per-reason durability-fsync counts —
        // the S29 named observability gap (`CommitStats` had them,
        // nothing exported them). Linked syncs' latency samples rebase
        // at their covering write's completion (ADR-0083 D4), so the
        // fsync percentiles above measure sync service time, never the
        // write+sync chain.
        push(&mut text, &format!("fsyncs_linked:{}", node.fsyncs_linked.get()));
        push(&mut text, &format!("fsyncs_seal:{}", node.fsyncs_seal.get()));
        push(&mut text, &format!("fsyncs_standalone:{}", node.fsyncs_standalone.get()));
        push(&mut text, &format!("fsyncs_completion:{}", node.fsyncs_completion.get()));
        // M4.5-S34 (ADR-0086): the barrier class the active segment runs
        // (fua = write-through frames, flush = linked fdatasync), the
        // write-through latency the `always` client actually waits on,
        // the direct class's two write-amplification disclosures, and the
        // tripwire that says the device is not delivering the class it
        // was probed for (never an automatic flip — the operator decides).
        let class = if node.barrier_class_fua.get() == 1 { "fua" } else { "flush" };
        push(&mut text, &format!("barrier_class:{class}"));
        // M4.5-S42 follow-up (campaign L's finding): `barrier_class` is
        // the *active segment's* class — a fresh cell says `flush` until
        // its class-upgrade rotation. This is the configured verdict.
        let configured = if node.io_class_configured_fua.get() == 1 { "fua" } else { "flush" };
        push(&mut text, &format!("io_class_configured:{configured}"));
        push(&mut text, &format!("fsyncs_fua:{}", node.fsyncs_fua.get()));
        push(&mut text, &format!("fua_latency_p50_us:{}", node.fua_p50_us.get()));
        push(&mut text, &format!("fua_latency_p99_us:{}", node.fua_p99_us.get()));
        push(&mut text, &format!("log_padding_bytes:{}", node.log_padding_bytes.get()));
        push(&mut text, &format!("zero_fill_bytes:{}", node.zero_fill_bytes.get()));
        push(&mut text, &format!("rotations_unzeroed:{}", node.rotations_unzeroed.get()));
        push(&mut text, &format!("rotations_upgrade:{}", node.rotations_upgrade.get()));
        push(&mut text, &format!("reopened_packed_tails:{}", node.reopened_packed_tails.get()));
        push(&mut text, &format!("barrier_class_degraded:{}", node.barrier_class_degraded.get()));
        // M4.5-S35 (ADR-0087 D5): the frame pipeline — configured depth,
        // the deepest it actually reached (a gate run proves it filled by
        // the second number), and the two bounded waits it introduces.
        push(&mut text, &format!("frames_in_flight:{}", node.frames_in_flight.get()));
        push(&mut text, &format!("frames_in_flight_max:{}", node.frames_in_flight_max.get()));
        push(&mut text, &format!("frame_waits_barrier:{}", node.frame_waits_barrier.get()));
        push(&mut text, &format!("frame_waits_rotation:{}", node.frame_waits_rotation.get()));
        push(&mut text, &format!("frame_waits_reorder:{}", node.frame_waits_reorder.get()));
        // M4.5-S39a: the fill policy in force and its hold episodes.
        push(&mut text, &format!("frame_waits_fill:{}", node.frame_waits_fill.get()));
        push(&mut text, &format!("fill_window_us:{}", node.fill_window_us.get()));
        push(&mut text, &format!("fill_target_bytes:{}", node.fill_target_bytes.get()));
        // M4.5-S43 (ADR-0092): the FLUSH-class group hold in force and
        // its hold episodes; the adaptive target beside them.
        push(&mut text, &format!("frame_waits_group:{}", node.frame_waits_group.get()));
        push(&mut text, &format!("flush_group_window_us:{}", node.flush_group_window_us.get()));
        push(&mut text, &format!("frame_records_last:{}", node.frame_records_last.get()));
        push(&mut text, &format!("group_round_target:{}", node.group_round_target.get()));
        // M4.5-S42 (ADR-0091 D5): the device model's provenance — read
        // beside `barrier_class` and `io_budget_model`, these three lines
        // say whether the node runs the product configuration.
        let provenance = node.io_provenance.get();
        push(&mut text, &format!("io_properties_source:{}", provenance.source.as_str()));
        push(&mut text, &format!("io_properties_schema:{}", provenance.schema));
        push(&mut text, &format!("io_properties_identity:{}", provenance.identity_str()));
        // M4.5-S36 (ADR-0088 D7): the device budget's ledger (names per
        // INFINITY_STYLE — units and qualifiers last), the seal pacer's
        // waits, the checkpoint domain's bytes, the derived trigger, and
        // the cell-scope write-amplification figure — undefined (0 with
        // the flag set) until the first checkpoint publishes.
        push(
            &mut text,
            &format!(
                "io_budget_model:{}",
                if node.io_budget_model_absent.get() == 1 { "absent" } else { "probed" }
            ),
        );
        push(
            &mut text,
            &format!("io_budget_write_bytes_per_s:{}", node.io_budget_write_bytes_per_s.get()),
        );
        push(
            &mut text,
            &format!("io_budget_read_bytes_per_s:{}", node.io_budget_read_bytes_per_s.get()),
        );
        let budget = node.io_budget.get();
        for class in inf_runtime::IoClass::ALL {
            let at = 3 * class.index();
            push(&mut text, &format!("io_budget_bytes_{}:{}", class.name(), budget[at]));
            push(&mut text, &format!("io_budget_ops_{}:{}", class.name(), budget[at + 1]));
            push(&mut text, &format!("io_budget_deferrals_{}:{}", class.name(), budget[at + 2]));
        }
        push(&mut text, &format!("frame_waits_pace:{}", node.frame_waits_pace.get()));
        push(&mut text, &format!("log_frame_bytes:{}", node.log_frame_bytes.get()));
        push(&mut text, &format!("ckpt_bytes_total:{}", node.ckpt_bytes_total.get()));
        push(&mut text, &format!("ckpt_bytes_last:{}", node.ckpt_bytes_last.get()));
        push(&mut text, &format!("ckpt_padding_bytes:{}", node.ckpt_padding_bytes.get()));
        push(&mut text, &format!("manifest_bytes_total:{}", node.manifest_bytes_total.get()));
        push(&mut text, &format!("ckpt_interval_bytes:{}", node.ckpt_interval_bytes.get()));
        // ADR-0088 D4 as amended: the cap's replay term and the cap.
        push(&mut text, &format!("ckpt_replay_bytes_per_s:{}", node.ckpt_replay_bytes_per_s.get()));
        push(&mut text, &format!("ckpt_cap_bytes:{}", node.ckpt_cap_bytes.get()));
        push(
            &mut text,
            &format!(
                "ckpt_io_mode:{}",
                if node.ckpt_io_mode_buffered.get() == 1 { "buffered" } else { "direct" }
            ),
        );
        push(
            &mut text,
            &format!("ckpt_records_since_begin:{}", node.ckpt_records_since_begin.get()),
        );
        push(
            &mut text,
            &format!(
                "write_amp_milli_log_checkpoint:{}",
                node.write_amp_milli_log_checkpoint.get()
            ),
        );
        push(
            &mut text,
            &format!(
                "write_amp_log_checkpoint_undefined:{}",
                node.write_amp_log_checkpoint_undefined.get()
            ),
        );
        // M4.5-S39b (ADR-0090 D4 as amended): the host-write figure with
        // zero-fill inside it (what recycling removes), the pool's
        // counters, and what recovery proved about recycled residue.
        push(
            &mut text,
            &format!("accounted_host_write_bytes:{}", node.accounted_host_write_bytes.get()),
        );
        push(
            &mut text,
            &format!(
                "write_amp_milli_accounted_host:{}",
                node.write_amp_milli_accounted_host.get()
            ),
        );
        push(&mut text, &format!("segments_recycled:{}", node.segments_recycled.get()));
        push(&mut text, &format!("recycle_misses:{}", node.recycle_misses.get()));
        push(&mut text, &format!("recycle_fallbacks:{}", node.recycle_fallbacks.get()));
        push(&mut text, &format!("recycle_pool_bytes:{}", node.recycle_pool_bytes.get()));
        push(&mut text, &format!("recycle_pool_full:{}", node.recycle_pool_full.get()));
        push(&mut text, &format!("segment_rotations:{}", node.segment_rotations.get()));
        push(&mut text, &format!("segment_preallocs:{}", node.segment_preallocs.get()));
        push(
            &mut text,
            &format!("segment_inline_preallocs:{}", node.segment_inline_preallocs.get()),
        );
        push(
            &mut text,
            &format!("segment_prealloc_failures:{}", node.segment_prealloc_failures.get()),
        );
        push(&mut text, &format!("recycle_waits_started:{}", node.recycle_waits_started.get()));
        push(&mut text, &format!("recycle_waits_satisfied:{}", node.recycle_waits_satisfied.get()));
        push(&mut text, &format!("recycle_waits_expired:{}", node.recycle_waits_expired.get()));
        push(
            &mut text,
            &format!("recycle_wait_active_bytes_max:{}", node.recycle_wait_active_bytes_max.get()),
        );
        push(
            &mut text,
            &format!("recover_segment_residue_stops:{}", node.recover_segment_residue_stops.get()),
        );
        push(
            &mut text,
            &format!(
                "recover_recycled_residue_slacks:{}",
                node.recover_recycled_residue_slacks.get()
            ),
        );
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
            ("recover_total_us", phases.total_ns / 1000),
        ] {
            push(&mut text, &format!("{field}:{value}"));
        }
        // Fuzzy-checkpoint gauges (M2-S10; `ckpt_age_s` derives at S21).
        push(&mut text, &format!("ckpts_completed:{}", node.ckpts_completed.get()));
        push(&mut text, &format!("ckpts_aborted:{}", node.ckpts_aborted.get()));
        push(&mut text, &format!("ckpt_last_unix_ms:{}", node.ckpt_last_unix_ms.get()));
        push(&mut text, &format!("ckpt_last_begin_lsn:{}", node.ckpt_last_begin_lsn.get()));
        push(&mut text, &format!("ckpt_buffer_bytes:{}", node.ckpt_buffer_bytes.get()));
        push(&mut text, &format!("ckpt_age_s:{}", node.ckpt_age_s.get()));
        // MANIFEST + truncation gauges (M2-S11 — the reclamation-bound
        // observables: live segments stay bounded once truncation runs).
        push(&mut text, &format!("manifests_published:{}", node.manifests_published.get()));
        push(&mut text, &format!("manifests_aborted:{}", node.manifests_aborted.get()));
        push(&mut text, &format!("segments_truncated:{}", node.segments_truncated.get()));
        push(&mut text, &format!("log_segments_live:{}", node.log_segments_live.get()));
        text.push_str("\r\n");
    }
    if wants("tiering") {
        tiering_section(ks, node, &mut text);
    }
    let stats = ks.stats();
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
        push(&mut text, &format!("evicted_keys:{}", stats.evicted_keys));
        push(&mut text, &format!("keyspace_hits:{}", stats.keyspace_hits));
        push(&mut text, &format!("keyspace_misses:{}", stats.keyspace_misses));
        // M4.5-S40: stop-and-copy index grows on this cell's flat stores
        // (a foreground latency step per doubling; the S40 timeline's event).
        push(&mut text, &format!("index_grows:{}", stats.index_grows));
        // M3-S10: per-cell path-program cache (extension fields).
        #[cfg(feature = "doc")]
        {
            let cache = node.path_cache.borrow();
            push(&mut text, &format!("path_cache_hits:{}", cache.hits()));
            push(&mut text, &format!("path_cache_misses:{}", cache.misses()));
            push(&mut text, &format!("path_cache_evictions:{}", cache.evictions()));
        }
        // M4.5-S04 (ADR-0076 D8): index-maintenance counters, cell-scope
        // fold (per-index detail rides `INF.IDX LIST` at S10). Nothing
        // skips, prunes, or degrades silently (L10). Cumulative per boot
        // — CONFIG RESETSTAT does not reset them (recorded deviation).
        {
            let idx = ks.idx_counters_total();
            push(&mut text, &format!("idx_maint_inserts:{}", idx.maint_inserts));
            push(&mut text, &format!("idx_maint_removes:{}", idx.maint_removes));
            push(&mut text, &format!("idx_maint_prunes:{}", idx.maint_prunes));
            push(&mut text, &format!("idx_skipped_sparse:{}", idx.skipped_sparse));
            push(&mut text, &format!("idx_skipped_inexact:{}", idx.skipped_inexact));
            push(&mut text, &format!("idx_skipped_nan:{}", idx.skipped_nan));
            push(&mut text, &format!("idx_skipped_toolong:{}", idx.skipped_toolong));
            push(&mut text, &format!("idx_degraded_trips:{}", idx.degraded_trips));
        }
        // M4.5-S05 (ADR-0077 D8): backfill progress, cell-scope fold —
        // phase counts plus cumulative walk totals (same per-boot
        // cumulative deviation as the idx_* lines above).
        {
            let backfill = ks.idx_backfill_info();
            push(&mut text, &format!("idx_backfill_walking:{}", backfill.walking));
            push(&mut text, &format!("idx_backfill_parked:{}", backfill.parked));
            push(&mut text, &format!("idx_backfill_published:{}", backfill.published));
            push(&mut text, &format!("idx_backfill_scanned:{}", backfill.docs_scanned_total));
            push(&mut text, &format!("idx_backfill_inserted:{}", backfill.entries_inserted_total));
        }
        // M4.5-S06 (ADR-0078 D6): this boot's sidecar rebuild-vs-load
        // fold (per-index rows ride `INF.IDX LIST` at S10; damaged
        // sections are unattributable and counted here — L10).
        {
            let sidecar = ks.idx_sidecar_info();
            push(&mut text, &format!("idx_sidecar_loaded:{}", sidecar.loaded));
            push(&mut text, &format!("idx_sidecar_rebuilt:{}", sidecar.rebuilt));
            push(&mut text, &format!("idx_sidecar_entries_loaded:{}", sidecar.entries_loaded));
            push(&mut text, &format!("idx_sidecar_damaged:{}", sidecar.damaged_sections));
        }
        push(&mut text, &format!("pubsub_channels:{}", node.pubsub_channels.get()));
        push(&mut text, &format!("pubsub_patterns:{}", node.pubsub_patterns.get()));
        push(
            &mut text,
            &format!("client_output_buffer_limit_disconnections:{}", node.cob_disconnections.get()),
        );
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
        push(&mut text, &format!("{}:{}", tw::WHEEL_BYTES, report.wheel_bytes));
        push(&mut text, &format!("{}:{}", tw::EVICT_BYTES, report.evict_bytes));
        push(&mut text, &format!("{}:{}", tw::DOC_TAPE_BYTES, report.doc_tape_bytes));
        push(&mut text, &format!("{}:{}", tw::DOC_ARENA_BYTES, report.doc_arena_bytes));
        push(&mut text, &format!("{}:{}", tw::DOC_RESIDENT_BYTES, report.doc_resident_bytes));
        push(&mut text, &format!("{}:{}", tw::DOC_INTERN_BYTES, report.doc_intern_bytes));
        push(&mut text, &format!("{}:{}", tw::DOC_SLACK_BYTES, report.doc_slack_bytes));
        push(&mut text, &format!("{}:{}", tw::DOC_SCRATCH_BYTES, report.doc_scratch_bytes));
        push(&mut text, &format!("{}:{}", tw::DOC_PATH_CACHE_BYTES, report.doc_path_cache_bytes));
        push(&mut text, &format!("idx_tree_bytes:{}", report.idx_tree_bytes));
        push(&mut text, &format!("idx_slack_bytes:{}", report.idx_slack_bytes));
        push(&mut text, &format!("wheel_fallback:{}", stats.wheel_fallback));
        push(&mut text, &format!("wheel_stale:{}", stats.wheel_stale));
        push(&mut text, &format!("evicted_keys:{}", stats.evicted_keys));
        push(&mut text, &format!("pubsub_fan_msgs:{}", node.pubsub_fan_msgs.get()));
        push(&mut text, &format!("pubsub_delivered:{}", node.pubsub_delivered.get()));
        push(&mut text, &format!("pubsub_state_bytes:{}", node.pubsub_state_bytes.get()));
        push(&mut text, &format!("{}:{}", tw::WIRE_BUFFERS_BYTES, node.wire_buffers_bytes.get()));
        push(&mut text, &format!("{}:{}", tw::CONN_STATE_BYTES, node.conn_state_bytes.get()));
        // Recycle-pool residency (v0.4.0-alpha RSS-attribution gauges):
        // the reply/command pools were the last unattributed malloc
        // consumers — the warm-up-grower hypothesis instrument, read
        // against `process_rss` over a soak.
        push(&mut text, &format!("reply_pool_bytes:{}", node.reply_pool_bytes.get()));
        push(&mut text, &format!("cmd_pool_bytes:{}", node.cmd_pool_bytes.get()));
        push(&mut text, &format!("cold_pool_bytes:{}", node.cold_pool_bytes.get()));
        push(&mut text, &format!("{}:{}", tw::PROCESS_RSS, process_rss_bytes()));
        text.push_str("\r\n");
    }
    if wants("keyspace") {
        push(&mut text, "# Keyspace");
        // One line per non-empty database (Redis shape) — per-ns numbers
        // reconcile with the aggregated sections above (M1-S09).
        for (db, store) in ks.dbs() {
            if !store.is_empty() {
                push(
                    &mut text,
                    &format!(
                        "db{db}:keys={},expires={},avg_ttl=0",
                        store.len(),
                        store.stats().ttl_live
                    ),
                );
            }
        }
        text.push_str("\r\n");
    }
    // Redis ends INFO without the final blank line duplicated.
    while text.ends_with("\r\n\r\n") {
        text.truncate(text.len() - 2);
    }
    w.verbatim(b"txt", text.as_bytes());
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
    let push = |text: &mut String, line: &str| {
        text.push_str(line);
        text.push_str("\r\n");
    };
    // M4-S03: tiering code-path counters (this cell's slice).
    let tiering = ks.tiering_counters();
    push(text, "# Tiering");
    push(text, &format!("tiering_tables:{}", ks.tiered_tables()));
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
    // M4.5-S37 step 1: the ceiling arm's count — present only in a
    // `bench-diagnostics` build, so a shipping INFO cannot be mistaken
    // for one.
    #[cfg(feature = "bench-diagnostics")]
    push(text, &format!("blind_overwrites_ceiling:{}", node.blind_overwrites_ceiling.get()));
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
    // M4.5-S37 (ADR-0093 D8): shadow-slot reconciliation — creation,
    // the verdicts, the reads, the gauges (open tickets, the pinned RAM
    // suffix and its cap), every bound's fallback, and the paths that
    // consult the ticket set. Same zero contract; the A/B and the DST
    // oracles read these.
    let shadow = ks.tiering_shadow();
    push(text, &format!("tiering_shadow_enabled:{}", shadow.enabled));
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
    push(text, &format!("tiering_shadow_retargeted:{}", shadow.retargeted));
    push(text, &format!("tiering_shadow_dropped_by_removal:{}", shadow.dropped_by_removal));
    push(text, &format!("tiering_shadow_deferred_walk:{}", shadow.deferred_walk));
    push(text, &format!("tiering_shadow_deferred_origin:{}", shadow.deferred_origin));
    push(text, &format!("tiering_shadow_dbsize_drains:{}", shadow.dbsize_drains));
    push(text, &format!("tiering_shadow_dbsize_reads:{}", shadow.dbsize_reads));
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
    }
    if meta.flags.contains(CmdFlags::DENYOOM) {
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
    ks: &mut Keyspace,
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
        // hot-per-cell (M1-S03 freeze): the executing cell applies its
        // pressure config immediately; peers apply on the scatter leg, and
        // the MAINTAIN version sweep covers boot-time mutation.
        push_pressure(ks, node);
        w.simple("OK");
    } else if sub.eq_ignore_ascii_case(b"RESETSTAT") {
        ks.reset_stats();
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

/// Pushes the typed CONFIG store's pressure keys into the keyspace
/// (M1-S03 `hot-per-cell`): `maxmemory` divides by the cell count — cells
/// are symmetric by contiguous slot ranges, so the per-cell share preserves
/// the node bound with zero shared state (L1).
pub(crate) fn push_pressure(ks: &mut Keyspace, node: &NodeInfo) {
    let cfg = node.config.borrow();
    let maxmemory: u64 = cfg.get("maxmemory").and_then(|v| v.parse().ok()).unwrap_or(0);
    let policy = cfg
        .get("maxmemory-policy")
        .and_then(EvictionPolicy::parse)
        .unwrap_or(EvictionPolicy::NoEviction);
    let samples: u32 = cfg.get("maxmemory-samples").and_then(|v| v.parse().ok()).unwrap_or(5);
    // M4-S19 (ADR-0062 D4): the reserved-VA admission bound rides the
    // same hot-per-cell sweep and the same per-cell division argument —
    // cells are symmetric, so the shares preserve the node bound.
    let va_limit: u64 = cfg
        .get("tiered-reserved-va-limit")
        .and_then(|v| v.parse().ok())
        .unwrap_or(inf_store::TIERED_VA_LIMIT_DEFAULT);
    // M4.5-S30 (ADR-0085 D6): promotion admission rides the same
    // hot-per-cell sweep — a boolean, so no per-cell division.
    let promote = cfg.get("tiered-promote-on-read").is_none_or(|v| v != "no");
    // M4.5-S37 (ADR-0093 D8): the shadow arm rides the same sweep;
    // absent or anything but `yes` is off (the shipping default).
    let shadow = cfg.get("tiered-shadow-overwrite").is_some_and(|v| v == "yes");
    drop(cfg);
    let cells = u64::from(node.cells.get().max(1));
    // Per-namespace MAXMEMORY shares divide by the same symmetric cell
    // count (M4-S27, ADR-0068 D2) — pushed before the pressure config so
    // the flag recompute inside `set_pressure` sees current shares.
    ks.set_budget_shares(cells);
    ks.set_pressure(PressureConfig { limit_bytes: maxmemory / cells, policy, samples });
    ks.set_tiered_va_limit(va_limit / cells);
    ks.set_tier_promote(promote);
    ks.set_tier_shadow(shadow);
}

// ---- INF.NS (M1-S08) -----------------------------------------------------------

/// `INF.NS CREATE name [MODE memory|durable|topic] [EVICTION policy]
/// [MAXMEMORY bytes] [MEM-BUDGET b] [DISK-BUDGET b] [MUTABLE-FRACTION ‰]
/// [MAINTAIN-SLICE b] [COLD-READ-QD n] [COMPACTION-DEAD-RATIO pct]
/// [COMPACTION-SLICE b] [BLOB-THRESHOLD b] [TIER-IO-MODE direct|buffered]
/// [TAIL-STALL-TIMEOUT ms] | SET name KEY value [KEY value ...] | LIST |
/// INFO name | DROP name` — the namespace registry surface (master plan
/// §4.2; the tiering keys are M4-S19, ADR-0062). Counters in INFO are
/// this cell's slice (same documented scope as `INFO` until the control
/// plane aggregates).
pub(crate) fn inf_ns(
    argv: &(impl Argv + ?Sized),
    ks: &mut Keyspace,
    cx: &mut ConnCx,
    node: &NodeInfo,
    w: &mut RespWriter<'_>,
) {
    let sub = argv.arg(1);
    if sub.eq_ignore_ascii_case(b"CREATE") {
        // On a node, INF.NS CREATE/DROP always ride the pump's DDL program
        // (id allocation + catalog persist — ADR-0015 D2/D3); this arm is
        // the planeless tier (compat candidate, embedded, unit tests):
        // memory-mode only, ids allocated locally, nothing persisted.
        let draft = match parse_ns_create(argv) {
            Ok(draft) => draft,
            Err(msg) => return w.error(&msg),
        };
        if draft.mode != NsMode::Memory {
            return w.error(
                "ERR durable namespaces need the node runtime (planeless tier is memory-only)",
            );
        }
        let id = next_local_ns_id(ks);
        match ks.ns_create(draft.with_id(id)) {
            Ok(()) => w.simple("OK"),
            Err(e) => ns_error(e, w),
        }
    } else if sub.eq_ignore_ascii_case(b"USE") {
        if argv.len() != 3 {
            return arity_error("INF.NS|USE", w);
        }
        let name = argv.arg(2);
        // `USE dbN` is SELECT symmetry: back to the default namespace.
        if let Some(db) = default_db_index(name) {
            cx.db = db as u16;
            cx.ns = crate::exec::ConnNamespace::Default;
            let _ = ks.db_mut(db);
            return w.simple("OK");
        }
        let Some(spec) = ks.ns_get(name) else {
            return w
                .error(&format!("ERR namespace '{}' not found", String::from_utf8_lossy(name)));
        };
        if spec.mode == NsMode::Topic {
            return w.error("ERR topic namespaces are not addressable before M5");
        }
        // M4-S26: the ADR-0062 D8 refusal is lifted — the tiered data
        // plane is wired (exec routing, cold-read suspension, WAL
        // staging with displacement origins, recovery composition).
        // `USE` of a tiered namespace now routes to the tiered arm.
        cx.ns = crate::exec::ConnNamespace::Named(spec.id);
        w.simple("OK")
    } else if sub.eq_ignore_ascii_case(b"SET") {
        // Planeless arm (unit tests, embedded); on a node the pump's DDL
        // program owns SET — hot-reload persists the catalog like any
        // DDL (M4-S19, ADR-0062 D3; memory pressure keys M4-S27,
        // ADR-0068 D3).
        let (name, update) = match parse_ns_set(argv, ks) {
            Ok(parsed) => parsed,
            Err(msg) => return w.error(&msg),
        };
        match apply_ns_set(ks, &name, update) {
            Ok(()) => w.simple("OK"),
            Err(e) => ns_error(e, w),
        }
    } else if sub.eq_ignore_ascii_case(b"DROP") {
        if argv.len() != 3 {
            return arity_error("INF.NS|DROP", w);
        }
        match ks.ns_drop(argv.arg(2)) {
            Ok(()) => w.simple("OK"),
            Err(e) => ns_error(e, w),
        }
    } else if sub.eq_ignore_ascii_case(b"LIST") {
        let named: Vec<Vec<u8>> = ks.ns_iter().map(|spec| spec.name.clone()).collect();
        w.array_header(inf_store::DEFAULT_DBS + named.len());
        for db in 0..inf_store::DEFAULT_DBS {
            w.bulk(format!("db{db}").as_bytes());
        }
        for name in &named {
            w.bulk(name);
        }
    } else if sub.eq_ignore_ascii_case(b"INFO") {
        if argv.len() != 3 {
            return arity_error("INF.NS|INFO", w);
        }
        let name = argv.arg(2);
        // Default namespaces report live per-cell counters; named entries
        // report their registry config (no keys until they are addressable
        // — the recorded M1 limitation).
        if let Some(db) = default_db_index(name) {
            let cfg = node.config.borrow();
            let policy = cfg.get("maxmemory-policy").unwrap_or("noeviction").to_string();
            let maxmemory = cfg.get("maxmemory").unwrap_or("0").to_string();
            drop(cfg);
            let (keys, expires, used) = match ks.db(db) {
                Some(store) => (store.len(), store.stats().ttl_live, store.used_bytes()),
                None => (0, 0, 0),
            };
            w.map_header(6);
            for (k, v) in [
                ("name", String::from_utf8_lossy(name).into_owned()),
                ("mode", "memory".to_string()),
                ("eviction", policy),
                ("maxmemory", maxmemory),
                ("keys", keys.to_string()),
                ("expires", expires.to_string()),
            ] {
                w.bulk(k.as_bytes());
                w.bulk(v.as_bytes());
            }
            let _ = used; // used_bytes joins the map when the control plane aggregates
            return;
        }
        let Some(spec) = ks.ns_get(name) else {
            return w
                .error(&format!("ERR namespace '{}' not found", String::from_utf8_lossy(name)));
        };
        let policy = spec.policy.map_or("inherit", EvictionPolicy::name).to_string();
        let maxmemory = spec.maxmemory.map_or("inherit".to_string(), |b| b.to_string());
        let fsync = spec.fsync.map_or("-", |f| match f {
            inf_store::FsyncClass::Everysec => "everysec",
            inf_store::FsyncClass::Always => "always",
        });
        // Named namespaces are addressable since M2-S08: live per-cell
        // counters (same documented cell-slice scope as `INFO`).
        let (keys, expires) = match ks.ns_store(spec.id) {
            Some(store) => (store.len(), store.stats().ttl_live),
            None => (0, 0),
        };
        let mut pairs = vec![
            ("name", String::from_utf8_lossy(&spec.name).into_owned()),
            ("id", spec.id.0.to_string()),
            ("mode", spec.mode.name().to_string()),
            ("fsync", fsync.to_string()),
            ("eviction", policy),
            ("maxmemory", maxmemory),
            ("keys", keys.to_string()),
            ("expires", expires.to_string()),
        ];
        // The tier block, key-for-key (M4-S19 — the ADR-0062 D2 table
        // read back; `INFO tiering` carries the live counters).
        if let Some(tier) = &spec.tier {
            pairs.push(("mem-budget", tier.mem_budget_bytes.to_string()));
            pairs.push(("disk-budget", tier.disk_budget_bytes.to_string()));
            pairs.push(("mutable-fraction", tier.mutable_permille.to_string()));
            pairs.push(("maintain-slice", tier.maintain_slice_bytes.to_string()));
            pairs.push(("cold-read-qd", tier.cold_read_qd.to_string()));
            pairs.push(("compaction-dead-ratio", tier.compaction_dead_ratio_pct.to_string()));
            pairs.push(("compaction-slice", tier.compaction_slice_bytes.to_string()));
            pairs.push(("blob-threshold", tier.blob_threshold_bytes.to_string()));
            pairs.push((
                "tier-io-mode",
                match tier.tier_io_mode {
                    inf_log::fs::TierIoMode::Direct => "direct".to_string(),
                    inf_log::fs::TierIoMode::Buffered => "buffered".to_string(),
                },
            ));
            pairs.push(("tail-stall-timeout", tier.tail_stall_timeout_ms.to_string()));
        }
        w.map_header(pairs.len());
        for (k, v) in pairs {
            w.bulk(k.as_bytes());
            w.bulk(v.as_bytes());
        }
    } else {
        w.error(&format!(
            "ERR Unknown subcommand or wrong number of arguments for '{}'. Try INF.NS CREATE|SET|USE|LIST|INFO|DROP.",
            String::from_utf8_lossy(sub)
        ));
    }
}

/// What one `INF.NS SET` call updates — decided by the target namespace's
/// shape (M4-S19 tiered keys; M4-S27 memory pressure keys, ADR-0068 D3).
pub(crate) enum NsSetUpdate {
    Tier(TierSpec),
    /// Final states for a memory namespace's pressure knobs (`None` =
    /// inherit the node config), seeded from the current spec so an
    /// unmentioned key passes through unchanged.
    MemoryPressure {
        policy: Option<EvictionPolicy>,
        maxmemory: Option<u64>,
    },
}

/// Applies a parsed `INF.NS SET` update — shared by the planeless arm and
/// the pump's DDL program (which fans and persists around it).
pub(crate) fn apply_ns_set(
    ks: &mut Keyspace,
    name: &[u8],
    update: NsSetUpdate,
) -> Result<(), NsError> {
    match update {
        NsSetUpdate::Tier(tier) => ks.ns_set_tier(name, tier),
        NsSetUpdate::MemoryPressure { policy, maxmemory } => {
            ks.ns_set_memory(name, policy, maxmemory)
        }
    }
}

/// Parses `INF.NS SET name KEY value [KEY value ...]` against the
/// namespace's current spec. Tiered namespaces take the ADR-0062 D3 tier
/// keys: Hot keys apply as overrides; CreateOnly keys (`TIER-IO-MODE`,
/// `COLD-READ-QD`) refuse typed — a hot-reload that would be a silent
/// no-op is worse than a refusal — and `MAXMEMORY`/`EVICTION` refuse with
/// the one-budget-authority error (ADR-0068 D1). Memory namespaces take
/// exactly `MAXMEMORY`/`EVICTION` (Hot — ADR-0068 D3; the value `inherit`
/// returns a knob to the node config, as `INF.NS INFO` displays it).
/// Durable namespaces refuse both key families typed. All-or-nothing: the
/// first invalid pair fails the call before anything mutates.
pub(crate) fn parse_ns_set(
    argv: &(impl Argv + ?Sized),
    ks: &Keyspace,
) -> Result<(Vec<u8>, NsSetUpdate), String> {
    if argv.len() < 5 || !(argv.len() - 3).is_multiple_of(2) {
        return Err("ERR wrong number of arguments for 'INF.NS|SET'".to_string());
    }
    let name = argv.arg(2).to_vec();
    let spec = ks
        .ns_get(&name)
        .ok_or_else(|| format!("ERR namespace '{}' not found", String::from_utf8_lossy(&name)))?;
    if let Some(current) = spec.tier {
        let update = parse_ns_set_tier(argv, current)?;
        return Ok((name, update));
    }
    if spec.mode == NsMode::Memory {
        let update = parse_ns_set_memory(argv, spec)?;
        return Ok((name, update));
    }
    // Durable, non-tiered: neither key family reloads (ADR-0068 D3).
    Err("ERR durable namespaces do not evict (ADR-0015 D5/ADR-0068) — \
         MAXMEMORY/EVICTION hot-reload applies to MODE memory namespaces"
        .to_string())
}

/// The tiered half of `INF.NS SET` (M4-S19, ADR-0062 D3).
fn parse_ns_set_tier(
    argv: &(impl Argv + ?Sized),
    current: TierSpec,
) -> Result<NsSetUpdate, String> {
    let mut tier = Some(current);
    let mut i = 3;
    while i < argv.len() {
        let opt = argv.arg(i);
        let value = argv.arg(i + 1);
        if opt.eq_ignore_ascii_case(b"TIER-IO-MODE") || opt.eq_ignore_ascii_case(b"COLD-READ-QD") {
            return Err(format!(
                "ERR {} is create-only (ADR-0062 D3 — drop and recreate to change it)",
                String::from_utf8_lossy(opt).to_uppercase()
            ));
        }
        if opt.eq_ignore_ascii_case(b"MAXMEMORY") || opt.eq_ignore_ascii_case(b"EVICTION") {
            return Err(format!(
                "ERR {} does not apply to tiered namespaces — MEM-BUDGET is their one budget \
                 authority (ADR-0062/ADR-0068)",
                String::from_utf8_lossy(opt).to_uppercase()
            ));
        }
        if parse_tier_key(&mut tier, opt, value)?.is_none() {
            return Err("ERR syntax error".to_string());
        }
        i += 2;
    }
    Ok(NsSetUpdate::Tier(tier.expect("seeded from the current spec")))
}

/// The memory half of `INF.NS SET` (M4-S27, ADR-0068 D3): only the two
/// Hot pressure keys; a tier key here answers the create-time refusal.
fn parse_ns_set_memory(argv: &(impl Argv + ?Sized), spec: &NsSpec) -> Result<NsSetUpdate, String> {
    let (mut policy, mut maxmemory) = (spec.policy, spec.maxmemory);
    let mut i = 3;
    while i < argv.len() {
        let opt = argv.arg(i);
        let value = argv.arg(i + 1);
        if opt.eq_ignore_ascii_case(b"EVICTION") {
            policy = if value.eq_ignore_ascii_case(b"inherit") {
                None
            } else {
                Some(
                    core::str::from_utf8(value)
                        .ok()
                        .and_then(|v| EvictionPolicy::parse(&v.to_lowercase()))
                        .ok_or("ERR unknown eviction policy")?,
                )
            };
        } else if opt.eq_ignore_ascii_case(b"MAXMEMORY") {
            maxmemory = if value.eq_ignore_ascii_case(b"inherit") {
                None
            } else {
                match core::str::from_utf8(value)
                    .ok()
                    .and_then(crate::config::parse_memory)
                    .ok_or("ERR invalid MAXMEMORY value")?
                {
                    // 0 = no per-namespace budget, matching the node key's
                    // `maxmemory 0` vocabulary.
                    0 => None,
                    bytes => Some(bytes),
                }
            };
        } else {
            return Err("ERR not a tiered namespace — tiering is set at CREATE with MEM-BUDGET \
                        (drop and recreate to add it, ADR-0062 D3); memory namespaces hot-reload \
                        MAXMEMORY and EVICTION only (ADR-0068 D3)"
                .to_string());
        }
        i += 2;
    }
    Ok(NsSetUpdate::MemoryPressure { policy, maxmemory })
}

/// `dbN` for N in 0..16.
fn default_db_index(name: &[u8]) -> Option<usize> {
    let rest = name.strip_prefix(b"db")?;
    let n: usize = core::str::from_utf8(rest).ok()?.parse().ok()?;
    (!rest.is_empty() && rest.len() <= 2 && n < inf_store::DEFAULT_DBS).then_some(n)
}

pub(crate) fn ns_error(e: NsError, w: &mut RespWriter<'_>) {
    match e {
        NsError::Exists => w.error("ERR namespace already exists"),
        NsError::Unknown => w.error("ERR namespace not found"),
        NsError::ModeNotSupported(mode) => w.error(&format!(
            "ERR namespace mode '{}' is not yet supported (topic namespaces arrive with M5)",
            mode.name()
        )),
        NsError::DefaultImmutable => {
            w.error("ERR db0..db15 are reserved default namespaces (SELECT)")
        }
        NsError::InvalidName => {
            w.error("ERR invalid namespace name (1..128 bytes of [a-zA-Z0-9_.-])")
        }
        NsError::FsyncRequiresDurable => {
            w.error("ERR FSYNC applies to MODE durable namespaces only")
        }
        NsError::EvictionNotAllowedDurable => w.error(
            "ERR durable namespaces do not evict (M2, ADR-0015); EVICTION applies to MODE memory",
        ),
        NsError::MaxmemoryNotAllowedTiered => w.error(
            "ERR MAXMEMORY/EVICTION do not apply to tiered namespaces — MEM-BUDGET is their one \
             budget authority (ADR-0062/ADR-0068)",
        ),
        NsError::PressureKeysNotHotDurable => w.error(
            "ERR durable namespaces do not evict (ADR-0015 D5/ADR-0068) — MAXMEMORY/EVICTION \
             hot-reload applies to MODE memory namespaces",
        ),
        NsError::TierRequiresDurable => w.error(
            "ERR tiering keys (MEM-BUDGET ...) apply to MODE durable namespaces only (ADR-0062)",
        ),
        NsError::InvalidTierConfig(reason) => w.error(&format!("ERR {reason}")),
        NsError::TierVaLimitExceeded { requested_bytes, admitted_bytes, limit_bytes } => {
            w.error(&format!(
                "ERR tiered namespace would exceed the node's reserved-VA limit \
                 (requested {requested_bytes}, admitted {admitted_bytes}, limit {limit_bytes} \
                 bytes per cell — CONFIG SET tiered-reserved-va-limit, ADR-0062 D4)"
            ))
        }
        NsError::NotTiered => w.error(
            "ERR not a tiered namespace — tiering is set at CREATE with MEM-BUDGET \
             (drop and recreate to add it, ADR-0062 D3)",
        ),
    }
}

/// A parsed `INF.NS CREATE` before id assignment (the id comes from the
/// node allocator on the pump path, or locally on the planeless tier).
pub(crate) struct NsSpecDraft {
    pub name: Vec<u8>,
    pub mode: NsMode,
    pub fsync: Option<inf_store::FsyncClass>,
    pub policy: Option<EvictionPolicy>,
    pub maxmemory: Option<u64>,
    pub tier: Option<TierSpec>,
}

impl NsSpecDraft {
    pub(crate) fn with_id(self, id: u32) -> NsSpec {
        NsSpec {
            id: inf_store::NsId(id),
            name: self.name,
            mode: self.mode,
            fsync: self.fsync,
            policy: self.policy,
            maxmemory: self.maxmemory,
            tier: self.tier,
        }
    }
}

/// Parses `INF.NS CREATE name [MODE m] [FSYNC always|everysec]
/// [EVICTION p] [MAXMEMORY b] [<tier keys> ...]` — shared by the
/// planeless arm and the pump's DDL program (registry rules validate
/// after id assignment). Tier keys (M4-S19, ADR-0062 D1/D2) accumulate
/// into a [`TierSpec`]; `MEM-BUDGET` is the tiered discriminator, so a
/// tier key without it fails typed at the end of the loop.
pub(crate) fn parse_ns_create(argv: &(impl Argv + ?Sized)) -> Result<NsSpecDraft, String> {
    if argv.len() < 3 {
        return Err("ERR wrong number of arguments for 'INF.NS|CREATE'".to_string());
    }
    let mut draft = NsSpecDraft {
        name: argv.arg(2).to_vec(),
        mode: NsMode::Memory,
        fsync: None,
        policy: None,
        maxmemory: None,
        tier: None,
    };
    let mut saw_mem_budget = false;
    let mut i = 3;
    while i < argv.len() {
        let opt = argv.arg(i);
        if i + 1 >= argv.len() {
            return Err("ERR syntax error".to_string());
        }
        let value = argv.arg(i + 1);
        if opt.eq_ignore_ascii_case(b"MODE") {
            draft.mode = core::str::from_utf8(value)
                .ok()
                .and_then(|v| NsMode::parse(&v.to_lowercase()))
                .ok_or("ERR unknown namespace mode (memory|durable|topic)")?;
        } else if opt.eq_ignore_ascii_case(b"FSYNC") {
            draft.fsync = Some(match value.to_ascii_lowercase().as_slice() {
                b"always" => inf_store::FsyncClass::Always,
                b"everysec" => inf_store::FsyncClass::Everysec,
                _ => return Err("ERR unknown FSYNC class (always|everysec)".to_string()),
            });
        } else if opt.eq_ignore_ascii_case(b"EVICTION") {
            draft.policy = Some(
                core::str::from_utf8(value)
                    .ok()
                    .and_then(|v| EvictionPolicy::parse(&v.to_lowercase()))
                    .ok_or("ERR unknown eviction policy")?,
            );
        } else if opt.eq_ignore_ascii_case(b"MAXMEMORY") {
            draft.maxmemory = Some(
                core::str::from_utf8(value)
                    .ok()
                    .and_then(crate::config::parse_memory)
                    .ok_or("ERR invalid MAXMEMORY value")?,
            );
        } else if let Some(applied) = parse_tier_key(&mut draft.tier, opt, value)? {
            saw_mem_budget |= applied;
        } else {
            return Err("ERR syntax error".to_string());
        }
        i += 2;
    }
    if draft.tier.is_some() && !saw_mem_budget {
        return Err("ERR tiering keys require MEM-BUDGET (the tiered discriminator — \
                    ADR-0062 D1)"
            .to_string());
    }
    Ok(draft)
}

/// One tier key applied onto an accumulating [`TierSpec`] (ADR-0062 D2
/// vocabulary; ranges validate at registration through
/// `TierSpec::validate` — one gauntlet, not two). Returns `Ok(None)` for
/// a non-tier key, `Ok(Some(is_mem_budget))` when applied.
fn parse_tier_key(
    tier: &mut Option<TierSpec>,
    opt: &[u8],
    value: &[u8],
) -> Result<Option<bool>, String> {
    let memory = |value: &[u8], key: &str| {
        core::str::from_utf8(value)
            .ok()
            .and_then(crate::config::parse_memory)
            .ok_or(format!("ERR invalid {key} value"))
    };
    let int = |value: &[u8], key: &str| {
        core::str::from_utf8(value)
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .ok_or(format!("ERR invalid {key} value"))
    };
    let key = opt.to_ascii_uppercase();
    let spec = match key.as_slice() {
        b"MEM-BUDGET"
        | b"DISK-BUDGET"
        | b"MUTABLE-FRACTION"
        | b"MAINTAIN-SLICE"
        | b"COLD-READ-QD"
        | b"COMPACTION-DEAD-RATIO"
        | b"COMPACTION-SLICE"
        | b"BLOB-THRESHOLD"
        | b"TIER-IO-MODE"
        | b"TAIL-STALL-TIMEOUT" => tier.get_or_insert_with(|| TierSpec::for_budget(0)),
        _ => return Ok(None),
    };
    match key.as_slice() {
        b"MEM-BUDGET" => {
            spec.mem_budget_bytes = memory(value, "MEM-BUDGET")?;
            return Ok(Some(true));
        }
        b"DISK-BUDGET" => spec.disk_budget_bytes = memory(value, "DISK-BUDGET")?,
        b"MUTABLE-FRACTION" => {
            spec.mutable_permille = u32::try_from(int(value, "MUTABLE-FRACTION")?)
                .map_err(|_| "ERR invalid MUTABLE-FRACTION value")?;
        }
        b"MAINTAIN-SLICE" => spec.maintain_slice_bytes = memory(value, "MAINTAIN-SLICE")?,
        b"COLD-READ-QD" => {
            spec.cold_read_qd = u16::try_from(int(value, "COLD-READ-QD")?)
                .map_err(|_| "ERR invalid COLD-READ-QD value")?;
        }
        b"COMPACTION-DEAD-RATIO" => {
            spec.compaction_dead_ratio_pct = u8::try_from(int(value, "COMPACTION-DEAD-RATIO")?)
                .map_err(|_| "ERR invalid COMPACTION-DEAD-RATIO value")?;
        }
        b"COMPACTION-SLICE" => spec.compaction_slice_bytes = memory(value, "COMPACTION-SLICE")?,
        b"BLOB-THRESHOLD" => {
            spec.blob_threshold_bytes = u32::try_from(memory(value, "BLOB-THRESHOLD")?)
                .map_err(|_| "ERR invalid BLOB-THRESHOLD value")?;
        }
        b"TIER-IO-MODE" => {
            spec.tier_io_mode = match value.to_ascii_lowercase().as_slice() {
                b"direct" => inf_log::fs::TierIoMode::Direct,
                b"buffered" => inf_log::fs::TierIoMode::Buffered,
                _ => return Err("ERR unknown TIER-IO-MODE (direct|buffered)".to_string()),
            };
        }
        b"TAIL-STALL-TIMEOUT" => {
            spec.tail_stall_timeout_ms = u32::try_from(int(value, "TAIL-STALL-TIMEOUT")?)
                .map_err(|_| "ERR invalid TAIL-STALL-TIMEOUT value")?;
        }
        _ => unreachable!("membership matched above"),
    }
    Ok(Some(false))
}

/// Planeless-tier id allocation: past the registered maximum, floor 16.
/// (Node ids come from the control allocator; this tier never persists.)
fn next_local_ns_id(ks: &Keyspace) -> u32 {
    ks.ns_iter().map(|s| s.id.0 + 1).max().unwrap_or(inf_store::FIRST_NAMED_NS_ID)
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
        // DEBUG OBJECT is string-only introspection today; a document key
        // reports length 0 rather than erroring (documented deviation —
        // the S11 matrix pins the JSON arms elsewhere).
        let len = store.strlen(key, now).unwrap_or(0);
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

    fn run(cx: &mut ConnCx, store: &mut Keyspace, parts: &[&[u8]]) -> Vec<u8> {
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
        let mut store = Keyspace::new(StoreConfig::default());
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
        let mut store = Keyspace::new(StoreConfig::default());
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
        let mut store = Keyspace::new(StoreConfig::default());
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

    #[cfg(feature = "doc")]
    #[test]
    fn info_exposes_every_document_and_tripwire_domain() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        assert_eq!(
            run(&mut cx, &mut store, &[b"JSON.SET", b"doc", b"$", br#"{"pad":"xxxxxxxx"}"#],),
            b"+OK\r\n"
        );
        let memory =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"memory"])).expect("ascii");
        for name in [
            "doc_tape_bytes",
            "doc_arena_bytes",
            "doc_resident_bytes",
            "doc_intern_bytes",
            "doc_slack_bytes",
            "doc_scratch_bytes",
            "doc_path_cache_bytes",
            "docs_live",
        ] {
            assert!(memory.contains(&format!("{name}:")), "missing {name}: {memory}");
        }
        let tripwires =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"tripwires"])).expect("ascii");
        for name in inf_foundation::tripwire::ALL {
            assert!(tripwires.contains(&format!("{name}:")), "missing {name}: {tripwires}");
        }
    }

    /// M3-S25 attribution fix: with a memory board wired, the memory
    /// section folds every cell's publication (`memory_scope:node`) — the
    /// serving cell fresh, peers from their last publish. Without one
    /// (bare harness), it renders and labels cell scope.
    #[test]
    fn info_memory_aggregates_across_cells_via_the_board() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        assert_eq!(run(&mut cx, &mut store, &[b"SET", b"k", b"v"]), b"+OK\r\n");
        let bare =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"memory"])).expect("ascii");
        assert!(bare.contains("memory_scope:cell"), "{bare}");

        let board = std::sync::Arc::new(crate::control::MemoryBoard::new(2));
        board.slot(1).publish(crate::control::MemoryGauges {
            used_bytes: 1_000,
            docs_live: 41,
            ..Default::default()
        });
        cx.node.cell.set(0);
        *cx.node.memory_board.borrow_mut() = Some(std::sync::Arc::clone(&board));
        let noded =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"memory"])).expect("ascii");
        assert!(noded.contains("memory_scope:node"), "{noded}");
        assert!(noded.contains("docs_live:41"), "peer docs fold into the total: {noded}");
        // The serving cell published its own slot at render time: the
        // totals now exceed the peer's contribution alone.
        assert!(board.totals().used_bytes > 1_000, "serving cell published its slot");
    }

    /// M4-S03/S13 degenerate-case contract, as an operator sees it: on a
    /// memory-mode node the section renders in full and **every**
    /// `tiering_*` field is identically zero, with no per-namespace line
    /// at all. This is the shape `inf-bench`'s m4 rows assert as a
    /// release blocker; breaking it here breaks the gate there.
    #[test]
    fn info_tiering_is_all_zero_without_a_tiered_namespace() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        run(&mut cx, &mut store, &[b"SET", b"k", b"v"]);
        let text =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"tiering"])).expect("ascii");
        assert!(text.contains("# Tiering"), "{text}");
        for line in text.lines().filter(|l| l.starts_with("tiering_")) {
            let (name, value) = line.split_once(':').expect("key:value shape");
            assert_eq!(value, "0", "{name} must be identically zero on a memory-mode node");
        }
        for name in [
            "tiering_user_bytes",
            "tiering_wal_bytes",
            "tiering_flush_bytes",
            "tiering_compaction_bytes",
            "tiering_written_bytes",
            // M4-S16: a cell with no tiered namespace has no ratio to
            // report and nothing that cannot answer — both read zero for
            // the same structural reason as the counters.
            "tiering_write_amp_milli_max",
            "tiering_write_amp_undefined_ns",
            // M4-S17 (ADR-0061 D8): no table, no extents — the blob leg
            // reads zero for the same structural reason.
            "tiering_blob_user_bytes",
            "tiering_blob_bytes",
            "tiering_blob_extents_live",
            "tiering_blob_extents_created",
            "tiering_blob_extents_reclaimed",
            "tiering_blob_reclaim_slices",
            "tiering_blob_rmw_ops",
            // M4-S18: the blob ratio aggregates and the reclaim backlog
            // observables — zero without a table, zero at quiescence.
            "tiering_blob_write_amp_milli_max",
            "tiering_blob_write_amp_undefined_ns",
            "tiering_blob_reclaimable",
            "tiering_blob_reclaim_deferred",
            // M4.5-S30 (ADR-0085 D6): no table, no promotion path — the
            // read-promotion observables and the filter's L5 term read
            // zero for the same structural reason.
            "tiering_promotions",
            "tiering_promoted_bytes",
            "tiering_promote_first_touch",
            "tiering_promote_skip_window",
            "tiering_promote_skip_pinned",
            "tiering_promote_skip_disk",
            "tiering_promote_skip_stale",
            "tiering_promote_skip_cap",
            "tiering_promote_filter_bytes",
            // M4.5-S37 (ADR-0093 D8): no table, no tickets.
            "tiering_shadow_enabled",
            "tiering_shadow_created",
            "tiering_shadow_pending",
            "tiering_shadow_pinned_bytes",
            "tiering_shadow_fallback_pin",
            "tiering_shadow_fallback_fence",
            "tiering_shadow_fallback_ticketed",
            "tiering_shadow_verified_pending",
            "tiering_shadow_dbsize_drains",
            "tiering_shadow_rebuild_reads",
            "tiering_shadow_scan_twins_emitted",
            "tiering_shadow_bytes",
        ] {
            assert!(text.contains(&format!("{name}:0")), "missing {name}: {text}");
        }
    }

    /// M4-S13: a tiered namespace publishes its watermarks, its budget,
    /// and its four write counters on one `tiering_ns<id>:` line, and the
    /// cell aggregate is the exact sum of those lines.
    #[test]
    fn info_tiering_renders_per_namespace_watermarks_and_write_counters() {
        use inf_store::{AddressSpaceConfig, DemotionConfig, LogicalAddr, NsId, TieredTable};

        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        let ns = NsId(17);
        let demote = DemotionConfig::for_budget(1 << 20, 4 << 10);
        assert!(
            store
                .materialize_tiered(
                    ns,
                    AddressSpaceConfig {
                        reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                        page_bytes: 4 << 10,
                        life_origin: LogicalAddr::ZERO,
                    },
                    demote,
                    64,
                )
                .is_ok()
        );
        let table = store.tiered_store_mut(ns).expect("materialized");
        table.insert(b"key", b"value", TieredTable::hash_key(b"key")).expect("fits");

        let text =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"tiering"])).expect("ascii");
        let line = text
            .lines()
            .find(|l| l.starts_with("tiering_ns17:"))
            .expect("the tiered namespace has a line");
        for field in
            ["head=", "flushed=", "ro_boundary=", "tail=", "committed_bytes=", "budget_bytes="]
        {
            assert!(line.contains(field), "missing {field}: {line}");
        }
        // `key` + `value` = 8 user bytes, charged at the record boundary
        // — not the encoded record length, not the wire bytes.
        assert!(line.contains("user_bytes=8"), "{line}");
        assert!(line.contains("wal_bytes=0"), "no WAL record was staged: {line}");
        assert!(text.contains("tiering_user_bytes:8"), "the aggregate sums the lines: {text}");
        assert!(text.contains("tiering_tables:1"), "{text}");
    }

    /// v0.4.0-alpha instrument fix: with a tiered namespace live, the
    /// RAM-hit percentile fields render **absent**, replaced by a named
    /// disclosure — the loop clock cannot resolve a command that never
    /// suspends, so numbers here would be silent zeros the M4 §7 gate
    /// could "pass" on. On a memory-mode node the fields stay literal
    /// zero (the §3.3 degenerate contract, asserted above).
    #[test]
    fn info_tiering_ram_hit_split_renders_absent_not_zero_when_tiered() {
        use inf_store::{AddressSpaceConfig, DemotionConfig, LogicalAddr, NsId};

        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        let ns = NsId(23);
        let demote = DemotionConfig::for_budget(1 << 20, 4 << 10);
        assert!(
            store
                .materialize_tiered(
                    ns,
                    AddressSpaceConfig {
                        reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                        page_bytes: 4 << 10,
                        life_origin: LogicalAddr::ZERO,
                    },
                    demote,
                    64,
                )
                .is_ok()
        );
        let text =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"tiering"])).expect("ascii");
        for absent in
            ["tiering_ram_hit_p50_us:", "tiering_ram_hit_p99_us:", "tiering_ram_hit_p999_us:"]
        {
            assert!(!text.contains(absent), "{absent} must be absent, not zero: {text}");
        }
        assert!(text.contains("tiering_ram_hit_split:unmeasured-iteration-clock"), "{text}");
        // The cold half of the split still renders numerically.
        assert!(text.contains("tiering_cold_p99_us:"), "{text}");
        // The appended shaping counters render beside the cold family
        // (v0.4.0-alpha instrument fix: pool-sizing stalls and typed
        // enqueue refusals were invisible in soak artifacts).
        assert!(text.contains("cold_pool_dry:0"), "{text}");
        assert!(text.contains("cold_queue_full:0"), "{text}");
    }

    /// v0.4.0-alpha instrument fix: the typed `-BUSY` staging-admission
    /// refusals surface in `INFO persistence` — the `would_fit`
    /// pre-check stages nothing, so no other counter records them (the
    /// 24 h soak took 31 M with no server-side trace).
    #[test]
    fn info_persistence_renders_admission_busy_refusals() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        cx.node.log_admission_busy.set(31_260_000);
        let text =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"persistence"])).expect("ascii");
        assert!(text.contains("log_admission_busy:31260000"), "{text}");
    }

    /// v0.4.0-alpha RSS-attribution gauges: the recycle-pool residency
    /// fields render in the tripwires section beside the other
    /// byte-attribution observables.
    #[test]
    fn info_tripwires_renders_recycle_pool_bytes() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        cx.node.reply_pool_bytes.set(4096 * 100);
        cx.node.cmd_pool_bytes.set(4096 * 7);
        cx.node.cold_pool_bytes.set(1 << 24);
        let text =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"tripwires"])).expect("ascii");
        assert!(text.contains("reply_pool_bytes:409600"), "{text}");
        assert!(text.contains("cmd_pool_bytes:28672"), "{text}");
        assert!(text.contains("cold_pool_bytes:16777216"), "{text}");
    }

    /// M4-S16: the per-namespace line carries the write-amplification
    /// ratio in milli-units, the cell aggregate is the **maximum** of
    /// those (never a blend), and a namespace that wrote bytes while
    /// admitting none says `undefined` and is counted rather than
    /// averaged away.
    #[test]
    fn info_tiering_reports_write_amplification_per_namespace() {
        use inf_log::{MutationEffect, StagingConfig, StagingRing};
        use inf_store::{AddressSpaceConfig, DemotionConfig, LogicalAddr, NsId, TieredTable};

        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        let mut ring = StagingRing::new(StagingConfig::default());
        for id in [7u32, 9u32] {
            let ns = NsId(id);
            let demote = DemotionConfig::for_budget(1 << 20, 4 << 10);
            assert!(
                store
                    .materialize_tiered(
                        ns,
                        AddressSpaceConfig {
                            reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                            page_bytes: 4 << 10,
                            life_origin: LogicalAddr::ZERO,
                        },
                        demote,
                        64,
                    )
                    .is_ok()
            );
        }
        // ns 7 admits user bytes and stages their WAL records: a measured
        // ratio. ns 9 stages a delete's WAL record and admits nothing:
        // unbounded, which is the arm a blended average would erase.
        let table = store.tiered_store_mut(NsId(7)).expect("materialized");
        let value = [0x41u8; 64];
        for i in 0..4u32 {
            let key = format!("k{i}");
            let effect =
                MutationEffect::StringSet { ns: NsId(7), key: key.as_bytes(), value: &value };
            table.stage_wal(&mut ring, &effect).expect("frame has room");
            table
                .insert(key.as_bytes(), &value, TieredTable::hash_key(key.as_bytes()))
                .expect("fits");
        }
        let measured = table.write_accounting();
        let expect_milli =
            measured.write_amplification().milli().expect("user bytes were admitted");
        let table = store.tiered_store_mut(NsId(9)).expect("materialized");
        table
            .stage_wal(&mut ring, &MutationEffect::Delete { ns: NsId(9), key: b"gone" })
            .expect("frame has room");

        let text =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"tiering"])).expect("ascii");
        let ns7 = text.lines().find(|l| l.starts_with("tiering_ns7:")).expect("ns 7 line");
        let ns9 = text.lines().find(|l| l.starts_with("tiering_ns9:")).expect("ns 9 line");
        assert!(expect_milli > 1_000, "WAL bytes exceed user bytes: {expect_milli}");
        assert!(ns7.contains(&format!("write_amp_milli={expect_milli}")), "{ns7}");
        assert!(ns9.contains("write_amp_milli=undefined"), "no denominator: {ns9}");
        assert!(
            text.contains(&format!("tiering_write_amp_milli_max:{expect_milli}")),
            "the aggregate is the worst measured namespace: {text}"
        );
        assert!(
            text.contains("tiering_write_amp_undefined_ns:1"),
            "the unbounded namespace is counted, not averaged: {text}"
        );
    }

    /// M4-S19 (ADR-0062): the `INF.NS` tiering surface — the D1 rule
    /// (tier keys require MODE durable + MEM-BUDGET), the D8 `USE`
    /// refusal, `SET` hot-reload with the CreateOnly refusals, and the
    /// tier block read back through `INF.NS INFO`.
    #[test]
    fn inf_ns_tiering_surface() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        // Tier keys without MEM-BUDGET refuse typed (the discriminator).
        let r = run(
            &mut cx,
            &mut store,
            &[b"INF.NS", b"CREATE", b"t", b"MODE", b"durable", b"DISK-BUDGET", b"1gb"],
        );
        assert!(r.starts_with(b"-ERR tiering keys require MEM-BUDGET"), "{r:?}");
        // MEM-BUDGET on MODE memory hits the D1 registry rule.
        let r = run(&mut cx, &mut store, &[b"INF.NS", b"CREATE", b"t", b"MEM-BUDGET", b"64mb"]);
        assert!(
            r.starts_with(b"-ERR tiering keys (MEM-BUDGET ...) apply to MODE durable"),
            "{r:?}"
        );
        // A tiered durable create parses, then hits the planeless
        // durable refusal — the node runtime owns real creation.
        let r = run(
            &mut cx,
            &mut store,
            &[b"INF.NS", b"CREATE", b"t", b"MODE", b"durable", b"MEM-BUDGET", b"64mb"],
        );
        assert!(r.starts_with(b"-ERR durable namespaces need the node runtime"), "{r:?}");

        // Materialize through the spec path (the node path's effect) and
        // drive the rest of the surface against it.
        let tier = TierSpec::for_budget(8 << 20);
        store
            .ns_create(NsSpec {
                id: inf_store::NsId(16),
                name: b"tiered".to_vec(),
                mode: NsMode::Durable,
                fsync: None,
                policy: None,
                maxmemory: None,
                tier: Some(tier),
            })
            .expect("create");
        // M4-S26: the D8 refusal is lifted — USE succeeds; the planeless
        // exec fallback still refuses data commands (plane-resident).
        let r = run(&mut cx, &mut store, &[b"INF.NS", b"USE", b"tiered"]);
        assert_eq!(r, b"+OK\r\n");
        cx.ns = crate::exec::ConnNamespace::Default;
        let r =
            run(&mut cx, &mut store, &[b"INF.NS", b"SET", b"tiered", b"MUTABLE-FRACTION", b"300"]);
        assert_eq!(r, b"+OK\r\n");
        assert_eq!(
            store.tiered_store_mut(inf_store::NsId(16)).expect("table").demotion().mutable_permille,
            300,
            "hot reload reached the table"
        );
        let r =
            run(&mut cx, &mut store, &[b"INF.NS", b"SET", b"tiered", b"TIER-IO-MODE", b"buffered"]);
        assert!(r.starts_with(b"-ERR TIER-IO-MODE is create-only"), "{r:?}");
        let r = run(
            &mut cx,
            &mut store,
            &[b"INF.NS", b"SET", b"tiered", b"COMPACTION-DEAD-RATIO", b"10"],
        );
        assert!(r.starts_with(b"-ERR COMPACTION-DEAD-RATIO is 50..=100"), "{r:?}");
        // INFO reads the whole tier block back, key for key.
        let r = run(&mut cx, &mut store, &[b"INF.NS", b"INFO", b"tiered"]);
        let text = String::from_utf8_lossy(&r);
        for field in [
            "mem-budget",
            "disk-budget",
            "mutable-fraction",
            "cold-read-qd",
            "compaction-dead-ratio",
            "blob-threshold",
            "tier-io-mode",
            "tail-stall-timeout",
        ] {
            assert!(text.contains(field), "missing {field}: {text}");
        }
        assert!(text.contains("300"), "the reloaded fraction renders: {text}");
        // ADR-0068 D1: the pressure keys refuse on a tiered namespace with
        // the one-budget-authority error, at SET like at CREATE.
        let r = run(&mut cx, &mut store, &[b"INF.NS", b"SET", b"tiered", b"MAXMEMORY", b"1mb"]);
        assert!(r.starts_with(b"-ERR MAXMEMORY does not apply to tiered namespaces"), "{r:?}");
    }

    /// M4-S27 (ADR-0068 D3): the `INF.NS` memory-pressure surface —
    /// `MAXMEMORY`/`EVICTION` hot-reload on memory namespaces (with the
    /// `inherit`/`0` reset vocabulary `INF.NS INFO` displays), typed
    /// refusals on durable namespaces and for tier keys on memory ones.
    #[test]
    fn inf_ns_memory_pressure_surface() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        let r = run(
            &mut cx,
            &mut store,
            &[b"INF.NS", b"CREATE", b"cache", b"EVICTION", b"allkeys-random"],
        );
        assert_eq!(r, b"+OK\r\n");
        let r = run(
            &mut cx,
            &mut store,
            &[b"INF.NS", b"SET", b"cache", b"MAXMEMORY", b"1mb", b"EVICTION", b"allkeys-lru"],
        );
        assert_eq!(r, b"+OK\r\n");
        let r = run(&mut cx, &mut store, &[b"INF.NS", b"INFO", b"cache"]);
        let text = String::from_utf8_lossy(&r).into_owned();
        assert!(text.contains("allkeys-lru"), "{text}");
        assert!(text.contains("1048576"), "{text}");
        let ns = store.ns_get(b"cache").expect("registered").id;
        let _ = store.ns_store_mut(ns);
        assert!(store.ns_free_for_write(ns, Nanos(1)).is_some(), "budget gate armed");
        // `inherit` / `0` return the knobs to the node config.
        let r = run(
            &mut cx,
            &mut store,
            &[b"INF.NS", b"SET", b"cache", b"MAXMEMORY", b"0", b"EVICTION", b"inherit"],
        );
        assert_eq!(r, b"+OK\r\n");
        assert!(store.ns_free_for_write(ns, Nanos(1)).is_none(), "budget gate disarmed");
        let r = run(&mut cx, &mut store, &[b"INF.NS", b"INFO", b"cache"]);
        let text = String::from_utf8_lossy(&r).into_owned();
        assert!(text.contains("inherit"), "{text}");
        // Tier keys on a memory namespace answer the create-time rule.
        let r = run(&mut cx, &mut store, &[b"INF.NS", b"SET", b"cache", b"MEM-BUDGET", b"8mb"]);
        assert!(r.starts_with(b"-ERR not a tiered namespace"), "{r:?}");
        // Durable namespaces refuse both keys typed (planeless memory-only
        // CREATE means the durable entry registers directly).
        store
            .ns_create(NsSpec {
                id: inf_store::NsId(31),
                name: b"ledger".to_vec(),
                mode: NsMode::Durable,
                fsync: None,
                policy: None,
                maxmemory: None,
                tier: None,
            })
            .expect("create");
        let r = run(&mut cx, &mut store, &[b"INF.NS", b"SET", b"ledger", b"MAXMEMORY", b"1mb"]);
        assert!(r.starts_with(b"-ERR durable namespaces do not evict"), "{r:?}");
    }

    /// M4-S18 (ADR-0061 D8): the per-namespace line splits the blob leg
    /// out as its own ratio — `blob_bytes / blob_user_bytes`, ≈ 1× by
    /// construction — and the cell aggregate is that ratio's maximum,
    /// never a fold into the record ratio (a byte written once is
    /// counted in exactly one leg).
    #[test]
    fn info_tiering_splits_the_blob_write_amplification_leg() {
        use std::path::Path;

        use inf_log::TierIoMode;
        use inf_log::blob::{ExtentId, ExtentWriter};
        use inf_log::fs::mem::MemFs;
        use inf_store::{AddressSpaceConfig, DemotionConfig, LogicalAddr, NsId, TieredTable};

        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
        let ns = NsId(21);
        let demote = DemotionConfig::for_budget(1 << 20, 4 << 10);
        assert!(
            store
                .materialize_tiered(
                    ns,
                    AddressSpaceConfig {
                        reserve_bytes: demote.ring_reserve_bytes().expect("valid budget"),
                        page_bytes: 4 << 10,
                        life_origin: LogicalAddr::ZERO,
                    },
                    demote,
                    64,
                )
                .is_ok()
        );
        let fs = MemFs::new();
        let value = vec![0x42u8; 9_000];
        let mut w = ExtentWriter::create(
            &fs,
            Path::new("shard-0"),
            ExtentId(1),
            0,
            ns,
            value.len() as u64,
            TierIoMode::Buffered,
        )
        .expect("create extent");
        w.append_chunk(&value).expect("chunk");
        let sealed = w.finish().expect("finish");
        let table = store.tiered_store_mut(ns).expect("materialized");
        table.note_blob_bytes(sealed.device_bytes());
        table
            .insert_extent(b"blob-key", TieredTable::hash_key(b"blob-key"), &sealed)
            .expect("fits");
        let expect_milli = table
            .write_accounting()
            .blob_write_amplification()
            .milli()
            .expect("blob bytes were admitted");
        assert!(expect_milli > 1_000, "device bytes exceed value bytes: {expect_milli}");

        let text =
            String::from_utf8(run(&mut cx, &mut store, &[b"INFO", b"tiering"])).expect("ascii");
        let line = text.lines().find(|l| l.starts_with("tiering_ns21:")).expect("ns line");
        assert!(line.contains("blob_user_bytes=9000"), "{line}");
        assert!(line.contains(&format!("blob_write_amp_milli={expect_milli}")), "{line}");
        assert!(
            text.contains(&format!("tiering_blob_write_amp_milli_max:{expect_milli}")),
            "the aggregate is the worst blob namespace: {text}"
        );
        assert!(
            text.contains("tiering_blob_write_amp_undefined_ns:0"),
            "a namespace with blob activity has a denominator: {text}"
        );
    }

    #[test]
    fn command_introspection_shapes() {
        let mut cx = ConnCx::default();
        let mut store = Keyspace::new(StoreConfig::default());
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
