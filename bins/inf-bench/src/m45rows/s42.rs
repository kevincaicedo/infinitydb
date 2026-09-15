//! M4.5-S42 rows: the first-boot device probe (ADR-0086 D7 / ADR-0091).

use super::*;

// ---- M4.5-S42: the stock first boot (ADR-0091 D1) ----------------------------

/// One S42 replicate's facts: the two boots of a fresh data directory
/// under the product default (`--device-probe auto`), the provenance
/// INFO reported after each, and the S35 AC leg + read leg run on the
/// directory the first boot probed.
struct S42Leg {
    first_boot_s: f64,
    second_boot_s: f64,
    first_source: String,
    second_source: String,
    schema: u64,
    identity: String,
    /// The class the probe wrote, and INFO's active-segment class at
    /// `loading:0` and after the AC leg (the upgrade rotation between).
    file_class: String,
    class_at_boot: String,
    class_after_leg: String,
    rotations_upgrade: u64,
    budget_model: String,
    ac: S35Leg,
    read_ops_per_sec: f64,
}

/// The value of an INFO field every cell agrees on, or `mixed(...)` —
/// a provenance the cells disagree on is a finding, never a median.
fn s42_agreed(infos: &[std::collections::BTreeMap<String, String>], field: &str) -> String {
    let mut values: Vec<&str> =
        infos.iter().map(|c| c.get(field).map_or("", String::as_str)).collect();
    values.sort_unstable();
    values.dedup();
    match values.as_slice() {
        [one] => (*one).to_string(),
        many => format!("mixed({})", many.join("|")),
    }
}

/// Boots the stock server on `dir` — no arm flags, the probe lifecycle
/// left to the binary — and returns the wall from spawn to `loading:0`
/// on every cell with the INFO scraped at that instant. The listener
/// accepts before recovery completes (`-LOADING`), so "ready" is the
/// cells' word, never the port's — the first smoke read a 0.00 s
/// second boot and a provenance the cells had not all published yet.
fn s42_boot(flags: &Flags, infinityd: &str, cells: u16, dir: &str) -> Result<S42Boot, String> {
    let mut extra: Vec<String> = vec!["--data-dir".into(), dir.to_string()];
    if let Some(pin) = flags.get("pin-start") {
        extra.push("--pin-start".into());
        extra.push(pin.to_string());
    }
    // Named, so the harness rule (`off` unless the spawn says otherwise)
    // does not turn the product's default into the dev tier.
    extra.push("--device-probe".into());
    extra.push("auto".into());
    let extra_refs: Vec<&str> = extra.iter().map(String::as_str).collect();
    let t0 = Instant::now();
    let mut server = spawn_infinityd(infinityd, cells, &extra_refs)?;
    let port = server.port;
    let deadline = Instant::now() + Duration::from_secs(300);
    loop {
        if let Some(status) = server.try_exited() {
            return Err(format!("s42: server exited during boot ({status})"));
        }
        if let Ok(infos) = scrape_cells(port, cells)
            && infos.len() == usize::from(cells)
            && sum_field(&infos, "loading") == 0
        {
            let wall_s = t0.elapsed().as_secs_f64();
            return Ok(S42Boot { server, port, wall_s, infos });
        }
        if Instant::now() >= deadline {
            return Err("s42: boot never reached loading:0 on every cell within 300 s".into());
        }
        #[allow(clippy::disallowed_methods)] // bench orchestration, not cell code
        std::thread::sleep(Duration::from_millis(5));
    }
}

/// The class the probe wrote (`barrier_class = "fua" | "flush"` in the
/// file the first boot left in `dir`), or `absent`. INFO's
/// `barrier_class` is the *active segment's* class — `flush` on a fresh
/// cell until ADR-0086 D4's upgrade rotation — so the configured class
/// is read where it was decided.
fn s42_file_class(dir: &str) -> String {
    let path = std::path::Path::new(dir).join("io-properties.toml");
    let Ok(text) = std::fs::read_to_string(path) else { return "absent".to_string() };
    text.lines()
        .find_map(|line| {
            let (key, value) = line.split_once('=')?;
            (key.trim() == "barrier_class").then(|| value.trim().trim_matches('"').to_string())
        })
        .unwrap_or_else(|| "absent".to_string())
}

/// A booted stock server: the guard, its port, the wall to `loading:0`
/// and the INFO every cell answered at that instant.
struct S42Boot {
    server: crate::gaterun::ServerGuard,
    port: u16,
    wall_s: f64,
    infos: Vec<std::collections::BTreeMap<String, String>>,
}

/// The M4.5-S42 row (ADR-0091 D6): a fresh data directory booted twice
/// under the product default — the first boot probes and writes the
/// schema-4 model (`probed-at-boot`), the second reads it (`file`) —
/// then the S35 AC leg (32 conns closed loop) and the pipelined read
/// leg on that directory. Gates: the probe's cost (first − second boot
/// ≤ 15 s), both provenances, the schema, and the AC leg's p50 over
/// the barrier of the class the probe chose; throughput and reads are
/// disclosed against the probed arm's replicate spread in the ledger.
pub(super) fn s42_first_boot_row(
    flags: &Flags,
    infinityd: &str,
    cells: u16,
    duration: u64,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    // The row is the stock boot: an arm flag would measure another
    // configuration under this row's name.
    for arm in [
        "barrier-class",
        "frames-in-flight",
        "device-probe",
        "device-write-mbps",
        "seal-pace",
        "fill-window-us",
        "fill-target-kib",
        "flush-group-window-us",
        "staging-mib",
        "model-absent",
    ] {
        if flags.get(arm).is_some() {
            return Err(format!("--only-s42 measures the stock boot; --{arm} is not an arm here"));
        }
    }
    let idle_s = flags.u64_or("leg-idle-s", S35_LEG_IDLE_S)?;
    m.note(format!(
        "s42 row: stock boot — `infinityd --data-dir <fresh> --cells {cells}{}` (no arm flags; \
         the binary's `--device-probe auto` default, named on the spawn) · {replicates} \
         replicates · first boot timed to ready (the probe), second boot on the same directory \
         timed to ready (the file), then the S35 AC leg ({S35_CONNS_AC} conns pipeline 1, \
         {duration} s) and the read leg (64 conns × P16) on that directory · {idle_s} s idle \
         before every durable leg",
        flags.get("pin-start").map(|p| format!(" --pin-start {p}")).unwrap_or_default()
    ));
    let mut raw = String::new();
    let mut legs: Vec<S42Leg> = Vec::new();
    for rep in 0..replicates {
        let dir = format!("{data_root}/s42-rep{rep}");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).map_err(|e| format!("{dir}: {e}"))?;
        s35_idle(idle_s, &format!("s42 rep{rep} first boot"));
        let first = s42_boot(flags, infinityd, cells, &dir)?;
        let first_boot_s = first.wall_s;
        let first_source = s42_agreed(&first.infos, "io_properties_source");
        let schema = crate::gaterun::max_field(&first.infos, "io_properties_schema");
        let identity = s42_agreed(&first.infos, "io_properties_identity");
        drop(first.server);
        let file_class = s42_file_class(&dir);
        let second = s42_boot(flags, infinityd, cells, &dir)?;
        let second_boot_s = second.wall_s;
        let second_source = s42_agreed(&second.infos, "io_properties_source");
        let class_at_boot = s42_agreed(&second.infos, "barrier_class");
        let budget_model = s42_agreed(&second.infos, "io_budget_model");
        let server = second.server;
        let port = second.port;
        create_ns(
            port,
            &[b"INF.NS", b"CREATE", b"s35alw", b"MODE", b"durable", b"FSYNC", b"always"],
        )?;
        await_fan(port, "s35alw", cells)?;
        let (p50_key, p99_key) = if file_class == "fua" {
            ("fua_latency_p50_us", "fua_latency_p99_us")
        } else {
            ("fsync_latency_p50_us", "fsync_latency_p99_us")
        };
        s35_idle(idle_s, &format!("s42 rep{rep} c{S35_CONNS_AC}"));
        let ac = s35_write_leg(port, cells, S35_CONNS_AC, duration, p50_key, p99_key)?;
        let after = scrape_cells(port, cells)?;
        let class_after_leg = s42_agreed(&after, "barrier_class");
        let rotations_upgrade = sum_field(&after, "rotations_upgrade");
        let rd = run_load(&LoadSpec {
            port,
            conns: 64,
            pipeline: 16,
            duration: Duration::from_secs(duration),
            warmup: Duration::from_secs(2),
            set_weight: 0,
            get_weight: 1,
            keys: FILL_KEYS,
            key_prefix: "s35alw:".into(),
            value_size: 1024,
            setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), b"s35alw".to_vec()]],
            ..LoadSpec::default()
        })?;
        if rd.errors > 0 {
            return Err(format!(
                "s42 rep{rep} read leg: {} errors (first: {:?})",
                rd.errors,
                rd.error_samples.first()
            ));
        }
        drop(server);
        let _ = std::fs::remove_dir_all(&dir);
        raw.push_str(&format!(
            "rep{rep} first_boot_s={first_boot_s:.2} second_boot_s={second_boot_s:.2} \
             source={first_source}→{second_source} schema={schema} identity={identity} \
             file_class={file_class} info_class={class_at_boot}→{class_after_leg} \
             rotations_upgrade={rotations_upgrade} budget_model={budget_model} \
             c{S35_CONNS_AC} ops/s={:<8.0} \
             p50_us={:<6.0} p99_us={:<7.0} barrier_p50_us={:<5.0} p50/barrier={:.2} \
             frames_in_flight_max={} acks/fsync={:.1} read c64 P16 ops/s={:.0}\n",
            ac.ops_per_sec,
            ac.p50_us,
            ac.p99_us,
            ac.barrier_p50_us,
            ac.p50_us / ac.barrier_p50_us.max(1.0),
            ac.frames_in_flight_max,
            ac.acks_per_fsync,
            rd.ops_per_sec
        ));
        println!("  s42 {}", raw.lines().last().unwrap_or(""));
        legs.push(S42Leg {
            first_boot_s,
            second_boot_s,
            first_source,
            second_source,
            schema,
            identity,
            file_class,
            class_at_boot,
            class_after_leg,
            rotations_upgrade,
            budget_model,
            ac,
            read_ops_per_sec: rd.ops_per_sec,
        });
    }
    let col = |f: &dyn Fn(&S42Leg) -> f64| -> f64 {
        let mut v: Vec<f64> = legs.iter().map(f).collect();
        median(&mut v)
    };
    let all =
        |pred: &dyn Fn(&S42Leg) -> bool| -> f64 { f64::from(u8::from(legs.iter().all(pred))) };
    m.set("s42:first_boot_s", col(&|l| l.first_boot_s));
    m.set("s42:second_boot_s", col(&|l| l.second_boot_s));
    m.set("s42:probe_overhead_s", col(&|l| l.first_boot_s - l.second_boot_s));
    m.set("s42:first_boot_probed", all(&|l| l.first_source == "probed-at-boot"));
    m.set("s42:second_boot_from_file", all(&|l| l.second_source == "file"));
    m.set("s42:identity_verified", all(&|l| l.identity == "verified"));
    m.set("s42:schema", col(&|l| l.schema as f64));
    m.set("s42:p50_over_barrier_x", col(&|l| l.ac.p50_us / l.ac.barrier_p50_us.max(1.0)));
    m.set("s42:always_c32_ops_per_sec", col(&|l| l.ac.ops_per_sec));
    m.set("s42:always_c32_p50_us", col(&|l| l.ac.p50_us));
    m.set("s42:always_c32_p99_us", col(&|l| l.ac.p99_us));
    m.set("s42:barrier_p50_us", col(&|l| l.ac.barrier_p50_us));
    m.set("s42:read_c64p16_ops_per_sec", col(&|l| l.read_ops_per_sec));
    m.set("s42:file_class_fua", all(&|l| l.file_class == "fua"));
    let classes: Vec<String> = legs
        .iter()
        .map(|l| {
            format!(
                "file {} / INFO {}→{} ({} upgrade rotations; {})",
                l.file_class,
                l.class_at_boot,
                l.class_after_leg,
                l.rotations_upgrade,
                l.budget_model
            )
        })
        .collect();
    m.note(format!(
        "s42 class per replicate — the probe's verdict, then INFO's active-segment class at \
         loading:0 → after the AC leg (ADR-0086 D4: a fresh cell upgrades at its first \
         rotation; `flush` at loading:0 is the not-yet-zeroed first segment, not the verdict), \
         the budget model: {}; every boot line is in the server stderr capture when \
         INF_GATERUN_STDERR_DIR is set",
        classes.join(" / ")
    ));
    m.row_open("first-boot-lifecycle");
    m.row_write_amp(
        "not measured by this row — S42 gates the probe lifecycle (ADR-0091 D6): the first \
         boot's cost over the second, both provenances, the schema, and the S35 AC leg on the \
         directory the boot probed; write amplification is S36's row",
    );
    m.raw_section("s42 per-replicate samples", &raw);
    Ok(())
}
