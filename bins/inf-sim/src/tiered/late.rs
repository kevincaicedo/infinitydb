//! The tiered scenario's late phases — 7c (a rebuilt winner carrying
//! several tickets, ADR-0093 A10), 8 (DISKFULL clamp → typed refusal →
//! reopen, ADR-0063), 9 (the S19 drop race through the wire), 10 and 10b
//! (reboots after a DROP and a cut inside one, ADR-0100). Each phase is a
//! straight-line script over [`Late`]; `Err(())` means "finish now" — the
//! violation is already on the report.

use super::*;

/// Cross-phase state the late phases share with the script.
pub(super) struct Late<'a> {
    pub(super) scenario: &'a TieredScenario,
    pub(super) harness: &'a DurableScenario,
    pub(super) clock: &'a Rc<VirtualClock>,
    pub(super) disk: &'a SimDisk,
    pub(super) observer: &'a TraceObserver,
    pub(super) rng: &'a mut SplitMix64,
    pub(super) report: &'a mut TieredNodeReport,
    /// Phase 2's observed keys (phase 9 races reads of them with the DROP).
    pub(super) observed: &'a BTreeMap<Vec<u8>, Vec<u8>>,
    /// Phase 1's `INF.NS CREATE` argv (phase 10b creates a second namespace
    /// with the same spec).
    pub(super) create: &'a [&'a [u8]],
}

/// A phase's verdict: `Err(())` ends the script (the report carries why).
pub(super) type Verdict<T = ()> = Result<T, ()>;

impl Late<'_> {
    fn fail(&mut self, what: String) {
        let seed = self.scenario.seed;
        self.report.violations.push(format!("seed {seed:#x}: {what}"));
    }
}

/// Phase 7c's rebuilt-winner values (the script wrote `rebuilt_value(b'g', i)`
/// pre-cut with the same shape).
fn rebuilt_value(tag: u8, i: u64) -> Vec<u8> {
    value_bytes(tag, 8, i, 1600)
}

/// Runs phases 7c through 10b in order; `total_keys` is phase 7b's DBSIZE.
pub(super) fn run(cx: &mut Late<'_>, node: Node, audit: MiniClient, total_keys: u64) -> Verdict {
    let (mut node, mut audit) = phase_7c(cx, node, audit, total_keys)?;
    phase_8(cx, &mut node, &mut audit)?;
    phase_9(cx, &mut node, &mut audit)?;
    phase_10(cx, node)
}

/// Phase 7c: DEL of a rebuilt winner carrying several tickets. Returns the
/// node and audit client after its two reboots.
fn phase_7c(
    cx: &mut Late<'_>,
    mut node: Node,
    mut audit: MiniClient,
    total_keys: u64,
) -> Verdict<(Node, MiniClient)> {
    let seed = cx.scenario.seed;
    let rebuilt_triple = inf_store::forced_collision_triple(seed ^ 0x7C7C_0001);
    let rebuilt_sk = inf_store::forced_collision_triple(seed ^ 0x7C7C_0002);
    // Review of 2026-08-30 (F-L07-01; batch 23, ADR-0093 A10): a boot
    // pairs every cold slot of one hash with its one RAM sibling, so one
    // winner can carry several tickets. Two shapes under one cut: on
    // every seed the third key of a triple written over two cold
    // collision keys (the synchronous path — the knob is irrelevant to
    // the rebuild); on the shadow arm a winner whose open same-key twin
    // and a cold collision key both survive the walk. The reconciler's
    // reads are failed through `shadow_reconcile_read_fail` so the
    // rebuilt tickets stay open until `DEL`, whose own read succeeds.
    // Pre-fix the DEL resolved one ticket and the cell died on the
    // store's release assert. A second reboot proves both deletions
    // durable — the same-key twin died through its own marker — and the
    // collision keys intact. Every row asserts its coverage.
    cx.report.rebuilt_rows = true;
    macro_rules! bail7c {
        ($($arg:tt)*) => {{
            cx.fail(format!($($arg)*));
            inf_foundation::fault::disarm(inf_server::fault::SHADOW_RECONCILE_READ_FAIL);
            return Err(());
        }};
    }
    macro_rules! expect7c {
        ($client:expr, $cmd:expr, $want:expr, $what:expr) => {{
            let want: &[u8] = $want;
            match $client.call(&mut node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, $cmd)
            {
                Ok(Some(reply)) if reply == want => {}
                other => bail7c!(
                    "REBUILT-TICKET VIOLATION seed {seed:#x}: {} answered {other:?}, wanted {}",
                    $what,
                    preview(want)
                ),
            }
            cx.report.commands_done += 1;
        }};
    }
    macro_rules! scrape7c {
        ($keys:expr) => {
            match info_sum(&mut node, cx.rng, cx.clock, cx.disk, cx.scenario, $keys) {
                Ok(v) => v,
                Err(err) => bail7c!("phase-7c scrape: {err}"),
            }
        };
    }
    let (t0, t1, t2) = (&rebuilt_triple[0], &rebuilt_triple[1], &rebuilt_triple[2]);
    let (s0, s2) = (&rebuilt_sk[0], &rebuilt_sk[2]);
    let (t0_v, t2_v, s0_v1) =
        (rebuilt_value(b'g', 0), rebuilt_value(b'g', 1), rebuilt_value(b'g', 2));
    let t1_v = rebuilt_value(b'h', 0);
    let s0_v2 = rebuilt_value(b'i', 0);
    let s2_v = rebuilt_value(b'j', 0);
    let mut expected_live = total_keys;
    inf_foundation::fault::arm(inf_server::fault::SHADOW_RECONCILE_READ_FAIL, FaultSpec::Always);
    if cx.scenario.shadow {
        // (1) s2 on the synchronous path (the arm off for one write,
        //     witnessed on every cell): a RAM record *below* the winner
        //     to come, so the pinned winner never blocks its demotion.
        let arm_off: &[&[u8]] = &[b"CONFIG", b"SET", b"tiered-shadow-overwrite", b"no"];
        expect7c!(audit, arm_off, b"+OK\r\n", "CONFIG SET tiered-shadow-overwrite no");
        let mut off_everywhere = false;
        for _ in 0..16 {
            if scrape7c!(&["tiering_shadow_enabled"])[0] == 0 {
                off_everywhere = true;
                break;
            }
        }
        if !off_everywhere {
            bail7c!("phase-7c arm-off fan did not reach every cell");
        }
        expect7c!(audit, &[b"SET", s2, &s2_v], b"+OK\r\n", "SET s2 (synchronous)");
        expected_live += 1;
        let arm_on: &[&[u8]] = &[b"CONFIG", b"SET", b"tiered-shadow-overwrite", b"yes"];
        expect7c!(audit, arm_on, b"+OK\r\n", "CONFIG SET tiered-shadow-overwrite yes");
        let mut on_everywhere = false;
        for _ in 0..16 {
            if scrape7c!(&["tiering_shadow_enabled"])[0] == u64::from(cx.scenario.cells) {
                on_everywhere = true;
                break;
            }
        }
        if !on_everywhere {
            bail7c!("phase-7c arm-on fan did not reach every cell");
        }
        // (2) s0 over its cold slot: the ticket (A → W), held open.
        let before = scrape7c!(&["tiering_shadow_created", "tiering_cold_resolves"]);
        expect7c!(audit, &[b"SET", s0, &s0_v2], b"+OK\r\n", "SET s0 (the shadow path)");
        let after = scrape7c!(&["tiering_shadow_created", "tiering_cold_resolves"]);
        if after[0] - before[0] != 1 {
            bail7c!(
                "REBUILT-TICKET ROW VACUOUS seed {seed:#x}: SET s0 opened {} tickets (its \
                 candidate was not cold, or the arm was off)",
                after[0] - before[0]
            );
        }
        cx.report.rebuilt_same_key_twins += 1;
        // (3) Fill the hashtag's cell past s2's commit page and keep
        //     filling until the flush lane confirms the page above the
        //     head the scrape saw: the walk refs every slot below the
        //     flushed watermark, so s2 needs flushing, not release — and
        //     the winner's pin never blocks a flush. The reboot's ticket
        //     count is the hard witness; a short fill is a vacuous row.
        //     N16 (batch 35): a fixed 1.5 MiB fill plus "advanced, then
        //     stable for four scrapes" was not that witness — the lane
        //     confirms in coarse units and holds a partial tail back, so
        //     whether s2's bytes were confirmed before the checkpoint
        //     depended on where the fill ended; on seed 0xd5ee0016 the
        //     lane stopped one unit short (cell 1: flushed 3 148 926, ro
        //     4 197 133, s2 ≈ 3.67 MiB), the walk latched W below s2, s2
        //     came back as a RAM image and the boot settled the twin by
        //     full key (A4′: two RAM siblings), leaving two tickets. The
        //     precondition is exact now: s2 ≤ the head at the scrape ≤
        //     the seal boundary + the mutable window, so `confirmed ≥ the
        //     1 MiB mark above that` puts s2 below any W a later walk
        //     latches — and filling is what moves the lane (pressure).
        let before = scrape7c!(&["tiering_flush_confirmed_bytes", "tiering_demote_sealed_bytes"]);
        let (flushed_before, sealed_before) = (before[0], before[1]);
        const PAGE: u64 = 1 << 20;
        const WINDOW_SLACK: u64 = 128 << 10;
        let s2_page_end = (sealed_before + WINDOW_SLACK).div_ceil(PAGE) * PAGE;
        let mut caught_up = false;
        let mut now = before;
        let mut batches = 0u64;
        while batches < 256 {
            for i in 0..32u64 {
                let mut key = inf_store::COLLISION_KEY_PREFIX.to_vec();
                key.extend_from_slice(format!("fill:{batches}:{i}").as_bytes());
                let value = value_bytes(b'f', 8, batches * 32 + i, 2048);
                expect7c!(audit, &[b"SET", &key, &value], b"+OK\r\n", "filler SET");
                cx.report.rebuilt_fill_sets += 1;
                expected_live += 1;
            }
            batches += 1;
            for _ in 0..8 {
                if let Err(err) = node.step(cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max) {
                    bail7c!("phase-7c fill: {err}");
                }
            }
            now = scrape7c!(&["tiering_flush_confirmed_bytes", "tiering_demote_sealed_bytes"]);
            if now[0] >= s2_page_end {
                caught_up = true;
                break;
            }
        }
        if !caught_up {
            bail7c!(
                "REBUILT-TICKET ROW VACUOUS seed {seed:#x}: {batches} filler batches never moved \
                 the flush lane past the page above s2 (mark {s2_page_end}; sealed \
                 {sealed_before} → {} B, confirmed {flushed_before} → {} B)",
                now[1],
                now[0]
            );
        }
        // A cold GET of s2 witnesses nothing here: the resolver reads the
        // ticketed twin first (same hash). The reboot's count decides.
    }
    // (4) The walk names the cold twins beside the RAM winners.
    expect7c!(audit, &[b"INF.CKPT", b"WAIT"], b"+OK\r\n", "INF.CKPT WAIT before the 7c cut");
    // F-L03-04 (ADR-0057 A3): no publication walked a table under a
    // stale checkpoint id — the leaked-pin witness stays zero.
    let behind = scrape7c!(&["tiering_walk_behind"])[0];
    if behind != 0 {
        bail7c!(
            "F-L03-04 VIOLATION seed {seed:#x}: {behind} publication(s) walked a tiered table \
             under a stale checkpoint id (a leaked walk pin was reused)"
        );
    }
    // (5) t1 over two cold collision keys: the synchronous path (every
    //     arm), a RAM image in the WAL tail.
    expect7c!(audit, &[b"SET", t1, &t1_v], b"+OK\r\n", "SET t1 (synchronous, two cold candidates)");
    expected_live += 1;
    // (6) The cut; the reboot rebuilds every pair with the reconciler's
    //     reads failing (ADR-0093 D4.3: the tickets stay).
    note_ckpt_witness(&node, cx.scenario.cells, cx.report);
    drop(node);
    cx.disk.power_cut(seed ^ 0x0FF5_EED7);
    node = match reboot_until_ready(
        cx.harness,
        cx.disk,
        cx.clock,
        cx.observer,
        cx.rng,
        cx.report,
        cx.scenario.step_ns_max,
    ) {
        Ok(node) => node,
        Err(err) => bail7c!("phase-7c reboot: {err}"),
    };
    cx.report.rebuilt_reboots += 1;
    audit = MiniClient::connect(&mut node, 0);
    expect7c!(audit, &[b"INF.NS", b"USE", NS_NAME], b"+OK\r\n", "USE after the 7c reboot");
    // The stale-read oracle first (ADR-0093 A12): the winner was sealed
    // and flushed below the walk watermark with its ticket open; if the
    // walk referenced it instead of imaging it, the key came back as two
    // cold slots and a read may serve the old one. (DBSIZE would drain
    // the tickets — it runs after the DELs.)
    if cx.scenario.shadow {
        expect7c!(audit, &[b"GET", s0], &bulk(&s0_v2), "GET s0 after the reboot (STALE READ)");
    }
    let want_tickets = 2 + if cx.scenario.shadow { 2 } else { 0 };
    let rows = scrape7c!(&[
        "tiering_shadow_pending",
        "tiering_shadow_rebuild_settled_same_key",
        "tiering_shadow_rebuild_settled_distinct",
        "tiering_shadow_rebuild_over_cap",
    ]);
    let pending = rows[0];
    if pending != want_tickets {
        // A boot settle means a hash had two RAM siblings (or the cap
        // bound) — the row's material moved; none means a pair the boot
        // should have formed is missing (product).
        let settled = rows[1] + rows[2] + rows[3];
        bail7c!(
            "REBUILT-TICKET {} seed {seed:#x}: the reboot rebuilt {pending} tickets, wanted \
             {want_tickets} (two cold collision slots beside one RAM sibling per winner; boot \
             settles same-key {} / distinct {} / over-cap {})",
            if settled > 0 { "ROW VACUOUS" } else { "VIOLATION" },
            rows[1],
            rows[2],
            rows[3]
        );
    }
    cx.report.rebuilt_tickets += pending;
    // (7) DEL each winner: every ticket drains — the collision keys
    //     stay, the same-key twin dies through its own marker.
    const REBUILT_KEYS: [&str; 3] = [
        "tiering_shadow_forced_by_delete",
        "tiering_shadow_resolved_collision",
        "tiering_shadow_pending",
    ];
    let before = scrape7c!(&REBUILT_KEYS);
    expect7c!(audit, &[b"DEL", t1], b":1\r\n", "DEL t1 (two rebuilt collision tickets)");
    cx.report.rebuilt_multi_dels += 1;
    expected_live -= 1;
    expect7c!(audit, &[b"GET", t1], b"$-1\r\n", "GET t1 after its DEL");
    expect7c!(audit, &[b"GET", t0], &bulk(&t0_v), "GET t0 (a collision key, untouched)");
    expect7c!(audit, &[b"GET", t2], &bulk(&t2_v), "GET t2 (a collision key, untouched)");
    if cx.scenario.shadow {
        expect7c!(audit, &[b"DEL", s0], b":1\r\n", "DEL s0 (a same-key twin and a collision key)");
        cx.report.rebuilt_multi_dels += 1;
        expected_live -= 1;
        expect7c!(audit, &[b"GET", s0], b"$-1\r\n", "GET s0 after its DEL");
        expect7c!(audit, &[b"GET", s2], &bulk(&s2_v), "GET s2 (the collision key, untouched)");
    }
    let after = scrape7c!(&REBUILT_KEYS);
    let want_collisions = if cx.scenario.shadow { 3 } else { 2 };
    if after[0] - before[0] != want_tickets
        || after[1] - before[1] != want_collisions
        || after[2] != 0
    {
        bail7c!(
            "REBUILT-TICKET VIOLATION seed {seed:#x}: DEL forced {} tickets (wanted \
             {want_tickets}), {} collision verdicts (wanted {want_collisions}), {} still pending",
            after[0] - before[0],
            after[1] - before[1],
            after[2]
        );
    }
    match dbsize_sum(&mut node, cx.rng, cx.clock, cx.disk, cx.scenario) {
        Ok(n) if n == expected_live => {}
        Ok(n) => bail7c!(
            "REBUILT-TICKET VIOLATION seed {seed:#x}: DBSIZE {n} after the DELs, wanted \
             {expected_live} (a phantom key: a flushed winner the walk referenced)"
        ),
        Err(err) => bail7c!("phase-7c DBSIZE: {err}"),
    }
    match scan_all_keys(&mut node, cx.rng, cx.clock, cx.disk, cx.scenario, cx.report) {
        Ok(keys) => {
            let named = |k: &[u8]| keys.iter().any(|x| x == k);
            if !named(t0)
                || !named(t2)
                || named(t1)
                || (cx.scenario.shadow && (named(s0) || !named(s2)))
            {
                bail7c!(
                    "REBUILT-TICKET VIOLATION seed {seed:#x}: SCAN after the DELs named t0 {} t1 \
                     {} t2 {} s0 {} s2 {}",
                    named(t0),
                    named(t1),
                    named(t2),
                    named(s0),
                    named(s2)
                );
            }
        }
        Err(err) => bail7c!("phase-7c SCAN: {err}"),
    }
    // (8) A second reboot without the fault: the deletions are durable
    //     (the twin's marker replayed), the collision keys intact, no
    //     ticket re-forms (the winners are gone).
    inf_foundation::fault::disarm(inf_server::fault::SHADOW_RECONCILE_READ_FAIL);
    note_ckpt_witness(&node, cx.scenario.cells, cx.report);
    drop(node);
    cx.disk.power_cut(seed ^ 0x0FF5_EED8);
    node = match reboot_until_ready(
        cx.harness,
        cx.disk,
        cx.clock,
        cx.observer,
        cx.rng,
        cx.report,
        cx.scenario.step_ns_max,
    ) {
        Ok(node) => node,
        Err(err) => bail7c!("phase-7c second reboot: {err}"),
    };
    cx.report.rebuilt_reboots += 1;
    audit = MiniClient::connect(&mut node, 0);
    expect7c!(audit, &[b"INF.NS", b"USE", NS_NAME], b"+OK\r\n", "USE after the second 7c reboot");
    expect7c!(audit, &[b"GET", t1], b"$-1\r\n", "GET t1 after the second reboot");
    expect7c!(audit, &[b"GET", t0], &bulk(&t0_v), "GET t0 after the second reboot");
    expect7c!(audit, &[b"GET", t2], &bulk(&t2_v), "GET t2 after the second reboot");
    if cx.scenario.shadow {
        expect7c!(audit, &[b"GET", s0], b"$-1\r\n", "GET s0 after the second reboot");
        expect7c!(audit, &[b"GET", s2], &bulk(&s2_v), "GET s2 after the second reboot");
        // CONFIG keys are not durable: the arm re-applies its knob for
        // the phases that follow, as phase 5 did.
        let knob: &[&[u8]] = &[b"CONFIG", b"SET", b"tiered-shadow-overwrite", b"yes"];
        expect7c!(audit, knob, b"+OK\r\n", "post-7c shadow CONFIG SET");
    }
    let pending = scrape7c!(&["tiering_shadow_pending"])[0];
    if pending != 0 {
        bail7c!(
            "REBUILT-TICKET VIOLATION seed {seed:#x}: {pending} tickets re-formed after the \
             deletions replayed"
        );
    }
    match dbsize_sum(&mut node, cx.rng, cx.clock, cx.disk, cx.scenario) {
        Ok(n) if n == expected_live => {}
        Ok(n) => bail7c!(
            "REBUILT-TICKET VIOLATION seed {seed:#x}: DBSIZE {n} after the second reboot, \
             wanted {expected_live}"
        ),
        Err(err) => bail7c!("phase-7c second-reboot DBSIZE: {err}"),
    }
    // Unused on the off arm; the shadow arm's twins are named above.
    let _ = s0_v1;

    Ok((node, audit))
}

/// Phase 8: DISKFULL clamp → typed refusal → reopen (ADR-0063).
fn phase_8(cx: &mut Late<'_>, node: &mut Node, audit: &mut MiniClient) -> Verdict {
    // A probe key guaranteed live before the clamp (GET/DEL at the cap
    // must have a target even if the clamp refuses instantly).
    let probe_value = value_bytes(b'p', 999, 0, 2 << 10);
    let probe: &[&[u8]] = &[b"SET", b"df:probe", &probe_value];
    match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, probe) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            cx.fail(format!("df:probe SET answered {other:?}"));
            return Err(());
        }
    }
    let clamp: &[&[u8]] = &[b"INF.NS", b"SET", NS_NAME, b"DISK-BUDGET", b"1mb"];
    match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, clamp) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            cx.fail(format!("DISK-BUDGET clamp answered {other:?}"));
            return Err(());
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
        let reply = match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, set)
        {
            Ok(Some(reply)) => reply,
            other => {
                cx.fail(format!("diskfull fill SET answered {other:?}"));
                return Err(());
            }
        };
        if reply.starts_with(b"-DISKFULL") {
            cx.report.diskfull_refusals += 1;
            if !reply.starts_with(b"-DISKFULL tiered namespace disk budget exhausted (used=") {
                cx.fail(format!("DISKFULL shape drifted: {}", preview(&reply)));
            }
            refused = true;
            break;
        }
        if reply != b"+OK\r\n" {
            cx.fail(format!("diskfull fill reply untyped: {}", preview(&reply)));
            return Err(());
        }
    }
    if !refused {
        cx.fail("the clamped disk budget never refused (ADR-0063 D2)".to_string());
    }
    // Refusal scope is new-byte placements only (ADR-0063 D1): reads and
    // deletes proceed at the cap.
    let get_probe: &[&[u8]] = &[b"GET", b"df:probe"];
    match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, get_probe) {
        Ok(Some(reply)) if reply == bulk(&probe_value) => {}
        other => cx.fail(format!("GET at the cap answered {other:?}")),
    }
    let del_probe: &[&[u8]] = &[b"DEL", b"df:probe"];
    match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, del_probe) {
        Ok(Some(reply)) if reply == b":1\r\n" => {}
        other => cx.fail(format!("DEL at the cap answered {other:?}")),
    }
    // Lift the budget: admission must reopen without operator surgery
    // (the M1-S07 honesty pattern — recovery is automatic).
    let lift: &[&[u8]] = &[b"INF.NS", b"SET", NS_NAME, b"DISK-BUDGET", b"64mb"];
    match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, lift) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            cx.fail(format!("DISK-BUDGET lift answered {other:?}"));
            return Err(());
        }
    }
    for i in 0..50u32 {
        let key = format!("dr:{i:03}").into_bytes();
        let value = value_bytes(b'r', 997, u64::from(i), 1 << 10);
        let set: &[&[u8]] = &[b"SET", &key, &value];
        let reply = match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, set)
        {
            Ok(Some(reply)) => reply,
            other => {
                cx.fail(format!("reopen SET answered {other:?}"));
                return Err(());
            }
        };
        if reply == b"+OK\r\n" {
            cx.report.diskfull_reopened = true;
            break;
        }
        if !reply.starts_with(b"-DISKFULL") {
            cx.fail(format!("reopen reply untyped: {}", preview(&reply)));
            return Err(());
        }
    }
    if !cx.report.diskfull_reopened {
        cx.fail("admission never reopened after the budget lifted".to_string());
    }

    Ok(())
}

/// Phase 9: the S19 drop race through the wire.
fn phase_9(cx: &mut Late<'_>, node: &mut Node, audit: &mut MiniClient) -> Verdict {
    let racer_cell = 0usize;
    let racer_fd = node.nets[racer_cell].borrow_mut().connect();
    node.nets[racer_cell]
        .borrow_mut()
        .client_send(racer_fd, &encode(&[b"INF.NS", b"USE", NS_NAME]));
    // Settle the USE reply before pipelining (one framed +OK).
    let mut rx = Vec::new();
    let mut settled = false;
    for _ in 0..STALL_STEPS {
        if node.step(cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max).is_err() {
            break;
        }
        cx.report.scheduler_steps += 1;
        let bytes = node.nets[racer_cell].borrow_mut().client_recv(racer_fd);
        rx.extend_from_slice(&bytes);
        if let Some(n) = reply_len(&rx) {
            let reply: Vec<u8> = rx.drain(..n).collect();
            if reply != b"+OK\r\n" {
                cx.fail(format!("racer USE answered {}", preview(&reply)));
                return Err(());
            }
            settled = true;
            break;
        }
    }
    if !settled {
        cx.report.stalled = true;
        cx.fail("racer USE stalled".to_string());
        return Err(());
    }
    let race_keys: Vec<Vec<u8>> = cx.observed.keys().take(30).cloned().collect();
    let mut batch = Vec::new();
    for key in &race_keys {
        batch.extend_from_slice(&encode(&[b"GET", key]));
    }
    node.nets[racer_cell].borrow_mut().client_send(racer_fd, &batch);
    // DROP races the pipelined reads from a second connection.
    let drop_ns: &[&[u8]] = &[b"INF.NS", b"DROP", NS_NAME];
    match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, drop_ns) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            cx.fail(format!("racing DROP answered {other:?}"));
            return Err(());
        }
    }
    // Every pipelined reply must arrive typed — a missing reply is the
    // hang this row exists to catch (§3.3 teardown vs in-flight custody).
    let mut answered = 0usize;
    let mut idle = 0u64;
    while answered < race_keys.len() {
        if let Err(err) = node.step(cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max) {
            cx.fail(format!("drop-race drain: {err}"));
            return Err(());
        }
        cx.report.scheduler_steps += 1;
        let bytes = node.nets[racer_cell].borrow_mut().client_recv(racer_fd);
        if bytes.is_empty() {
            idle += 1;
            if idle >= STALL_STEPS {
                cx.report.stalled = true;
                cx.fail(format!(
                    "DROP-RACE HANG: {} of {} pipelined replies never arrived",
                    race_keys.len() - answered,
                    race_keys.len()
                ));
                return Err(());
            }
        } else {
            idle = 0;
        }
        rx.extend_from_slice(&bytes);
        while let Some(n) = reply_len(&rx) {
            let reply: Vec<u8> = rx.drain(..n).collect();
            answered += 1;
            match reply.first() {
                Some(b'$') if reply.starts_with(b"$-1") => cx.report.drop_replies_other += 1,
                Some(b'$') => cx.report.drop_replies_value += 1,
                Some(b'-') => cx.report.drop_replies_other += 1,
                _ => cx.fail(format!("untyped drop-race reply: {}", preview(&reply))),
            }
            if answered == race_keys.len() {
                break;
            }
        }
    }
    // The audit connection sits on the dropped namespace: its PING must
    // answer the typed dropped-namespace error (never a hang or a crash).
    let ping: &[&[u8]] = &[b"PING"];
    match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, ping) {
        Ok(Some(reply)) if reply.starts_with(b"-ERR") => {}
        other => cx.fail(format!("post-drop PING (dropped ns) answered {other:?}")),
    }
    // The node itself stays live: a fresh connection serves.
    let mut fresh = MiniClient::connect(node, 0);
    match fresh.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, ping) {
        Ok(Some(reply)) if reply == b"+PONG\r\n" => {}
        other => cx.fail(format!("post-drop PING (fresh conn) answered {other:?}")),
    }
    let use_dropped: &[&[u8]] = &[b"INF.NS", b"USE", NS_NAME];
    match fresh.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, use_dropped) {
        Ok(Some(reply)) if reply.starts_with(b"-ERR") => {}
        other => cx.fail(format!("USE of the dropped ns answered {other:?}")),
    }

    Ok(())
}

/// Phases 10 and 10b: the reboot after the DROP (ADR-0100, review C13) and
/// a power cut inside a second DROP (ADR-0100 D5).
fn phase_10(cx: &mut Late<'_>, node: Node) -> Verdict {
    let use_dropped: &[&[u8]] = &[b"INF.NS", b"USE", NS_NAME];
    // Nothing checkpointed since phase 9's drop, so a cell's MANIFEST may
    // still carry the namespace's tier section. The pre-ADR node refused
    // exactly this boot ("MANIFEST carries a tier section for ns 16 the
    // catalog does not know"); the catalog's tombstone now explains the
    // residue and recovery sweeps `ns-16/cold`. Disclosed, never assumed:
    // a seed whose MANIFESTs no longer name the namespace (a checkpoint
    // raced the drop) is counted inert for this row.
    note_ckpt_witness(&node, cx.scenario.cells, cx.report);
    drop(node);
    for cell in 0..cx.scenario.cells {
        let shard = PathBuf::from("node").join(format!("shard-{cell}"));
        if let Ok(Some(manifest)) = inf_log::read_manifest(cx.disk, &shard)
            && manifest.tiers.iter().any(|t| t.ns == DROPPED_NS_ID)
        {
            cx.report.drop_reboot_manifest_residue = true;
        }
    }
    let mut node = match reboot_until_ready(
        cx.harness,
        cx.disk,
        cx.clock,
        cx.observer,
        cx.rng,
        cx.report,
        cx.scenario.step_ns_max,
    ) {
        Ok(node) => node,
        Err(what) => {
            cx.fail(format!("post-drop reboot: {what}"));
            return Err(());
        }
    };
    cx.report.drop_reboot_ok = true;
    let mut after = MiniClient::connect(&mut node, 0);
    match after.call(&mut node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, use_dropped) {
        Ok(Some(reply)) if reply.starts_with(b"-ERR") => {}
        other => cx.fail(format!("dropped ns came back after the reboot: {other:?}")),
    }
    let residue = tier_residue_files(cx.disk, DROPPED_NS_ID);
    if residue > 0 {
        cx.fail(format!("{residue} tier files survived the drop's boot sweep (ADR-0100 D6)"));
    }

    // ---- phase 10b: a power cut inside a second DROP (ADR-0100 D5) -------
    // A fresh tiered namespace with a checkpointed tier section; its DROP
    // goes on the wire and the power is cut a seeded number of steps
    // later — before or after the catalog swap, the sweep decides. The
    // reboot must land on exactly one of the two D5 outcomes: the
    // namespace whole (every key served — the teardown hold kept its
    // files), or gone with its residue swept. Anything else is a finding.
    let create2: Vec<&[u8]> =
        cx.create.iter().map(|arg| if *arg == NS_NAME { CUT_NS_NAME } else { *arg }).collect();
    let mut ddl = MiniClient::connect(&mut node, 0);
    for (argv, what) in
        [(create2.as_slice(), "CREATE"), (&[&b"INF.NS"[..], b"USE", CUT_NS_NAME][..], "USE")]
    {
        match ddl.call(&mut node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, argv) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            other => {
                cx.fail(format!("phase 10b {what} answered {other:?}"));
                return Err(());
            }
        }
    }
    for i in 0..CUT_NS_KEYS {
        let key = format!("cut:{i}");
        let value = format!("v{i}");
        let set: &[&[u8]] = &[b"SET", key.as_bytes(), value.as_bytes()];
        match ddl.call(&mut node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, set) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {}
            other => {
                cx.fail(format!("phase 10b SET answered {other:?}"));
                return Err(());
            }
        }
    }
    let ckpt: &[&[u8]] = &[b"INF.CKPT", b"WAIT"];
    match ddl.call(&mut node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, ckpt) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            cx.fail(format!("phase 10b INF.CKPT WAIT answered {other:?}"));
            return Err(());
        }
    }
    // F-L03-04 (ADR-0057 A3): every publication so far walked its tiered
    // tables under its own id — the leaked-pin witness stays zero (the
    // `ckpt_direct_refused_after` seeds aborted an early walk mid-pin).
    match info_sum(&mut node, cx.rng, cx.clock, cx.disk, cx.scenario, &["tiering_walk_behind"]) {
        Ok(v) if v[0] == 0 => {}
        Ok(v) => {
            cx.fail(format!(
                "F-L03-04 VIOLATION seed {:#x}: {} publication(s) walked a tiered table \
                     under a stale checkpoint id (a leaked walk pin was reused)",
                cx.scenario.seed, v[0]
            ));
            return Err(());
        }
        Err(err) => {
            cx.fail(format!("phase 10b INFO tiering: {err}"));
            return Err(());
        }
    }
    // Engagement (a vacuous arm is a rotted arm): the `EINVAL` seeds
    // must have downgraded some cell mid-walk, and the section-bound
    // seeds must have split sections for the bound.
    let (downgrades, splits) = {
        let (d, b) = ckpt_witness(&node, cx.scenario.cells);
        (cx.report.ckpt_downgrades + d, cx.report.ckpt_bound_splits + b)
    };
    if cx.scenario.ckpt_direct_refused_after.is_some() && downgrades == 0 {
        cx.fail(format!(
            "EINVAL ARM VACUOUS seed {:#x}: no cell downgraded its checkpoint staging",
            cx.scenario.seed
        ));
        return Err(());
    }
    if cx.scenario.ckpt_section_bound.is_some() && splits == 0 {
        cx.fail(format!(
            "SECTION-BOUND ARM VACUOUS seed {:#x}: no section sealed for the bound",
            cx.scenario.seed
        ));
        return Err(());
    }
    let cut_steps = cx.rng.next_below(CUT_STEPS_MAX);
    cx.report.drop_cut_steps = cut_steps;
    let dropper_cell = (cx.rng.next_u64() as usize) % cx.scenario.cells as usize;
    let dropper_fd = node.nets[dropper_cell].borrow_mut().connect();
    node.nets[dropper_cell]
        .borrow_mut()
        .client_send(dropper_fd, &encode(&[b"INF.NS", b"DROP", CUT_NS_NAME]));
    for _ in 0..cut_steps {
        cx.report.scheduler_steps += 1;
        if let Err(err) = node.step(cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max) {
            cx.fail(format!("phase 10b step: {err}"));
            return Err(());
        }
    }
    note_ckpt_witness(&node, cx.scenario.cells, cx.report);
    drop(node);
    cx.disk.power_cut(cx.scenario.seed ^ 0x0D20_9C07);
    let mut node = match reboot_until_ready(
        cx.harness,
        cx.disk,
        cx.clock,
        cx.observer,
        cx.rng,
        cx.report,
        cx.scenario.step_ns_max,
    ) {
        Ok(node) => node,
        Err(what) => {
            cx.fail(format!("reboot after the cut inside DROP: {what}"));
            return Err(());
        }
    };
    let mut audit2 = MiniClient::connect(&mut node, 0);
    let use_cut: &[&[u8]] = &[b"INF.NS", b"USE", CUT_NS_NAME];
    match audit2.call(&mut node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, use_cut) {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {
            // Whole: the cut preceded the swap — every key serves.
            cx.report.drop_cut_outcome = DropCutOutcome::Whole;
            for i in 0..CUT_NS_KEYS {
                let key = format!("cut:{i}");
                let want = format!("${}\r\nv{i}\r\n", format!("v{i}").len());
                let get: &[&[u8]] = &[b"GET", key.as_bytes()];
                match audit2.call(
                    &mut node,
                    cx.rng,
                    cx.clock,
                    cx.disk,
                    cx.scenario.step_ns_max,
                    get,
                ) {
                    Ok(Some(reply)) if reply == want.as_bytes() => {}
                    other => cx.fail(format!(
                        "cut before the swap restored the namespace but {key} answered \
                             {other:?} (ADR-0100 D5: the teardown hold must keep every file)"
                    )),
                }
            }
        }
        Ok(Some(reply)) if reply.starts_with(b"-ERR") => {
            // Swept: the swap was durable — the namespace is gone and
            // nothing of it remains on cx.disk.
            cx.report.drop_cut_outcome = DropCutOutcome::Swept;
            let residue = tier_residue_files(cx.disk, CUT_NS_ID);
            if residue > 0 {
                cx.fail(format!(
                    "{residue} tier files survived the sweep after a cut past the swap"
                ));
            }
        }
        other => cx.fail(format!("USE after the cut inside DROP answered {other:?}")),
    }

    Ok(())
}
