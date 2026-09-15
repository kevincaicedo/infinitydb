//! S37 cardinality and D9 controls. Every timing has a checked workload.

use super::{
    CONNS_HIGH, CONNS_LOW, FILL_KEYS, MEM_BUDGET, S37_DEFAULT_KEYS, S37_DEL_DEFAULT_CYCLES,
    S37_DEL_DEFAULT_KEYS, await_fan, config_set, create_ns, s35_idle,
};
use crate::cli::Flags;
use crate::gaterun::{Measurements, ServerGuard, median, scrape_cells, spawn_infinityd, sum_field};
use crate::load::{LoadReport, LoadSpec, run as run_load};
use crate::resp::{connect, request};
use std::net::TcpStream;
use std::time::{Duration, Instant};

#[derive(Clone, Copy)]
struct Shape {
    cells: u16,
    keys: u64,
    window: u64,
    cycles: u64,
    idle_s: u64,
}

pub(super) fn dbsize_row(
    flags: &Flags,
    binary: &str,
    cells: u16,
    replicates: usize,
    data_root: &str,
    m: &mut Measurements,
) -> Result<(), String> {
    let shape = Shape {
        cells,
        keys: flags.u64_or("s37-keys", S37_DEFAULT_KEYS)?,
        window: flags.u64_or("s37-del-keys", S37_DEL_DEFAULT_KEYS)?,
        cycles: flags.u64_or("s37-del-cycles", S37_DEL_DEFAULT_CYCLES)?,
        idle_s: flags.u64_or("leg-idle-s", 40)?,
    };
    if shape.window == 0
        || shape.cycles == 0
        || shape.window.checked_mul(shape.cycles).is_none_or(|n| n > shape.keys)
    {
        return Err("DBSIZE windows must be nonzero and fit the filled keyspace".into());
    }
    m.note(format!(
        "S37 DBSIZE: {cells} cells, {} filled keys, {} fresh windows of {} keys per leg; \
         A=shadow off, B=shadow on with reconciliation paused; alternating AB/BA; \
         one timed drain per window, then 32 empty-drain controls. Drain samples are \
         reported individually, never as a statistically unsupported p99.9.",
        shape.keys, shape.cycles, shape.window
    ));
    let mut raw = String::new();
    let mut timings = [Vec::new(), Vec::new()];
    for rep in 0..replicates {
        for slot in 0..2 {
            let arm = (rep + slot) % 2;
            let label = if arm == 0 { "A" } else { "B" };
            let dir = format!("{data_root}/s37-dbsize-{label}-rep{rep}");
            let server = dbsize_server(flags, binary, &dir, shape)?;
            config_set(server.port, "tiered-shadow-reconcile", "no")?;
            config_set(
                server.port,
                "tiered-shadow-overwrite",
                if arm == 0 { "no" } else { "yes" },
            )?;
            for cycle in 0..shape.cycles {
                raw.push_str(&format!("rep{rep} {label} cycle{cycle}\n"));
                let elapsed = dbsize_cycle(server.port, shape, cycle, arm == 1, &mut raw)?;
                timings[arm].push(elapsed);
                println!("s37 DBSIZE rep{rep} {label} cycle{cycle}: {elapsed:.0} us");
            }
            drop(server);
            std::fs::remove_dir_all(&dir).map_err(|e| format!("remove {dir}: {e}"))?;
        }
    }
    let plain_us = median(&mut timings[0]);
    let ticketed_us = median(&mut timings[1]);
    m.set("s37:dbsize_plain_median_us", plain_us);
    m.set("s37:dbsize_ticketed_median_us", ticketed_us);
    m.note(format!(
        "DBSIZE drain medians: A={plain_us:.0} us, B={ticketed_us:.0} us; individual samples below."
    ));
    m.row_open("s37-dbsize");
    m.row_write_amp("not a write-amplification measurement; SET only prepares the checked drain");
    m.raw_section("S37 DBSIZE under tickets", &raw);
    Ok(())
}

fn dbsize_server(
    flags: &Flags,
    binary: &str,
    dir: &str,
    shape: Shape,
) -> Result<ServerGuard, String> {
    // Refuse existing data rather than remove anything not created by this run.
    std::fs::create_dir(dir).map_err(|e| format!("create fresh {dir}: {e}"))?;
    crate::m2rows::copy_probe_file(flags, std::path::Path::new(dir))?;
    let mut extra = vec!["--data-dir".to_string(), dir.to_string()];
    if let Some(pin) = flags.get("pin-start") {
        extra.extend(["--pin-start".to_string(), pin.to_string()]);
    }
    extra.extend(crate::m2rows::pipeline_args(flags));
    let args: Vec<&str> = extra.iter().map(String::as_str).collect();
    s35_idle(shape.idle_s, "DBSIZE before fill");
    let server = spawn_infinityd(binary, shape.cells, &args)?;
    create_ns(
        server.port,
        &[
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
        ],
    )?;
    await_fan(server.port, "s37tier", shape.cells)?;
    fill(server.port, "s37tier", shape.keys, 0, 4)?;
    s35_idle(shape.idle_s, "DBSIZE post-fill quiescence");
    Ok(server)
}

fn fill(port: u16, namespace: &str, keys: u64, from: u64, pipeline: usize) -> Result<(), String> {
    let result = run_load(&LoadSpec {
        fill: Some(keys),
        fill_from: from,
        pipeline,
        ..spec(port, namespace, keys)
    })?;
    if result.errors != 0 || result.ops != keys {
        return Err(format!(
            "{namespace} fill: {} ops, {} errors ({} BUSY; {:?}), expected {keys}",
            result.ops, result.errors, result.busy_retryable, result.error_samples
        ));
    }
    Ok(())
}

fn spec(port: u16, namespace: &str, keys: u64) -> LoadSpec {
    LoadSpec {
        port,
        keys,
        conns: CONNS_LOW,
        value_size: 1024,
        key_prefix: format!("{namespace}:"),
        setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), namespace.as_bytes().to_vec()]],
        ..LoadSpec::default()
    }
}

fn dbsize_cycle(
    port: u16,
    shape: Shape,
    cycle: u64,
    shadow: bool,
    raw: &mut String,
) -> Result<f64, String> {
    fill(port, "s37tier", shape.window, cycle * shape.window, 1)?;
    let before = scrape_cells(port, shape.cells)?;
    let required = [
        "tiering_shadow_pending",
        "tiering_shadow_verified_pending",
        "tiering_shadow_dbsize_reads",
    ];
    if before.iter().any(|cell| required.iter().any(|key| !cell.contains_key(*key))) {
        return Err(
            "DBSIZE instrument requires shadow pending/verified/read counters on every cell".into(),
        );
    }
    let pending = sum_field(&before, required[0]);
    let unverified = pending
        .checked_sub(sum_field(&before, required[1]))
        .ok_or("DBSIZE invalid counters: verified exceeds pending")?;
    println!(
        "s37 DBSIZE coverage: shadow={shadow} pending={pending} unverified={unverified} window={}",
        shape.window
    );
    validate_coverage(shadow, unverified, shape.window)?;
    let mut client = connect("127.0.0.1", port)?;
    let selected = request(&mut client, &[b"INF.NS", b"USE", b"s37tier"])?;
    if selected != b"+OK\r\n" {
        return Err(format!("DBSIZE namespace selection: {}", String::from_utf8_lossy(&selected)));
    }
    let drain_us = timed_count(&mut client, shape.keys)?;
    let after = scrape_cells(port, shape.cells)?;
    let remaining = sum_field(&after, required[0])
        .checked_sub(sum_field(&after, required[1]))
        .ok_or("DBSIZE invalid counters after drain: verified exceeds pending")?;
    let reads = sum_field(&after, required[2]).saturating_sub(sum_field(&before, required[2]));
    if remaining != 0 || reads != unverified {
        println!("DBSIZE failed drain INFO: before={before:?} after={after:?}");
        return Err(format!(
            "DBSIZE incomplete drain: unverified {unverified}, reads {reads}, remaining {remaining}"
        ));
    }
    let mut control = Vec::new();
    for _ in 0..32 {
        control.push(timed_count(&mut client, shape.keys)?);
    }
    let control_max = control.iter().copied().fold(0.0, f64::max);
    raw.push_str(&format!(
        "pending={pending} unverified={unverified} reads={reads} remaining={remaining} count={} \
         drain_us={drain_us:.0} empty_drain_median_us={:.0} empty_drain_max_us={control_max:.0}\n",
        shape.keys,
        median(&mut control)
    ));
    raw.push_str(&format!("INFO before={before:?}\nINFO after={after:?}\n"));
    Ok(drain_us)
}

fn validate_coverage(shadow: bool, unverified: u64, window: u64) -> Result<(), String> {
    if (shadow && unverified as f64 / (window as f64) < 0.7) || (!shadow && unverified != 0) {
        return Err(format!(
            "DBSIZE invalid coverage: shadow={shadow} unverified={unverified} window={window}"
        ));
    }
    Ok(())
}

fn timed_count(client: &mut TcpStream, expected: u64) -> Result<f64, String> {
    let started = Instant::now();
    let reply = request(client, &[b"DBSIZE"])?;
    let elapsed = started.elapsed().as_secs_f64() * 1e6;
    if reply != format!(":{expected}\r\n").as_bytes() {
        return Err(format!("DBSIZE expected {expected}: {}", String::from_utf8_lossy(&reply)));
    }
    Ok(elapsed)
}

pub(super) struct Controls {
    flat: LoadReport,
    read: LoadReport,
    repeat: LoadReport,
}

pub(super) fn controls(
    port: u16,
    cells: u16,
    duration: u64,
    keys: u64,
    idle_s: u64,
    raw: &mut String,
) -> Result<Controls, String> {
    for (namespace, count) in [("s37flat", keys), ("s37read", FILL_KEYS)] {
        create_ns(
            port,
            &[b"INF.NS", b"CREATE", namespace.as_bytes(), b"MODE", b"durable", b"FSYNC", b"always"],
        )?;
        await_fan(port, namespace, cells)?;
        fill(port, namespace, count, 0, 16)?;
    }
    s35_idle(idle_s, "S37 non-tiered always control post-fill");
    let flat = checked_load(
        &LoadSpec {
            conns: CONNS_HIGH,
            pipeline: 1,
            set_weight: 1,
            get_weight: 0,
            duration: Duration::from_secs(duration),
            warmup: Duration::from_secs(2),
            ..spec(port, "s37flat", keys)
        },
        cells,
        "flat-c256",
        raw,
    )?;
    s35_idle(idle_s, "S37 filled S35 read control");
    let read_spec = LoadSpec {
        pipeline: 16,
        set_weight: 0,
        get_weight: 1,
        duration: Duration::from_secs(duration),
        warmup: Duration::from_secs(2),
        ..spec(port, "s37read", FILL_KEYS)
    };
    let read = checked_load(&read_spec, cells, "read-c64p16", raw)?;
    let repeat = checked_load(&read_spec, cells, "read-repeat-c64p16", raw)?;
    Ok(Controls { flat, read, repeat })
}

fn checked_load(
    spec: &LoadSpec,
    cells: u16,
    label: &str,
    raw: &mut String,
) -> Result<LoadReport, String> {
    let before = scrape_cells(spec.port, cells)?;
    let (result, cpu_pct) = measured_load(spec)?;
    if result.ops == 0 || result.errors != 0 || result.nils != 0 {
        return Err(format!(
            "{label}: ops={} errors={} nils={}",
            result.ops, result.errors, result.nils
        ));
    }
    let after = scrape_cells(spec.port, cells)?;
    raw.push_str(&format!(
        "{label} ops/s={:.0} p50_us={} p99_us={} p999_us={} errors={} nils={} \
             generator_cpu_pct={cpu_pct:.1}\n",
        result.ops_per_sec,
        result.p50_us,
        result.p99_us,
        result.p999_us,
        result.errors,
        result.nils
    ));
    raw.push_str(&format!("INFO before={before:?}\nINFO after={after:?}\n"));
    Ok(result)
}

pub(super) fn measured_load(spec: &LoadSpec) -> Result<(LoadReport, f64), String> {
    let started = Instant::now();
    let ticks = crate::gaterun::cpu_ticks_of(std::process::id());
    let result = run_load(spec)?;
    let ticks = crate::gaterun::cpu_ticks_of(std::process::id()).saturating_sub(ticks);
    let cpu_pct =
        ticks as f64 / crate::gaterun::CLOCK_TICKS_PER_S as f64 / started.elapsed().as_secs_f64()
            * 100.0;
    Ok((result, cpu_pct))
}

pub(super) fn summarize_controls(rows: &[(String, f64, Controls)], m: &mut Measurements) {
    let mut parity = Vec::new();
    let mut read_a = Vec::new();
    let mut read_b = Vec::new();
    let mut aa = Vec::new();
    for (arm, tiered_ops, control) in rows {
        if arm == "B" {
            parity.push(tiered_ops / control.flat.ops_per_sec);
            read_b.push(control.read.ops_per_sec);
        } else {
            read_a.push(control.read.ops_per_sec);
            aa.push((control.repeat.ops_per_sec / control.read.ops_per_sec - 1.0).abs());
        }
    }
    let parity = median(&mut parity);
    let read_ratio = median(&mut read_b) / median(&mut read_a);
    let noise = median(&mut aa);
    m.set("s37:parity_c256", parity);
    m.set("s37:read_ops_b_over_a", read_ratio);
    m.set("s37:read_aa_absolute_delta", noise);
    m.note(format!(
        "D9 controls: tiered/flat c256={parity:.4}, read B/A={read_ratio:.4}, A/A \
         absolute fractional difference={noise:.4}."
    ));
    m.note(
        "D9 controls: matched non-tiered always c256 denominator, S35 c64/P16 filled hot read \
         shape, consecutive A/A noise samples. Raw INFO carries tripwires and attribution; no \
         competitor claim.",
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_empty_or_drained_ticket_set_cannot_validate_the_shadow_measurement() {
        // A fast DBSIZE with no unverified tickets does not measure the drain.
        assert!(validate_coverage(true, 0, 12_288).is_err());
        assert!(validate_coverage(true, 8_000, 12_288).is_err());
        assert!(validate_coverage(true, 11_000, 12_288).is_ok());
        assert!(validate_coverage(false, 1, 12_288).is_err());
        assert!(validate_coverage(false, 0, 12_288).is_ok());
    }
}
