//! `inf-bench gate-run m1` (M1-S17): the feature-pressure matrix rows the
//! milestone's exit gates name — baseline regression leg, TTL-heavy mix,
//! the 1M-same-instant expiry storm, eviction pressure against `maxmemory`,
//! FLUSHALL under live reads, the pub/sub fan-out + background rows with
//! the slow-subscriber kill, and the hardened ≤ 1.0× RSS leg.
//!
//! Tier honesty (L10): identical to the m0 flow — dev runs report measured
//! values with non-binding verdicts; only `--reference-box` runs can bind
//! the milestone verdict. Rows whose tooling lives elsewhere (zipfian LFU
//! parity, M0-regression A/B vs the M0 baseline artifact, 24 h soak,
//! flamegraph attribution) report PENDING from this command by design.

use std::io::{Read as _, Write as _};
use std::net::TcpStream;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime};

use crate::cli::Flags;
use crate::gaterun::{
    Measurements, env_gate, finish_report, load_gates, scrape_cells, spawn_infinityd, spawn_redis,
    sum_field,
};
use crate::load::{LoadSpec, render, run as run_load};
use crate::resp::{connect, encode_command, request};

fn control(port: u16, argv: &[&[u8]]) -> Result<Vec<u8>, String> {
    let mut conn = connect("127.0.0.1", port)?;
    request(&mut conn, argv)
}

fn unix_ms() -> u64 {
    SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).expect("clock after epoch").as_millis()
        as u64
}

/// A fleet of subscriber connections kept drained by a few poller threads.
/// `conns × channels_per_conn` subscriptions; every conn also watches the
/// shared fan channel, so one PUBLISH to it delivers `conns` copies.
struct SubFleet {
    conns: Vec<TcpStream>,
    delivered: AtomicU64,
    stop: AtomicBool,
}

const FAN_CHANNEL: &[u8] = b"fan:shared";

impl SubFleet {
    fn subscribe(port: u16, conns: usize, channels_per_conn: usize) -> Result<SubFleet, String> {
        let mut fleet = Vec::with_capacity(conns);
        for i in 0..conns {
            let mut stream = connect("127.0.0.1", port)?;
            let mut argv: Vec<Vec<u8>> = vec![b"SUBSCRIBE".to_vec(), FAN_CHANNEL.to_vec()];
            for c in 1..channels_per_conn {
                argv.push(format!("fan:{}:{}", i, c).into_bytes());
            }
            let refs: Vec<&[u8]> = argv.iter().map(Vec::as_slice).collect();
            stream.write_all(&encode_command(&refs)).map_err(|e| format!("subscribe: {e}"))?;
            // Confirmations: one frame per subscription; drain roughly
            // (frames are small; exact framing is the poller's job).
            let mut got = 0usize;
            let mut buf = [0u8; 4096];
            while got < channels_per_conn {
                let n = stream.read(&mut buf).map_err(|e| format!("confirm: {e}"))?;
                if n == 0 {
                    return Err("server closed a subscriber during setup".into());
                }
                got += buf[..n].iter().filter(|&&b| b == b'*').count();
            }
            stream.set_nonblocking(true).map_err(|e| format!("nonblocking: {e}"))?;
            fleet.push(stream);
        }
        Ok(SubFleet { conns: fleet, delivered: AtomicU64::new(0), stop: AtomicBool::new(false) })
    }

    /// Drains every subscriber until `stop`; counts delivered frames by the
    /// crude-but-monotone `*` marker (message frames are arrays).
    fn drain_until_stopped(&self) {
        let mut buf = vec![0u8; 64 * 1024];
        while !self.stop.load(Ordering::Relaxed) {
            let mut idle = true;
            for mut conn in &self.conns {
                match conn.read(&mut buf) {
                    Ok(0) | Err(_) => {}
                    Ok(n) => {
                        idle = false;
                        let frames = buf[..n].iter().filter(|&&b| b == b'*').count() as u64;
                        self.delivered.fetch_add(frames, Ordering::Relaxed);
                    }
                }
            }
            if idle {
                std::thread::yield_now();
            }
        }
    }
}

#[allow(clippy::too_many_lines)] // orchestration script: linear rows, not branchy logic
pub fn cmd_gate_run_m1(flags: &Flags) -> Result<(), String> {
    let gates_list = load_gates(flags, "m1")?;
    let artifacts_root = flags.str_or("artifacts-root", ".artifacts/m1");
    let replicates: usize = flags.usize_or("replicates", 3)?;
    let duration: u64 = flags.u64_or("duration", 10)?;
    let cells: u16 = flags.u16_or("cells", 4)?;
    let storm_keys: u64 = flags.u64_or("storm-keys", 1_000_000)?;
    let flushall_keys: u64 = flags.u64_or("flushall-keys", 2_000_000)?;
    let fill_keys: u64 = flags.u64_or("fill-keys", 10_000_000)?;
    let maxmemory_mb: u64 = flags.u64_or("maxmemory-mb", 256)?;
    let subs: usize = flags.usize_or("subs", 512)?;
    let sub_channels: usize = flags.usize_or("sub-channels", 50)?;
    let infinityd = flags.str_or("infinityd-bin", "target/release/infinityd");
    let redis_bin = flags.str_or("redis-bin", "redis-server");
    let reference_box = flags.bool("reference-box");

    let env_ok = env_gate(flags)?;
    let mut m = Measurements::new();
    if !env_ok {
        m.note("env-check FAILED and was overridden (--unsafe-env): not citation-grade");
    }
    if !reference_box {
        m.note("dev-tier run: reference-box gates report measured values, non-binding verdicts");
    }

    // Row 1 — baseline (M0-regression leg): same shape as the m0 pipelined
    // replicates; comparison against the M0 baseline artifact is a manual
    // A/B (the gate's source stays external).
    println!("\n== row: baseline pipelined ({duration}s × {replicates}) ==");
    let server = spawn_infinityd(&infinityd, cells, &[])?;
    let mut base_ops: Vec<f64> = Vec::new();
    let mut base_p999: Vec<f64> = Vec::new();
    for rep in 0..replicates {
        let spec = LoadSpec {
            port: server.port,
            duration: Duration::from_secs(duration),
            ..Default::default()
        };
        let report = run_load(&spec)?;
        println!("  rep {rep}: {:.0} ops/s, p999 {} µs", report.ops_per_sec, report.p999_us);
        m.raw_section(&format!("baseline rep {rep}"), &render(&report));
        base_ops.push(report.ops_per_sec);
        base_p999.push(report.p999_us as f64);
    }
    m.set("loadgen:baseline_ops_per_sec", crate::gaterun::median(&mut base_ops));
    m.set("loadgen:baseline_p999_us", crate::gaterun::median(&mut base_p999));

    // Row 2 — TTL-heavy: every SET carries PX 100..5000 ms; the wheel and
    // active-expiry slices churn under foreground load.
    println!("\n== row: TTL-heavy mix ==");
    let spec = LoadSpec {
        port: server.port,
        duration: Duration::from_secs(duration),
        set_weight: 1,
        get_weight: 1,
        ttl_range_ms: Some((100, 5_000)),
        ..Default::default()
    };
    let report = run_load(&spec)?;
    println!("  {:.0} ops/s, p999 {} µs", report.ops_per_sec, report.p999_us);
    m.raw_section("ttl-heavy", &render(&report));
    m.set("loadgen:ttl_heavy_p999_us", report.p999_us as f64);
    let infos = scrape_cells(server.port, cells)?;
    m.note(format!(
        "ttl-heavy: expired_active {} · expired_lazy {} across cells",
        sum_field(&infos, "expired_active"),
        sum_field(&infos, "expired_lazy")
    ));
    control(server.port, &[b"FLUSHALL"])?;

    // Row 3 — expiry storm: `storm_keys` keys all expire at one absolute
    // instant while GET traffic runs through it; foreground p99.9 is the
    // gate, drain time the second half.
    println!("\n== row: expiry storm ({storm_keys} same-instant keys) ==");
    let storm_at = Instant::now() + Duration::from_millis(20_000);
    let fill = LoadSpec {
        port: server.port,
        conns: 32,
        pipeline: 64,
        fill: Some(storm_keys),
        key_prefix: "storm:".into(),
        duration: Duration::from_secs(3600),
        pxat_ms: Some(unix_ms() + 20_000),
        ..Default::default()
    };
    run_load(&fill)?;
    let read_spec = LoadSpec {
        port: server.port,
        set_weight: 0,
        get_weight: 1,
        keys: storm_keys,
        key_prefix: "storm:".into(),
        duration: Duration::from_secs(30),
        ..Default::default()
    };
    let report = run_load(&read_spec)?;
    println!("  reads through the storm: p999 {} µs", report.p999_us);
    m.raw_section("expiry-storm reads", &render(&report));
    m.set("loadgen:expiry_storm_p999_us", report.p999_us as f64);
    let drain_deadline = Instant::now() + Duration::from_secs(60);
    let drained_at = loop {
        let reply = control(server.port, &[b"DBSIZE"])?;
        let size: u64 = String::from_utf8_lossy(&reply)
            .trim_start_matches(':')
            .trim()
            .parse()
            .unwrap_or(u64::MAX);
        if size == 0 {
            break Some(Instant::now());
        }
        if Instant::now() > drain_deadline {
            break None;
        }
        std::thread::yield_now();
    };
    match drained_at {
        Some(t) => {
            let drain_s = t.saturating_duration_since(storm_at).as_secs_f64();
            println!("  storm drained {drain_s:.2}s after the deadline instant");
            m.set("loadgen:expiry_drain_s", drain_s);
        }
        None => m.note("expiry storm did NOT drain within 60 s — debt escalation finding"),
    }

    // Row 4 — eviction pressure: a write storm against maxmemory; reads stay
    // flat, the bound holds, OOM never replaces eviction under these
    // policies.
    println!("\n== row: eviction pressure (maxmemory {maxmemory_mb}mb, allkeys-lru) ==");
    let mb = maxmemory_mb * 1024 * 1024;
    control(server.port, &[b"CONFIG", b"SET", b"maxmemory", mb.to_string().as_bytes()])?;
    control(server.port, &[b"CONFIG", b"SET", b"maxmemory-policy", b"allkeys-lru"])?;
    let spec = LoadSpec {
        port: server.port,
        duration: Duration::from_secs(duration.max(10)),
        set_weight: 1,
        get_weight: 1,
        value_size: 512,
        // Working set sized ≫ the budget so pressure is continuous.
        keys: (mb / 256).max(1_000_000),
        ..Default::default()
    };
    let report = run_load(&spec)?;
    println!("  {:.0} ops/s, p999 {} µs", report.ops_per_sec, report.p999_us);
    m.raw_section("eviction-pressure", &render(&report));
    m.set("loadgen:eviction_storm_p999_us", report.p999_us as f64);
    let infos = scrape_cells(server.port, cells)?;
    let evicted = sum_field(&infos, "evicted_keys");
    // The pressure bound compares LOGICAL used bytes (live records + index +
    // wheel + CMS — the shape `Keyspace::used_bytes` enforces); INFO's
    // `used_memory` additionally carries allocator slack + wire buffers +
    // conn state, whose bound is the RSS/attribution story, not this gate.
    let logical = sum_field(&infos, "records_live_bytes")
        + sum_field(&infos, "index_bytes")
        + sum_field(&infos, "wheel_bytes")
        + sum_field(&infos, "evict_bytes");
    let resident = sum_field(&infos, "used_memory");
    m.set("loadgen:eviction_used_over_limit", logical as f64 / mb as f64);
    m.note(format!(
        "eviction pressure: {evicted} evictions; logical {logical} B vs limit {mb} B \
         (resident incl. slack/buffers: {resident} B)"
    ));
    if evicted == 0 {
        m.note("WARNING: zero evictions — the row did not generate pressure (check sizing)");
    }
    control(server.port, &[b"CONFIG", b"SET", b"maxmemory", b"0"])?;
    control(server.port, &[b"CONFIG", b"SET", b"maxmemory-policy", b"noeviction"])?;
    control(server.port, &[b"FLUSHALL"])?;

    // Row 5 — FLUSHALL under load: live GET traffic while FLUSHALL lands
    // mid-run; the scatter must not cliff foreground p99.
    println!("\n== row: FLUSHALL under load ({flushall_keys} keys) ==");
    let fill = LoadSpec {
        port: server.port,
        conns: 32,
        pipeline: 64,
        fill: Some(flushall_keys),
        duration: Duration::from_secs(3600),
        ..Default::default()
    };
    run_load(&fill)?;
    let port = server.port;
    let disturber = std::thread::spawn(move || -> Result<f64, String> {
        // Test orchestration thread, not cell code.
        #[allow(clippy::disallowed_methods)]
        std::thread::sleep(Duration::from_secs(3));
        let t0 = Instant::now();
        control(port, &[b"FLUSHALL"])?;
        Ok(t0.elapsed().as_secs_f64() * 1000.0)
    });
    let spec = LoadSpec {
        port: server.port,
        set_weight: 0,
        get_weight: 1,
        keys: flushall_keys,
        duration: Duration::from_secs(10),
        ..Default::default()
    };
    let report = run_load(&spec)?;
    let flush_ms = disturber.join().expect("disturber thread")?;
    println!(
        "  reads through FLUSHALL: p99 {} µs (FLUSHALL itself took {flush_ms:.1} ms)",
        report.p99_us
    );
    m.raw_section("flushall-under-load", &render(&report));
    m.set("loadgen:flushall_p99_us", report.p99_us as f64);
    m.note(format!("FLUSHALL command latency: {flush_ms:.1} ms over {flushall_keys} keys"));

    // Row 6 — pub/sub: fan-out p99 to every subscriber (the PUBLISH reply is
    // delivery-acked, so reply RTT bounds delivery), then KV latency with
    // publish traffic in the background, then the slow-subscriber kill.
    println!("\n== row: pub/sub ({subs} conns × {sub_channels} subscriptions) ==");
    let fleet = SubFleet::subscribe(server.port, subs, sub_channels)?;
    m.note(format!(
        "pub/sub registry pressure: {} subscriptions across {} connections",
        subs * sub_channels,
        subs
    ));
    let stop_pub = AtomicBool::new(false);
    std::thread::scope(|scope| -> Result<(), String> {
        let drainers: Vec<_> =
            (0..4).map(|_| scope.spawn(|| fleet.drain_until_stopped())).collect();
        // Fan-out p99: each PUBLISH delivers to every conn; the reply is the
        // receiver count after per-cell delivery acks.
        let mut publisher = connect("127.0.0.1", server.port)?;
        let mut rtts_ms: Vec<f64> = Vec::new();
        for i in 0..100 {
            let payload = format!("fanout-{i}");
            let t0 = Instant::now();
            let reply = request(&mut publisher, &[b"PUBLISH", FAN_CHANNEL, payload.as_bytes()])?;
            rtts_ms.push(t0.elapsed().as_secs_f64() * 1000.0);
            let receivers: i64 = String::from_utf8_lossy(&reply)
                .trim_start_matches(':')
                .trim()
                .parse()
                .unwrap_or(-1);
            if receivers != subs as i64 {
                return Err(format!(
                    "fan-out receiver count {receivers} != {subs} subscribed connections"
                ));
            }
        }
        rtts_ms.sort_by(|a, b| a.partial_cmp(b).expect("no NaN"));
        let p99 = rtts_ms[(rtts_ms.len() * 99) / 100 - 1];
        println!("  fan-out PUBLISH p99: {p99:.2} ms to {subs} receivers");
        m.set("loadgen:pubsub_fanout_p99_ms", p99);

        // Background row: KV load while a publisher saturates the channel.
        let bg = scope.spawn(|| -> Result<u64, String> {
            let mut conn = connect("127.0.0.1", server.port)?;
            let mut published = 0u64;
            while !stop_pub.load(Ordering::Relaxed) {
                request(&mut conn, &[b"PUBLISH", FAN_CHANNEL, b"bg"])?;
                published += 1;
            }
            Ok(published)
        });
        let spec = LoadSpec {
            port: server.port,
            duration: Duration::from_secs(duration),
            ..Default::default()
        };
        let report = run_load(&spec)?;
        stop_pub.store(true, Ordering::Relaxed);
        let published = bg.join().expect("bg publisher")?;
        println!(
            "  KV under pub/sub background: p999 {} µs ({published} publishes behind it)",
            report.p999_us
        );
        m.raw_section("pubsub-background", &render(&report));
        m.set("loadgen:pubsub_bg_p999_us", report.p999_us as f64);

        fleet.stop.store(true, Ordering::Relaxed);
        for d in drainers {
            d.join().expect("drainer");
        }
        m.note(format!(
            "pub/sub deliveries drained by the fleet: {}",
            fleet.delivered.load(Ordering::Relaxed)
        ));
        Ok(())
    })?;
    drop(fleet);

    // Slow subscriber: a conn that never reads dies at the configured output
    // cap instead of bloating the cell.
    println!("\n== row: slow-subscriber kill ==");
    control(
        server.port,
        &[b"CONFIG", b"SET", b"client-output-buffer-limit", b"pubsub 262144 65536 2"],
    )?;
    let mut slow = connect("127.0.0.1", server.port)?;
    slow.write_all(&encode_command(&[b"SUBSCRIBE", b"slow:chan"]))
        .map_err(|e| format!("slow subscribe: {e}"))?;
    let mut publisher = connect("127.0.0.1", server.port)?;
    let payload = vec![0x42u8; 16 * 1024];
    let mut killed = false;
    let kill_deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < kill_deadline {
        request(&mut publisher, &[b"PUBLISH", b"slow:chan", &payload])?;
        // The kill lands within one MAINTAIN round; the closed socket shows
        // up as EOF/reset on the unread subscriber.
        slow.set_nonblocking(true).ok();
        let mut probe = [0u8; 1024];
        match slow.read(&mut probe) {
            Ok(0) => {
                killed = true;
                break;
            }
            Ok(_) => {} // deliveries we deliberately leave mostly unread
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}
            Err(_) => {
                killed = true;
                break;
            }
        }
    }
    m.set("loadgen:slow_subscriber_killed", f64::from(u8::from(killed)));
    let infos = scrape_cells(server.port, cells)?;
    m.note(format!(
        "slow-subscriber: killed={killed} · client_output_buffer_limit_disconnections={}",
        sum_field(&infos, "client_output_buffer_limit_disconnections")
    ));
    control(
        server.port,
        &[b"CONFIG", b"SET", b"client-output-buffer-limit", b"pubsub 33554432 8388608 60"],
    )?;
    drop(server);

    // Row 7 — the hardened L5 gate: RSS ≤ 1.0× Redis on the reference
    // corpus (10M × (16 B, 64 B)).
    if flags.bool("skip-fill") {
        m.note("RSS fill leg skipped (--skip-fill)");
    } else {
        println!("\n== row: RSS @ {fill_keys} keys × (16 B, 64 B) vs Redis ==");
        let ours = spawn_infinityd(&infinityd, cells, &[])?;
        let fill = LoadSpec {
            port: ours.port,
            conns: 32,
            pipeline: 64,
            fill: Some(fill_keys),
            duration: Duration::from_secs(3600),
            ..Default::default()
        };
        run_load(&fill)?;
        let our_rss = ours.rss_bytes();
        drop(ours);
        match spawn_redis(&redis_bin) {
            Err(e) => m.note(format!("redis RSS leg skipped: {e}")),
            Ok(redis) => {
                let fill = LoadSpec { port: redis.port, ..fill.clone() };
                run_load(&fill)?;
                let redis_rss = redis.rss_bytes();
                let ratio = our_rss as f64 / redis_rss as f64;
                println!("  RSS: infinityd {our_rss} B vs redis {redis_rss} B => {ratio:.3}x");
                m.set("external:rss_attribution", ratio);
            }
        }
    }

    // Row 8 — hit-rate parity (--with-zipfian): the zipfian LFU trace-replay
    // that backs the `hit_rate_parity` gate. Hit rate is algorithm-determined,
    // not governor-sensitive, so it binds anywhere the trace is replayed.
    if flags.bool("with-zipfian") {
        let keyspace = flags.u64_or("zipfian-keyspace", 1_000_000)?;
        let zipf_ops = flags.u64_or("zipfian-ops", 5_000_000)?;
        let zipf_mb = flags.u64_or("zipfian-maxmemory-mb", 512)?;
        println!("\n== row: zipfian LFU hit-rate parity (keyspace {keyspace}, {zipf_ops} ops) ==");
        let params = crate::zipfian::ParityParams {
            keyspace: keyspace.min(u64::from(u32::MAX)) as u32,
            warmup: zipf_ops,
            ops: zipf_ops,
            value_size: 64,
            theta: 0.99,
            seed: 0x5EED_2026_C0DE,
            maxmemory_bytes: zipf_mb * 1024 * 1024,
            cells: 1,
            window: 256,
        };
        let parity = crate::zipfian::run_parity(&infinityd, &redis_bin, &params)?;
        let pp = parity.pp_below();
        println!(
            "  InfinityDB {:.2}% vs Redis {:.2}% => {pp:+.2} pp below Redis",
            parity.infinity.hit_rate() * 100.0,
            parity.redis.hit_rate() * 100.0
        );
        m.set("external:zipfian_lfu", pp);
    }

    finish_report(
        "m1",
        &gates_list,
        &m,
        env_ok,
        reference_box,
        &artifacts_root,
        &format!(
            "cells: {cells} · replicates: {replicates} · duration: {duration}s · storm: \
             {storm_keys} · subs: {subs}×{sub_channels}"
        ),
    )
}
