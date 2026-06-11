//! Frozen tripwire counter names (milestone M0 §3.2, master plan §16).
//!
//! These are the always-on architecture health metrics — the instruments
//! that would have caught Vortex's batch=1.0 in week one. Names and
//! semantics are frozen at M0 exit; renaming one requires an ADR because
//! gate reports, CI checks, and dashboards key on these strings.

/// SQEs per io_uring submit (gate: ≥ 16 under pipelined load — L3).
pub const SQES_PER_SUBMIT: &str = "sqes_per_submit";
/// CQEs harvested per reap call.
pub const CQES_PER_REAP: &str = "cqes_per_reap";
/// Commands executed per reactor-loop iteration.
pub const CMDS_PER_ITER: &str = "cmds_per_iter";
/// Fabric messages moved per published batch.
pub const FABRIC_MSGS_PER_BATCH: &str = "fabric_msgs_per_batch";
/// p99.9 of reactor-loop iteration wall time, microseconds (gate: < 500).
pub const LOOP_ITER_P999_US: &str = "loop_iter_p999_us";

/// Memory attribution domains (L5). `sum(domains)` vs RSS divergence > 10%
/// fails CI (master plan §7.1).
pub const RECORDS_LIVE_BYTES: &str = "records_live_bytes";
pub const RECORDS_SLACK_BYTES: &str = "records_slack_bytes";
pub const INDEX_BYTES: &str = "index_bytes";
pub const WIRE_BUFFERS_BYTES: &str = "wire_buffers_bytes";
pub const CONN_STATE_BYTES: &str = "conn_state_bytes";
pub const PROCESS_RSS: &str = "process_rss";

/// All tripwire counter names, for report generators and scrape validation.
pub const ALL: &[&str] = &[
    SQES_PER_SUBMIT,
    CQES_PER_REAP,
    CMDS_PER_ITER,
    FABRIC_MSGS_PER_BATCH,
    LOOP_ITER_P999_US,
    RECORDS_LIVE_BYTES,
    RECORDS_SLACK_BYTES,
    INDEX_BYTES,
    WIRE_BUFFERS_BYTES,
    CONN_STATE_BYTES,
    PROCESS_RSS,
];
