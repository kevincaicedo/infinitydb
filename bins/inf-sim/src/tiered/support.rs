//! Tiered-scenario support: value shapes, the command generator and
//! writer pumps, the tier-read fault probe, INFO/DBSIZE scrapes, the
//! `SCAN` walk, and reboot-until-ready.

use super::*;

/// Deterministic value bytes: a `tag:id:sent:` stamp cycled to `len`
/// (exact expectations need exact bytes, not lengths).
pub(super) fn value_bytes(tag: u8, id: usize, sent: u64, len: usize) -> Vec<u8> {
    let stamp = format!("{}:{id}:{sent}:", tag as char).into_bytes();
    stamp.iter().copied().cycle().take(len).collect()
}

/// Builds the next tiered command + its exact expected reply: inline
/// SETs (1–3 KiB — ring residents that demote), blob SETs (6–10 KiB —
/// out-of-line extents, ADR-0061; plus a 15–20 KiB slice straddling the
/// 16,368 B one-frame cold window — review of 2026-08-30 §5.5 Group 0:
/// the old 10,239 B cap left C2's multi-window cold reads unreachable
/// by any seed), exact GETs, and counted DELs. Overwrites across the
/// arms exercise blob-over-inline, inline-over-blob, and cold-candidate
/// displacement (ADR-0057 D4).
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
        } else if roll < 61 {
            *blob_sets += 1;
            (b'b', (6 << 10) + writer.rng.next_below(4096) as usize)
        } else {
            *blob_sets += 1;
            (b'B', 15 * 1024 + writer.rng.next_below(5 << 10) as usize)
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
pub(super) fn pump_writers(
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
                    "REPLY VIOLATION writer {} key {:?}: expected {}, got {}",
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

/// Phase 6a (F-L04-02): see the call site. `Err` is a harness failure
/// (a stalled call); oracle violations ride the report.
#[allow(clippy::too_many_arguments)]
pub(super) fn tier_read_fault_probe(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &TieredScenario,
    audit: &mut MiniClient,
    observed: &BTreeMap<Vec<u8>, Vec<u8>>,
    report: &mut TieredNodeReport,
) -> Result<(), String> {
    let seed = scenario.seed;
    report.tier_read_fault_arm = true;
    let mut armed = 0u64;
    for (path, _) in disk.image() {
        if path.extension().is_some_and(|ext| ext == "itier") {
            disk.inject(&path, inf_server::DeviceFault::ReadEio, 1)
                .map_err(|err| format!("tier-read arm {path:?}: {err}"))?;
            armed += 1;
        }
    }
    if armed == 0 {
        return Err("TIER-READ FAULT ARM VACUOUS: no tier file to arm after phase 6".to_owned());
    }
    let errors_before = info_sum(node, rng, clock, disk, scenario, &["tiering_cold_read_errors"])
        .map_err(|err| format!("cold-read error scrape: {err}"))?[0];
    let faults_before = disk.faults_fired();
    let cold_io: &[u8] = b"-ERR cold read failed (tier I/O error)\r\n";
    for (key, want) in observed {
        let get: &[&[u8]] = &[b"GET", key];
        let mut reply = audit
            .call(node, rng, clock, disk, scenario.step_ns_max, get)
            .map_err(|err| format!("probe GET {key:?}: {err}"))?
            .ok_or_else(|| format!("probe GET {key:?} stalled"))?;
        if reply == cold_io {
            report.tier_read_error_replies += 1;
            reply = audit
                .call(node, rng, clock, disk, scenario.step_ns_max, get)
                .map_err(|err| format!("probe retry GET {key:?}: {err}"))?
                .ok_or_else(|| format!("probe retry GET {key:?} stalled"))?;
            if reply == cold_io {
                report.violations.push(format!(
                    "TIER-READ FAULT STICKS seed {seed:#x} key {:?}: one armed EIO, the retry \
                     failed too",
                    String::from_utf8_lossy(key)
                ));
                continue;
            }
        }
        if &reply != want {
            report.violations.push(format!(
                "TIER-READ PROBE VIOLATION seed {seed:#x} key {:?}: audit observed {}, probe \
                 read {}",
                String::from_utf8_lossy(key),
                preview(want),
                preview(&reply)
            ));
        }
    }
    report.tier_read_faults_fired = disk.faults_fired() - faults_before;
    report.tier_read_faults_unconsumed = disk.clear_faults();
    if report.tier_read_faults_fired == 0 {
        report.violations.push(format!(
            "TIER-READ FAULT ARM VACUOUS seed {seed:#x}: {armed} tier files armed, the probe's \
             {} reads consumed none",
            observed.len()
        ));
        return Ok(());
    }
    if report.tier_read_error_replies != report.tier_read_faults_fired {
        report.violations.push(format!(
            "TIER-READ FAULT FOLDED seed {seed:#x}: {} device errors fired, {} typed replies \
             reached the client (F-L04-02 / L06-02: a device error is never a miss or a value)",
            report.tier_read_faults_fired, report.tier_read_error_replies
        ));
    }
    // The operator's witness: one increment per fault (cell scoped, summed).
    let errors_after = info_sum(node, rng, clock, disk, scenario, &["tiering_cold_read_errors"])
        .map_err(|err| format!("cold-read error scrape: {err}"))?[0];
    if errors_after - errors_before != report.tier_read_faults_fired {
        report.violations.push(format!(
            "TIER-READ FAULT UNCOUNTED seed {seed:#x}: {} device errors fired, \
             tiering_cold_read_errors rose by {}",
            report.tier_read_faults_fired,
            errors_after - errors_before
        ));
    }
    Ok(())
}

pub(super) fn preview(reply: &[u8]) -> String {
    let cut = reply.len().min(48);
    format!(
        "{:?}{}",
        String::from_utf8_lossy(&reply[..cut]),
        if reply.len() > cut { "…" } else { "" }
    )
}

/// The first `tiering_ns<id>:` line's `head` and `flushed` watermarks
/// (cell scope — the connection's cell).
pub(super) fn ns_watermarks(text: &str) -> Option<(u64, u64)> {
    let line = text.lines().find(|l| l.starts_with("tiering_ns"))?;
    let body = line.split_once(':')?.1;
    let field = |name: &str| -> Option<u64> {
        body.split(',')
            .find_map(|kv| kv.strip_prefix(name)?.strip_prefix('='))
            .and_then(|v| v.parse().ok())
    };
    Some((field("head")?, field("flushed")?))
}

/// Extracts one `key:value` integer from an `INFO` section text.
/// `DBSIZE` summed over every cell (the tiered table is cell-scoped),
/// each through a connection pinned to its cell.
/// `DBSIZE` on the namespace, asked of **every** cell through a
/// namespace-bound probe: since the review of 2026-08-28 (M4.5-S37
/// finding 2) the answer is the node-wide count on every cell — the
/// scatter-sum through `ApplyNs` — so the cells must agree, and the one
/// value is returned. (Before, each cell answered its own table and the
/// harness summed them; a disagreement now is a violation.)
pub(super) fn dbsize_sum(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &TieredScenario,
) -> Result<u64, String> {
    let mut agreed: Option<u64> = None;
    for cell in 0..usize::from(scenario.cells) {
        let mut probe = MiniClient::connect(node, cell);
        let use_ns: &[&[u8]] = &[b"INF.NS", b"USE", NS_NAME];
        match probe.call(node, rng, clock, disk, scenario.step_ns_max, use_ns) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            other => return Err(format!("USE on cell {cell} answered {other:?}")),
        }
        let dbsize: &[&[u8]] = &[b"DBSIZE"];
        let n = match probe.call(node, rng, clock, disk, scenario.step_ns_max, dbsize) {
            Ok(Some(reply)) if reply.starts_with(b":") => {
                String::from_utf8_lossy(&reply[1..]).trim().parse::<u64>().unwrap_or(0)
            }
            other => return Err(format!("DBSIZE on cell {cell} answered {other:?}")),
        };
        match agreed {
            None => agreed = Some(n),
            Some(first) if first == n => {}
            Some(first) => {
                return Err(format!(
                    "DBSIZE VIOLATION: cell {cell} answered {n}, cell 0 answered {first} — a \
                     namespace-bound DBSIZE is node-wide on every cell"
                ));
            }
        }
    }
    Ok(agreed.unwrap_or(0))
}

/// The batch-33 engagement witnesses of one node life, summed over
/// cells: checkpoint staging downgrades (the `EINVAL` arm) and sections
/// sealed for the section bound (the section-bound arm). Folded into
/// the report before every cut — the counters are life-scoped.
pub(super) fn ckpt_witness(node: &Node, cells: u16) -> (u64, u64) {
    (0..usize::from(cells)).fold((0, 0), |acc, cell| {
        let s = node.plane(cell).durable_stats();
        (
            acc.0 + s.as_ref().map_or(0, |s| s.ckpt_io_mode_downgrades),
            acc.1 + s.as_ref().map_or(0, |s| s.ckpt_bound_splits),
        )
    })
}

pub(super) fn note_ckpt_witness(node: &Node, cells: u16, report: &mut TieredNodeReport) {
    let (downgrades, splits) = ckpt_witness(node, cells);
    report.ckpt_downgrades += downgrades;
    report.ckpt_bound_splits += splits;
}

/// `INFO tiering` fields summed over every cell (the section is
/// cell-scoped), one value per key in `keys`' order.
pub(super) fn info_sum(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &TieredScenario,
    keys: &[&str],
) -> Result<Vec<u64>, String> {
    let mut totals = vec![0u64; keys.len()];
    for cell in 0..usize::from(scenario.cells) {
        let mut probe = MiniClient::connect(node, cell);
        let text = info_tiering(&mut probe, node, rng, clock, disk, scenario.step_ns_max)
            .map_err(|err| format!("INFO on cell {cell}: {err}"))?;
        for (i, key) in keys.iter().enumerate() {
            totals[i] += info_field(&text, key);
        }
    }
    Ok(totals)
}

/// Every key one `SCAN` walk names (the at-least-once contract;
/// duplicates are legal and kept), asked of **every** cell — and each
/// cell's walk must be complete **by itself**. Review of 2026-08-30
/// (§5.5): this helper used to perform the per-cell fan-out itself and
/// union the results, which encoded C1 (a namespace-bound `SCAN`
/// serving one cell of `cells`) as correct behaviour by construction.
/// Since the C1 fix the server scatters the walk (`ScatterScope::Ns`),
/// so the oracle now demands the C1 contract instead of doing the
/// server's work for it: a cell whose enumeration's key **set**
/// disagrees with cell 0's is a violation (the `dbsize_sum` shape).
/// Returns cell 0's enumeration.
pub(super) fn scan_all_keys(
    node: &mut Node,
    rng: &mut SplitMix64,
    clock: &Rc<VirtualClock>,
    disk: &SimDisk,
    scenario: &TieredScenario,
    report: &mut TieredNodeReport,
) -> Result<Vec<Vec<u8>>, String> {
    struct FirstWalk {
        keys: Vec<Vec<u8>>,
        set: BTreeSet<Vec<u8>>,
    }
    let mut first: Option<FirstWalk> = None;
    for cell in 0..usize::from(scenario.cells) {
        let mut probe = MiniClient::connect(node, cell);
        let use_ns: &[&[u8]] = &[b"INF.NS", b"USE", NS_NAME];
        match probe.call(node, rng, clock, disk, scenario.step_ns_max, use_ns) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            other => return Err(format!("USE on cell {cell} answered {other:?}")),
        }
        let mut keys = Vec::new();
        let mut cursor = 0u64;
        for _ in 0..4096 {
            let cursor_text = cursor.to_string();
            let scan: &[&[u8]] = &[b"SCAN", cursor_text.as_bytes(), b"COUNT", b"512"];
            // F-L17-13 (L3): a page that resolves K cold slots must not
            // cost K reactor iterations — the sequential loop paid one
            // device round trip *and* one iteration per key; a batched
            // page enqueues a chunk before it suspends. Cold intents come
            // from the cell's own `INFO tiering` (the counters are
            // cell-scoped, and so is this probe); iterations are the
            // steps this page took to answer.
            let cold_before =
                info_tiering(&mut probe, node, rng, clock, disk, scenario.step_ns_max)
                    .map(|t| info_field(&t, "cold_reads_enqueued"))?;
            probe.send(node, scan);
            let mut steps = 0u64;
            let reply = loop {
                if steps >= STALL_STEPS {
                    return Err(format!("SCAN on cell {cell} stalled after {steps} steps"));
                }
                node.step(rng, clock, disk, scenario.step_ns_max)
                    .map_err(|e| format!("SCAN on cell {cell}: {e}"))?;
                steps += 1;
                if let Some(reply) = probe.recv(node) {
                    break reply;
                }
            };
            let cold = info_tiering(&mut probe, node, rng, clock, disk, scenario.step_ns_max)
                .map(|t| info_field(&t, "cold_reads_enqueued"))?
                .saturating_sub(cold_before);
            if cold > 0 {
                report.scan_cold_pages += 1;
                report.scan_cold_reads += cold;
            }
            if cold >= SCAN_BATCH_ORACLE_MIN_COLD && steps >= cold {
                report.violations.push(format!(
                    "SCAN BATCHING VIOLATION: cell {cell} page at cursor {cursor} resolved \
                     {cold} cold slots in {steps} iterations — one device round trip per \
                     key, never a batch (F-L17-13, L3 / ADR-0055 D1)"
                ));
            }
            let (next, named) = parse_scan_reply(&reply)
                .ok_or_else(|| format!("SCAN on cell {cell}: unparsable {}", preview(&reply)))?;
            keys.extend(named);
            cursor = next;
            if cursor == 0 {
                break;
            }
        }
        let set: BTreeSet<Vec<u8>> = keys.iter().cloned().collect();
        match &first {
            None => first = Some(FirstWalk { keys, set }),
            Some(walk) if walk.set == set => {}
            Some(walk) => {
                return Err(format!(
                    "SCAN VIOLATION: cell {cell} enumerated {} distinct keys, cell 0 \
                     enumerated {} — a namespace-bound SCAN walk is node-complete on \
                     every cell (review 2026-08-30 C1)",
                    set.len(),
                    walk.set.len()
                ));
            }
        }
    }
    Ok(first.map(|walk| walk.keys).unwrap_or_default())
}

/// `*2 $cursor *N $key…` → `(cursor, keys)`; `None` on any other shape.
fn parse_scan_reply(reply: &[u8]) -> Option<(u64, Vec<Vec<u8>>)> {
    fn line(rest: &mut &[u8]) -> Option<Vec<u8>> {
        let end = rest.windows(2).position(|w| w == b"\r\n")?;
        let out = rest[..end].to_vec();
        *rest = &rest[end + 2..];
        Some(out)
    }
    fn bulk_item(rest: &mut &[u8]) -> Option<Vec<u8>> {
        let head = line(rest)?;
        let len: usize = std::str::from_utf8(head.strip_prefix(b"$")?).ok()?.parse().ok()?;
        if rest.len() < len + 2 {
            return None;
        }
        let out = rest[..len].to_vec();
        *rest = &rest[len + 2..];
        Some(out)
    }
    let mut rest = reply;
    if line(&mut rest)? != b"*2" {
        return None;
    }
    let cursor: u64 = std::str::from_utf8(&bulk_item(&mut rest)?).ok()?.parse().ok()?;
    let count_line = line(&mut rest)?;
    let count: usize = std::str::from_utf8(count_line.strip_prefix(b"*")?).ok()?.parse().ok()?;
    let mut keys = Vec::with_capacity(count);
    for _ in 0..count {
        keys.push(bulk_item(&mut rest)?);
    }
    Some((cursor, keys))
}

/// Cold slots a `SCAN` page must resolve before the batching oracle
/// speaks: below this a page's iteration count is dominated by the
/// reply itself, not by its reads.
const SCAN_BATCH_ORACLE_MIN_COLD: u64 = 16;

pub(super) fn info_field(text: &str, key: &str) -> u64 {
    text.lines()
        .find_map(|line| line.strip_prefix(key).and_then(|rest| rest.strip_prefix(':')))
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(0)
}

/// `INFO tiering` through a [`MiniClient`], bulk payload decoded to text.
#[allow(clippy::too_many_arguments)] // one call site's plumbing, like MiniClient::call
pub(super) fn info_tiering(
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

/// Boots the node on the surviving image and steps it until every cell is
/// ready (the phase-4 reboot loop without the mid-recovery second cut).
pub(super) fn reboot_until_ready(
    harness: &DurableScenario,
    disk: &SimDisk,
    clock: &Rc<VirtualClock>,
    observer: &TraceObserver,
    rng: &mut SplitMix64,
    report: &mut TieredNodeReport,
    step_ns_max: u64,
) -> Result<Node, String> {
    let mut node = boot(harness, PathBuf::from("node"), disk, clock, observer)
        .map_err(|err| format!("boot refused: {err}"))?;
    let mut steps = 0u64;
    while !node.ready() {
        steps += 1;
        report.scheduler_steps += 1;
        if let Err(err) = node.step(rng, clock, disk, step_ns_max) {
            return Err(format!("recovery failed: {err}"));
        }
        if steps > STALL_STEPS {
            report.stalled = true;
            return Err("recovery stalled".to_string());
        }
    }
    Ok(node)
}

/// Files under `shard-*/ns-{ns}/cold/` in the disk image.
pub(super) fn tier_residue_files(disk: &SimDisk, ns: u32) -> usize {
    let needle = format!("ns-{ns}/cold/");
    disk.image().iter().filter(|(path, _)| path.to_string_lossy().contains(&needle)).count()
}

pub(super) fn finish(
    mut report: TieredNodeReport,
    observer: &TraceObserver,
    clock: &Rc<VirtualClock>,
) -> TieredNodeReport {
    if report.dir_open_fault_arm {
        report.dir_open_faults_fired =
            inf_foundation::fault::fired(inf_log::fault::TIER_DIR_OPEN_FAIL);
        inf_foundation::fault::disarm(inf_log::fault::TIER_DIR_OPEN_FAIL);
        if report.dir_open_faults_fired == 0 {
            report.violations.push(
                "DIR-OPEN FAULT ROW VACUOUS: the point was armed and no tier file was created"
                    .to_owned(),
            );
        }
    }
    report.trace = observer.trace_bytes();
    report.trace_hash = hash64(&report.trace, 0x71E7);
    report.sim_seconds = clock.now().0.saturating_sub(1) as f64 / 1e9;
    report
}
