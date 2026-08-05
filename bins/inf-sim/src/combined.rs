//! The M2.5-S14 combined scenario: the unified product shape the two
//! fleets never ran together — durable (`always` + `everysec`) writers,
//! named-memory-namespace writers, pub/sub fan-out, and PEXPIRE traffic
//! interleaved on ONE node, then power-cut and recovered (double-cut on
//! the inherited `seed % 8 == 3` cadence).
//!
//! Composition, not a fork: the node, boot, reboot loop, and §8.2
//! durability audit come from [`crate::durable`]; the subscriber plans,
//! delivery ledger, and per-publisher FIFO checks come from
//! [`crate::harness`].
//!
//! Phase order: subscribers confirm → mixed traffic (memory writers
//! publish; durable writers stream SET/DEL/GET) → **pub/sub quiesces
//! before the cut** — pub/sub is not durable, so a mid-flight cut would
//! legally drop deliveries and void the exact-count contract; quiescing
//! first keeps the harness oracle's teeth and leaves the cut to exercise
//! durable + memory recovery — → seeded cut tail → power cut → reboot →
//! audits.
//!
//! Oracles, composed unchanged: the §8.2 admissible-set audit (`alw` /
//! `esec`), per-publisher FIFO + exact final delivery ledger, pub/sub
//! registry drain at quiescence, everysec ack-deferral. New
//! cross-semantics oracles (L2 made assertable):
//!
//! 1. **Memory is volatile across the cut** — every memory-namespace key
//!    written before the cut must read absent after recovery; a write
//!    path that logs memory records (or replay that fails to isolate
//!    them) resurrects one and fails here.
//! 2. **No memory record in the recovered log** — a post-recovery scan
//!    of every readable frame asserts zero records naming a non-durable
//!    namespace, catching L2 leakage at the log tier independently of
//!    the volatility check.
//!
//! Deviations from the pure scenarios, disclosed: memory writers live in
//! a named memory-mode namespace (`mem`), not the default DB — that is
//! the surface where a "unify the write path" staging bug is plausible
//! and where both new oracles have signal; the m1 harness's model-based
//! live-record reconciliation is not rerun here (it needs the model
//! keyspace) — the registry-drain and delivery-ledger checks are.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::rc::Rc;

use inf_foundation::hash64;
use inf_foundation::rng::{Entropy, SplitMix64};
use inf_foundation::time::{Clock, Nanos, VirtualClock};
use inf_log::{
    ReaderConfig, RecordView, SegmentId, SegmentReader, read_manifest, scan_log_dir_from,
};
use inf_server::{SimDisk, load_catalog_from};
use inf_store::NsMode;

use crate::durable::{
    AuditTally, DurableScenario, EVERYSEC_ACK_BOUND, MiniClient, NsClass, OpRec, Pending,
    STALL_STEPS, TraceObserver, Writer, admissible_states, boot, build_disk, bulk, encode,
    required_index, survival_audit,
};
use crate::harness::{SimSubscriber, SubState, subscription_plan};
use crate::resp::{SubFrame, parse_sub_frame, reply_len};

#[derive(Clone, Debug)]
pub struct CombinedScenario {
    /// The durable half: cells, writer counts, cut/double-cut cadence,
    /// stall device. `double_cut` is inherited from here (`seed % 8 == 3`
    /// — the m2-durable cadence, ~1/8 of the fleet).
    pub durable: DurableScenario,
    /// Pub/sub plane (harness semantics): dedicated subscriber
    /// connections, confirmed before any publisher fires.
    pub subscribers: usize,
    pub channels: u64,
    /// PUBLISH share of the memory writers' mix, percent.
    pub publish_percent: u64,
    /// Short-TTL PEXPIRE share of the memory writers' mix, percent.
    pub expire_percent: u64,
}

impl CombinedScenario {
    #[must_use]
    pub fn m2_combined(seed: u64) -> CombinedScenario {
        let mut durable = DurableScenario::m2_durable(seed);
        durable.mem_writers = 3;
        CombinedScenario {
            durable,
            subscribers: 4,
            channels: 4,
            publish_percent: 15,
            expire_percent: 10,
        }
    }
}

/// What one seeded combined run produced. `trace` is the determinism
/// artifact (apply events including the post-recovery audit reads).
#[derive(Debug)]
pub struct CombinedReport {
    pub trace: Vec<u8>,
    pub trace_hash: u64,
    pub violations: Vec<String>,
    pub stalled: bool,
    pub commands_done: u64,
    pub sim_seconds: f64,
    pub required_ops: u64,
    pub allowed_lost_ops: u64,
    pub audited_keys: u64,
    /// Memory-namespace keys audited for volatility (all must be absent
    /// post-recovery).
    pub memory_keys_audited: u64,
    pub scheduler_steps: u64,
    pub refused_boot: bool,
    pub published: u64,
    pub delivered: u64,
    /// Stall-engagement disclosure, as in the durable report (L10).
    pub always_ack_latency_ms_max: u64,
}

impl CombinedReport {
    #[must_use]
    pub fn ok(&self) -> bool {
        !self.stalled && self.violations.is_empty()
    }
}

/// The memory writers' extended mix: PUBLISH (`publish_percent`), short
/// PEXPIRE (`expire_percent`), then the SET/DEL/GET shape on the
/// writer's private keys. A TTL-tainted key (short PEXPIRE landed —
/// exact expectations void) is always repaired with a SET before any
/// other op touches it. Returns the wire bytes, the pending expectation,
/// and the published channel (the caller updates the delivery ledger).
fn next_memory_command(
    writer: &mut Writer,
    scenario: &CombinedScenario,
    chan_receivers: &[i64],
) -> (Vec<u8>, Pending, Option<u64>) {
    let roll = writer.rng.next_below(100);
    if roll < scenario.publish_percent {
        let chan = writer.rng.next_below(scenario.channels.max(1));
        writer.pub_seq[chan as usize] += 1;
        let payload = format!("m:{}:{}", writer.id, writer.pub_seq[chan as usize]).into_bytes();
        let channel = format!("chan:{chan}").into_bytes();
        let wire = encode(&[b"PUBLISH", &channel, &payload]);
        // The planned receiver count is exact: subscribers confirmed
        // before any publisher fired and unwind only after publishers
        // are done (the harness happens-before, kept).
        let expect = format!(":{}\r\n", chan_receivers[chan as usize]).into_bytes();
        let pending =
            Pending { key: channel, state_after: None, expect, mutates: false, taints: false };
        return (wire, pending, Some(chan));
    }
    if roll < scenario.publish_percent + scenario.expire_percent {
        // PEXPIRE a live, untainted key: writer-private keys + a
        // sequential window make `:1` exact. No live candidate ⇒ fall
        // through to the SET below (deterministic either way — the
        // ledger deciding the draw count is itself seed-determined).
        let candidates: Vec<Vec<u8>> = writer
            .ledger
            .iter()
            .filter(|(key, ops)| {
                ops.last().is_some_and(|op| op.state_after.is_some())
                    && !writer.tainted.contains(*key)
            })
            .map(|(key, _)| key.clone())
            .collect();
        if !candidates.is_empty() {
            let key = candidates[writer.rng.next_below(candidates.len() as u64) as usize].clone();
            let ms = format!("{}", 20 + writer.rng.next_below(500));
            let wire = encode(&[b"PEXPIRE", &key, ms.as_bytes()]);
            let state_after = writer.last_state(&key);
            let pending = Pending {
                key,
                state_after,
                expect: b":1\r\n".to_vec(),
                mutates: false,
                taints: true,
            };
            return (wire, pending, None);
        }
    }
    let key = writer.key(scenario.durable.keys_per_writer);
    if writer.tainted.contains(&key) || roll < 70 {
        // SET — also the forced un-taint path: SET clears the TTL and
        // restores exact expectations for the key.
        let value = format!(
            "v:{}:{}:{}",
            writer.id,
            writer.sent,
            writer.rng.next_below(scenario.durable.value_max)
        )
        .into_bytes();
        let wire = encode(&[b"SET", &key, &value]);
        let pending = Pending {
            key,
            state_after: Some(value),
            expect: b"+OK\r\n".to_vec(),
            mutates: true,
            taints: false,
        };
        (wire, pending, None)
    } else if roll < 85 {
        let existed = writer.last_state(&key).is_some();
        let expect = if existed { b":1\r\n".to_vec() } else { b":0\r\n".to_vec() };
        let wire = encode(&[b"DEL", &key]);
        (wire, Pending { key, state_after: None, expect, mutates: true, taints: false }, None)
    } else {
        let expect = match writer.last_state(&key) {
            Some(value) => bulk(&value),
            None => b"$-1\r\n".to_vec(),
        };
        let state_after = writer.last_state(&key);
        let wire = encode(&[b"GET", &key]);
        (wire, Pending { key, state_after, expect, mutates: false, taints: false }, None)
    }
}

/// Runs one seeded combined scenario. See the module docs for the phase
/// order and oracle inventory.
#[allow(clippy::too_many_lines)] // one linear phase script, like run_durable_scenario
#[must_use]
pub fn run_combined_scenario(scenario: &CombinedScenario) -> CombinedReport {
    let dur = &scenario.durable;
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(dur.seed, dur.stall.as_ref());
    let observer = TraceObserver::default();
    // A scheduler stream distinct from the durable scenario's, so shared
    // seeds don't correlate the two fleets.
    let mut rng = SplitMix64::new(dur.seed ^ 0xC0B1_4ED5);
    let mut report = CombinedReport {
        trace: Vec::new(),
        trace_hash: 0,
        violations: Vec::new(),
        stalled: false,
        commands_done: 0,
        sim_seconds: 0.0,
        required_ops: 0,
        allowed_lost_ops: 0,
        audited_keys: 0,
        memory_keys_audited: 0,
        scheduler_steps: 0,
        refused_boot: false,
        published: 0,
        delivered: 0,
        always_ack_latency_ms_max: 0,
    };
    let fail = |report: &mut CombinedReport, what: String| {
        report.violations.push(what);
    };

    // ---- boot 1 + DDL: two durable namespaces + one memory-mode -------
    let mut node = match boot(dur, PathBuf::from("node"), &disk, &clock, &observer) {
        Ok(node) => node,
        Err(err) => {
            fail(&mut report, format!("boot 1 failed: {err}"));
            return finish(report, &observer, &clock);
        }
    };
    let mut setup = MiniClient::connect(&mut node, 0);
    let ddl: [&[&[u8]]; 3] = [
        &[b"INF.NS", b"CREATE", b"alw", b"MODE", b"durable", b"FSYNC", b"always"],
        &[b"INF.NS", b"CREATE", b"esec", b"MODE", b"durable", b"FSYNC", b"everysec"],
        &[b"INF.NS", b"CREATE", b"mem", b"MODE", b"memory"],
    ];
    for argv in ddl {
        let reply = setup.call(&mut node, &mut rng, &clock, &disk, dur.step_ns_max, argv);
        match reply {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            other => {
                fail(&mut report, format!("DDL {argv:?} answered {other:?}"));
                return finish(report, &observer, &clock);
            }
        }
    }

    // ---- subscribers: connect + subscribe up front ---------------------
    let mut subs = Vec::new();
    for s in 0..scenario.subscribers {
        let cell = (rng.next_u64() % u64::from(dur.cells)) as usize;
        let fd = node.nets[cell].borrow_mut().connect();
        let plan = subscription_plan(s, scenario.channels.max(1));
        let sub = SimSubscriber {
            index: s,
            cell,
            fd,
            plan,
            state: SubState::Subscribing(0),
            rx: Vec::new(),
            received: 0,
            last_seq: BTreeMap::new(),
        };
        node.nets[cell].borrow_mut().client_send(fd, &sub.subscribe_wire());
        subs.push(SimSubscriber { state: SubState::Subscribing(sub.subscriptions()), ..sub });
    }
    let chan_count = scenario.channels.max(1) as usize;
    let mut chan_receivers = vec![0i64; chan_count];
    for sub in &subs {
        for (c, slot) in chan_receivers.iter_mut().enumerate() {
            if sub.watches(c as u64) {
                *slot += 1;
            }
        }
    }
    let mut published_per: BTreeMap<(u64, usize), u64> = BTreeMap::new();
    let mut chan_published = vec![0u64; chan_count];

    // ---- writers: durable classes + named-memory (all USE their ns) ----
    let mut writers = Vec::new();
    let classes = [
        (NsClass::Always, dur.always_writers),
        (NsClass::Everysec, dur.esec_writers),
        (NsClass::Memory, dur.mem_writers),
    ];
    let mut id = 0usize;
    for (class, count) in classes {
        for _ in 0..count {
            let cell = (rng.next_u64() % u64::from(dur.cells)) as usize;
            let fd = node.nets[cell].borrow_mut().connect();
            let writer =
                Writer::new(id, cell, fd, class, dur.seed, dur.ops_per_writer, true, chan_count);
            node.nets[cell]
                .borrow_mut()
                .client_send(fd, &encode(&[b"INF.NS", b"USE", class.name()]));
            writers.push(writer);
            id += 1;
        }
    }

    // ---- mixed traffic → pub/sub quiescence → seeded cut tail ----------
    // The cut may only land after pub/sub quiesced (module docs); the
    // tail length is seeded so the cut still sweeps every durable
    // pipeline stage across the corpus.
    let mut cut_tail: Option<u64> = None;
    let mut idle_steps = 0u64;
    loop {
        report.scheduler_steps += 1;
        if let Err(err) = node.step(&mut rng, &clock, &disk, dur.step_ns_max) {
            fail(&mut report, format!("traffic phase: {err}"));
            return finish(report, &observer, &clock);
        }

        // Subscriber pump: drain frames, classify, verify deliveries.
        let mut progress = 0u64;
        for sub in &mut subs {
            if sub.state == SubState::Closed {
                continue;
            }
            let rx = node.nets[sub.cell].borrow_mut().client_recv(sub.fd);
            progress += rx.len() as u64;
            sub.rx.extend_from_slice(&rx);
            while let Some(n) = reply_len(&sub.rx) {
                let frame: Vec<u8> = sub.rx.drain(..n).collect();
                match (parse_sub_frame(&frame), sub.state) {
                    (SubFrame::Confirm { .. }, SubState::Subscribing(left)) => {
                        sub.state = if left == 1 {
                            SubState::Listening
                        } else {
                            SubState::Subscribing(left - 1)
                        };
                    }
                    (SubFrame::Confirm { .. }, SubState::Unsubscribing(left)) => {
                        if left == 1 {
                            node.nets[sub.cell].borrow_mut().client_close(sub.fd);
                            sub.state = SubState::Closed;
                        } else {
                            sub.state = SubState::Unsubscribing(left - 1);
                        }
                    }
                    (
                        SubFrame::Message { channel, payload }
                        | SubFrame::PMessage { channel, payload },
                        SubState::Listening | SubState::Unsubscribing(_),
                    ) => {
                        report.delivered += 1;
                        sub.deliver(&channel, &payload, &mut report.violations);
                    }
                    (frame, state) => report
                        .violations
                        .push(format!("subscriber {} got {frame:?} in state {state:?}", sub.index)),
                }
            }
        }
        let subs_ready = subs.iter().all(|s| !matches!(s.state, SubState::Subscribing(_)));

        // Writer pump (the durable scenario's shape + taint/publish
        // bookkeeping + the everysec-deferral oracle).
        for writer in &mut writers {
            let mut net = node.nets[writer.cell].borrow_mut();
            let bytes = net.client_recv(writer.fd);
            progress += bytes.len() as u64;
            writer.rx.extend_from_slice(&bytes);
            while let Some(n) = reply_len(&writer.rx) {
                let reply: Vec<u8> = writer.rx.drain(..n).collect();
                if writer.setup {
                    if reply != b"+OK\r\n" {
                        report
                            .violations
                            .push(format!("writer {}: USE answered {reply:?}", writer.id));
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
                        "writer {} key {:?}: expected {:?}, got {:?}",
                        writer.id,
                        String::from_utf8_lossy(&pending.key),
                        String::from_utf8_lossy(&pending.expect),
                        String::from_utf8_lossy(&reply)
                    ));
                }
                if pending.taints {
                    writer.tainted.insert(pending.key.clone());
                } else if pending.mutates {
                    writer.tainted.remove(&pending.key);
                }
                if pending.mutates {
                    let ops = writer.ledger.entry(pending.key.clone()).or_default();
                    let rec = ops.last_mut().expect("sent op has a ledger entry");
                    rec.acked_at = Some(clock.now());
                    let latency = clock.now().saturating_sub(rec.sent_at);
                    match writer.class {
                        NsClass::Everysec if latency > EVERYSEC_ACK_BOUND => {
                            report.violations.push(format!(
                                "EVERYSEC DEFERRAL VIOLATION seed {:#x} writer {} key {:?}: \
                                 ack latency {} ms exceeds {} ms — everysec acked behind the \
                                 device",
                                dur.seed,
                                writer.id,
                                String::from_utf8_lossy(&pending.key),
                                latency.as_millis(),
                                EVERYSEC_ACK_BOUND.as_millis()
                            ));
                        }
                        NsClass::Always => {
                            report.always_ack_latency_ms_max =
                                report.always_ack_latency_ms_max.max(latency.as_millis());
                        }
                        _ => {}
                    }
                }
                writer.replied += 1;
                report.commands_done += 1;
            }
            if writer.setup || writer.inflight.is_some() || writer.sent >= writer.quota {
                continue;
            }
            // Publishers hold fire until every subscriber confirmed
            // (confirmed ⇒ reachable, the plane's happens-before).
            if writer.class == NsClass::Memory && !subs_ready {
                continue;
            }
            let (wire, pending, publish) = if writer.class == NsClass::Memory {
                next_memory_command(writer, scenario, &chan_receivers)
            } else {
                let (wire, pending) = writer.next_command(dur);
                (wire, pending, None)
            };
            if let Some(chan) = publish {
                report.published += 1;
                chan_published[chan as usize] += 1;
                *published_per.entry((chan, writer.id)).or_insert(0) += 1;
            }
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

        // Memory writers done ⇒ subscribers unwind once every published
        // message reached them (a loss parks this transition ⇒ stall ⇒ a
        // replayable seed — the harness contract).
        let publishers_done = writers
            .iter()
            .filter(|w| w.class == NsClass::Memory)
            .all(|w| !w.setup && w.replied == w.quota);
        if publishers_done {
            for sub in &mut subs {
                if sub.state != SubState::Listening {
                    continue;
                }
                let expected: u64 = (0..chan_count as u64)
                    .filter(|&c| sub.watches(c))
                    .map(|c| chan_published[c as usize])
                    .sum();
                if sub.received == expected {
                    node.nets[sub.cell].borrow_mut().client_send(sub.fd, &sub.unsubscribe_wire());
                    sub.state = SubState::Unsubscribing(sub.subscriptions());
                }
            }
        }
        let quiesced = publishers_done && subs.iter().all(|s| s.state == SubState::Closed);
        match &mut cut_tail {
            None if quiesced => {
                // Pub/sub accounting at quiescence: registries drained.
                let (channels, patterns, bytes) = node.pubsub_gauges();
                if channels != 0 || patterns != 0 || bytes != 0 {
                    report.violations.push(format!(
                        "pub/sub registries not empty at quiescence \
                         ({channels} channels, {patterns} patterns, {bytes} bytes)"
                    ));
                }
                let durable_ops: u64 =
                    writers.iter().filter(|w| w.class != NsClass::Memory).map(|w| w.quota).sum();
                cut_tail = Some(200 + rng.next_below(durable_ops * 3));
            }
            None => {}
            Some(0) => break,
            Some(steps) => *steps -= 1,
        }

        if progress == 0 {
            idle_steps += 1;
            if idle_steps >= STALL_STEPS && writers.iter().any(|w| w.replied < w.sent) {
                let stuck: Vec<String> = writers
                    .iter()
                    .filter(|w| w.replied < w.sent)
                    .map(|w| format!("writer {} ({:?})", w.id, w.class))
                    .collect();
                report.stalled = true;
                fail(
                    &mut report,
                    format!(
                        "WATERMARK LIVENESS VIOLATION seed {:#x}: combined traffic stalled \
                         before the cut with unacked in-flight ops ({})",
                        dur.seed,
                        stuck.join(", ")
                    ),
                );
                return finish(report, &observer, &clock);
            }
            // Pub/sub never quiesced (a lost delivery parks the unwind).
            if idle_steps >= STALL_STEPS && cut_tail.is_none() {
                report.stalled = true;
                let (published, delivered) = (report.published, report.delivered);
                fail(
                    &mut report,
                    format!(
                        "seed {:#x}: pub/sub failed to quiesce (published {published}, \
                         delivered {delivered})",
                        dur.seed
                    ),
                );
                return finish(report, &observer, &clock);
            }
        } else {
            idle_steps = 0;
        }
    }

    // Delivery oracle, exact final ledger: per (channel, publisher),
    // every watching subscriber saw exactly the published count.
    for sub in &subs {
        for (&(chan, publisher), &count) in &published_per {
            if !sub.watches(chan) {
                continue;
            }
            let got = sub.last_seq.get(&(chan, publisher)).copied().unwrap_or(0);
            if got != count {
                report.violations.push(format!(
                    "subscriber {} chan {chan} publisher {publisher}: saw seq {got}, \
                     published {count}",
                    sub.index
                ));
            }
        }
    }

    // ---- POWER CUT ------------------------------------------------------
    let cut_time = clock.now();
    drop(node); // the process dies: in-flight state vanishes
    disk.power_cut(dur.seed ^ 0x0FF5_EED0);

    // ---- reboot (+ optional second cut mid-recovery, inherited) ---------
    let mut boots = 0;
    let node = loop {
        boots += 1;
        let mut node = match boot(dur, PathBuf::from("node"), &disk, &clock, &observer) {
            Ok(node) => node,
            Err(err) => {
                fail(&mut report, format!("reboot {boots} refused: {err}"));
                return finish(report, &observer, &clock);
            }
        };
        let double = dur.double_cut && boots == 1;
        let recovery_budget = if double { 1 + rng.next_below(200) } else { u64::MAX };
        let mut steps = 0u64;
        let mut failed = None;
        while !node.ready() && steps < recovery_budget {
            steps += 1;
            report.scheduler_steps += 1;
            if let Err(err) = node.step(&mut rng, &clock, &disk, dur.step_ns_max) {
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
            // The ADR-0018 taxonomy refusal is a legal outcome; §8.2
            // binds survival, audited directly off the surviving image.
            // Memory volatility is not wire-auditable here; the L2 log
            // scan still runs — a leaked memory record cannot hide
            // behind the refusal.
            if err.to_string().contains("log corruption") {
                report.refused_boot = true;
                let mut tally = AuditTally::default();
                survival_audit(dur, &disk, &writers, cut_time, &mut tally);
                report.required_ops += tally.required_ops;
                report.allowed_lost_ops += tally.allowed_lost_ops;
                report.audited_keys += tally.audited_keys;
                report.violations.extend(tally.violations);
                l2_log_scan(scenario, &disk, &mut report.violations);
            } else {
                fail(&mut report, format!("recovery failed on boot {boots}: {err}"));
            }
            return finish(report, &observer, &clock);
        }
        if node.ready() {
            break node;
        }
        // The second cut: recovery itself was interrupted (idempotence),
        // now with memory + expiry + quiesced pub/sub state present.
        drop(node);
        disk.power_cut(dur.seed ^ 0x0FF5_EED1 ^ boots);
    };
    let mut node = node;

    // ---- audit: durable admissible sets --------------------------------
    let mut audit = MiniClient::connect(&mut node, 0);
    for class in [NsClass::Always, NsClass::Everysec] {
        let reply = audit.call(
            &mut node,
            &mut rng,
            &clock,
            &disk,
            dur.step_ns_max,
            &[b"INF.NS", b"USE", class.name()],
        );
        if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
            fail(&mut report, format!("audit USE {class:?} answered {reply:?}"));
            return finish(report, &observer, &clock);
        }
        for writer in writers.iter().filter(|w| w.class == class) {
            for (key, ops) in &writer.ledger {
                report.audited_keys += 1;
                let required = required_index(class, ops, cut_time);
                report.required_ops += required.map_or(0, |i| i as u64 + 1);
                report.allowed_lost_ops += ops.len() as u64 - required.map_or(0, |i| i as u64 + 1);
                let reply = match audit.call(
                    &mut node,
                    &mut rng,
                    &clock,
                    &disk,
                    dur.step_ns_max,
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
                        "DURABILITY VIOLATION seed {:#x} class {class:?} key {:?}: recovered \
                         {:?} is outside the admissible set (required op index {required:?}, \
                         {} ops)",
                        dur.seed,
                        String::from_utf8_lossy(key),
                        String::from_utf8_lossy(&reply),
                        ops.len()
                    ));
                }
            }
        }
    }

    // ---- audit: memory volatility (L2, side 1) --------------------------
    // Every memory-namespace key written before the cut must read absent
    // after recovery — memory namespaces never touch the log, so nothing
    // can resurrect them.
    let reply = audit.call(
        &mut node,
        &mut rng,
        &clock,
        &disk,
        dur.step_ns_max,
        &[b"INF.NS", b"USE", NsClass::Memory.name()],
    );
    if !matches!(reply, Ok(Some(ref ok)) if ok == b"+OK\r\n") {
        fail(&mut report, format!("audit USE mem answered {reply:?}"));
        return finish(report, &observer, &clock);
    }
    for writer in writers.iter().filter(|w| w.class == NsClass::Memory) {
        for key in writer.ledger.keys() {
            report.memory_keys_audited += 1;
            let reply = match audit.call(
                &mut node,
                &mut rng,
                &clock,
                &disk,
                dur.step_ns_max,
                &[b"GET", key],
            ) {
                Ok(Some(reply)) => reply,
                other => {
                    fail(&mut report, format!("memory audit GET {key:?} answered {other:?}"));
                    return finish(report, &observer, &clock);
                }
            };
            if reply != b"$-1\r\n" {
                report.violations.push(format!(
                    "MEMORY VOLATILITY VIOLATION seed {:#x} key {:?}: memory-namespace key \
                     survived the power cut (recovered {:?}) — memory state leaked into the \
                     durable path (L2)",
                    dur.seed,
                    String::from_utf8_lossy(key),
                    String::from_utf8_lossy(&reply)
                ));
            }
        }
    }

    // ---- audit: no memory record in the recovered log (L2, side 2) ------
    l2_log_scan(scenario, &disk, &mut report.violations);

    finish(report, &observer, &clock)
}

/// The L2 log-tier oracle (M2.5-S14): walk every readable frame of every
/// cell's recovered log and assert zero records naming a non-durable
/// namespace (`CkptBegin` markers carry ns 0 by convention and are
/// exempt). Catches "memory namespaces never touch the log" violations
/// at the mechanism, independent of the volatility audit. One bounded
/// violation per cell (first lsn + count), never a flood.
fn l2_log_scan(scenario: &CombinedScenario, disk: &SimDisk, violations: &mut Vec<String>) {
    let data_dir = PathBuf::from("node");
    let durable_ns: BTreeSet<u32> = match load_catalog_from(disk, &data_dir) {
        Ok(Some(catalog)) => catalog
            .entries
            .iter()
            .filter(|spec| spec.mode == NsMode::Durable)
            .map(|spec| spec.id.0)
            .collect(),
        // Catalog verdicts belong to the survival/durability audits.
        _ => return,
    };
    for cell in 0..scenario.durable.cells {
        let shard = data_dir.join(format!("shard-{cell}"));
        let log_dir = shard.join("log");
        let Ok(manifest) = read_manifest(disk, &shard) else { continue };
        let floor = manifest.as_ref().map_or(SegmentId(0), inf_log::Manifest::floor);
        let Ok(outcome) = scan_log_dir_from(disk, &log_dir, floor) else { continue };
        let mut leaked = 0u64;
        let mut first: Option<String> = None;
        'segments: for &segment in outcome.scan.segments() {
            let Ok(mut reader) =
                SegmentReader::open(disk, &log_dir, segment, ReaderConfig::default())
            else {
                break 'segments;
            };
            loop {
                match reader.next_frame() {
                    Ok(Some(frame)) => {
                        for record in frame.records() {
                            let Ok((lsn, record)) = record else { break 'segments };
                            let ns = match record {
                                RecordView::StringPostImage { ns, .. }
                                | RecordView::Delete { ns, .. }
                                | RecordView::ExpireAt { ns, .. }
                                | RecordView::NsOp { ns, .. }
                                | RecordView::DocDelta { ns, .. }
                                | RecordView::DocFull { ns, .. }
                                | RecordView::ColdDisplace { ns, .. }
                                | RecordView::StringExtentRef { ns, .. } => ns,
                                RecordView::CkptBegin { .. } => continue,
                            };
                            if !durable_ns.contains(&ns.0) {
                                leaked += 1;
                                if first.is_none() {
                                    first = Some(format!("ns {} at lsn {lsn:?}", ns.0));
                                }
                            }
                        }
                    }
                    Ok(None) => break,
                    Err(_) => break 'segments,
                }
            }
        }
        if leaked > 0 {
            violations.push(format!(
                "L2 VIOLATION seed {:#x} cell {cell}: {leaked} non-durable-namespace record(s) \
                 in the recovered log (first: {}) — memory namespaces must never touch the log",
                scenario.durable.seed,
                first.expect("leaked > 0 records a first")
            ));
        }
    }
}

fn finish(
    mut report: CombinedReport,
    observer: &TraceObserver,
    clock: &Rc<VirtualClock>,
) -> CombinedReport {
    report.trace = observer.trace_bytes();
    report.trace_hash = hash64(&report.trace, 0xC0B1);
    report.sim_seconds = clock.now().0.saturating_sub(1) as f64 / 1e9;
    report
}
