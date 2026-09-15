//! M4.5-S37 rows: shadow-slot reconciliation and the ticketed-DEL row
//! (ADR-0093).

use super::*;

// ---- M4.5-S37 step 1: the cold-overwrite ceiling (bench-diagnostics arm) ------

/// Keys of the S37 ceiling row unless `--s37-keys` says otherwise:
/// 1 M × 1 KiB ≈ 250 MB per cell against the row's 128 MB tiered budget
/// — a beyond-RAM table where roughly half of every SET's candidates
/// are cold, so the verifying read the ceiling arm skips is on the
/// path of a large share of the leg (the share is measured, not
/// assumed: `cold_reads_issued` on A, `blind_overwrites_ceiling` on B).
pub(super) const S37_DEFAULT_KEYS: u64 = 1_000_000;

/// One S37 leg: the S29 tiered shape (closed loop, pipeline 1, 1 KiB)
/// with the cold-read and blind-overwrite counts of the leg and the
/// shaper's own readings. `cold_read_qd_p99` is the device QD sampled
/// at each issue (ADR-0055 D2) on a whole-session histogram — a later
/// leg's reading carries the earlier legs' samples and is disclosed as
/// such; the queue-full / pool-dry counters are per-leg deltas.
struct S37Leg {
    ops_per_sec: f64,
    p50_us: f64,
    p99_us: f64,
    p999_us: f64,
    sets: u64,
    cold_resolves: u64,
    blind: u64,
    cold_qd_p99: u64,
    cold_read_p99_us: u64,
    cold_queue_full: u64,
    cold_pool_dry: u64,
    /// M4.5-S37 (ADR-0093 D8/D9): the shadow arm's per-leg deltas —
    /// tickets opened, the same-key verdicts, every fallback summed,
    /// stale completions — and the whole-session peaks.
    shadow_created: u64,
    shadow_resolved: u64,
    shadow_fallbacks: u64,
    shadow_stale: u64,
    shadow_pending_peak: u64,
    shadow_pinned_peak: u64,
    shadow_reads_foreground: u64,
}

fn s37_leg(
    port: u16,
    cells: u16,
    conns: usize,
    duration: u64,
    keys: u64,
    raw: &mut String,
) -> Result<S37Leg, String> {
    let before = scrape_cells(port, cells)?;
    let (report, cpu_pct) = s37measure::measured_load(&LoadSpec {
        port,
        conns,
        pipeline: 1,
        duration: Duration::from_secs(duration),
        warmup: Duration::from_secs(2),
        set_weight: 1,
        get_weight: 0,
        keys,
        key_prefix: "s37tier:".into(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s37tier".to_vec()]],
        ..LoadSpec::default()
    })?;
    if report.errors > report.busy_retryable {
        return Err(format!(
            "s37 c{conns}: {} non-BUSY errors (first: {:?})",
            report.errors - report.busy_retryable,
            report.error_samples.first()
        ));
    }
    let after = scrape_cells(port, cells)?;
    raw.push_str(&format!(
        "ops={} errors={} busy_retryable={} generator_cpu_pct={cpu_pct:.1}\n",
        report.ops, report.errors, report.busy_retryable
    ));
    raw.push_str(&format!("INFO before={before:?}\nINFO after={after:?}\n"));
    let d = |f: &str| sum_field(&after, f).saturating_sub(sum_field(&before, f));
    Ok(S37Leg {
        ops_per_sec: report.ops_per_sec,
        p50_us: report.p50_us as f64,
        p99_us: report.p99_us as f64,
        p999_us: report.p999_us as f64,
        sets: report.ops,
        // `cold_reads_issued`, never `tiering_cold_resolves` (that one
        // counts address classifications — candidate probes and retries,
        // ~2.9 per SET in campaign J — not reads).
        cold_resolves: d("cold_reads_issued"),
        blind: d("blind_overwrites_ceiling"),
        cold_qd_p99: crate::gaterun::max_field(&after, "cold_read_qd_p99"),
        cold_read_p99_us: crate::gaterun::max_field(&after, "cold_read_p99_us"),
        cold_queue_full: d("cold_queue_full"),
        cold_pool_dry: d("cold_pool_dry"),
        shadow_created: d("tiering_shadow_created"),
        shadow_resolved: d("tiering_shadow_resolved_same_key")
            + d("tiering_shadow_resolved_collision"),
        shadow_fallbacks: d("tiering_shadow_fallback_off")
            + d("tiering_shadow_fallback_fence")
            + d("tiering_shadow_fallback_multi")
            + d("tiering_shadow_fallback_ticketed")
            + d("tiering_shadow_fallback_tickets")
            + d("tiering_shadow_fallback_pin")
            + d("tiering_shadow_fallback_origin")
            + d("tiering_shadow_fallback_staging"),
        shadow_stale: d("tiering_shadow_stale"),
        shadow_pending_peak: crate::gaterun::max_field(&after, "tiering_shadow_pending_peak"),
        shadow_pinned_peak: crate::gaterun::max_field(&after, "tiering_shadow_pinned_bytes_peak"),
        shadow_reads_foreground: d("tiering_shadow_reads_foreground"),
    })
}

/// An S37 arm: what the server is spawned with beyond the row's fixed
/// flags, what the namespace DDL carries beyond the fixed tier block,
/// and the CONFIG keys set after the fan (ADR-0093 D8: the shadow arm
/// is a hot key on the shipping binary). The first arm of a row is its
/// baseline.
struct S37Arm {
    label: String,
    server_args: Vec<String>,
    ddl_extra: Vec<Vec<u8>>,
    config: Vec<(&'static str, &'static str)>,
}

/// The row's arms. Without `--s37-cold-read-qd`: step 1's ceiling pair
/// (A = the shipping path, B = `--blind-overwrite-ceiling`, a
/// `bench-diagnostics` build). With `--s37-cold-read-qd 64,128,256`:
/// step 2's first discriminator (plan S37, 2026-08-23) — the shipping
/// binary with the ADR-0055 D2 cap widened through the namespace's
/// `COLD-READ-QD` key, the first value the baseline (64 = the default).
/// Memory per cell is the pool underneath the cap: `qd × 16 KiB`
/// (`COLD_POOL_BUF`), disclosed in the row note.
fn s37_arms(flags: &Flags) -> Result<(Vec<S37Arm>, bool), String> {
    // M4.5-S37 step 2 (ADR-0093 D9): the shadow arm — A = the shipping
    // path (knob off), B = `tiered-shadow-overwrite yes`; the same
    // binary, no `bench-diagnostics`.
    if flags.bool("s37-shadow") {
        if flags.get("s37-cold-read-qd").is_some() {
            return Err("--s37-shadow and --s37-cold-read-qd are different rows".into());
        }
        return Ok((
            vec![
                S37Arm {
                    label: "A".into(),
                    server_args: Vec::new(),
                    ddl_extra: Vec::new(),
                    config: vec![("tiered-shadow-overwrite", "no")],
                },
                S37Arm {
                    label: "B".into(),
                    server_args: Vec::new(),
                    ddl_extra: Vec::new(),
                    config: vec![("tiered-shadow-overwrite", "yes")],
                },
            ],
            false,
        ));
    }
    let Some(list) = flags.get("s37-cold-read-qd") else {
        return Ok((
            vec![
                S37Arm {
                    label: "A".into(),
                    server_args: Vec::new(),
                    ddl_extra: Vec::new(),
                    config: Vec::new(),
                },
                S37Arm {
                    label: "B".into(),
                    server_args: vec!["--blind-overwrite-ceiling".into()],
                    ddl_extra: Vec::new(),
                    config: Vec::new(),
                },
            ],
            true,
        ));
    };
    let mut arms = Vec::new();
    for item in list.split(',') {
        let qd: u16 = item.trim().parse().map_err(|e| format!("--s37-cold-read-qd {item}: {e}"))?;
        if qd == 0 {
            return Err("--s37-cold-read-qd: a queue depth of 0 is refused by the server".into());
        }
        arms.push(S37Arm {
            label: format!("qd{qd}"),
            server_args: Vec::new(),
            ddl_extra: vec![b"COLD-READ-QD".to_vec(), qd.to_string().into_bytes()],
            config: Vec::new(),
        });
    }
    if arms.len() < 2 {
        return Err("--s37-cold-read-qd wants at least two values (baseline first)".into());
    }
    Ok((arms, false))
}

/// The M4.5-S37 row: the beyond-RAM tiered `always` write legs (64 and
/// 256 conns) on every arm, each replicate rotating the arm order (two
/// arms = ABBA), fresh server + fill per leg. **Step 1** (no
/// `--s37-cold-read-qd`): B is an upper bound from an unsound build —
/// its gain is what removing the verifying cold read could ever buy;
/// the predeclared rule (plan S37) reads "< 15 % throughput and < 20 %
/// p99 ⇒ step 2 `Rejected`". **Step 2's discriminator**
/// (`--s37-cold-read-qd`): the same legs with the shaper's cap widened —
/// if the widest cap recovers most of A's gap to the ceiling, the read's
/// cost is queueing in the cap (optimize the shaper, no shadow slots);
/// if it barely moves, the read itself is the cost (shadow slots,
/// ADR-first). The baseline's `cold_read_qd_p99` says whether the cap
/// bound at all (≈ the cap = saturated).
pub(super) fn s37_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let keys = flags.u64_or("s37-keys", S37_DEFAULT_KEYS)?;
    let idle_s = flags.u64_or("leg-idle-s", 0)?;
    let controls_enabled = flags.bool("s37-controls");
    if controls_enabled && (!flags.bool("s37-shadow") || !flags.bool("read-leg-fill")) {
        return Err("--s37-controls requires --s37-shadow --read-leg-fill".into());
    }
    let mut controls = Vec::new();
    let (arms, ceiling) = s37_arms(flags)?;
    let labels: Vec<&str> = arms.iter().map(|a| a.label.as_str()).collect();
    if ceiling {
        m.note(format!(
            "s37 row: {cells} cells · {replicates} replicates (ABBA) · tiered always, \
             MEM-BUDGET {MEM_BUDGET}/cell, {keys} keys × 1 KiB filled per leg, then 100 % SET \
             closed-loop pipeline 1 at {CONNS_LOW} and {CONNS_HIGH} conns for {duration} s · \
             arm B = --blind-overwrite-ceiling (unsound ceiling instrument; bench-diagnostics \
             build)"
        ));
    } else if flags.bool("s37-shadow") {
        m.note(format!(
            "s37 row (shadow-slot arm, ADR-0093 D9): {cells} cells · {replicates} replicates \
             (ABBA) · tiered always, MEM-BUDGET {MEM_BUDGET}/cell, {keys} keys × 1 KiB filled \
             per leg, then 100 % SET closed-loop pipeline 1 at {CONNS_LOW} and {CONNS_HIGH} \
             conns for {duration} s · A = tiered-shadow-overwrite no (the shipping path), B = \
             yes — the same binary; per-leg shadow counters on the raw line"
        ));
    } else {
        m.note(format!(
            "s37 row (step 2 discriminator): {cells} cells · {replicates} replicates (arm order \
             rotated per replicate) · tiered always, MEM-BUDGET {MEM_BUDGET}/cell, {keys} keys × \
             1 KiB filled per leg, then 100 % SET closed-loop pipeline 1 at {CONNS_LOW} and \
             {CONNS_HIGH} conns for {duration} s · arms = COLD-READ-QD {} on the shipping \
             binary (baseline first; ADR-0055 D2 cap, pool = qd × 16 KiB per cell)",
            labels.join(", ")
        ));
    }
    let mut raw = String::new();
    let mut legs: Vec<(usize, String, usize, S37Leg)> = Vec::new();
    for rep in 0..replicates {
        for slot in 0..arms.len() {
            let arm = &arms[(rep + slot) % arms.len()];
            let dir = format!("{data_root}/s37-{}-rep{rep}", arm.label);
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
            crate::m2rows::copy_probe_file(flags, std::path::Path::new(&dir))?;
            let mut extra: Vec<String> = vec!["--data-dir".into(), dir.clone()];
            if let Some(pin) = flags.get("pin-start") {
                extra.push("--pin-start".into());
                extra.push(pin.to_string());
            }
            extra.extend(crate::m2rows::pipeline_args(flags));
            extra.extend(arm.server_args.iter().cloned());
            let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
            s35_idle(idle_s, &format!("s37 {} rep{rep}", arm.label));
            let server = spawn_infinityd(infinityd, cells, &extra_refs)?;
            let port = server.port;
            let mut ddl: Vec<&[u8]> = vec![
                b"INF.NS",
                b"CREATE",
                b"s37tier",
                b"MODE",
                b"durable",
                b"FSYNC",
                b"always",
                b"MEM-BUDGET",
                MEM_BUDGET.as_bytes(),
                b"DISK-BUDGET",
                b"10gb",
                b"TIER-IO-MODE",
                b"direct",
            ];
            ddl.extend(arm.ddl_extra.iter().map(Vec::as_slice));
            create_ns(port, &ddl)?;
            await_fan(port, "s37tier", cells)?;
            for (key, value) in &arm.config {
                config_set(port, key, value)?;
            }
            if ceiling {
                let infos = scrape_cells(port, cells)?;
                if !infos.iter().all(|c| c.contains_key("blind_overwrites_ceiling")) {
                    return Err("s37: INFO has no blind_overwrites_ceiling — the binary is not a \
                                bench-diagnostics build (cargo build --release --features \
                                bench-diagnostics -p infinityd)"
                        .into());
                }
            }
            let fill = run_load(&LoadSpec {
                port,
                conns: 64,
                pipeline: 4,
                fill: Some(keys),
                keys,
                key_prefix: "s37tier:".into(),
                value_size: 1024,
                setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s37tier".to_vec()]],
                ..LoadSpec::default()
            })?;
            if fill.errors > 0 {
                return Err(format!("s37 {} rep{rep} fill: {} errors", arm.label, fill.errors));
            }
            for conns in [CONNS_LOW, CONNS_HIGH] {
                s35_idle(idle_s, &format!("s37 {} rep{rep} c{conns} post-fill", arm.label));
                raw.push_str(&format!("rep{rep} {} c{conns} counters\n", arm.label));
                let leg = s37_leg(port, cells, conns, duration, keys, &mut raw)?;
                raw.push_str(&format!(
                    "rep{rep} {:<5} c{conns:<3} ops/s={:<8.0} p50_us={:<6.0} p99_us={:<7.0} \
                     p999_us={:<7.0} sets={} cold_resolves={} ({:.3}/set) blind={} ({:.3}/set) \
                     cold_qd_p99={} cold_read_p99_us={} cold_queue_full={} cold_pool_dry={} \
                     shadow[created={} ({:.3}/set) resolved={} fallbacks={} ({:.3}/set) \
                     stale={} pending_peak={} pinned_peak={} reads_fg={}]\n",
                    arm.label,
                    leg.ops_per_sec,
                    leg.p50_us,
                    leg.p99_us,
                    leg.p999_us,
                    leg.sets,
                    leg.cold_resolves,
                    leg.cold_resolves as f64 / leg.sets.max(1) as f64,
                    leg.blind,
                    leg.blind as f64 / leg.sets.max(1) as f64,
                    leg.cold_qd_p99,
                    leg.cold_read_p99_us,
                    leg.cold_queue_full,
                    leg.cold_pool_dry,
                    leg.shadow_created,
                    leg.shadow_created as f64 / leg.sets.max(1) as f64,
                    leg.shadow_resolved,
                    leg.shadow_fallbacks,
                    leg.shadow_fallbacks as f64 / leg.sets.max(1) as f64,
                    leg.shadow_stale,
                    leg.shadow_pending_peak,
                    leg.shadow_pinned_peak,
                    leg.shadow_reads_foreground,
                ));
                println!("  s37 {}", raw.lines().last().unwrap_or(""));
                legs.push((rep, arm.label.clone(), conns, leg));
            }
            if controls_enabled {
                raw.push_str(&format!("rep{rep} {} D9 controls\n", arm.label));
                let control = s37measure::controls(port, cells, duration, keys, idle_s, &mut raw)?;
                let tiered_ops = legs.last().expect("c256 leg was measured").3.ops_per_sec;
                controls.push((arm.label.clone(), tiered_ops, control));
            }
            drop(server);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    let find = |rep: usize, arm: &str, conns: usize| {
        legs.iter().find(|(r, a, c, _)| *r == rep && a == arm && *c == conns).map(|(.., l)| l)
    };
    if controls_enabled {
        s37measure::summarize_controls(&controls, m);
    }
    let key = |name: String| -> &'static str { Box::leak(name.into_boxed_str()) };
    for conns in [CONNS_LOW, CONNS_HIGH] {
        let tag: &'static str = if conns == CONNS_LOW { "c64" } else { "c256" };
        if ceiling {
            let mut ops_x = Vec::new();
            let mut p50_gain = Vec::new();
            let mut p99_gain = Vec::new();
            let mut blind_share = Vec::new();
            let mut cold_share = Vec::new();
            for rep in 0..replicates {
                let (Some(a), Some(b)) = (find(rep, "A", conns), find(rep, "B", conns)) else {
                    continue;
                };
                ops_x.push(b.ops_per_sec / a.ops_per_sec.max(1.0));
                p50_gain.push(a.p50_us / b.p50_us.max(1.0));
                p99_gain.push(a.p99_us / b.p99_us.max(1.0));
                blind_share.push(b.blind as f64 / b.sets.max(1) as f64);
                cold_share.push(a.cold_resolves as f64 / a.sets.max(1) as f64);
            }
            if ops_x.is_empty() {
                continue;
            }
            m.set(key(format!("s37:ceiling_ops_x_{tag}")), median(&mut ops_x));
            m.set(key(format!("s37:ceiling_p50_gain_x_{tag}")), median(&mut p50_gain));
            m.set(key(format!("s37:ceiling_p99_gain_x_{tag}")), median(&mut p99_gain));
            m.set(key(format!("s37:blind_share_arm_b_{tag}")), median(&mut blind_share));
            m.set(key(format!("s37:cold_resolve_share_arm_a_{tag}")), median(&mut cold_share));
            continue;
        }
        // The shadow arm (ADR-0093 D9): B over A per replicate, medians,
        // plus B's own engagement — tickets per SET, the fallback share
        // (the D9 falsifier at 0.5), stale completions, the pinned peak.
        if flags.bool("s37-shadow") {
            let mut ops_x = Vec::new();
            let mut p50_x = Vec::new();
            let mut p99_x = Vec::new();
            let mut created = Vec::new();
            let mut fallback = Vec::new();
            let mut stale = Vec::new();
            let mut pinned = Vec::new();
            let mut cold_a = Vec::new();
            for rep in 0..replicates {
                let (Some(a), Some(b)) = (find(rep, "A", conns), find(rep, "B", conns)) else {
                    continue;
                };
                ops_x.push(b.ops_per_sec / a.ops_per_sec.max(1.0));
                p50_x.push(b.p50_us / a.p50_us.max(1.0));
                p99_x.push(b.p99_us / a.p99_us.max(1.0));
                created.push(b.shadow_created as f64 / b.sets.max(1) as f64);
                fallback.push(b.shadow_fallbacks as f64 / b.sets.max(1) as f64);
                stale.push(b.shadow_stale as f64 / b.shadow_resolved.max(1) as f64);
                pinned.push(b.shadow_pinned_peak as f64);
                cold_a.push(a.cold_resolves as f64 / a.sets.max(1) as f64);
            }
            if ops_x.is_empty() {
                continue;
            }
            let (ops, p50, p99) = (median(&mut ops_x), median(&mut p50_x), median(&mut p99_x));
            m.set(key(format!("s37:shadow_ops_x_{tag}")), ops);
            m.set(key(format!("s37:shadow_p50_x_{tag}")), p50);
            m.set(key(format!("s37:shadow_p99_x_{tag}")), p99);
            m.set(key(format!("s37:shadow_created_share_b_{tag}")), median(&mut created));
            m.set(key(format!("s37:shadow_fallback_share_b_{tag}")), median(&mut fallback));
            m.set(key(format!("s37:shadow_stale_share_b_{tag}")), median(&mut stale));
            m.set(key(format!("s37:shadow_pinned_peak_bytes_b_{tag}")), median(&mut pinned));
            m.note(format!(
                "s37 {tag} shadow B vs A: ops {ops:.3} × · p50 {p50:.3} × · p99 {p99:.3} × · \
                 tickets/SET {:.3} · fallbacks/SET {:.3} · stale/resolved {:.3} · pinned peak \
                 {:.0} B · A's cold reads/SET {:.3} (medians of {} pairs)",
                median(&mut created.clone()),
                median(&mut fallback.clone()),
                median(&mut stale.clone()),
                median(&mut pinned.clone()),
                median(&mut cold_a),
                ops_x.len()
            ));
            continue;
        }
        // The discriminator: every wider cap against the baseline cap,
        // per replicate, medians; the widest cap also under the fixed
        // keys the gate rows read.
        let base = labels[0];
        let mut base_qd: Vec<f64> = Vec::new();
        for rep in 0..replicates {
            if let Some(b) = find(rep, base, conns) {
                base_qd.push(b.cold_qd_p99 as f64);
            }
        }
        if !base_qd.is_empty() {
            m.set(key(format!("s37:qd_base_cold_qd_p99_{tag}")), median(&mut base_qd));
        }
        for (i, arm) in labels.iter().enumerate().skip(1) {
            let mut ops_x = Vec::new();
            let mut p50_x = Vec::new();
            let mut p99_x = Vec::new();
            let mut arm_qd = Vec::new();
            let mut arm_read_p99 = Vec::new();
            let mut queue_full = Vec::new();
            let mut pool_dry = Vec::new();
            for rep in 0..replicates {
                let (Some(b), Some(a)) = (find(rep, base, conns), find(rep, arm, conns)) else {
                    continue;
                };
                ops_x.push(a.ops_per_sec / b.ops_per_sec.max(1.0));
                p50_x.push(a.p50_us / b.p50_us.max(1.0));
                p99_x.push(a.p99_us / b.p99_us.max(1.0));
                arm_qd.push(a.cold_qd_p99 as f64);
                arm_read_p99.push(a.cold_read_p99_us as f64);
                queue_full.push(a.cold_queue_full as f64);
                pool_dry.push(a.cold_pool_dry as f64);
            }
            if ops_x.is_empty() {
                continue;
            }
            let (ops, p50, p99) = (median(&mut ops_x), median(&mut p50_x), median(&mut p99_x));
            m.set(key(format!("s37:{arm}_ops_x_{tag}")), ops);
            m.set(key(format!("s37:{arm}_p50_x_{tag}")), p50);
            m.set(key(format!("s37:{arm}_p99_x_{tag}")), p99);
            m.note(format!(
                "s37 {tag} {arm} vs {base}: ops {ops:.3} × · p50 {p50:.3} × · p99 {p99:.3} × · \
                 cold_read_qd_p99 {:.0} (base {:.0}) · cold_read_p99_us {:.0} · queue_full {:.0} \
                 · pool_dry {:.0} (medians of {} pairs)",
                median(&mut arm_qd),
                median(&mut base_qd.clone()),
                median(&mut arm_read_p99),
                median(&mut queue_full),
                median(&mut pool_dry),
                ops_x.len()
            ));
            if i == labels.len() - 1 {
                m.set(key(format!("s37:qd_wide_ops_x_{tag}")), ops);
                m.set(key(format!("s37:qd_wide_p99_x_{tag}")), p99);
            }
        }
    }
    m.row_open(if ceiling {
        "cold-overwrite-ceiling"
    } else if flags.bool("s37-shadow") {
        "shadow-slot-arm"
    } else {
        "cold-read-qd-discriminator"
    });
    if flags.bool("s37-shadow") {
        m.row_write_amp(
            "S37 step 2 (ADR-0093 D9): the shadow arm B (tiered-shadow-overwrite yes) over \
             the shipping path A on the beyond-RAM tiered always write legs — the same binary; \
             the reconciler still pays the read off the critical path, so B's ceiling is the \
             device's read rate, not the blind arm's; `shadow_fallback_share_b` above 0.5 is \
             the predeclared falsifier (bimodal by construction); not a write-amplification row",
        );
    } else if ceiling {
        m.row_write_amp(
            "S37 step 1 (plan rule): B ÷ A throughput and A ÷ B p99 on the beyond-RAM tiered \
             always write legs; B is an UNSOUND upper bound (the cold record is orphaned) — \
             \"< 15 % throughput and < 20 % p99 ⇒ step 2 Rejected\"; `blind_share_arm_b` is the \
             share of B's SETs that skipped a cold read (0 = the instrument never engaged), \
             `cold_resolve_share_arm_a` the share of A's SETs that paid one",
        );
    } else {
        m.row_write_amp(
            "S37 step 2 discriminator (plan S37, 2026-08-23): each wider COLD-READ-QD arm over \
             the baseline cap on the beyond-RAM tiered always write legs — `qd_wide_ops_x` \
             against the ceiling's gap decides queueing (shaper) vs the read (shadow slots); \
             `qd_base_cold_qd_p99` ≈ the cap means the cap bound; a p99 ratio above 1.1 is a \
             tail cost the wider cap charges; not a write-amplification row",
        );
    }
    m.raw_section("s37 per-leg samples", &raw);
    Ok(())
}

// ---- M4.5-S37: the ticketed-DEL RSS/tail row (ADR-0093 A13, batch 30) ------

/// Keys per SET/DEL window of the ticketed-`DEL` row unless
/// `--s37-del-keys` says otherwise: 3 072 per cell at four cells — under
/// `SHADOW_TICKETS_CAP` (4 096) so every cold key of the window opens a
/// ticket on arm B instead of falling back at the cap, and ≈ 3 MiB of
/// pinned suffix per cell against the 16 MiB pin cap.
pub(super) const S37_DEL_DEFAULT_KEYS: u64 = 12_288;
/// SET/DEL cycles per leg (each on a fresh key window) unless
/// `--s37-del-cycles` says otherwise: four windows = 49 152 `DEL`s per
/// leg, enough samples for a p99.9 that is not one request.
pub(super) const S37_DEL_DEFAULT_CYCLES: u64 = 4;

/// One SET/DEL cycle of the ticketed-`DEL` row: the window's SET pass
/// (tickets opened on B — the reconciler is paused so they stay open)
/// and its DEL pass (every ticketed winner walks its ticket: the twin's
/// Foreground read, the markers, the delete), with the process RSS
/// sampled through the DEL pass.
struct S37DelCycle {
    sets: u64,
    tickets: u64,
    set_fallbacks: u64,
    dels: u64,
    forced: u64,
    refused: u64,
    pending_after: u64,
    reads_fg: u64,
    del_ops_per_sec: f64,
    del_p50_us: f64,
    del_p99_us: f64,
    del_p999_us: f64,
    del_max_us: u64,
    rss_before_del: u64,
    rss_peak_del: u64,
    rss_after_del: u64,
}

fn s37_del_cycle(
    port: u16,
    pid: u32,
    cells: u16,
    from: u64,
    keys: u64,
) -> Result<S37DelCycle, String> {
    let window = |op: crate::load::FillOp| LoadSpec {
        port,
        conns: CONNS_LOW,
        pipeline: 1,
        fill: Some(keys),
        fill_from: from,
        fill_op: op,
        keys,
        key_prefix: "s37tier:".into(),
        value_size: 1024,
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s37tier".to_vec()]],
        ..LoadSpec::default()
    };
    let before = scrape_cells(port, cells)?;
    let set = run_load(&window(crate::load::FillOp::Set))?;
    if set.errors > 0 {
        return Err(format!("s37 ticketed-DEL SET window @{from}: {} errors", set.errors));
    }
    let mid = scrape_cells(port, cells)?;
    let d_set = |f: &str| sum_field(&mid, f).saturating_sub(sum_field(&before, f));
    let rss_before_del = crate::gaterun::rss_bytes_of(pid);
    let stop = std::sync::atomic::AtomicBool::new(false);
    let peak = std::sync::atomic::AtomicU64::new(rss_before_del);
    let del = std::thread::scope(|scope| {
        scope.spawn(|| {
            while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                peak.fetch_max(
                    crate::gaterun::rss_bytes_of(pid),
                    std::sync::atomic::Ordering::Relaxed,
                );
                #[allow(clippy::disallowed_methods)] // bench sampler, not cell code
                std::thread::sleep(Duration::from_millis(20));
            }
        });
        let del = run_load(&window(crate::load::FillOp::Del));
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        del
    })?;
    if del.errors > del.busy_retryable {
        return Err(format!(
            "s37 ticketed-DEL DEL window @{from}: {} non-BUSY errors (first: {:?})",
            del.errors - del.busy_retryable,
            del.error_samples.first()
        ));
    }
    let rss_after_del = crate::gaterun::rss_bytes_of(pid);
    let after = scrape_cells(port, cells)?;
    let d_del = |f: &str| sum_field(&after, f).saturating_sub(sum_field(&mid, f));
    Ok(S37DelCycle {
        sets: set.ops,
        tickets: d_set("tiering_shadow_created"),
        set_fallbacks: d_set("tiering_shadow_fallback_off")
            + d_set("tiering_shadow_fallback_fence")
            + d_set("tiering_shadow_fallback_multi")
            + d_set("tiering_shadow_fallback_ticketed")
            + d_set("tiering_shadow_fallback_tickets")
            + d_set("tiering_shadow_fallback_pin")
            + d_set("tiering_shadow_fallback_origin"),
        dels: del.ops,
        forced: d_del("tiering_shadow_forced_by_delete"),
        refused: d_del("tiering_shadow_delete_run_refused"),
        pending_after: sum_field(&after, "tiering_shadow_pending"),
        reads_fg: d_del("tiering_shadow_reads_foreground"),
        del_ops_per_sec: del.ops_per_sec,
        del_p50_us: del.p50_us as f64,
        del_p99_us: del.p99_us as f64,
        del_p999_us: del.p999_us as f64,
        del_max_us: del.max_us,
        rss_before_del,
        rss_peak_del: peak.load(std::sync::atomic::Ordering::Relaxed),
        rss_after_del,
    })
}

/// One leg of the ticketed-`DEL` row, folded over its cycles: medians
/// of the per-cycle DEL readings, the worst RSS growth through any DEL
/// pass, the coverage sums.
struct S37DelLeg {
    cycles: usize,
    sets: u64,
    tickets: u64,
    set_fallbacks: u64,
    dels: u64,
    forced: u64,
    refused: u64,
    pending_after: u64,
    reads_fg: u64,
    del_ops_per_sec: f64,
    del_p50_us: f64,
    del_p99_us: f64,
    del_p999_us: f64,
    del_max_us: u64,
    /// Worst `peak − before` over the DEL passes.
    del_rss_growth: u64,
    /// `after the last DEL − before the first SET`.
    rss_end_delta: i64,
}

/// The M4.5-S37 ticketed-`DEL` RSS/tail row (ADR-0093 A13 — the
/// evidence batch 29 named): A = the shipping path, B =
/// `tiered-shadow-overwrite yes` with `tiered-shadow-reconcile no`, so
/// every cold key the SET window meets opens a ticket that stays open
/// until its `DEL` walks it (the twin's Foreground read + its own
/// marker) — the walk's memory and tail isolated from the reconciler's
/// cadence. Fresh server + 1 M-key fill per leg (the knob after the
/// fill), then `--s37-del-cycles` windows of `--s37-del-keys` keys: SET
/// the window (pipeline 1, 64 conns), DEL the window (same), RSS sampled
/// through the DEL. ABBA across replicates. Every reading is
/// informational — the row records, the D9 campaign decides.
pub(super) fn s37_ticketed_del_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let keys = flags.u64_or("s37-keys", S37_DEFAULT_KEYS)?;
    let del_keys = flags.u64_or("s37-del-keys", S37_DEL_DEFAULT_KEYS)?;
    let cycles = flags.u64_or("s37-del-cycles", S37_DEL_DEFAULT_CYCLES)?;
    if del_keys == 0 || cycles == 0 || del_keys * cycles > keys {
        return Err(format!(
            "--s37-del-keys {del_keys} × --s37-del-cycles {cycles} must be > 0 and ≤ --s37-keys \
             {keys}"
        ));
    }
    let idle_s = flags.u64_or("leg-idle-s", 0)?;
    let arms: [(&str, Vec<(&str, &str)>); 2] = [
        ("A", vec![("tiered-shadow-overwrite", "no")]),
        ("B", vec![("tiered-shadow-overwrite", "yes"), ("tiered-shadow-reconcile", "no")]),
    ];
    m.note(format!(
        "s37 ticketed-DEL row (ADR-0093 A13): {cells} cells · {replicates} replicates (ABBA) · \
         tiered always, MEM-BUDGET {MEM_BUDGET}/cell, {keys} keys × 1 KiB filled per leg, then \
         {cycles} cycles × {del_keys} keys: SET the window (tickets on B), DEL the window \
         (pipeline 1, {CONNS_LOW} conns), VmRSS sampled every 20 ms through the DEL · A = \
         tiered-shadow-overwrite no, B = yes + tiered-shadow-reconcile no (tickets held open \
         until DEL) — the same binary; per-cycle readings on the raw line"
    ));
    let mut raw = String::new();
    let mut legs: Vec<(usize, String, S37DelLeg)> = Vec::new();
    for rep in 0..replicates {
        for slot in 0..arms.len() {
            let (label, config) = &arms[(rep + slot) % arms.len()];
            let dir = format!("{data_root}/s37-tdel-{label}-rep{rep}");
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
            crate::m2rows::copy_probe_file(flags, std::path::Path::new(&dir))?;
            let mut extra: Vec<String> = vec!["--data-dir".into(), dir.clone()];
            if let Some(pin) = flags.get("pin-start") {
                extra.push("--pin-start".into());
                extra.push(pin.to_string());
            }
            extra.extend(crate::m2rows::pipeline_args(flags));
            let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
            s35_idle(idle_s, &format!("s37 ticketed-DEL {label} rep{rep}"));
            let server = spawn_infinityd(infinityd, cells, &extra_refs)?;
            let (port, pid) = (server.port, server.pid());
            let ddl: Vec<&[u8]> = vec![
                b"INF.NS",
                b"CREATE",
                b"s37tier",
                b"MODE",
                b"durable",
                b"FSYNC",
                b"always",
                b"MEM-BUDGET",
                MEM_BUDGET.as_bytes(),
                b"DISK-BUDGET",
                b"10gb",
                b"TIER-IO-MODE",
                b"direct",
            ];
            create_ns(port, &ddl)?;
            await_fan(port, "s37tier", cells)?;
            let fill = run_load(&LoadSpec {
                port,
                conns: 64,
                pipeline: 4,
                fill: Some(keys),
                keys,
                key_prefix: "s37tier:".into(),
                value_size: 1024,
                setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s37tier".to_vec()]],
                ..LoadSpec::default()
            })?;
            if fill.errors > 0 {
                return Err(format!(
                    "s37 ticketed-DEL {label} rep{rep} fill: {} errors",
                    fill.errors
                ));
            }
            s35_idle(idle_s, &format!("s37 ticketed-DEL {label} rep{rep} post-fill"));
            for (key, value) in config {
                config_set(port, key, value)?;
            }
            let rss_base = crate::gaterun::rss_bytes_of(pid);
            let mut rows: Vec<S37DelCycle> = Vec::new();
            for cycle in 0..cycles {
                let from = cycle * del_keys;
                let c = s37_del_cycle(port, pid, cells, from, del_keys)?;
                raw.push_str(&format!(
                    "rep{rep} {label} cycle{cycle} keys@{from}: sets={} tickets={} ({:.3}/set) \
                     set_fallbacks={} dels={} forced={} ({:.3}/del) refused={} pending_after={} \
                     reads_fg={} del_ops/s={:.0} del_p50_us={:.0} del_p99_us={:.0} \
                     del_p999_us={:.0} del_max_us={} rss_before_del={} rss_peak_del={} \
                     rss_after_del={} (growth {})\n",
                    c.sets,
                    c.tickets,
                    c.tickets as f64 / c.sets.max(1) as f64,
                    c.set_fallbacks,
                    c.dels,
                    c.forced,
                    c.forced as f64 / c.dels.max(1) as f64,
                    c.refused,
                    c.pending_after,
                    c.reads_fg,
                    c.del_ops_per_sec,
                    c.del_p50_us,
                    c.del_p99_us,
                    c.del_p999_us,
                    c.del_max_us,
                    c.rss_before_del,
                    c.rss_peak_del,
                    c.rss_after_del,
                    c.rss_peak_del.saturating_sub(c.rss_before_del),
                ));
                println!("  s37 {}", raw.lines().last().unwrap_or(""));
                rows.push(c);
            }
            let rss_end = crate::gaterun::rss_bytes_of(pid);
            let med = |f: &dyn Fn(&S37DelCycle) -> f64| {
                let mut v: Vec<f64> = rows.iter().map(f).collect();
                median(&mut v)
            };
            let leg = S37DelLeg {
                cycles: rows.len(),
                sets: rows.iter().map(|c| c.sets).sum(),
                tickets: rows.iter().map(|c| c.tickets).sum(),
                set_fallbacks: rows.iter().map(|c| c.set_fallbacks).sum(),
                dels: rows.iter().map(|c| c.dels).sum(),
                forced: rows.iter().map(|c| c.forced).sum(),
                refused: rows.iter().map(|c| c.refused).sum(),
                pending_after: rows.last().map_or(0, |c| c.pending_after),
                reads_fg: rows.iter().map(|c| c.reads_fg).sum(),
                del_ops_per_sec: med(&|c| c.del_ops_per_sec),
                del_p50_us: med(&|c| c.del_p50_us),
                del_p99_us: med(&|c| c.del_p99_us),
                del_p999_us: med(&|c| c.del_p999_us),
                del_max_us: rows.iter().map(|c| c.del_max_us).max().unwrap_or(0),
                del_rss_growth: rows
                    .iter()
                    .map(|c| c.rss_peak_del.saturating_sub(c.rss_before_del))
                    .max()
                    .unwrap_or(0),
                rss_end_delta: rss_end as i64 - rss_base as i64,
            };
            raw.push_str(&format!(
                "rep{rep} {label} leg: cycles={} sets={} tickets={} set_fallbacks={} dels={} \
                 forced={} ({:.3}/del) refused={} pending_after={} reads_fg={} del_ops/s={:.0} \
                 del_p50_us={:.0} del_p99_us={:.0} del_p999_us={:.0} del_max_us={} \
                 del_rss_growth={} rss_base={} rss_end={} (delta {})\n",
                leg.cycles,
                leg.sets,
                leg.tickets,
                leg.set_fallbacks,
                leg.dels,
                leg.forced,
                leg.forced as f64 / leg.dels.max(1) as f64,
                leg.refused,
                leg.pending_after,
                leg.reads_fg,
                leg.del_ops_per_sec,
                leg.del_p50_us,
                leg.del_p99_us,
                leg.del_p999_us,
                leg.del_max_us,
                leg.del_rss_growth,
                rss_base,
                rss_end,
                leg.rss_end_delta,
            ));
            println!("  s37 {}", raw.lines().last().unwrap_or(""));
            legs.push((rep, (*label).to_string(), leg));
            drop(server);
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
    let find = |rep: usize, arm: &str| {
        legs.iter().find(|(r, a, _)| *r == rep && a == arm).map(|(.., l)| l)
    };
    let mut ops_x = Vec::new();
    let mut p50_x = Vec::new();
    let mut p99_x = Vec::new();
    let mut p999_x = Vec::new();
    let mut p99_a = Vec::new();
    let mut p99_b = Vec::new();
    let mut p999_b = Vec::new();
    let mut max_b = Vec::new();
    let mut share_b = Vec::new();
    let mut share_a = Vec::new();
    let mut tickets_b = Vec::new();
    let mut refused_b = Vec::new();
    let mut growth_a = Vec::new();
    let mut growth_b = Vec::new();
    let mut end_a = Vec::new();
    let mut end_b = Vec::new();
    for rep in 0..replicates {
        let (Some(a), Some(b)) = (find(rep, "A"), find(rep, "B")) else {
            continue;
        };
        ops_x.push(b.del_ops_per_sec / a.del_ops_per_sec.max(1.0));
        p50_x.push(b.del_p50_us / a.del_p50_us.max(1.0));
        p99_x.push(b.del_p99_us / a.del_p99_us.max(1.0));
        p999_x.push(b.del_p999_us / a.del_p999_us.max(1.0));
        p99_a.push(a.del_p99_us);
        p99_b.push(b.del_p99_us);
        p999_b.push(b.del_p999_us);
        max_b.push(b.del_max_us as f64);
        share_b.push(b.forced as f64 / b.dels.max(1) as f64);
        share_a.push(a.forced as f64 / a.dels.max(1) as f64);
        tickets_b.push(b.tickets as f64 / b.sets.max(1) as f64);
        refused_b.push(b.refused as f64);
        growth_a.push(a.del_rss_growth as f64);
        growth_b.push(b.del_rss_growth as f64);
        end_a.push(a.rss_end_delta as f64);
        end_b.push(b.rss_end_delta as f64);
    }
    if ops_x.is_empty() {
        return Err("s37 ticketed-DEL: no A/B pair completed".into());
    }
    let pairs = ops_x.len();
    let (ops, p50, p99, p999) =
        (median(&mut ops_x), median(&mut p50_x), median(&mut p99_x), median(&mut p999_x));
    m.set("s37:tdel_ops_x_c64", ops);
    m.set("s37:tdel_p50_x_c64", p50);
    m.set("s37:tdel_p99_x_c64", p99);
    m.set("s37:tdel_p999_x_c64", p999);
    m.set("s37:tdel_p99_us_a_c64", median(&mut p99_a));
    m.set("s37:tdel_p99_us_b_c64", median(&mut p99_b));
    m.set("s37:tdel_p999_us_b_c64", median(&mut p999_b));
    m.set("s37:tdel_max_us_b_c64", median(&mut max_b));
    m.set("s37:tdel_ticketed_share_b", median(&mut share_b));
    m.set("s37:tdel_ticketed_share_a", median(&mut share_a));
    m.set("s37:tdel_tickets_per_set_b", median(&mut tickets_b));
    m.set("s37:tdel_refused_b", median(&mut refused_b));
    m.set("s37:tdel_rss_growth_bytes_a", median(&mut growth_a));
    m.set("s37:tdel_rss_growth_bytes_b", median(&mut growth_b));
    m.set("s37:tdel_rss_end_delta_bytes_a", median(&mut end_a));
    m.set("s37:tdel_rss_end_delta_bytes_b", median(&mut end_b));
    m.note(format!(
        "s37 ticketed-DEL B vs A (c64, pipeline 1): DEL ops {ops:.3} × · p50 {p50:.3} × · p99 \
         {p99:.3} × · p99.9 {p999:.3} × · B p99 {:.0} µs / p99.9 {:.0} µs / max {:.0} µs · \
         ticketed share B {:.3} (A {:.3}) · tickets/SET B {:.3} · refused B {:.0} · RSS growth \
         through a DEL pass A {:.0} B / B {:.0} B · RSS end−base A {:.0} B / B {:.0} B (medians \
         of {pairs} pairs; per-leg = medians over cycles, growth = worst cycle)",
        median(&mut p99_b.clone()),
        median(&mut p999_b.clone()),
        median(&mut max_b.clone()),
        median(&mut share_b.clone()),
        median(&mut share_a.clone()),
        median(&mut tickets_b.clone()),
        median(&mut refused_b.clone()),
        median(&mut growth_a.clone()),
        median(&mut growth_b.clone()),
        median(&mut end_a.clone()),
        median(&mut end_b.clone()),
    ));
    m.raw_section("s37 ticketed-DEL per-cycle samples", &raw);
    Ok(())
}
