//! The tiered scenario's late phases — 7c (a rebuilt winner carrying
//! several tickets, ADR-0093 A10), 7d (two GETs of one cold blob key in
//! flight together, issue #11), 7e (a cold burst on a quiet node, delivered
//! a turn's allowance at a time, ADR-0181 R1), 8 (DISKFULL clamp → typed refusal →
//! reopen, ADR-0063), 9 (the S19 drop race through the wire), 10 and 10b
//! (reboots after a DROP and a cut inside one, ADR-0100). Each phase is a
//! straight-line script over [`Late`]; `Err(())` means "finish now" — the
//! violation is already on the report. The listing-repeat class (ADR-0202
//! D5) writes its residue between phases 8 and 9 and reads its engagement
//! after phase 10's reboot.

use super::*;
use crate::resp::Reply;

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
    #[track_caller]
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
    // Phase 7c's rows stand on its pre-cut collision keys being cold when
    // its writes and its reboot meet them; in the replay-above-window class
    // no checkpoint publishes in the run, so each reboot replays the whole
    // log and where those records land is the demotion's, not the row's:
    // the phase does not run (`rebuilt_rows` stays false — disclosed).
    let (mut node, mut audit) = if cx.scenario.replay_above_window {
        (node, audit)
    } else {
        phase_7c(cx, node, audit, total_keys)?
    };
    phase_7d(cx, &mut node, &mut audit)?;
    phase_7e(cx, &mut node, &mut audit)?;
    phase_8(cx, &mut node, &mut audit)?;
    listing_residue(cx, &mut node, &mut audit)?;
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

/// Phase 7d's steps share these: a failure is on the report and `Err`
/// ends the script.
macro_rules! bail7d {
    ($cx:expr, $($arg:tt)*) => {{
        $cx.fail(format!($($arg)*));
        return Err(());
    }};
}
macro_rules! scrape7d {
    ($cx:expr, $node:expr, $keys:expr) => {
        match info_sum($node, $cx.rng, $cx.clock, $cx.disk, $cx.scenario, $keys) {
            Ok(v) => v,
            Err(err) => bail7d!($cx, "phase-7d scrape: {err}"),
        }
    };
}
macro_rules! call7d {
    ($cx:expr, $node:expr, $client:expr, $cmd:expr, $want:expr, $what:expr) => {{
        let want: &[u8] = $want;
        match $client.call($node, $cx.rng, $cx.clock, $cx.disk, $cx.scenario.step_ns_max, $cmd) {
            Ok(Some(reply)) if reply == want => {}
            other => bail7d!(
                $cx,
                "PROMOTED-EXTENT VIOLATION seed {:#x}: {} answered {}, wanted {}",
                $cx.scenario.seed,
                $what,
                other.map_or_else(
                    |e| e.to_string(),
                    |r| r.map_or("nothing".to_owned(), |r| preview(&r))
                ),
                preview(want)
            ),
        }
        $cx.report.commands_done += 1;
    }};
}

/// Phase 7d (issue #11, the nightly of 2026-11-06): two GETs of one cold
/// blob key in flight together. The first read to resume is the key's
/// second touch and promotes the record (ADR-0085 D2), a `StringExtent`
/// whose RAM bytes are its 24-byte extent reference; the other read
/// re-resolves after its own wait onto that RAM copy and must still serve
/// the value from the extent, never the reference. A pair counts when
/// both reads resolved cold and it promoted; a seed with no counted pair
/// is a vacuous row.
fn phase_7d(cx: &mut Late<'_>, node: &mut Node, audit: &mut MiniClient) -> Verdict {
    // A pair that finds its key in RAM (compaction's copy-forward moved it
    // after its first touch) fills once more and asks again; the filter's
    // tag survives a relocation, so the next cold pair is the second touch.
    const PAIR_ATTEMPTS: u32 = 8;
    // One hashtag: the keys and the filler share one owning cell, so the
    // fill presses the cell whose head must pass the keys.
    let keys: Vec<Vec<u8>> = (0..4).map(|i| format!("{{p7d}}promo:{i}").into_bytes()).collect();
    // ≥ 6 KiB: above the namespace's 4 KiB `BLOB-THRESHOLD`, so each
    // value lives in an extent and its record is the reference.
    let values: Vec<Vec<u8>> =
        (0..keys.len()).map(|i| value_bytes(b'q', 997, i as u64, (6 << 10) + 512 * i)).collect();
    let wants: Vec<Vec<u8>> = values.iter().map(|value| bulk(value)).collect();
    for (key, value) in keys.iter().zip(&values) {
        call7d!(cx, node, audit, &[b"SET", key, value], b"+OK\r\n", "SET of a blob key");
    }
    let mut batches = cool_to_first_touch(cx, node, audit, &keys, &wants)?;
    let use_ns: &[&[u8]] = &[b"INF.NS", b"USE", NS_NAME];
    let mut pair = [
        MiniClient::connect(node, 0),
        MiniClient::connect(node, usize::from(cx.scenario.cells) - 1),
    ];
    for client in &mut pair {
        call7d!(cx, node, client, use_ns, b"+OK\r\n", "USE of a pair client");
    }
    let mut engaged = 0u64;
    for (key, want) in keys.iter().zip(&wants) {
        for _ in 0..PAIR_ATTEMPTS {
            if promoting_pair(cx, node, &mut pair, key, want)? {
                engaged += 1;
                break;
            }
            fill_batch(cx, node, audit, "p7d", batches)?;
            batches += 1;
        }
    }
    if engaged == 0 {
        let seed = cx.scenario.seed;
        bail7d!(
            cx,
            "PROMOTED-PAIR ROW VACUOUS seed {seed:#x}: none of {} keys' pairs both read cold and \
             promoted in {PAIR_ATTEMPTS} attempts each",
            keys.len()
        );
    }
    cx.report.promoted_extent_pairs += engaged;
    Ok(())
}

/// A filler batch on one cell: 32 inline 2 KiB writes whose keys share the
/// hashtag `tag`, so the batch presses the one cell that owns them, then a
/// few scheduler steps for MAINTAIN to demote and release.
fn fill_batch(
    cx: &mut Late<'_>,
    node: &mut Node,
    audit: &mut MiniClient,
    tag: &str,
    batch: u64,
) -> Verdict {
    const FILL_KEYS: u64 = 32;
    for i in 0..FILL_KEYS {
        let key = format!("{{{tag}}}fill:{batch}:{i}").into_bytes();
        let value = value_bytes(b'f', 997, batch * FILL_KEYS + i, 2048);
        let set: &[&[u8]] = &[b"SET", &key, &value];
        match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, set) {
            Ok(Some(reply)) if reply == b"+OK\r\n" => {}
            other => bail7d!(
                cx,
                "{tag} filler SET {} answered {}",
                preview(&key),
                other.map_or_else(
                    |e| e.to_string(),
                    |r| r.map_or("nothing".to_owned(), |r| preview(&r))
                )
            ),
        }
        cx.report.commands_done += 1;
    }
    for _ in 0..8 {
        if let Err(err) = node.step(cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max) {
            bail7d!(cx, "{tag} fill: {err}");
        }
    }
    Ok(())
}

/// Phase 7d's cooling: filler batches until each key's GET is a verified
/// cold read, its first touch (ADR-0085 D2). Bounded; a key still in RAM
/// at the bound is a vacuous row. Returns the batches written.
fn cool_to_first_touch(
    cx: &mut Late<'_>,
    node: &mut Node,
    audit: &mut MiniClient,
    keys: &[Vec<u8>],
    wants: &[Vec<u8>],
) -> Verdict<u64> {
    const FILL_BATCHES_MAX: u64 = 64;
    let seed = cx.scenario.seed;
    let mut touched = vec![false; keys.len()];
    let mut batches = 0u64;
    while touched.iter().any(|t| !t) {
        if batches == FILL_BATCHES_MAX {
            let ram = touched.iter().filter(|t| !**t).count();
            bail7d!(
                cx,
                "PROMOTED-PAIR ROW VACUOUS seed {seed:#x}: {batches} filler batches left {ram} \
                 of {} blob keys without a cold first read",
                keys.len()
            );
        }
        fill_batch(cx, node, audit, "p7d", batches)?;
        batches += 1;
        for (i, key) in keys.iter().enumerate() {
            if touched[i] {
                continue;
            }
            let before = scrape7d!(cx, node, &["tiering_promote_first_touch"])[0];
            call7d!(cx, node, audit, &[b"GET", key], &wants[i], "a blob key's first GET");
            touched[i] = scrape7d!(cx, node, &["tiering_promote_first_touch"])[0] > before;
        }
    }
    Ok(batches)
}

/// Phase 7d's pair: two GETs of `key` from two connections in flight
/// together; both must answer the value. `true` when both reads resolved
/// cold and the pair promoted — the second read re-resolved onto the
/// promoted reference.
fn promoting_pair(
    cx: &mut Late<'_>,
    node: &mut Node,
    pair: &mut [MiniClient; 2],
    key: &[u8],
    want: &[u8],
) -> Verdict<bool> {
    let seed = cx.scenario.seed;
    let before = scrape7d!(cx, node, &["tiering_promotions", "tiering_cold_resolves"]);
    for client in pair.iter_mut() {
        client.send(node, &[b"GET", key]);
    }
    let mut replies: [Option<Vec<u8>>; 2] = [None, None];
    let mut steps = 0u64;
    while replies.iter().any(Option::is_none) {
        if steps == STALL_STEPS {
            cx.report.stalled = true;
            bail7d!(cx, "PROMOTED-PAIR STALL seed {seed:#x}: the pair's GETs never both answered");
        }
        steps += 1;
        cx.report.scheduler_steps += 1;
        if let Err(err) = node.step(cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max) {
            bail7d!(cx, "phase-7d pair step: {err}");
        }
        for (client, reply) in pair.iter_mut().zip(&mut replies) {
            if reply.is_none() {
                *reply = client.recv(node);
            }
        }
    }
    cx.report.commands_done += 2;
    for (side, reply) in replies.iter().flatten().enumerate() {
        if reply != want {
            bail7d!(
                cx,
                "PROMOTED-EXTENT VIOLATION seed {seed:#x}: GET {} in flight beside a promoting \
                 read answered {} on pair client {side}, wanted {}",
                preview(key),
                preview(reply),
                preview(want)
            );
        }
    }
    let after = scrape7d!(cx, node, &["tiering_promotions", "tiering_cold_resolves"]);
    Ok(after[0] > before[0] && after[1] >= before[1] + 2)
}

/// Phase 7e's readers: one `GET` a connection, since a connection's pipeline
/// holds one cold read at a time; four turns of a turn's 64 visits.
const BURST_READERS: usize = 256;

/// Phase 7e's keys' hashtag: every burst and filler key on one owning cell,
/// the cell its readers connect to.
const BURST_TAG: &str = "p7e";

/// A burst key's value: small, so one device read covers many adjacent keys.
const BURST_VALUE_LEN: usize = 400;

/// Rounds of phase 7e's delivery: the burst's own reads, then at most two rounds of re-reads. A
/// reader reads again after its resume when its candidate held another key's record (a
/// fingerprint match), its key's cold address moved while it waited, or its image ran past its
/// window; each further round needs one more such miss, so a reader still waiting at the last
/// round's bound is a stuck client.
const BURST_ROUNDS_MAX: u32 = 3;

/// Phase 7e: a cold burst, then its delivery in rounds (ADR-0181 R1, R2). Burst keys on one cell
/// are written and pushed out of RAM by filler batches; then one `GET` of each from its own
/// connection to the owning cell, all reaching it at one reap, so they merge into fewer reads than
/// the device's depth, while the device holds tier reads until every read the burst issues is
/// queued there; released at one driver entry, their receipts make more members ready than a turn
/// visits. A round is the tier reads the device holds, released at one driver entry. Other tier
/// reads may run beside the burst (a replay-above-window node keeps reading its tier), so a
/// round's receipt is its own: the step at which the owner's sim driver has counted the reads it
/// released, set once. From that step the device holds tier reads again, so no other read
/// completes and no other window becomes ready, and a sim cell iterates every step, so the
/// server's `before_execute` delivers the round alone. The burst's reads are the first round; a
/// read the device comes to hold after a receipt is a re-read, the next round's. Oracle: each reply
/// is its value, and a cold one arrives within ⌈readers left / 64⌉ + 2 steps of its round's
/// receipt (the harness's reply log against the device's own count): a reply past that bound is
/// late. The next round is released once the held reads are steady, and only while the owner's
/// cold host has no member left to visit (its pending count: a member a receipt made ready and no
/// turn visited is late). A reader with no read held for `STALL_STEPS`, or still waiting at round
/// [`BURST_ROUNDS_MAX`]'s bound, is a stuck client. The phase is quiet of copy-forward too: from
/// before its keys are written to its last reply, no file sealed gives a burst key back to RAM
/// ([`burst_quiet`]). Engagement: some step left the owner's cold host members to visit (its
/// pending count, read between steps: a wiring half), so a turn's allowance left members to a
/// later turn; else VACUOUS.
fn phase_7e(cx: &mut Late<'_>, node: &mut Node, audit: &mut MiniClient) -> Verdict {
    let seed = cx.scenario.seed;
    let ratio = burst_quiet(cx, node, audit)?;
    let keys: Vec<Vec<u8>> =
        (0..BURST_READERS).map(|i| format!("{{{BURST_TAG}}}burst:{i:03}").into_bytes()).collect();
    let values: Vec<Vec<u8>> =
        (0..BURST_READERS).map(|i| value_bytes(b'e', 7, i as u64, BURST_VALUE_LEN)).collect();
    for (key, value) in keys.iter().zip(&values) {
        call7d!(cx, node, audit, &[b"SET", key, value], b"+OK\r\n", "SET of a burst key");
    }
    burst_cool(cx, node, audit)?;
    let owner = super::support::owner_cell(cx.scenario.cells, &keys[0]);
    let use_ns: &[&[u8]] = &[b"INF.NS", b"USE", NS_NAME];
    let mut burst = Burst {
        owner,
        readers: Vec::with_capacity(BURST_READERS),
        wants: values.iter().map(|value| bulk(value)).collect(),
        answered: vec![false; BURST_READERS],
        left: BURST_READERS,
    };
    for _ in 0..BURST_READERS {
        let mut reader = MiniClient::connect(node, owner);
        call7d!(cx, node, reader, use_ns, b"+OK\r\n", "USE of a burst reader");
        burst.readers.push(reader);
    }
    // The burst reaches the owner at one reap, as a stopped-then-resumed
    // client's would: its requests are queued together and merge, so every
    // read the burst issues fits the device's depth and is held whole.
    node.nets[owner].borrow_mut().deliver_whole();
    cx.disk.hold_reads(Some(is_tier_file));
    for (reader, key) in burst.readers.iter_mut().zip(&keys) {
        reader.send(node, &[b"GET", key]);
    }
    let held = burst_hold(cx, node, &mut burst);
    cx.disk.hold_reads(None);
    let held = held?;
    if held == 0 {
        bail7d!(cx, "COLD BURST VACUOUS seed {seed:#x}: the burst issued no tier read");
    }
    let cold = burst.left;
    let delivered = burst_deliver(cx, node, &mut burst, held);
    cx.disk.hold_reads(None);
    node.nets[owner].borrow_mut().deliver_chunked();
    let pending_max = delivered?;
    if pending_max == 0 {
        bail7d!(
            cx,
            "COLD BURST VACUOUS seed {seed:#x}: {cold} GETs read cold and no step left a member \
             to visit: no turn's allowance left one to a later turn"
        );
    }
    let pending = scrape7d!(cx, node, &["cold_delivery_pending_members"])[0];
    if pending != 0 {
        bail7d!(cx, "COLD BURST seed {seed:#x}: {pending} members left after every reply");
    }
    let set_back: &[&[u8]] = &[b"INF.NS", b"SET", NS_NAME, b"COMPACTION-DEAD-RATIO", &ratio];
    call7d!(cx, node, audit, set_back, b"+OK\r\n", "phase 7e's dead ratio set back");
    Ok(())
}

/// Phase 7e's compaction dead ratio, in percent: at 100 the dead-ratio arm
/// takes a file only with every byte dead.
const BURST_DEAD_RATIO_PCT: &[u8] = b"100";

/// Phase 7e's quiet of copy-forward, set before the burst's keys are
/// written. Compaction re-appends a file's live records at the RAM tail,
/// and a file sealed while the keys cool is a candidate at its seal when
/// its dead bytes are half its data (a node that replayed its whole log
/// and demoted it again holds mostly dead bytes): it would give burst keys
/// back to RAM at addresses above the sentinel, which reads cold. So the
/// namespace's dead ratio is set to [`BURST_DEAD_RATIO_PCT`]; a candidate
/// already picked ends on a file sealed before the keys were written. The
/// pressure arm takes the widest dead ratio whatever the setting, but only
/// under a disk budget, so the namespace must have none, else VACUOUS.
/// Returns the ratio it read, which the phase sets back after its last
/// reply.
fn burst_quiet(cx: &mut Late<'_>, node: &mut Node, audit: &mut MiniClient) -> Verdict<Vec<u8>> {
    let seed = cx.scenario.seed;
    let info: &[&[u8]] = &[b"INF.NS", b"INFO", NS_NAME];
    let reply = match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, info) {
        Ok(Some(reply)) => reply,
        other => bail7d!(
            cx,
            "phase-7e INF.NS INFO answered {}",
            other.map_or_else(
                |e| e.to_string(),
                |r| r.map_or("nothing".to_owned(), |r| preview(&r))
            )
        ),
    };
    cx.report.commands_done += 1;
    let fields =
        (ns_info_field(&reply, b"compaction-dead-ratio"), ns_info_field(&reply, b"disk-budget"));
    let (Some(ratio), Some(budget)) = fields else {
        bail7d!(cx, "phase-7e INF.NS INFO carries no tier block: {}", preview(&reply));
    };
    if budget != b"0" {
        bail7d!(
            cx,
            "COLD BURST VACUOUS seed {seed:#x}: the namespace's disk-budget is {}, so \
             compaction's pressure arm may give a burst key back to RAM",
            String::from_utf8_lossy(&budget)
        );
    }
    let quiet: &[&[u8]] =
        &[b"INF.NS", b"SET", NS_NAME, b"COMPACTION-DEAD-RATIO", BURST_DEAD_RATIO_PCT];
    call7d!(cx, node, audit, quiet, b"+OK\r\n", "phase 7e's dead ratio");
    Ok(ratio)
}

/// `field`'s value in an `INF.NS INFO` reply, an array of field and value
/// bulks.
fn ns_info_field(reply: &[u8], field: &[u8]) -> Option<Vec<u8>> {
    let Ok(Reply::Array(items)) = crate::resp::try_parse_reply(reply) else {
        return None;
    };
    items.chunks_exact(2).find_map(|pair| match pair {
        [Reply::Bulk(name), Reply::Bulk(value)] if name == field => Some(value.clone()),
        _ => None,
    })
}

/// Phase 7e's readers and what they answered.
struct Burst {
    /// The cell that owns every burst key; its readers connect to it.
    owner: usize,
    readers: Vec<MiniClient>,
    /// Each reader's expected reply.
    wants: Vec<Vec<u8>>,
    answered: Vec<bool>,
    /// Readers not yet answered.
    left: usize,
}

impl Burst {
    /// Reads every reply that has arrived and judges it against its value.
    /// Returns how many arrived.
    fn collect(&mut self, cx: &mut Late<'_>, node: &mut Node) -> Verdict<usize> {
        let seed = cx.scenario.seed;
        let mut arrived = 0;
        for (i, reader) in self.readers.iter_mut().enumerate() {
            if self.answered[i] {
                continue;
            }
            let Some(reply) = reader.recv(node) else { continue };
            if reply != self.wants[i] {
                bail7d!(
                    cx,
                    "COLD BURST VIOLATION seed {seed:#x}: reader {i} answered {}, wanted {}",
                    preview(&reply),
                    preview(&self.wants[i])
                );
            }
            self.answered[i] = true;
            arrived += 1;
        }
        self.left -= arrived;
        cx.report.commands_done += arrived as u64;
        Ok(arrived)
    }
}

/// Phase 7e's cooling: filler batches on the burst's cell until a sentinel
/// key written after the burst keys reads cold; the burst keys, older, aged
/// out of RAM before it. Bounded as phase 7d's; a sentinel still in RAM at
/// the bound is a vacuous row.
fn burst_cool(cx: &mut Late<'_>, node: &mut Node, audit: &mut MiniClient) -> Verdict {
    const FILL_BATCHES_MAX: u64 = 64;
    let seed = cx.scenario.seed;
    let sentinel = format!("{{{BURST_TAG}}}sentinel").into_bytes();
    let value = value_bytes(b's', 7, 0, BURST_VALUE_LEN);
    call7d!(cx, node, audit, &[b"SET", &sentinel, &value], b"+OK\r\n", "SET of the sentinel");
    let want = bulk(&value);
    for batch in 0..FILL_BATCHES_MAX {
        fill_batch(cx, node, audit, BURST_TAG, batch)?;
        let before = scrape7d!(cx, node, &["tiering_cold_resolves"])[0];
        call7d!(cx, node, audit, &[b"GET", &sentinel], &want, "GET of the sentinel");
        if scrape7d!(cx, node, &["tiering_cold_resolves"])[0] > before {
            return Ok(());
        }
    }
    bail7d!(
        cx,
        "COLD BURST VACUOUS seed {seed:#x}: {FILL_BATCHES_MAX} filler batches left the sentinel \
         in RAM"
    );
}

/// Steps until the owner's held tier reads stop growing: every read the
/// burst issues is queued at the device. A reply that arrives meanwhile is
/// a `GET` answered from RAM, judged and kept out of the cold count.
/// Returns the reads held.
fn burst_hold(cx: &mut Late<'_>, node: &mut Node, burst: &mut Burst) -> Verdict<usize> {
    steady_held_reads(STALL_STEPS, || {
        if let Err(err) = node.step(cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max) {
            bail7d!(cx, "phase-7e hold step: {err}");
        }
        cx.report.scheduler_steps += 1;
        burst.collect(cx, node)?;
        Ok(node.driver(burst.owner).held_reads())
    })
}

/// One round of phase 7e's delivery.
#[derive(Clone, Copy)]
struct Round {
    /// 1 for the burst's own reads, then each round of re-reads.
    index: u32,
    /// The tier reads the device held, which the round releases.
    held: usize,
    /// Readers unanswered at the release.
    readers: u64,
    /// Steps after the round's receipt within which its replies arrive:
    /// ⌈`readers` / a turn's visits⌉ + 2.
    bound: u64,
}

/// Steps until every burst reader's reply is in, in rounds: the first
/// releases the `held` reads of the burst, and each round's replies are
/// judged against its own receipt. Past a round's bound with readers left,
/// the reads the device came to hold are the next round's. From a receipt
/// the device holds tier reads again, until the caller lifts the hold.
/// Returns the most members a step left the owner's cold host to visit.
fn burst_deliver(
    cx: &mut Late<'_>,
    node: &mut Node,
    burst: &mut Burst,
    held: usize,
) -> Verdict<u64> {
    let seed = cx.scenario.seed;
    let turn = u64::from(inf_runtime::limits::COLD_DELIVERIES_PER_TURN);
    let mut pending_max = 0;
    let mut round = Round { index: 1, held, readers: 0, bound: 0 };
    loop {
        round.readers = burst.left as u64;
        round.bound = round.readers.div_ceil(turn) + 2;
        cx.report.burst_rounds = round.index;
        if burst_round(cx, node, burst, round, &mut pending_max)? {
            return Ok(pending_max);
        }
        round.held = burst_wait(cx, node, burst, round, &mut pending_max)?;
        if round.index == BURST_ROUNDS_MAX {
            cx.report.stalled = true;
            let left = burst.left;
            bail7d!(
                cx,
                "COLD BURST STALL seed {seed:#x}: cold-burst-round-cap: {left} readers still wait \
                 on a read after {} rounds (a stuck client)",
                round.index
            );
        }
        round.index += 1;
        cx.disk.hold_reads(None);
    }
}

/// One round of [`burst_deliver`]: steps from its release until every
/// reader has answered (`true`), or until its bound has passed since its
/// receipt with readers left (`false`). The receipt is the step at which
/// the owner's driver has counted the round's held reads, set once; from it
/// the device holds tier reads again.
fn burst_round(
    cx: &mut Late<'_>,
    node: &mut Node,
    burst: &mut Burst,
    round: Round,
    pending_max: &mut u64,
) -> Verdict<bool> {
    let seed = cx.scenario.seed;
    let released = node.driver(burst.owner).observed_io().read_completions;
    let received = released + round.held as u64;
    let mut mark = None;
    for step in 0..STALL_STEPS {
        if let Err(err) = node.step(cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max) {
            bail7d!(cx, "phase-7e burst step: {err}");
        }
        cx.report.scheduler_steps += 1;
        let completions = node.driver(burst.owner).observed_io().read_completions;
        if mark.is_none() && completions >= received {
            mark = Some(step);
            cx.disk.hold_reads(Some(is_tier_file));
        }
        *pending_max = (*pending_max).max(node.plane(burst.owner).cold_pending_for_sim());
        let arrived = burst.collect(cx, node)?;
        if arrived > 0 && completions == released {
            bail7d!(cx, "COLD BURST seed {seed:#x}: a cold GET answered before any receipt");
        }
        if burst.left == 0 {
            if mark.is_none() {
                bail7d!(
                    cx,
                    "COLD BURST VACUOUS seed {seed:#x}: every reply arrived before the owner \
                     counted the {} reads the hold released: no reply was judged",
                    round.held
                );
            }
            return Ok(true);
        }
        if mark.is_some_and(|mark| step - mark == round.bound) {
            return Ok(false);
        }
    }
    cx.report.stalled = true;
    let left = burst.left;
    bail7d!(cx, "COLD BURST STALL seed {seed:#x}: {left} readers never answered (a stuck client)");
}

/// Past a round's bound with readers left: steps until the tier reads the
/// device holds are steady above 0, and returns them, the next round's. A
/// reply meanwhile is late: no read completed since the receipt, so the
/// receipt made its member ready. A member still unvisited then is late
/// too, whatever read is held; no read held at `STALL_STEPS` is a stuck
/// client.
fn burst_wait(
    cx: &mut Late<'_>,
    node: &mut Node,
    burst: &mut Burst,
    round: Round,
    pending_max: &mut u64,
) -> Verdict<usize> {
    let seed = cx.scenario.seed;
    let turn = inf_runtime::limits::COLD_DELIVERIES_PER_TURN;
    let (index, bound, readers) = (round.index, round.bound, round.readers);
    let held = steady_held_reads(STALL_STEPS, || {
        if let Err(err) = node.step(cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max) {
            bail7d!(cx, "phase-7e wait step: {err}");
        }
        cx.report.scheduler_steps += 1;
        *pending_max = (*pending_max).max(node.plane(burst.owner).cold_pending_for_sim());
        let arrived = burst.collect(cx, node)?;
        if arrived > 0 {
            bail7d!(
                cx,
                "COLD BURST VIOLATION seed {seed:#x}: cold-burst-late: {arrived} replies arrived \
                 past round {index}'s bound of {bound} steps (⌈{readers} / {turn}⌉ + 2)"
            );
        }
        Ok(node.driver(burst.owner).held_reads())
    })?;
    let left = burst.left;
    if held == 0 {
        cx.report.stalled = true;
        bail7d!(
            cx,
            "COLD BURST STALL seed {seed:#x}: {left} readers never answered and the device holds \
             no read of theirs (a stuck client)"
        );
    }
    let unvisited = node.plane(burst.owner).cold_pending_for_sim();
    if unvisited > 0 {
        bail7d!(
            cx,
            "COLD BURST VIOLATION seed {seed:#x}: cold-burst-late: cold-burst-unvisited: \
             {unvisited} members a receipt made ready are unvisited past round {index}'s bound \
             of {bound} steps, {held} reads held"
        );
    }
    Ok(held)
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

/// The listing-repeat class's residue, after phase 8 and before phase 9's
/// `DROP`: [`LISTING_RESIDUE_EXTENTS_PER_CELL`] fresh keys owned by each
/// cell, each `SET` to a value at the blob threshold, so each is one extent
/// file in the namespace's `cold/`, durable before its `+OK` (`always`).
/// A key's cell is its slot's, so the keys are found, not chosen: one is
/// written only while its cell has fewer, and the search stops at eight
/// times the keys the cells need (each cell's share is about one in
/// `cells`). A refused `SET`, or a cell left short, is a violation.
fn listing_residue(cx: &mut Late<'_>, node: &mut Node, audit: &mut MiniClient) -> Verdict {
    // The planted canary writes no residue: every sweep then fits one batch,
    // and the draw's engagement check must read it unengaged.
    if !cx.scenario.listing_repeats || cfg!(inf_canary_listing_residue_unscaled) {
        return Ok(());
    }
    let cells = cx.scenario.cells;
    let want = LISTING_RESIDUE_EXTENTS_PER_CELL;
    let mut owned = vec![0usize; usize::from(cells)];
    let keys_max = 8 * want * usize::from(cells);
    for key_index in 0..keys_max {
        if owned.iter().all(|&count| count >= want) {
            break;
        }
        let key = format!("lr:{key_index}").into_bytes();
        let cell = support::owner_cell(cells, &key);
        if owned[cell] >= want {
            continue;
        }
        let value = value_bytes(b'l', key_index, 0, BLOB_THRESHOLD_BYTES);
        let set: &[&[u8]] = &[b"SET", &key, &value];
        match audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, set) {
            Ok(Some(ok)) if ok == b"+OK\r\n" => {
                owned[cell] += 1;
                cx.report.commands_done += 1;
            }
            other => {
                cx.fail(format!("listing residue SET {key_index} answered {other:?}"));
                return Err(());
            }
        }
    }
    if owned.iter().any(|&count| count < want) {
        cx.fail(format!("listing residue: {keys_max} keys filled {owned:?} of {want} a cell"));
        return Err(());
    }
    Ok(())
}

/// The listing-repeat draw's engagement (ADR-0117 A1.2), read after the
/// post-drop reboot from the disk's own count: a drawn run whose sweep was
/// returned no removed name again ran no `Gone` arm, so it is
/// `listing-repeats-unengaged`, never a green result.
fn check_listing_repeats(cx: &mut Late<'_>) {
    if !cx.scenario.listing_repeats {
        return;
    }
    let served = cx.disk.listing_repeats_served();
    cx.report.listing_repeats_served = served;
    if served == 0 {
        cx.fail(
            "listing-repeats-unengaged: LISTING-REPEAT DRAW VACUOUS: the post-drop reboot's sweep \
             was returned no removed name again"
                .to_string(),
        );
    }
}

/// Whether any cell's device holds a tier read now: the harness's own
/// reading of the driver's queue, no product counter.
fn tier_reads_held(node: &Node, cells: u16) -> bool {
    (0..usize::from(cells)).any(|cell| node.driver(cell).held_reads() > 0)
}

/// GETs the drop race pipelines behind its first, held, read.
const RACE_PIPELINED_KEYS: usize = 29;

/// The hashtag of the drop race's own filler: every key on one cell.
const RACE_FILL_TAG: &str = "p9";

/// Filler batches the drop race writes, at most, until the filler's first
/// key reads cold: 64 × 32 × 2 KiB = 4 MiB on one cell, above the
/// namespace's 3 MiB memory budget, so that key leaves RAM inside the bound
/// (phase 7d cools its keys under the same bound).
const RACE_FILL_BATCHES_MAX: u64 = 64;

/// What a drop-race probe GET met at the device.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Probe {
    /// A tier read of the namespace's files is held at the device: the
    /// probe's own (its key lives in a tier file and its reply waits on the
    /// read) or one MAINTAIN issued while the probe ran.
    Held,
    /// The GET answered with no tier read held.
    Answered,
}

/// Sends `GET key` on `racer` and steps until the device holds a tier read
/// or the GET answers. A held read ends the probe first: the GET's reply,
/// sent or not, is the race's to drain.
fn probe(cx: &mut Late<'_>, node: &mut Node, racer: &mut MiniClient, key: &[u8]) -> Verdict<Probe> {
    cx.report.drop_race_probes += 1;
    racer.send(node, &[b"GET", key]);
    for _ in 0..STALL_STEPS {
        if let Err(err) = node.step(cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max) {
            cx.fail(format!("drop-race probe: {err}"));
            return Err(());
        }
        cx.report.scheduler_steps += 1;
        if tier_reads_held(node, cx.scenario.cells) {
            return Ok(Probe::Held);
        }
        if let Some(reply) = racer.recv(node) {
            if !matches!(reply.first(), Some(b'$' | b'-')) {
                cx.fail(format!("untyped drop-race probe reply: {}", preview(&reply)));
                return Err(());
            }
            return Ok(Probe::Answered);
        }
    }
    cx.report.stalled = true;
    cx.fail(format!("drop-race probe GET {} stalled", preview(key)));
    Err(())
}

/// The drop race's first read: GETs on `racer`, one at a time with every
/// tier read held at the device, until one is held. Phase 2's keys go
/// first, in order; on a seed where every one answers from RAM, a filler
/// on one cell pushes its own first key out of RAM, probed after each
/// batch (the filler leg starts there). `Answered` only past the filler's
/// bound: then no DROP meets a held read and the engagement clause is red.
fn race_first_read(
    cx: &mut Late<'_>,
    node: &mut Node,
    audit: &mut MiniClient,
    racer: &mut MiniClient,
) -> Verdict<Probe> {
    let observed = cx.observed;
    cx.disk.hold_reads(Some(is_tier_file));
    let phase_2_keys = if cx.scenario.drop_race_filler { 0 } else { observed.len() };
    for key in observed.keys().take(phase_2_keys) {
        if probe(cx, node, racer, key)? == Probe::Held {
            return Ok(Probe::Held);
        }
    }
    let first_fill_key = format!("{{{RACE_FILL_TAG}}}fill:0:0").into_bytes();
    for batch in 0..RACE_FILL_BATCHES_MAX {
        cx.disk.hold_reads(None);
        fill_batch(cx, node, audit, RACE_FILL_TAG, batch)?;
        cx.report.drop_race_fill_batches += 1;
        cx.disk.hold_reads(Some(is_tier_file));
        if probe(cx, node, racer, &first_fill_key)? == Probe::Held {
            return Ok(Probe::Held);
        }
    }
    Ok(Probe::Answered)
}

/// Phase 9: the namespace drop race through the wire. Its first GET is one
/// whose tier read the device holds until the DROP is answered, so the drop
/// is applied while a read of the namespace's tier files is queued: the
/// dropped owner keeps each file open until its read ends (the custody
/// oracle). The race's other GETs queue behind it on one connection.
fn phase_9(cx: &mut Late<'_>, node: &mut Node, audit: &mut MiniClient) -> Verdict {
    let mut racer = MiniClient::connect(node, 0);
    let use_ns: &[&[u8]] = &[b"INF.NS", b"USE", NS_NAME];
    match racer.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, use_ns) {
        Ok(Some(reply)) if reply == b"+OK\r\n" => {}
        Ok(None) => {
            cx.report.stalled = true;
            cx.fail("racer USE stalled".to_string());
            return Err(());
        }
        other => {
            cx.fail(format!("racer USE answered {other:?}"));
            return Err(());
        }
    }
    let first = race_first_read(cx, node, audit, &mut racer)?;
    let observed = cx.observed;
    let pipelined = observed.keys().take(RACE_PIPELINED_KEYS);
    let mut in_flight = usize::from(first == Probe::Held);
    for key in pipelined {
        racer.send(node, &[b"GET", key]);
        in_flight += 1;
    }
    // DROP races the pipelined reads from a second connection.
    let drop_ns: &[&[u8]] = &[b"INF.NS", b"DROP", NS_NAME];
    let dropped = audit.call(node, cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max, drop_ns);
    if tier_reads_held(node, cx.scenario.cells) {
        cx.report.drop_reads_held += 1;
    }
    cx.disk.hold_reads(None);
    match dropped {
        Ok(Some(ok)) if ok == b"+OK\r\n" => {}
        other => {
            cx.fail(format!("racing DROP answered {other:?}"));
            return Err(());
        }
    }
    drain_race(cx, node, &mut racer, in_flight)?;
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

/// Every reply of the race must arrive typed: a missing reply is the hang
/// this row exists to catch, a dropped owner's teardown stranding a read.
fn drain_race(
    cx: &mut Late<'_>,
    node: &mut Node,
    racer: &mut MiniClient,
    in_flight: usize,
) -> Verdict {
    let mut answered = 0usize;
    let mut idle = 0u64;
    while answered < in_flight {
        if let Err(err) = node.step(cx.rng, cx.clock, cx.disk, cx.scenario.step_ns_max) {
            cx.fail(format!("drop-race drain: {err}"));
            return Err(());
        }
        cx.report.scheduler_steps += 1;
        let before = answered;
        while answered < in_flight
            && let Some(reply) = racer.recv(node)
        {
            answered += 1;
            match reply.first() {
                Some(b'$') if reply.starts_with(b"$-1") => cx.report.drop_replies_other += 1,
                Some(b'$') => cx.report.drop_replies_value += 1,
                Some(b'-') => cx.report.drop_replies_other += 1,
                _ => cx.fail(format!("untyped drop-race reply: {}", preview(&reply))),
            }
        }
        idle = if answered == before { idle + 1 } else { 0 };
        if idle >= STALL_STEPS {
            cx.report.stalled = true;
            cx.fail(format!(
                "DROP-RACE HANG: {} of {in_flight} pipelined replies never arrived",
                in_flight - answered
            ));
            return Err(());
        }
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
            // A name no other line prints: the run's summary spells
            // `post-drop reboot` on every run, red or green.
            cx.fail(format!("post-drop-reboot-failed — {what}"));
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
    check_listing_repeats(cx);

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
