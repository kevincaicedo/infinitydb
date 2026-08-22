//! `m4-tiered` (M4-S26): the **command-driven** tiered data plane under
//! deterministic simulation — the join of the two proven halves the S26
//! ledger names: the real-TCP `node_e2e` rows (commands against the wired
//! plane) and the store-tier DST rows (`m4-steel`, `m4-diskfull`,
//! `m4-cold`, `m4-recovery`). Here the **whole node** — exec routing,
//! cold-read suspension, MAINTAIN drivers, WAL staging with displacement
//! origins, hybrid checkpoints, MANIFEST v2, boot recovery — runs over
//! one [`SimDisk`] behind simulated RESP connections, so the nightly
//! sweep interleaves what `node_e2e` can only sample.
//!
//! One seeded run, in phases:
//!
//! 1. Boot a durable node; `INF.NS CREATE` a tiered namespace (small
//!    `MEM-BUDGET` so demotion engages inside the run; `BLOB-THRESHOLD`
//!    low so the ADR-0061 extent leg carries real traffic; `FSYNC
//!    always` so the §8.2 promise binds every acked write).
//! 2. Seeded SET/GET/DEL traffic with **exact** per-reply expectations
//!    (client-private keys, sequential per connection): inline values
//!    demote through the ring, ≥-threshold values ride blob extents,
//!    overwrites/deletes of cold candidates stage displacement markers.
//! 3. A seeded **power cut** mid-traffic tears every un-fsynced byte.
//! 4. Reboot (every 8th seed cuts again mid-recovery) → recovery
//!    composes MANIFEST v2 → tier files → hybrid checkpoint → WAL tail.
//! 5. Command audit: every ledger key `GET`s a §8.2-admissible state
//!    (`always` class — every acked op is required to survive).
//! 6. Post-recovery re-pressure: fresh writers overflow the budget in
//!    the recovered life; a bounded poll asserts the MAINTAIN flush
//!    drivers are **live** (flush-confirmed bytes must advance — a
//!    recovered plane whose demotion is wedged is a finding, not a
//!    timeout).
//! 7. Cold re-read sweep: every audited key must serve the exact bytes
//!    the audit observed (recovered + re-demoted content, CRC-verified
//!    cold reads included).
//! 8. `DISK-BUDGET` hot-reload clamp (ADR-0063): the typed `DISKFULL`
//!    refusal over commands, reads/deletes proceeding at the cap, and
//!    admission **reopening** when the budget lifts (recovery is
//!    automatic — the M1-S07 honesty pattern's disk twin).
//! 9. The S19 drop-race finale: pipelined cold GETs race `INF.NS DROP`;
//!    every reply is typed, the node answers `PING` after.
//!
//! Coverage is disclosed, never assumed (ADR-0045 D4): flushed bytes,
//! cold resolves, blob sets, refusals, and drop-race reply classes are
//! reported per seed and aggregated in sweep manifests. Every event
//! folds into `trace_hash`; `--verify-determinism` runs the scenario
//! twice and requires trace identity (L7).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_server::StallConfig;

use crate::durable::{
    DurableScenario, DurableWorkload, MiniClient, Node, NsClass, OpRec, Pending, STALL_STEPS,
    TraceObserver, Writer, admissible_states, boot, build_disk, bulk, encode, m2_stall_config,
    required_index,
};
use crate::net::Plant;
use crate::resp::reply_len;

/// The tiered namespace every phase drives.
const NS_NAME: &[u8] = b"t";

/// `MEM-BUDGET` + `MAINTAIN-SLICE` at the smallest admissible pair: the
/// alloc-admission window (budget + slice) must clear four region pages
/// (4 × `REGION_PAGE_BYTES` = 4 MiB), and 3 MiB + 1 MiB lands exactly on
/// it with a 4 MiB ring — the `node_e2e` proven shape.
const MEM_BUDGET: &[u8] = b"3mb";
const MAINTAIN_SLICE: &[u8] = b"1mb";
/// `MUTABLE-FRACTION` 100‰ of 3 MiB = a ~300 KiB per-cell mutable
/// target: phase-2 traffic overflows it, so demotion, flush, and cold
/// reads happen inside the run, not past its end.
const MUTABLE_FRACTION: &[u8] = b"100";
/// The phase-6 clamp (hot-reload): 10‰ = ~30 KiB — any recovered tail
/// plus the re-pressure fill overflows it by construction, making the
/// flush-liveness oracle structural rather than statistical.
const MUTABLE_FRACTION_CLAMP: &[u8] = b"10";
/// `BLOB-THRESHOLD` at its 4 KiB floor: the blob generator arm (≥ 6 KiB
/// values) stores out of line, the inline arm (≤ 4 KiB) never does.
const BLOB_THRESHOLD: &[u8] = b"4kb";
/// `TIER-IO-MODE buffered`: the simulated disk models a buffered device
/// (every store-tier sim scenario runs Buffered; the plane's `Direct`
/// default is a real-NVMe posture the sim cannot honor).
const TIER_IO_MODE: &[u8] = b"buffered";

/// Scenario knobs — the DSL v0 shape (a struct, not a language).
#[derive(Clone, Debug)]
pub struct TieredScenario {
    pub seed: u64,
    pub cells: u16,
    /// Phase-2 writers (ids 0..writers; keys are client-private).
    pub writers: usize,
    pub ops_per_writer: u64,
    pub keys_per_writer: u64,
    /// Post-recovery writers (fresh ids and key ranges — exact
    /// expectations stay valid because their keys start absent).
    pub post_writers: usize,
    pub post_ops_per_writer: u64,
    /// Max virtual nanoseconds per scheduler step.
    pub step_ns_max: u64,
    /// A second power cut lands *during* recovery (idempotence).
    pub double_cut: bool,
    pub segment_bytes: u32,
    pub ckpt_interval_bytes: u64,
    /// Device service-time model (the S14 reference stall device by
    /// default — fsync latency is part of the interleaving space).
    pub stall: Option<StallConfig>,
}

impl TieredScenario {
    #[must_use]
    pub fn m4_tiered(seed: u64) -> TieredScenario {
        // Sizing note: seal marks land per commit page (1 MiB at plane
        // tier — `REGION_PAGE_BYTES`), and the flush watermark holds
        // back a chunk whose end lies past the last full durable frame,
        // so *confirmed* flush needs each cell's ring ≥ ~2.2 MiB (two
        // page marks → two chunk ends, the earlier one confirmable).
        // 0.55 × ops × ~3.5 KiB ≈ 4.6 MiB per phase, hash-split across
        // two cells ≈ 2.3 MiB/cell, clears it in both lives.
        TieredScenario {
            seed,
            cells: 2,
            writers: 6,
            ops_per_writer: 400,
            keys_per_writer: 24,
            post_writers: 6,
            post_ops_per_writer: 400,
            step_ns_max: 2_000_000,
            double_cut: seed % 8 == 3,
            // Document-scenario sizing: 64 KiB segments hold the worst
            // group-commit frame (inline values ≤ 3 KiB); the 24 KiB
            // checkpoint interval keeps hybrid walks, MANIFEST swaps,
            // and truncation cycling inside a short run.
            segment_bytes: 64 << 10,
            ckpt_interval_bytes: 24 << 10,
            stall: Some(m2_stall_config()),
        }
    }

    /// The harness plumbing view of this scenario (`boot` + `Node::step`
    /// read cells/seed/plant/segment/ckpt through the durable shape; the
    /// workload fields are unused — this module drives its own traffic).
    fn harness(&self) -> DurableScenario {
        DurableScenario {
            seed: self.seed,
            workload: DurableWorkload::KeyValue,
            cells: self.cells,
            always_writers: 0,
            esec_writers: 0,
            mem_writers: 0,
            ops_per_writer: 0,
            keys_per_writer: self.keys_per_writer,
            value_max: 0,
            step_ns_max: self.step_ns_max,
            double_cut: self.double_cut,
            plant: Plant::None,
            segment_bytes: self.segment_bytes,
            ckpt_interval_bytes: self.ckpt_interval_bytes,
            ckpt_stream_bytes_per_sec: None,
            ckpt_section_bytes: None,
            stall: self.stall.clone(),
            replay_canary: false,
            io_mode: inf_server::SegmentIoMode::Buffered,
            frames_in_flight: 1,
            device: Default::default(),
            budget_oracle: false,
            reorder_oracle: false,
            ckpt_direct_refused_after: None,
            fill: Default::default(),
            prelude: None,
            recycle_slots: 0,
            recycle_oracle: false,
        }
    }
}

/// What one seeded run produced. Coverage counters are disclosures
/// (ADR-0045 D4) — sweeps aggregate them so a fleet that stopped
/// demoting or stopped refusing is visible in the manifest.
#[derive(Debug, Default)]
pub struct TieredNodeReport {
    pub trace: Vec<u8>,
    pub trace_hash: u64,
    pub violations: Vec<String>,
    pub stalled: bool,
    /// The reboot refused with the ADR-0018 taxonomy error — legal
    /// (§8.4 prefers refusing to serve over truncating possibly-covered
    /// data), counted, and the run ends early with phases 5–9 skipped.
    pub refused_boot: bool,
    pub commands_done: u64,
    pub scheduler_steps: u64,
    pub sim_seconds: f64,
    pub audited_keys: u64,
    pub required_ops: u64,
    pub allowed_lost_ops: u64,
    /// Tier bytes flush-confirmed before the cut (life 1).
    pub flushed_pre_cut_bytes: u64,
    /// Tier bytes flush-confirmed at the end of phase 6 (life 2+).
    pub flushed_final_bytes: u64,
    /// Cold resolves observed in the final life (suspension path).
    pub cold_resolves: u64,
    /// SETs at or above `BLOB-THRESHOLD` (the ADR-0061 extent leg).
    pub blob_sets: u64,
    /// Typed `DISKFULL` refusals observed at the clamped budget.
    pub diskfull_refusals: u64,
    /// Admission reopened after the budget lifted (phase 8's second
    /// half succeeded).
    pub diskfull_reopened: bool,
    /// Drop-race replies that carried a value (read won the race).
    pub drop_replies_value: u64,
    /// Drop-race replies that answered typed errors/nils (drop won).
    pub drop_replies_other: u64,
}

impl TieredNodeReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        !self.stalled && self.violations.is_empty()
    }
}

/// Deterministic value bytes: a `tag:id:sent:` stamp cycled to `len`
/// (exact expectations need exact bytes, not lengths).
fn value_bytes(tag: u8, id: usize, sent: u64, len: usize) -> Vec<u8> {
    let stamp = format!("{}:{id}:{sent}:", tag as char).into_bytes();
    stamp.iter().copied().cycle().take(len).collect()
}

/// Builds the next tiered command + its exact expected reply: inline
/// SETs (1–3 KiB — ring residents that demote), blob SETs (6–10 KiB —
/// out-of-line extents, ADR-0061), exact GETs, and counted DELs.
/// Overwrites across the arms exercise blob-over-inline,
/// inline-over-blob, and cold-candidate displacement (ADR-0057 D4).
fn next_tiered_command(
    writer: &mut Writer,
    scenario: &TieredScenario,
    blob_sets: &mut u64,
) -> (Vec<u8>, Pending) {
    let key = writer.key(scenario.keys_per_writer);
    let roll = writer.rng.next_below(100);
    if roll < 65 {
        let (tag, len) = if roll < 55 {
            (b'i', 3072 + writer.rng.next_below(1024) as usize)
        } else {
            *blob_sets += 1;
            (b'b', (6 << 10) + writer.rng.next_below(4096) as usize)
        };
        let value = value_bytes(tag, writer.id, writer.sent, len);
        let wire = encode(&[b"SET", &key, &value]);
        let pending = Pending {
            key,
            state_after: Some(value),
            expect: b"+OK\r\n".to_vec(),
            mutates: true,
            taints: false,
        };
        (wire, pending)
    } else if roll < 82 {
        let state_after = writer.last_state(&key);
        let expect = state_after.as_ref().map_or(b"$-1\r\n".to_vec(), |v| bulk(v));
        let wire = encode(&[b"GET", &key]);
        (wire, Pending { key, state_after, expect, mutates: false, taints: false })
    } else {
        let existed = writer.last_state(&key).is_some();
        let expect = if existed { b":1\r\n".to_vec() } else { b":0\r\n".to_vec() };
        let wire = encode(&[b"DEL", &key]);
        (wire, Pending { key, state_after: None, expect, mutates: true, taints: false })
    }
}

/// One traffic pump round for every writer on `node`: drain replies
/// (asserting exact expectations + recording acks), then send the next
/// command where a slot is free. Returns delivered-byte+send progress
/// (the stall detector's currency).
fn pump_writers(
    node: &mut Node,
    writers: &mut [Writer],
    scenario: &TieredScenario,
    clock: &Rc<VirtualClock>,
    report: &mut TieredNodeReport,
    blob_sets: &mut u64,
) -> u64 {
    let mut progress = 0u64;
    for writer in writers.iter_mut() {
        let mut net = node.nets[writer.cell].borrow_mut();
        let bytes = net.client_recv(writer.fd);
        progress += bytes.len() as u64;
        writer.rx.extend_from_slice(&bytes);
        while let Some(n) = reply_len(&writer.rx) {
            let reply: Vec<u8> = writer.rx.drain(..n).collect();
            if writer.setup {
                if reply != b"+OK\r\n" {
                    report.violations.push(format!("writer {}: USE answered {reply:?}", writer.id));
                }
                writer.setup = false;
                continue;
            }
            let Some(pending) = writer.inflight.take() else {
                report
                    .violations
                    .push(format!("writer {}: unsolicited reply {reply:?}", writer.id));
                continue;
            };
            if reply != pending.expect {
                report.violations.push(format!(
                    "writer {} key {:?}: expected {}, got {}",
                    writer.id,
                    String::from_utf8_lossy(&pending.key),
                    preview(&pending.expect),
                    preview(&reply)
                ));
            }
            if pending.mutates {
                let ops = writer.ledger.entry(pending.key.clone()).or_default();
                let rec = ops.last_mut().expect("sent op has a ledger entry");
                rec.acked_at = Some(clock.now());
            }
            writer.replied += 1;
            report.commands_done += 1;
        }
        if writer.setup || writer.inflight.is_some() || writer.sent >= writer.quota {
            continue;
        }
        let (wire, pending) = next_tiered_command(writer, scenario, blob_sets);
        if pending.mutates {
            writer.ledger.entry(pending.key.clone()).or_default().push(OpRec {
                state_after: pending.state_after.clone(),
                sent_at: clock.now(),
                acked_at: None,
            });
        }
        writer.inflight = Some(pending);
        net.client_send(writer.fd, &wire);
        writer.sent += 1;
        progress += 1;
    }
    progress
}

fn preview(reply: &[u8]) -> String {
    let cut = reply.len().min(48);
    format!(
        "{:?}{}",
        String::from_utf8_lossy(&reply[..cut]),
        if reply.len() > cut { "…" } else { "" }
    )
}

/// Extracts one `key:value` integer from an `INFO` section text.
fn info_field(text: &str, key: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix(key).and_then(|rest| rest.strip_prefix(':')))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// `INFO tiering` through a [`MiniClient`], bulk payload decoded to text.
#[allow(clippy::too_many_arguments)] // one call site's plumbing, like MiniClient::call
fn info_tiering(
    client: &mut MiniClient,
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &inf_server::SimDisk,
    step_ns_max: u64,
) -> Result<String, String> {
    let reply = client
        .call(node, rng, clock, disk, step_ns_max, &[b"INFO", b"tiering"])
        .map_err(|e| format!("INFO tiering: {e}"))?
        .ok_or_else(|| "INFO tiering stalled".to_string())?;
    if !reply.starts_with(b"$") {
        return Err(format!("INFO tiering answered {}", preview(&reply)));
    }
    let header =
        reply.windows(2).position(|w| w == b"\r\n").ok_or_else(|| "INFO framing".to_string())?;
    Ok(String::from_utf8_lossy(&reply[header + 2..reply.len() - 2]).into_owned())
}

/// Runs one seeded tiered-node scenario (the phase list in the module
/// docs). Violations carry the seed and the exact key/reply — a sweep
/// line is a complete repro via `--seed`.
#[allow(clippy::too_many_lines)] // one linear phase script, like run_durable_scenario
#[must_use]
pub fn run_tiered_scenario(scenario: &TieredScenario) -> TieredNodeReport {
    let harness = scenario.harness();
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(scenario.seed, scenario.stall.as_ref());
    let observer = TraceObserver::default();
    let mut rng = SplitMix64::new(scenario.seed ^ 0x71E7_ED00);
    let mut report = TieredNodeReport::default();
    let seed = scenario.seed;
    let fail = |report: &mut TieredNodeReport, what: String| {
        report.violations.push(format!("seed {seed:#x}: {what}"));
    };

    // ---- phase 1: boot + tiered DDL -----------------------------------
    let mut node = match boot(&harness, PathBuf::from("node"), &disk, &clock, &observer) {
        Ok(node) => node,
        Err(err) => {
            fail(&mut report, format!("boot 1 failed: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    let mut setup = MiniClient::connect(&mut node, 0);
    let create: &[&[u8]] = &[
        b"INF.NS",
        b"CREATE",
        NS_NAME,
        b"MODE",
        b"durable",
        b"FSYNC",
        b"always",
        b"MEM-BUDGET",
        MEM_BUDGET,
        b"MUTABLE-FRACTION",
        MUTABLE_FRACTION,
        b"MAINTAIN-SLICE",
        MAINTAIN_SLICE,
        b"BLOB-THRESHOLD",
        BLOB_THRESHOLD,
        b"TIER-IO-MODE",
        TIER_IO_MODE,
    ];
    match setup.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, create) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            fail(&mut report, format!("tiered CREATE answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    }

    // ---- phase 2: seeded traffic until the cut -------------------------
    let mut writers = Vec::new();
    for id in 0..scenario.writers {
        let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
        let fd = node.nets[cell].borrow_mut().connect();
        let writer = Writer::new(
            id,
            cell,
            fd,
            NsClass::Always,
            scenario.seed,
            scenario.ops_per_writer,
            true,
            0,
        );
        node.nets[cell].borrow_mut().client_send(fd, &encode(&[b"INF.NS", b"USE", NS_NAME]));
        writers.push(writer);
    }
    let total_ops: u64 = writers.iter().map(|w| w.quota).sum();
    // The cut window starts after the fill typically demotes and extends
    // past typical completion — early-cut and post-completion-cut seeds
    // both exist in the corpus (their coverage counters disclose which).
    let cut_step = 600 + rng.next_below(total_ops * 6);
    let mut blob_sets = 0u64;
    let mut idle_steps = 0u64;
    for _ in 0..cut_step {
        report.scheduler_steps += 1;
        if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
            fail(&mut report, format!("traffic phase: {err}"));
            return finish(report, &observer, &clock);
        }
        let progress =
            pump_writers(&mut node, &mut writers, scenario, &clock, &mut report, &mut blob_sets);
        if progress == 0 {
            idle_steps += 1;
            if idle_steps >= STALL_STEPS && writers.iter().any(|w| w.replied < w.sent) {
                let stuck: Vec<String> = writers
                    .iter()
                    .filter(|w| w.replied < w.sent)
                    .map(|w| format!("writer {}", w.id))
                    .collect();
                report.stalled = true;
                fail(
                    &mut report,
                    format!(
                        "traffic stalled before the cut with in-flight ops ({})",
                        stuck.join(", ")
                    ),
                );
                return finish(report, &observer, &clock);
            }
        } else {
            idle_steps = 0;
        }
    }
    report.blob_sets = blob_sets;
    // Pre-cut coverage scrape (pumps a bounded number of extra steps —
    // part of the deterministic schedule, and the cut still lands with
    // writers mid-flight because the scrape never quiesces them).
    match info_tiering(&mut setup, &mut node, &mut rng, &clock, &disk, scenario.step_ns_max) {
        Ok(text) => {
            report.flushed_pre_cut_bytes = info_field(&text, "tiering_flush_confirmed_bytes");
        }
        Err(err) => fail(&mut report, format!("pre-cut scrape: {err}")),
    }

    // ---- phase 3: POWER CUT --------------------------------------------
    let cut_time = clock.now();
    drop(node);
    disk.power_cut(scenario.seed ^ 0x0FF5_EED0);

    // ---- phase 4: reboot (+ optional second cut mid-recovery) -----------
    let mut boots = 0;
    let node = loop {
        boots += 1;
        let mut node = match boot(&harness, PathBuf::from("node"), &disk, &clock, &observer) {
            Ok(node) => node,
            Err(err) => {
                fail(&mut report, format!("reboot {boots} refused: {err}"));
                return finish(report, &observer, &clock);
            }
        };
        let double = scenario.double_cut && boots == 1;
        let recovery_budget = if double { 1 + rng.next_below(200) } else { u64::MAX };
        let mut steps = 0u64;
        let mut failed = None;
        while !node.ready() && steps < recovery_budget {
            steps += 1;
            report.scheduler_steps += 1;
            if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
                failed = Some(err);
                break;
            }
            if steps > STALL_STEPS {
                report.stalled = true;
                fail(&mut report, format!("recovery stalled on boot {boots}"));
                return finish(report, &observer, &clock);
            }
        }
        if let Some(err) = failed {
            if err.to_string().contains("log corruption") {
                // The ADR-0018 taxonomy refusal is a LEGAL outcome
                // (§8.4): counted and disclosed; the sweep manifest
                // separates refusals from violations. The command-driven
                // phases need a serving node, so the run ends here.
                report.refused_boot = true;
                eprintln!("refused boot {boots}: {err}");
            } else {
                fail(&mut report, format!("recovery failed on boot {boots}: {err}"));
            }
            return finish(report, &observer, &clock);
        }
        if node.ready() {
            break node;
        }
        drop(node);
        disk.power_cut(scenario.seed ^ 0x0FF5_EED1 ^ boots);
    };
    let mut node = node;

    // ---- phase 5: the §8.2 command audit (never-none at node scale) -----
    let mut audit = MiniClient::connect(&mut node, 0);
    let reply = audit.call(
        &mut node,
        &mut rng,
        &clock,
        &disk,
        scenario.step_ns_max,
        &[b"INF.NS", b"USE", NS_NAME],
    );
    if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
        fail(&mut report, format!("acked tiered CREATE lost: audit USE answered {reply:?}"));
        return finish(report, &observer, &clock);
    }
    // Observed post-recovery replies, kept for the phase-7 cold sweep
    // (phases 6–8 never touch phase-2 keys, so equality stays exact).
    let mut observed: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    for writer in &writers {
        for (key, ops) in &writer.ledger {
            report.audited_keys += 1;
            let required = required_index(NsClass::Always, ops, cut_time);
            report.required_ops += required.map_or(0, |i| i as u64 + 1);
            report.allowed_lost_ops += ops.len() as u64 - required.map_or(0, |i| i as u64 + 1);
            let reply = match audit.call(
                &mut node,
                &mut rng,
                &clock,
                &disk,
                scenario.step_ns_max,
                &[b"GET", key],
            ) {
                Ok(Some(reply)) => reply,
                other => {
                    fail(&mut report, format!("audit GET {key:?} answered {other:?}"));
                    return finish(report, &observer, &clock);
                }
            };
            let admissible: Vec<Vec<u8>> = admissible_states(ops, required)
                .iter()
                .map(|state| state.as_ref().map_or(b"$-1\r\n".to_vec(), |v| bulk(v)))
                .collect();
            if !admissible.contains(&reply) {
                report.violations.push(format!(
                    "DURABILITY VIOLATION seed {seed:#x} key {:?}: recovered {} is outside \
                     the admissible set (required op index {required:?}, {} ops)",
                    String::from_utf8_lossy(key),
                    preview(&reply),
                    ops.len()
                ));
            }
            observed.insert(key.clone(), reply);
        }
    }

    // ---- phase 6: re-pressure + MAINTAIN flush liveness ------------------
    // Clamp the mutable fraction first (hot-reload): the ~30 KiB target
    // guarantees the re-pressure fill overflows on every cell, so "flush
    // never advanced" below is a wedged driver, never a small workload.
    let clamp_mutable: &[&[u8]] =
        &[b"INF.NS", b"SET", NS_NAME, b"MUTABLE-FRACTION", MUTABLE_FRACTION_CLAMP];
    match audit.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, clamp_mutable) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            fail(&mut report, format!("MUTABLE-FRACTION clamp answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    }
    let flushed_base =
        match info_tiering(&mut audit, &mut node, &mut rng, &clock, &disk, scenario.step_ns_max) {
            Ok(text) => info_field(&text, "tiering_flush_confirmed_bytes"),
            Err(err) => {
                fail(&mut report, format!("phase-6 base scrape: {err}"));
                return finish(report, &observer, &clock);
            }
        };
    let mut post_writers = Vec::new();
    for i in 0..scenario.post_writers {
        let id = 100 + i; // fresh id ⇒ fresh key range ⇒ exact expectations
        let cell = (rng.next_u64() % u64::from(scenario.cells)) as usize;
        let fd = node.nets[cell].borrow_mut().connect();
        let writer = Writer::new(
            id,
            cell,
            fd,
            NsClass::Always,
            scenario.seed,
            scenario.post_ops_per_writer,
            true,
            0,
        );
        node.nets[cell].borrow_mut().client_send(fd, &encode(&[b"INF.NS", b"USE", NS_NAME]));
        post_writers.push(writer);
    }
    let mut idle_steps = 0u64;
    let mut post_blob_sets = 0u64;
    while post_writers.iter().any(|w| w.setup || w.replied < w.quota) {
        report.scheduler_steps += 1;
        if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
            fail(&mut report, format!("re-pressure phase: {err}"));
            return finish(report, &observer, &clock);
        }
        let progress = pump_writers(
            &mut node,
            &mut post_writers,
            scenario,
            &clock,
            &mut report,
            &mut post_blob_sets,
        );
        if progress == 0 {
            idle_steps += 1;
            if idle_steps >= STALL_STEPS {
                report.stalled = true;
                fail(&mut report, "re-pressure traffic stalled".to_string());
                return finish(report, &observer, &clock);
            }
        } else {
            idle_steps = 0;
        }
    }
    report.blob_sets += post_blob_sets;
    // Flush liveness: the recovered life appended well past the mutable
    // target, so flush-confirmed bytes MUST advance. Each poll pumps
    // bounded steps; a wedged MAINTAIN driver is the violation.
    let mut flushed_now = flushed_base;
    let mut last_text = String::new();
    for _ in 0..64 {
        match info_tiering(&mut audit, &mut node, &mut rng, &clock, &disk, scenario.step_ns_max) {
            Ok(text) => {
                flushed_now = info_field(&text, "tiering_flush_confirmed_bytes");
                report.cold_resolves = info_field(&text, "tiering_cold_resolves");
                last_text = text;
                if flushed_now > flushed_base {
                    break;
                }
            }
            Err(err) => {
                fail(&mut report, format!("flush-liveness scrape: {err}"));
                return finish(report, &observer, &clock);
            }
        }
    }
    if flushed_now <= flushed_base {
        let gauge = |key: &str| info_field(&last_text, key);
        fail(
            &mut report,
            format!(
                "MAINTAIN FLUSH LIVENESS VIOLATION: flush-confirmed bytes stuck at \
                 {flushed_base} after the recovered life overflowed its budget \
                 (allocated={} committed={} sealed={} demote_slices={} flush_slices={} \
                 tail_allocs={} stalls={})",
                gauge("tiering_allocated_bytes"),
                gauge("tiering_committed_bytes"),
                gauge("tiering_demote_sealed_bytes"),
                gauge("tiering_demote_slices"),
                gauge("tiering_flush_slices"),
                gauge("tiering_tail_allocs"),
                gauge("tiering_tail_alloc_stalls"),
            ),
        );
    }
    report.flushed_final_bytes = flushed_now;

    // ---- phase 7: cold re-read sweep (exact bytes after re-demotion) -----
    for (key, want) in &observed {
        let reply = match audit.call(
            &mut node,
            &mut rng,
            &clock,
            &disk,
            scenario.step_ns_max,
            &[b"GET", key],
        ) {
            Ok(Some(reply)) => reply,
            other => {
                fail(&mut report, format!("cold sweep GET {key:?} answered {other:?}"));
                return finish(report, &observer, &clock);
            }
        };
        if &reply != want {
            report.violations.push(format!(
                "COLD REREAD VIOLATION seed {seed:#x} key {:?}: audit observed {}, re-read {}",
                String::from_utf8_lossy(key),
                preview(want),
                preview(&reply)
            ));
        }
    }
    // Post-sweep coverage scrape: the sweep's reads are where recovered
    // + re-demoted keys actually go cold (disclosure, ADR-0045 D4).
    match info_tiering(&mut audit, &mut node, &mut rng, &clock, &disk, scenario.step_ns_max) {
        Ok(text) => {
            report.cold_resolves = info_field(&text, "tiering_cold_resolves");
            report.flushed_final_bytes = info_field(&text, "tiering_flush_confirmed_bytes");
        }
        Err(err) => fail(&mut report, format!("post-sweep scrape: {err}")),
    }

    // ---- phase 8: DISKFULL clamp → typed refusal → reopen (ADR-0063) -----
    // A probe key guaranteed live before the clamp (GET/DEL at the cap
    // must have a target even if the clamp refuses instantly).
    let probe_value = value_bytes(b'p', 999, 0, 2 << 10);
    let probe: &[&[u8]] = &[b"SET", b"df:probe", &probe_value];
    match audit.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, probe) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            fail(&mut report, format!("df:probe SET answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    }
    let clamp: &[&[u8]] = &[b"INF.NS", b"SET", NS_NAME, b"DISK-BUDGET", b"1mb"];
    match audit.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, clamp) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            fail(&mut report, format!("DISK-BUDGET clamp answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    }
    // The admission projection (disk_used + unflushed tail, ADR-0063 D2)
    // sits far above 1 MiB by now — the typed refusal must arrive within
    // a few attempts (earlier OKs are legal while the refresh lands).
    let mut refused = false;
    for i in 0..50u32 {
        let key = format!("df:{i:03}").into_bytes();
        let value = value_bytes(b'd', 998, u64::from(i), 2 << 10);
        let set: &[&[u8]] = &[b"SET", &key, &value];
        let reply = match audit.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, set)
        {
            Ok(Some(reply)) => reply,
            other => {
                fail(&mut report, format!("diskfull fill SET answered {other:?}"));
                return finish(report, &observer, &clock);
            }
        };
        if reply.starts_with(b"-DISKFULL") {
            report.diskfull_refusals += 1;
            if !reply.starts_with(b"-DISKFULL tiered namespace disk budget exhausted (used=") {
                fail(&mut report, format!("DISKFULL shape drifted: {}", preview(&reply)));
            }
            refused = true;
            break;
        }
        if reply != b"+OK\r\n" {
            fail(&mut report, format!("diskfull fill reply untyped: {}", preview(&reply)));
            return finish(report, &observer, &clock);
        }
    }
    if !refused {
        fail(&mut report, "the clamped disk budget never refused (ADR-0063 D2)".to_string());
    }
    // Refusal scope is new-byte placements only (ADR-0063 D1): reads and
    // deletes proceed at the cap.
    let get_probe: &[&[u8]] = &[b"GET", b"df:probe"];
    match audit.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, get_probe) {
        Ok(Some(reply)) if reply == bulk(&probe_value) => {}
        other => fail(&mut report, format!("GET at the cap answered {other:?}")),
    }
    let del_probe: &[&[u8]] = &[b"DEL", b"df:probe"];
    match audit.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, del_probe) {
        Ok(Some(reply)) if reply == b":1\r\n" => {}
        other => fail(&mut report, format!("DEL at the cap answered {other:?}")),
    }
    // Lift the budget: admission must reopen without operator surgery
    // (the M1-S07 honesty pattern — recovery is automatic).
    let lift: &[&[u8]] = &[b"INF.NS", b"SET", NS_NAME, b"DISK-BUDGET", b"64mb"];
    match audit.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, lift) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            fail(&mut report, format!("DISK-BUDGET lift answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    }
    for i in 0..50u32 {
        let key = format!("dr:{i:03}").into_bytes();
        let value = value_bytes(b'r', 997, u64::from(i), 1 << 10);
        let set: &[&[u8]] = &[b"SET", &key, &value];
        let reply = match audit.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, set)
        {
            Ok(Some(reply)) => reply,
            other => {
                fail(&mut report, format!("reopen SET answered {other:?}"));
                return finish(report, &observer, &clock);
            }
        };
        if reply == b"+OK\r\n" {
            report.diskfull_reopened = true;
            break;
        }
        if !reply.starts_with(b"-DISKFULL") {
            fail(&mut report, format!("reopen reply untyped: {}", preview(&reply)));
            return finish(report, &observer, &clock);
        }
    }
    if !report.diskfull_reopened {
        fail(&mut report, "admission never reopened after the budget lifted".to_string());
    }

    // ---- phase 9: the S19 drop-race through the wire ----------------------
    let racer_cell = 0usize;
    let racer_fd = node.nets[racer_cell].borrow_mut().connect();
    node.nets[racer_cell]
        .borrow_mut()
        .client_send(racer_fd, &encode(&[b"INF.NS", b"USE", NS_NAME]));
    // Settle the USE reply before pipelining (one framed +OK).
    let mut rx = Vec::new();
    let mut settled = false;
    for _ in 0..STALL_STEPS {
        if node.step(&mut rng, &clock, &disk, scenario.step_ns_max).is_err() {
            break;
        }
        report.scheduler_steps += 1;
        let bytes = node.nets[racer_cell].borrow_mut().client_recv(racer_fd);
        rx.extend_from_slice(&bytes);
        if let Some(n) = reply_len(&rx) {
            let reply: Vec<u8> = rx.drain(..n).collect();
            if reply != b"+OK\r\n" {
                fail(&mut report, format!("racer USE answered {}", preview(&reply)));
                return finish(report, &observer, &clock);
            }
            settled = true;
            break;
        }
    }
    if !settled {
        report.stalled = true;
        fail(&mut report, "racer USE stalled".to_string());
        return finish(report, &observer, &clock);
    }
    let race_keys: Vec<Vec<u8>> = observed.keys().take(30).cloned().collect();
    let mut batch = Vec::new();
    for key in &race_keys {
        batch.extend_from_slice(&encode(&[b"GET", key]));
    }
    node.nets[racer_cell].borrow_mut().client_send(racer_fd, &batch);
    // DROP races the pipelined reads from a second connection.
    let drop_ns: &[&[u8]] = &[b"INF.NS", b"DROP", NS_NAME];
    match audit.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, drop_ns) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            fail(&mut report, format!("racing DROP answered {other:?}"));
            return finish(report, &observer, &clock);
        }
    }
    // Every pipelined reply must arrive typed — a missing reply is the
    // hang this row exists to catch (§3.3 teardown vs in-flight custody).
    let mut answered = 0usize;
    let mut idle = 0u64;
    while answered < race_keys.len() {
        if let Err(err) = node.step(&mut rng, &clock, &disk, scenario.step_ns_max) {
            fail(&mut report, format!("drop-race drain: {err}"));
            return finish(report, &observer, &clock);
        }
        report.scheduler_steps += 1;
        let bytes = node.nets[racer_cell].borrow_mut().client_recv(racer_fd);
        if bytes.is_empty() {
            idle += 1;
            if idle >= STALL_STEPS {
                report.stalled = true;
                fail(
                    &mut report,
                    format!(
                        "DROP-RACE HANG: {} of {} pipelined replies never arrived",
                        race_keys.len() - answered,
                        race_keys.len()
                    ),
                );
                return finish(report, &observer, &clock);
            }
        } else {
            idle = 0;
        }
        rx.extend_from_slice(&bytes);
        while let Some(n) = reply_len(&rx) {
            let reply: Vec<u8> = rx.drain(..n).collect();
            answered += 1;
            match reply.first() {
                Some(b'$') if reply.starts_with(b"$-1") => report.drop_replies_other += 1,
                Some(b'$') => report.drop_replies_value += 1,
                Some(b'-') => report.drop_replies_other += 1,
                _ => fail(&mut report, format!("untyped drop-race reply: {}", preview(&reply))),
            }
            if answered == race_keys.len() {
                break;
            }
        }
    }
    // The audit connection sits on the dropped namespace: its PING must
    // answer the typed dropped-namespace error (never a hang or a crash).
    let ping: &[&[u8]] = &[b"PING"];
    match audit.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, ping) {
        Ok(Some(reply)) if reply.starts_with(b"-ERR") => {}
        other => fail(&mut report, format!("post-drop PING (dropped ns) answered {other:?}")),
    }
    // The node itself stays live: a fresh connection serves.
    let mut fresh = MiniClient::connect(&mut node, 0);
    match fresh.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, ping) {
        Ok(Some(reply)) if reply == b"+PONG\r\n" => {}
        other => fail(&mut report, format!("post-drop PING (fresh conn) answered {other:?}")),
    }
    let use_dropped: &[&[u8]] = &[b"INF.NS", b"USE", NS_NAME];
    match fresh.call(&mut node, &mut rng, &clock, &disk, scenario.step_ns_max, use_dropped) {
        Ok(Some(reply)) if reply.starts_with(b"-ERR") => {}
        other => fail(&mut report, format!("USE of the dropped ns answered {other:?}")),
    }

    finish(report, &observer, &clock)
}

fn finish(
    mut report: TieredNodeReport,
    observer: &TraceObserver,
    clock: &Rc<VirtualClock>,
) -> TieredNodeReport {
    report.trace = observer.trace_bytes();
    report.trace_hash = hash64(&report.trace, 0x71E7);
    report.sim_seconds = clock.now().0.saturating_sub(1) as f64 / 1e9;
    report
}
