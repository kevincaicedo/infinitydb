//! `inf-bench ycsb` (M4-S22): YCSB-style workloads A–F over a tiered
//! namespace at dataset = N× the namespace memory budget, zipfian θ=0.99
//! and uniform rows, with the §18/§19 split-reporting rule mechanized:
//! memory-hit and cold-read percentiles render **separately, always** — a
//! blended p99 over a bimodal distribution is a lie by construction, so
//! the combined client-side number is context, never the headline.
//!
//! Documented adaptations (ADR-0025 D3 — no silent workload substitution;
//! all four render in every report preamble):
//! - **E** runs as cursor-scan slices (`SCAN <cursor> COUNT <1..=100>`,
//!   pipeline 1 per connection): the keyspace is unordered, so YCSB's
//!   ordered range-scan does not exist here.
//! - **Values** are a single constant-byte payload of `--value-size`
//!   (default 1 KiB ≈ YCSB's 10×100 B fields): this is a KV engine, not a
//!   field-structured store, and value *size* is the workload parameter.
//! - **D**'s "latest" distribution reads `frontier − zipf_rank` with a
//!   per-connection frontier estimate (exact recency ordering needs a
//!   global op sequencer the generator deliberately does not have).
//! - **Harness**: `inf-bench` drives these rows — memtier cannot drive a
//!   10× RAM zipfian tier workload (§6); the report banner says so.
//!
//! Until the command-wiring story (M4-S26) lifts the ADR-0062 D8 `USE`
//! refusal, the tiered rows cannot exist: the run then drops to
//! **harness-validation mode** — the same generator, rows, and report
//! machinery against a memory-mode namespace, every tiered-only figure a
//! named-absent line. The mode is probed live (the S20 pattern): a wired
//! plane flips the run to tiered rows automatically, and a tiered run
//! whose INFO scrape lacks the **cold** split-histogram fields fails
//! loudly — the split is the deliverable, not an optional extra.
//!
//! The **memory-hit** half is derived client-side ([`derive_mem_hit`],
//! ADR-0071 D2). Its server-side fields were withdrawn on 2026-08-08: the
//! reactor's per-iteration clock cannot time a command that never
//! suspends, so they recorded 0 µs for every memory hit. Refusing the row
//! on their absence — which is what this harness did between that fix and
//! ADR-0071 — silenced the tiered leg of a 32 h soak for 31.4 hours
//! (readiness F17). A missing *citation* instrument must not become a
//! refusal to run *load*.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use inf_foundation::LogHistogram;
use inf_foundation::rng::{Entropy, SplitMix64};

use crate::cli::Flags;
use crate::gaterun::{
    Measurements, ServerGuard, env_gate, finish_report, load_gates, max_field, scrape_cells,
    spawn_infinityd, sum_field,
};
use crate::load::{LoadSpec, make_key, render, run as run_load};
use crate::m2rows::delta_pct;
use crate::resp::{connect, encode_command, reply_len, request};
use crate::writeamp;

/// One deterministic-from-seed key shape shared by the loader and every
/// workload generator (`load::make_key` is the single source of truth).
const KEY_PREFIX: &str = "y:";
const KEY_SIZE: usize = 16;

/// The cold half of the served-from split contract (M4-S26 / ADR-0064 D3).
/// Command wiring must emit exactly these: µs percentiles from the
/// resolver-tagged cold completion histogram, plus the two S10 cold
/// tripwires. A tiered run missing any of them is invalid by construction
/// and fails — see [`split_section`].
const COLD_SPLIT_FIELDS: [&str; 5] = [
    "tiering_cold_p50_us",
    "tiering_cold_p99_us",
    "tiering_cold_p999_us",
    "cold_read_qd_p99",
    "coalesce_ratio_milli",
];

/// The **withdrawn** server-side memory-hit fields (ADR-0071 D1). They were
/// part of the D3 contract until 2026-08-08, when the reactor's
/// per-iteration clock was shown to make them structurally unmeasurable: a
/// command that never suspends reads the same iteration timestamp at
/// enqueue and completion, so every memory hit recorded 0 µs. The server
/// now renders `tiering_ram_hit_split:unmeasured-iteration-clock` in their
/// place, and the memory-hit half of the split is **derived client-side**
/// ([`derive_mem_hit`]).
///
/// This array exists so the absence is *named* rather than silent: the
/// report prints the withdrawal with its reason, and a future server that
/// starts emitting them again is detected and reported rather than
/// ignored. It is deliberately **not** a refusal list — the D3 refusal
/// existed to keep an unsplit row out of a *citation*, and turning it into
/// a refusal to *run* silenced the tiered workload of the 32 h soak of
/// 2026-08-08 for 31.4 of its 32.1 hours (readiness F17).
const WITHDRAWN_RAM_HIT_FIELDS: [&str; 3] =
    ["tiering_ram_hit_p50_us", "tiering_ram_hit_p99_us", "tiering_ram_hit_p999_us"];

/// Ceiling on the cold fraction for which the [`derive_mem_hit`]
/// truncation is meaningful. Above it the "hot set" is not being served
/// from memory in any useful sense and the derived percentiles describe a
/// population too small to gate on.
const MEM_HIT_MAX_COLD_FRACTION: f64 = 0.5;

/// Minimum client ops in a row before its derived memory-hit percentiles
/// are allowed to carry a gate value (p99.9 needs a population).
const MEM_HIT_MIN_OPS: u64 = 100_000;

/// YCSB-style Zipf(θ) over `n` ranks — Gray et al.'s quick method, lifted
/// from the `cold_shaping` bench (the generator the plan names as the
/// correct one to promote into `inf-bench`). Rank 0 is the hottest.
/// Callers scramble ranks through `hash64` (the YCSB "scrambled zipfian")
/// so the hot set scatters across the keyspace: adjacent hot indices
/// would cluster into few pages and quietly turn a 10× RAM run into a
/// RAM-resident one — the exact pitfall the plan warns about.
struct Zipf {
    n: u64,
    theta: f64,
    alpha: f64,
    zetan: f64,
    eta: f64,
    /// Analytic share of the hottest 1% of ranks (zeta(n/100)/zeta(n)) —
    /// the empirical self-check compares against this.
    top1_share: f64,
}

impl Zipf {
    fn new(n: u64, theta: f64) -> Zipf {
        assert!(n >= 100, "zipf keyspace below the top-1% validation floor");
        let top_k = n / 100;
        let mut zetan = 0.0f64;
        let mut zeta_top = 0.0f64;
        for i in 1..=n {
            let term = 1.0 / (i as f64).powf(theta);
            zetan += term;
            if i <= top_k {
                zeta_top += term;
            }
        }
        let zeta2 = 1.0 + 0.5f64.powf(theta);
        let alpha = 1.0 / (1.0 - theta);
        let eta = (1.0 - (2.0 / n as f64).powf(1.0 - theta)) / (1.0 - zeta2 / zetan);
        Zipf { n, theta, alpha, zetan, eta, top1_share: zeta_top / zetan }
    }

    fn sample(&self, rng: &mut SplitMix64) -> u64 {
        let u = unit(rng);
        let uz = u * self.zetan;
        if uz < 1.0 {
            return 0;
        }
        if uz < 1.0 + 0.5f64.powf(self.theta) {
            return 1;
        }
        let rank = (self.n as f64 * (self.eta * u - self.eta + 1.0).powf(self.alpha)) as u64;
        rank.min(self.n - 1)
    }
}

fn unit(rng: &mut SplitMix64) -> f64 {
    (rng.next_u64() >> 11) as f64 / (1u64 << 53) as f64
}

/// Key distributions for the read side of a workload.
#[derive(Clone, Copy, PartialEq)]
enum Dist {
    Zipfian,
    Uniform,
    /// D's recency distribution: `frontier − zipf_rank`, clamped.
    Latest,
}

impl Dist {
    fn name(self) -> &'static str {
        match self {
            Dist::Zipfian => "zipfian",
            Dist::Uniform => "uniform",
            Dist::Latest => "latest",
        }
    }
}

/// One YCSB workload row. Percentages sum to 100 (asserted in tests).
struct Workload {
    id: &'static str,
    read_pct: u64,
    update_pct: u64,
    insert_pct: u64,
    rmw_pct: u64,
    scan_pct: u64,
    dist: Dist,
}

/// The A–F table (YCSB core workloads; E and value shape adapted per the
/// module doc). `uniform` variants override `dist` at row construction.
const WORKLOADS: [Workload; 6] = [
    Workload {
        id: "a",
        read_pct: 50,
        update_pct: 50,
        insert_pct: 0,
        rmw_pct: 0,
        scan_pct: 0,
        dist: Dist::Zipfian,
    },
    Workload {
        id: "b",
        read_pct: 95,
        update_pct: 5,
        insert_pct: 0,
        rmw_pct: 0,
        scan_pct: 0,
        dist: Dist::Zipfian,
    },
    Workload {
        id: "c",
        read_pct: 100,
        update_pct: 0,
        insert_pct: 0,
        rmw_pct: 0,
        scan_pct: 0,
        dist: Dist::Zipfian,
    },
    Workload {
        id: "d",
        read_pct: 95,
        update_pct: 0,
        insert_pct: 5,
        rmw_pct: 0,
        scan_pct: 0,
        dist: Dist::Latest,
    },
    Workload {
        id: "e",
        read_pct: 0,
        update_pct: 0,
        insert_pct: 5,
        rmw_pct: 0,
        scan_pct: 95,
        dist: Dist::Zipfian,
    },
    Workload {
        id: "f",
        read_pct: 50,
        update_pct: 0,
        insert_pct: 0,
        rmw_pct: 50,
        scan_pct: 0,
        dist: Dist::Zipfian,
    },
];

/// Everything one YCSB row needs; cheap to clone per connection thread.
#[derive(Clone)]
struct RowSpec {
    port: u16,
    ns: String,
    conns: usize,
    pipeline: usize,
    duration: Duration,
    warmup: Duration,
    keys: u64,
    value_size: usize,
    seed: u64,
}

/// What one connection returns: merged into the row report.
struct ConnOut {
    ops: u64,
    errors: u64,
    nils: u64,
    hist_us: LogHistogram,
    /// Rank draws in the hottest 1% (zipfian rows) / total draws — the
    /// in-run half of the skew self-check.
    hot_draws: u64,
    total_draws: u64,
    /// Running `hash64` over the op stream (kind byte + key index) — the
    /// reproducibility artifact the AC asserts on.
    checksum: u64,
}

/// A request in flight, by reply semantics.
enum PendKind {
    /// One op, one reply (read / update / insert; also the RMW write,
    /// which carries the read's start instant in `Pending::sent`).
    Simple,
    /// The read half of an RMW: its reply schedules the write with the
    /// original start instant (the transaction is one op end-to-end).
    RmwRead { key: Vec<u8> },
    /// A SCAN slice: its reply carries the next cursor.
    Scan,
}

struct Pending {
    sent: Instant,
    kind: PendKind,
}

/// Extracts the next cursor from a SCAN reply: `*2\r\n$<n>\r\n<cursor>\r\n…`.
/// Bounded, iterative; `None` for any other shape (the caller treats that
/// as an error reply already counted by the error path).
fn scan_cursor(frame: &[u8]) -> Option<Vec<u8>> {
    let rest = frame.strip_prefix(b"*2\r\n")?;
    let rest = rest.strip_prefix(b"$")?;
    let line_end = rest.windows(2).position(|w| w == b"\r\n")?;
    let len: usize = std::str::from_utf8(&rest[..line_end]).ok()?.parse().ok()?;
    let cursor_at = line_end + 2;
    if len > 40 || rest.len() < cursor_at + len {
        return None; // cursors are short engine tokens; anything else is not a SCAN reply
    }
    Some(rest[cursor_at..cursor_at + len].to_vec())
}

/// Per-connection generator + socket state for one row.
struct Conn {
    stream: TcpStream,
    rng: SplitMix64,
    inflight: VecDeque<Pending>,
    /// RMW writes scheduled by completed reads; drained before new ops.
    rmw_writes: VecDeque<(Instant, Vec<u8>)>,
    cursor: Vec<u8>,
    insert_next: u64,
    inserted: u64,
    out: ConnOut,
    tx: Vec<u8>,
    rx: Vec<u8>,
    rx_at: usize,
}

impl Conn {
    fn note_stream(&mut self, kind: u8, index: u64) {
        let mut buf = [0u8; 9];
        buf[0] = kind;
        buf[1..].copy_from_slice(&index.to_le_bytes());
        self.out.checksum = inf_foundation::hash64(&buf, self.out.checksum);
    }

    /// Draws the target key index for a read/update per the distribution.
    fn draw_index(&mut self, spec: &RowSpec, w: &Workload, zipf: &Zipf) -> u64 {
        self.out.total_draws += 1;
        match w.dist {
            Dist::Uniform => self.rng.next_u64() % spec.keys,
            Dist::Zipfian => {
                let rank = zipf.sample(&mut self.rng);
                if rank < zipf.n / 100 {
                    self.out.hot_draws += 1;
                }
                // Scrambled zipfian: identity permutes, shares don't.
                inf_foundation::hash64(&rank.to_le_bytes(), 0x5C2A) % spec.keys
            }
            Dist::Latest => {
                let rank = zipf.sample(&mut self.rng);
                if rank < zipf.n / 100 {
                    self.out.hot_draws += 1;
                }
                let frontier = spec.keys + self.inserted * spec.conns as u64;
                frontier.saturating_sub(1 + rank).min(frontier - 1)
            }
        }
    }

    /// Encodes the next op into `tx`; returns false past the deadline.
    fn push_op(
        &mut self,
        spec: &RowSpec,
        w: &Workload,
        zipf: &Zipf,
        value: &[u8],
        key_spec: &LoadSpec,
        deadline: Instant,
    ) -> bool {
        // Scheduled RMW writes finish started transactions first — they
        // run even past the deadline so no transaction is left half-done.
        if let Some((started, key)) = self.rmw_writes.pop_front() {
            self.tx.extend_from_slice(&encode_command(&[b"SET", &key, value]));
            self.inflight.push_back(Pending { sent: started, kind: PendKind::Simple });
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        let die = self.rng.next_u64() % 100;
        let (read_end, update_end, insert_end, rmw_end) = (
            w.read_pct,
            w.read_pct + w.update_pct,
            w.read_pct + w.update_pct + w.insert_pct,
            w.read_pct + w.update_pct + w.insert_pct + w.rmw_pct,
        );
        if die < read_end {
            let index = self.draw_index(spec, w, zipf);
            self.note_stream(b'r', index);
            let key = make_key(key_spec, index);
            self.tx.extend_from_slice(&encode_command(&[b"GET", &key]));
            self.inflight.push_back(Pending { sent: Instant::now(), kind: PendKind::Simple });
        } else if die < update_end {
            let index = self.draw_index(spec, w, zipf);
            self.note_stream(b'u', index);
            let key = make_key(key_spec, index);
            self.tx.extend_from_slice(&encode_command(&[b"SET", &key, value]));
            self.inflight.push_back(Pending { sent: Instant::now(), kind: PendKind::Simple });
        } else if die < insert_end {
            let index = spec.keys + self.insert_next;
            self.insert_next += spec.conns as u64;
            self.inserted += 1;
            self.note_stream(b'i', index);
            let key = make_key(key_spec, index);
            self.tx.extend_from_slice(&encode_command(&[b"SET", &key, value]));
            self.inflight.push_back(Pending { sent: Instant::now(), kind: PendKind::Simple });
        } else if die < rmw_end {
            let index = self.draw_index(spec, w, zipf);
            self.note_stream(b'm', index);
            let key = make_key(key_spec, index);
            self.tx.extend_from_slice(&encode_command(&[b"GET", &key]));
            self.inflight
                .push_back(Pending { sent: Instant::now(), kind: PendKind::RmwRead { key } });
        } else {
            let count = 1 + self.rng.next_u64() % 100; // YCSB max scan length
            self.note_stream(b's', count);
            let cursor = self.cursor.clone();
            self.tx.extend_from_slice(&encode_command(&[
                b"SCAN",
                &cursor,
                b"COUNT",
                count.to_string().as_bytes(),
            ]));
            self.inflight.push_back(Pending { sent: Instant::now(), kind: PendKind::Scan });
        }
        true
    }

    /// Consumes one complete reply frame; records latency per op.
    fn on_reply(&mut self, frame: &[u8], warmup_end: Instant) {
        let pending = self.inflight.pop_front().expect("reply without a request");
        if frame.starts_with(b"-") {
            self.out.errors += 1;
        }
        match pending.kind {
            PendKind::Simple => {
                if pending.sent >= warmup_end {
                    self.out.hist_us.record(pending.sent.elapsed().as_micros() as u64);
                    self.out.ops += 1;
                    if frame.starts_with(b"$-1") {
                        self.out.nils += 1;
                    }
                }
            }
            PendKind::RmwRead { key } => {
                // The write half carries the read's start instant: the
                // recorded latency spans the whole transaction.
                self.rmw_writes.push_back((pending.sent, key));
            }
            PendKind::Scan => {
                if let Some(next) = scan_cursor(frame) {
                    self.cursor = next;
                }
                if pending.sent >= warmup_end {
                    self.out.hist_us.record(pending.sent.elapsed().as_micros() as u64);
                    self.out.ops += 1;
                }
            }
        }
    }
}

fn run_ycsb_conn(
    spec: &RowSpec,
    w: &Workload,
    zipf: &Zipf,
    conn_index: usize,
    warmup_end: Instant,
    deadline: Instant,
) -> Result<ConnOut, String> {
    let mut stream = connect("127.0.0.1", spec.port)?;
    let reply = request(&mut stream, &[b"INF.NS", b"USE", spec.ns.as_bytes()])?;
    if reply.starts_with(b"-") {
        return Err(format!("ycsb USE {} failed: {}", spec.ns, String::from_utf8_lossy(&reply)));
    }
    let key_spec = key_shape_spec(spec);
    let value = vec![0xABu8; spec.value_size];
    let mut conn = Conn {
        stream,
        rng: SplitMix64::new(spec.seed ^ (0x9C5B + conn_index as u64)),
        inflight: VecDeque::with_capacity(spec.pipeline),
        rmw_writes: VecDeque::new(),
        cursor: b"0".to_vec(),
        insert_next: conn_index as u64,
        inserted: 0,
        out: ConnOut {
            ops: 0,
            errors: 0,
            nils: 0,
            hist_us: LogHistogram::new(),
            hot_draws: 0,
            total_draws: 0,
            checksum: 0x5EED ^ conn_index as u64,
        },
        tx: Vec::with_capacity(16 * 1024),
        rx: Vec::with_capacity(64 * 1024),
        rx_at: 0,
    };
    let mut chunk = [0u8; 64 * 1024];
    loop {
        // Send phase. No "done" latch: `push_op` drains owed RMW writes
        // before consulting the deadline, so a write scheduled by a read
        // that completed *after* the deadline still goes out (the latch
        // version span here forever with the write never sent — caught
        // live on the first F-row smoke).
        conn.tx.clear();
        while conn.inflight.len() < spec.pipeline {
            if !conn.push_op(spec, w, zipf, &value, &key_spec, deadline) {
                break;
            }
        }
        if !conn.tx.is_empty() {
            let tx = std::mem::take(&mut conn.tx);
            conn.stream.write_all(&tx).map_err(|e| format!("ycsb write: {e}"))?;
            conn.tx = tx;
        }
        if conn.inflight.is_empty() {
            // Nothing in flight and push_op declined to send: past the
            // deadline with no RMW writes owed — the connection is done.
            debug_assert!(conn.rmw_writes.is_empty(), "owed RMW write with nothing in flight");
            break;
        }
        let n = conn.stream.read(&mut chunk).map_err(|e| format!("ycsb read: {e}"))?;
        if n == 0 {
            return Err("server closed connection under ycsb load".into());
        }
        conn.rx.extend_from_slice(&chunk[..n]);
        while let Some(end) = reply_len(&conn.rx[conn.rx_at..]) {
            let frame = conn.rx[conn.rx_at..conn.rx_at + end].to_vec();
            conn.on_reply(&frame, warmup_end);
            conn.rx_at += end;
            if conn.inflight.is_empty() {
                break;
            }
        }
        if conn.rx_at > 0 {
            conn.rx.drain(..conn.rx_at);
            conn.rx_at = 0;
        }
    }
    Ok(conn.out)
}

/// The `LoadSpec` that defines the key shape (prefix + width) — shared
/// with the fill so generator and loader can never drift apart.
fn key_shape_spec(spec: &RowSpec) -> LoadSpec {
    LoadSpec {
        port: spec.port,
        keys: spec.keys,
        key_prefix: KEY_PREFIX.into(),
        key_size: KEY_SIZE,
        value_size: spec.value_size,
        seed: spec.seed,
        ..Default::default()
    }
}

/// Merged row result: the client-side combined report + self-check terms.
struct RowOut {
    ops: u64,
    errors: u64,
    nils: u64,
    ops_per_sec: f64,
    p50_us: u64,
    p99_us: u64,
    p999_us: u64,
    max_us: u64,
    hot_share_pct: f64,
    checksum: u64,
    /// The merged client latency histogram, kept whole. The memory-hit
    /// half of the split is a quantile of *this* distribution, so the row
    /// cannot throw it away after taking p50/p99/p99.9 (ADR-0071 D2).
    hist_us: LogHistogram,
}

fn run_row(spec: &RowSpec, w: &Workload, zipf: &Zipf) -> Result<RowOut, String> {
    let pipeline = if w.scan_pct > 0 { 1 } else { spec.pipeline };
    let spec = RowSpec { pipeline, ..spec.clone() };
    let started = Instant::now();
    let warmup_end = started + spec.warmup;
    let deadline = warmup_end + spec.duration;
    let results: Vec<Result<ConnOut, String>> = std::thread::scope(|scope| {
        let (spec, w) = (&spec, w);
        let handles: Vec<_> = (0..spec.conns)
            .map(|i| scope.spawn(move || run_ycsb_conn(spec, w, zipf, i, warmup_end, deadline)))
            .collect();
        handles.into_iter().map(|h| h.join().expect("ycsb conn thread")).collect()
    });
    let elapsed = started.elapsed().saturating_sub(spec.warmup).as_secs_f64();
    let mut hist = LogHistogram::new();
    let (mut ops, mut errors, mut nils) = (0u64, 0u64, 0u64);
    let (mut hot, mut total) = (0u64, 0u64);
    let mut checksums: Vec<u8> = Vec::new();
    for result in results {
        let conn = result?;
        ops += conn.ops;
        errors += conn.errors;
        nils += conn.nils;
        hot += conn.hot_draws;
        total += conn.total_draws;
        hist.merge(&conn.hist_us);
        checksums.extend_from_slice(&conn.checksum.to_le_bytes());
    }
    Ok(RowOut {
        ops,
        errors,
        nils,
        ops_per_sec: ops as f64 / elapsed,
        p50_us: hist.percentile(50.0),
        p99_us: hist.percentile(99.0),
        p999_us: hist.percentile(99.9),
        max_us: hist.max(),
        hot_share_pct: if total == 0 { 0.0 } else { hot as f64 * 100.0 / total as f64 },
        checksum: inf_foundation::hash64(&checksums, 0x5EED),
        hist_us: hist,
    })
}

/// Pre-run generator self-check: draws `samples` ranks and compares the
/// top-1% share against the analytic zeta ratio. A buggy skew generator
/// quietly turns the 10× RAM run RAM-resident — so this refuses the run,
/// it does not footnote it.
fn validate_zipf(zipf: &Zipf, seed: u64, samples: u64) -> Result<String, String> {
    let mut rng = SplitMix64::new(seed ^ 0x21F);
    let top_k = zipf.n / 100;
    let mut hot = 0u64;
    for _ in 0..samples {
        if zipf.sample(&mut rng) < top_k {
            hot += 1;
        }
    }
    let measured = hot as f64 * 100.0 / samples as f64;
    let analytic = zipf.top1_share * 100.0;
    let line = format!(
        "zipf self-check: top-1% share measured {measured:.2}% vs analytic {analytic:.2}% \
         (θ={}, n={}, {samples} draws)",
        zipf.theta, zipf.n
    );
    if (measured - analytic).abs() > 1.5 {
        return Err(format!("{line} — outside ±1.5pp, the skew generator is broken"));
    }
    Ok(line)
}

/// In-process reproducibility assert (the S22 AC): two independently
/// constructed generators over the same seed must produce byte-identical
/// op streams. Runs the first workload's first-connection stream twice.
fn verify_seed(spec: &RowSpec, zipf: &Zipf, ops: u64) -> Result<String, String> {
    let stream_checksum = || -> u64 {
        let key_spec = key_shape_spec(spec);
        let mut conn_rng = SplitMix64::new(spec.seed ^ 0x9C5B);
        let mut checksum = 0x5EEDu64;
        for _ in 0..ops {
            let index = {
                let rank = zipf.sample(&mut conn_rng);
                inf_foundation::hash64(&rank.to_le_bytes(), 0x5C2A) % spec.keys
            };
            let key = make_key(&key_spec, index);
            let mut buf = Vec::with_capacity(9 + key.len());
            buf.push(b'r');
            buf.extend_from_slice(&index.to_le_bytes());
            buf.extend_from_slice(&key);
            checksum = inf_foundation::hash64(&buf, checksum);
        }
        checksum
    };
    let (first, second) = (stream_checksum(), stream_checksum());
    if first != second {
        return Err(format!(
            "seed verification failed: two generations of the same stream disagree \
             ({first:#018x} vs {second:#018x}) — the generator is not deterministic"
        ));
    }
    Ok(format!("seed verification: {ops} ops regenerated identically (checksum {first:#018x})"))
}

/// The memory-hit half of the split, derived client-side (ADR-0071 D2).
///
/// The two populations in a tiered row's latency distribution are cleanly
/// separated in practice (memory hits are tens of µs, cold reads are
/// milliseconds). Given the cold fraction `f`, the fastest `1 − f` of the
/// client distribution *is* the memory-hit population, so the memory-hit
/// percentile at `X` is the overall quantile at `X · (1 − f)`.
///
/// Separation is checked, not assumed: the derived p99.9 must land below
/// the server's measured cold p50, or the row is rendered with its numbers
/// and refused a gate value. That check is what makes this an instrument
/// rather than an assumption.
struct MemHit {
    ops: u64,
    cold_reads: u64,
    cold_resolves: u64,
    cold_frac: f64,
    p50_us: u64,
    p99_us: u64,
    p999_us: u64,
    /// The server's cold p50 this row was checked against — rendered so
    /// the separation margin is visible on a passing row too, not only in
    /// the refusal text of a failing one.
    cold_p50_us: u64,
    /// `Err(reason)` = derived and rendered, but not gate-eligible.
    eligible: Result<(), String>,
}

/// Derives [`MemHit`] from one row's client histogram and the cold-read
/// counters scraped across it.
///
/// `cold_reads` (not `cold_resolves`) is the numerator: one client op that
/// suspends resolves cold **twice** — once before the read, once on resume
/// — so `cold_resolves` overstates the cold population by the re-resolve
/// factor (measured 2.33× on 2026-08-08). Coalescing pushes the other way
/// (one read can serve several ops), which *under*states `f`, keeps more of
/// the tail inside the truncation, and can only make the derived
/// percentiles worse — the safe direction for a gate.
fn derive_mem_hit(out: &RowOut, cold_reads: u64, cold_resolves: u64, cold_p50_us: u64) -> MemHit {
    let cold_frac = if out.ops == 0 { 1.0 } else { (cold_reads as f64 / out.ops as f64).min(1.0) };
    let keep = (1.0 - cold_frac).max(0.0);
    let at = |p: f64| out.hist_us.percentile(p * keep);
    let (p50_us, p99_us, p999_us) = (at(50.0), at(99.0), at(99.9));
    let eligible = if out.ops < MEM_HIT_MIN_OPS {
        Err(format!("{} ops < {MEM_HIT_MIN_OPS} — too few for a p99.9 gate value", out.ops))
    } else if cold_frac > MEM_HIT_MAX_COLD_FRACTION {
        Err(format!(
            "cold fraction {:.1}% > {:.0}% — the hot set is not memory-resident in this row, so \
             the truncation describes no useful population",
            cold_frac * 100.0,
            MEM_HIT_MAX_COLD_FRACTION * 100.0
        ))
    } else if cold_reads == 0 {
        // The RAM-resident reference leg (ADR-0064 D4 / ADR-0071 D6):
        // nothing demoted, so this row's client population is *unimodal*
        // and the truncation is the identity. There is no second mode to
        // separate from — the separation check is vacuous here, not
        // failed. Refusing it made the reference leg ineligible by
        // construction, which made `compare_hot_set` exclude every row
        // and left the §7 hot-set gate unable to bind at all.
        Ok(())
    } else if cold_p50_us == 0 {
        Err(format!(
            "server cold p50 reads 0 while {cold_reads} cold reads were issued in this row — \
             that is a broken instrument, not a memory-resident population"
        ))
    } else if p999_us >= cold_p50_us {
        Err(format!(
            "separation check FAILED: derived memory-hit p99.9 {p999_us} µs >= server cold p50 \
             {cold_p50_us} µs — the two populations overlap and the quantile truncation cannot \
             tell them apart (client tail spread {} µs vs cold service {cold_p50_us} µs)",
            p999_us.saturating_sub(p50_us)
        ))
    } else {
        Ok(())
    };
    MemHit {
        ops: out.ops,
        cold_reads,
        cold_resolves,
        cold_frac,
        p50_us,
        p99_us,
        p999_us,
        cold_p50_us,
        eligible,
    }
}

/// The reference carrier (ADR-0071 D3): one line per row, so the
/// RAM-resident leg and the tiered leg — which cannot share a process,
/// because they need different dataset sizes and therefore different
/// fills — still compare through the same instrument in the same campaign.
fn render_mem_hit_tsv(mem_hits: &[(String, MemHit)]) -> String {
    let mut out = String::from(
        "# inf-bench ycsb — client-derived memory-hit split (ADR-0071 D2)\n\
         # row\tops\tcold_frac\tp50_us\tp99_us\tp999_us\teligible\n",
    );
    for (name, mem) in mem_hits {
        out.push_str(&format!(
            "{name}\t{}\t{:.6}\t{}\t{}\t{}\t{}\n",
            mem.ops,
            mem.cold_frac,
            mem.p50_us,
            mem.p99_us,
            mem.p999_us,
            if mem.eligible.is_ok() { "ok" } else { "ineligible" },
        ));
    }
    out
}

/// One reference-leg row: the three percentiles plus whether that leg
/// considered the row gate-eligible.
struct RefRow {
    p50_us: u64,
    p99_us: u64,
    p999_us: u64,
    eligible: bool,
}

/// Reads a `mem-hit.tsv` written by [`render_mem_hit_tsv`]. `path` may name
/// the file itself or the gate-run directory that holds it.
fn load_mem_hit_tsv(path: &str) -> Result<std::collections::BTreeMap<String, RefRow>, String> {
    let candidates = [PathBuf::from(path), PathBuf::from(path).join("mem-hit.tsv")];
    let (found, text) = candidates
        .iter()
        .find_map(|p| std::fs::read_to_string(p).ok().map(|t| (p.clone(), t)))
        .ok_or_else(|| {
            format!(
                "--hot-set-reference {path}: no mem-hit.tsv here or at {path}/mem-hit.tsv — the \
                 reference leg must be an `inf-bench ycsb --dataset-multiple 1` run of this \
                 harness (ADR-0071 D3)"
            )
        })?;
    let mut rows = std::collections::BTreeMap::new();
    for (n, line) in text.lines().enumerate() {
        if line.starts_with('#') || line.trim().is_empty() {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        if f.len() != 7 {
            return Err(format!(
                "{}: line {} has {} fields, expected 7 — not a mem-hit.tsv",
                found.display(),
                n + 1,
                f.len()
            ));
        }
        let num = |i: usize| -> Result<u64, String> {
            f[i].parse::<u64>().map_err(|e| format!("{}: line {}: {e}", found.display(), n + 1))
        };
        rows.insert(
            f[0].to_string(),
            RefRow {
                p50_us: num(3)?,
                p99_us: num(4)?,
                p999_us: num(5)?,
                eligible: f[6].trim() == "ok",
            },
        );
    }
    if rows.is_empty() {
        return Err(format!("{}: no rows", found.display()));
    }
    Ok(rows)
}

/// Compares this (tiered) leg's memory-hit split against the RAM-resident
/// reference leg and sets the three `ycsb:hot_set_*_delta_pct` gate values
/// to the **worst** matched row per percentile. A row that either leg
/// ruled ineligible is named and excluded — never silently dropped.
fn compare_hot_set(
    m: &mut Measurements,
    mem_hits: &[(String, MemHit)],
    reference: &std::collections::BTreeMap<String, RefRow>,
    path: &str,
) -> (String, String) {
    let mut table = format!(
        "reference leg: {path}\n\n\
         | row | percentile | reference µs | tiered µs | delta |\n|---|---|---|---|---|\n"
    );
    let mut excluded: Vec<String> = Vec::new();
    let mut worst: [Option<f64>; 3] = [None; 3];
    let mut matched = 0usize;
    for (name, mem) in mem_hits {
        let Some(reference_row) = reference.get(name) else {
            excluded.push(format!("{name} (absent from the reference leg)"));
            continue;
        };
        if let Err(reason) = &mem.eligible {
            excluded.push(format!("{name} (tiered leg: {reason})"));
            continue;
        }
        if !reference_row.eligible {
            excluded.push(format!("{name} (reference leg ruled it ineligible)"));
            continue;
        }
        matched += 1;
        for (i, (label, a, b)) in [
            ("p50", reference_row.p50_us, mem.p50_us),
            ("p99", reference_row.p99_us, mem.p99_us),
            ("p99.9", reference_row.p999_us, mem.p999_us),
        ]
        .into_iter()
        .enumerate()
        {
            let delta = delta_pct(a as f64, b as f64);
            table.push_str(&format!("| {name} | {label} | {a} | {b} | {delta:+.2}% |\n"));
            worst[i] = Some(worst[i].map_or(delta, |w: f64| w.max(delta)));
        }
    }
    if !excluded.is_empty() {
        table.push_str(&format!(
            "\nexcluded rows (named, not dropped):\n- {}\n",
            excluded.join("\n- ")
        ));
    }
    for (key, value) in [
        ("ycsb:hot_set_p50_delta_pct", worst[0]),
        ("ycsb:hot_set_p99_delta_pct", worst[1]),
        ("ycsb:hot_set_p999_delta_pct", worst[2]),
    ] {
        if let Some(value) = value {
            m.set(key, value);
        }
    }
    let note = if matched == 0 {
        format!(
            "hot-set gate: NO gate value — 0 of {} rows matched an eligible reference row \
             ({}); the gate stays PENDING rather than binding on a partial comparison",
            mem_hits.len(),
            excluded.join("; ")
        )
    } else {
        format!(
            "hot-set gate: {matched} row(s) compared against the RAM-resident reference leg \
             (worst per percentile binds); {} row(s) excluded and named in the section",
            excluded.len()
        )
    };
    (note, table)
}

impl MemHit {
    fn render(&self) -> String {
        let verdict = match &self.eligible {
            Ok(()) => "gate-eligible (separation check passed)".to_string(),
            Err(reason) => format!("NOT gate-eligible — {reason}"),
        };
        let separation = if self.cold_reads == 0 {
            "separation: not applicable — 0 cold reads in this row, so the client population is \
             unimodal (the RAM-resident reference shape, ADR-0071 D6) and the truncation is the \
             identity\n  "
                .to_string()
        } else {
            format!(
                "separation: mem_hit p99.9 {} µs vs server cold p50 {} µs (client tail spread \
                 {} µs — the truncation only separates the two modes while cold service exceeds \
                 it)\n  ",
                self.p999_us,
                self.cold_p50_us,
                self.p999_us.saturating_sub(self.p50_us),
            )
        };
        format!(
            "memory-hit split (client-derived, ADR-0071 D2):\n  \
             cold_frac = {:.4}% (cold_reads {} · cold_resolves {} — re-resolve ratio {:.2}×)\n  \
             mem_hit p50_us = {} · p99_us = {} · p999_us = {}\n  \
             {separation}{verdict}\n",
            self.cold_frac * 100.0,
            self.cold_reads,
            self.cold_resolves,
            if self.cold_reads == 0 {
                0.0
            } else {
                self.cold_resolves as f64 / self.cold_reads as f64
            },
            self.p50_us,
            self.p99_us,
            self.p999_us,
        )
    }
}

/// Renders the served-from split for one row.
///
/// Tiered rows **must** find every [`COLD_SPLIT_FIELDS`] entry in the
/// scrape (max across cells binds — per-cell histograms cannot merge from
/// percentiles, disclosed). The memory-hit half arrives as `mem`, derived
/// client-side; the withdrawn server fields are named, never demanded
/// (ADR-0071 D1). In harness-validation mode the section is one
/// named-absent contract line.
fn split_section(
    scrape: &[std::collections::BTreeMap<String, String>],
    tiered_live: bool,
    mem: Option<&MemHit>,
) -> Result<String, String> {
    if !tiered_live {
        return Ok("memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the \
                   ADR-0062 D8 refusal; M4-S26 emits the cold split service histograms \
                   (resolver-tagged {ro, cold}) under the COLD_SPLIT_FIELDS names\n"
            .into());
    }
    let mut lines = String::new();
    for field in COLD_SPLIT_FIELDS {
        let present = scrape.iter().any(|cells| cells.contains_key(field));
        if !present {
            return Err(format!(
                "tiered row ran but `{field}` is missing from INFO tiering — the cold split \
                 histogram contract (M4-S26 / ADR-0064 D3 as amended by ADR-0071 D1) is not \
                 met; a tiered row without the cold split is invalid by construction (§18/§19)"
            ));
        }
        let worst = scrape
            .iter()
            .filter_map(|cells| cells.get(field))
            .filter_map(|v| v.parse::<u64>().ok())
            .max()
            .unwrap_or(0);
        lines.push_str(&format!("{field} (worst cell) = {worst}\n"));
    }
    let returned: Vec<&str> = WITHDRAWN_RAM_HIT_FIELDS
        .into_iter()
        .filter(|f| scrape.iter().any(|cells| cells.contains_key(*f)))
        .collect();
    if returned.is_empty() {
        let split_note = scrape
            .iter()
            .find_map(|cells| cells.get("tiering_ram_hit_split"))
            .map_or_else(|| "(field absent)".to_string(), Clone::clone);
        lines.push_str(&format!(
            "server ram-hit fields: WITHDRAWN — {} named-absent, server says `{split_note}`; the \
             reactor's per-iteration clock cannot time a non-suspending command (ADR-0064 \
             amendment 2026-08-08). The memory-hit half below is client-derived.\n",
            WITHDRAWN_RAM_HIT_FIELDS.join(", ")
        ));
    } else {
        lines.push_str(&format!(
            "server ram-hit fields: UNEXPECTEDLY PRESENT ({}) — ADR-0071 D1 withdrew them; a \
             server emitting them again means the instrument changed under this harness. \
             Reconcile before citing the client-derived numbers below.\n",
            returned.join(", ")
        ));
    }
    lines.push_str(&format!(
        "tiering_cold_resolves = {}\n",
        sum_field(scrape, "tiering_cold_resolves")
    ));
    match mem {
        Some(mem) => lines.push_str(&mem.render()),
        None => lines.push_str(
            "memory-hit split: NOT DERIVED — no pre-row cold-counter baseline for this row\n",
        ),
    }
    Ok(lines)
}

fn render_row(out: &RowOut, w: &Workload, dist: Dist) -> String {
    let dist_name = dist.name();
    format!(
        "workload = {} ({dist_name})\nops = {}\nerrors = {}\nnils = {}\nops_per_sec = {:.0}\n\
         combined_client p50_us = {} · p99_us = {} · p999_us = {} · max_us = {}\n\
         (combined = context only; the split section below is the honest read)\n\
         hot_share_top1pct = {:.2}%\nstream_checksum = {:#018x}\n",
        w.id,
        out.ops,
        out.errors,
        out.nils,
        out.ops_per_sec,
        out.p50_us,
        out.p99_us,
        out.p999_us,
        out.max_us,
        out.hot_share_pct,
        out.checksum,
    )
}

/// Probes whether the tiered data plane is wired: `Ok(None)` = wired,
/// `Ok(Some(refusal))` = the D8 refusal (harness-validation mode).
fn probe_tiered(port: u16, ns: &str) -> Result<Option<String>, String> {
    let mut stream = connect("127.0.0.1", port)?;
    let reply = request(&mut stream, &[b"INF.NS", b"USE", ns.as_bytes()])?;
    if reply.starts_with(b"-ERR tiered namespaces are not command-addressable") {
        return Ok(Some(String::from_utf8_lossy(&reply).trim().to_string()));
    }
    if reply.starts_with(b"-") {
        return Err(format!("INF.NS USE {ns}: {}", String::from_utf8_lossy(&reply)));
    }
    Ok(None)
}

/// Bounded-wait control command through the `-LOADING` window (the S20
/// pattern: a durable node's socket accepts before recovery finishes).
fn control(port: u16, argv: &[&[u8]]) -> Result<Vec<u8>, String> {
    for _ in 0..100 {
        let mut stream = connect("127.0.0.1", port)?;
        let reply = request(&mut stream, argv)?;
        if reply.starts_with(b"-LOADING") {
            #[allow(clippy::disallowed_methods)] // bench control path, not cell code
            std::thread::sleep(Duration::from_millis(100));
            continue;
        }
        if reply.starts_with(b"-") {
            return Err(format!(
                "control command {:?} failed: {}",
                String::from_utf8_lossy(argv[0]),
                String::from_utf8_lossy(&reply)
            ));
        }
        return Ok(reply);
    }
    Err("server stayed in LOADING for 10 s".into())
}

/// DBSIZE on the row namespace over one connection (USE is per-conn state).
/// Namespace key count summed across all cells. On a live node `DBSIZE`
/// over a named namespace answers with the **connected cell's** share
/// only (measured 2026-07-31: 40 keys on 4 cells read 11/10/9/10 per
/// connection) — so this scrapes per cell, the `scrape_cells` REUSEPORT
/// pattern, until every distinct cell has answered.
fn ns_dbsize(port: u16, ns: &str, cells: u16) -> Result<u64, String> {
    let mut seen: std::collections::BTreeMap<u16, u64> = std::collections::BTreeMap::new();
    for _ in 0..512 {
        let mut stream = connect("127.0.0.1", port)?;
        let info = crate::resp::parse_info(&request(&mut stream, &[b"INFO"])?);
        let cell: u16 = info.get("cell").and_then(|v| v.parse().ok()).unwrap_or(0);
        if seen.contains_key(&cell) {
            continue;
        }
        let reply = request(&mut stream, &[b"INF.NS", b"USE", ns.as_bytes()])?;
        if reply.starts_with(b"-") {
            return Err(format!("USE {ns}: {}", String::from_utf8_lossy(&reply)));
        }
        let reply = request(&mut stream, &[b"DBSIZE"])?;
        let text = String::from_utf8_lossy(&reply);
        let count: u64 = text
            .trim_start_matches(':')
            .trim()
            .parse()
            .map_err(|e| format!("DBSIZE reply {text:?}: {e}"))?;
        seen.insert(cell, count);
        if seen.len() == usize::from(cells) {
            return Ok(seen.values().sum());
        }
    }
    Err(format!("ns_dbsize scraped {}/{cells} cells (REUSEPORT spread)", seen.len()))
}

struct DataDirGuard(PathBuf);

impl DataDirGuard {
    fn create(path: PathBuf) -> Result<DataDirGuard, String> {
        std::fs::create_dir_all(&path).map_err(|e| format!("data dir {path:?}: {e}"))?;
        Ok(DataDirGuard(path))
    }
}

impl Drop for DataDirGuard {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// `inf-bench ycsb` — see the module doc. Artifacts default to
/// `.artifacts/m4/s22`.
#[allow(clippy::too_many_lines)] // orchestration script: linear rows, not branchy logic
pub fn cmd_ycsb(args: &[String]) -> Result<(), String> {
    let flags = Flags::parse(
        args,
        &[
            "unsafe-env",
            "allow-dirty",
            "reference-box",
            "verify-seed",
            "skip-fill",
            "fill-only",
            "named-absent",
        ],
        &[
            "workloads",
            "distribution",
            "theta",
            "mem-budget-mb",
            "dataset-multiple",
            "value-size",
            "duration",
            "conns",
            "pipeline",
            "cells",
            "pin-start",
            "seed",
            "data-root",
            "infinityd-bin",
            "artifacts-root",
            "gates",
            "unsafe-env",
            "allow-dirty",
            "reference-box",
            "verify-seed",
            // Soak/attach mode (M4-S23): drive an already-running node.
            "attach-port",
            "ns",
            "skip-fill",
            "fill-only",
            "named-absent",
            // Hot-set gate (ADR-0071 D3): the RAM-resident reference leg's
            // gate-run dir (or its mem-hit.tsv) to compare against.
            "hot-set-reference",
        ],
    )?;
    let gates_list = load_gates(&flags, "m4")?;
    let artifacts_root = flags.str_or("artifacts-root", ".artifacts/m4/s22");
    let workloads = flags.str_or("workloads", "a,b,c,d,e,f");
    let distribution = flags.str_or("distribution", "both");
    let theta: f64 =
        flags.get("theta").map_or(Ok(0.99), |v| v.parse()).map_err(|e| format!("--theta: {e}"))?;
    let mem_budget_mb: u64 = flags.u64_or("mem-budget-mb", 64)?;
    let dataset_multiple: u64 = flags.u64_or("dataset-multiple", 10)?;
    let value_size: usize = flags.usize_or("value-size", 1024)?;
    let duration = Duration::from_secs(flags.u64_or("duration", 20)?);
    let conns: usize = flags.usize_or("conns", 8)?;
    let pipeline: usize = flags.usize_or("pipeline", 8)?;
    let cells: u16 = flags.u16_or("cells", 4)?;
    let pin_start = flags.str_or("pin-start", "4");
    let seed: u64 = flags.u64_or("seed", 0x1D0C_2026)?;
    let data_root = flags.str_or("data-root", &std::env::temp_dir().to_string_lossy());
    let bin = flags.str_or("infinityd-bin", "target/release/infinityd");
    let reference_box = flags.bool("reference-box");
    let env_ok = env_gate(&flags)?;

    let dataset_bytes = mem_budget_mb * (1 << 20) * dataset_multiple;
    let keys = (dataset_bytes / value_size as u64).max(100);
    let mut m = Measurements::new();

    // Preamble (ADR-0025 D3): every adaptation named before any number.
    m.note("harness: inf-bench ycsb — memtier cannot drive a 10× RAM zipfian tier workload (§6)");
    m.note(
        "E-adaptation: cursor-scan slices (SCAN <cursor> COUNT 1..=100, pipeline 1) — the \
         keyspace is unordered, no ordered range-scan exists (documented deviation)",
    );
    m.note(
        "value shape: single constant-byte value of --value-size (default 1 KiB ≈ YCSB's \
         10×100 B fields); D's `latest` uses a per-connection frontier estimate",
    );
    m.note(format!(
        "dataset: {keys} keys × {value_size} B = {} MiB = {dataset_multiple}× the {mem_budget_mb} \
         MiB memory budget · seed {seed:#x} · θ {theta}",
        dataset_bytes >> 20
    ));

    // Attach mode (M4-S23 soak legs) drives an already-running node and
    // an existing namespace; otherwise this run owns node + DDL.
    let attach_port: Option<u16> = match flags.get("attach-port") {
        Some(v) => Some(v.parse().map_err(|e| format!("--attach-port: {e}"))?),
        None => None,
    };
    let (_data_guard, _server_guard, port) = match attach_port {
        Some(p) => (None, None, p),
        None => {
            // Node boot: durable, real data dir (tier files need a device).
            let guard = DataDirGuard::create(
                PathBuf::from(&data_root).join(format!("inf-m4-s22-{}", std::process::id())),
            )?;
            let dir_s = guard.0.to_string_lossy().into_owned();
            let server: ServerGuard =
                spawn_infinityd(&bin, cells, &["--data-dir", &dir_s, "--pin-start", &pin_start])?;
            let port = server.port;
            (Some(guard), Some(server), port)
        }
    };

    let ns = flags.str_or("ns", "ycsb");
    if attach_port.is_none() {
        let budget_arg = format!("{mem_budget_mb}mb");
        // Disk budget: dataset + WA-gate headroom (3×) + the 5% reserve.
        let disk_budget_arg = format!("{}mb", (dataset_bytes * 4) >> 20);
        control(
            port,
            &[
                b"INF.NS",
                b"CREATE",
                ns.as_bytes(),
                b"MODE",
                b"durable",
                b"FSYNC",
                b"everysec",
                b"MEM-BUDGET",
                budget_arg.as_bytes(),
                b"DISK-BUDGET",
                disk_budget_arg.as_bytes(),
            ],
        )?;
    }

    // Mode probe (the S20 goes-stale-loudly pattern, inverted: a wired
    // plane flips this run to the real tiered rows automatically).
    // `--named-absent` forces the harness-validation rendering — the soak
    // uses it while its target is a durable (non-tiered) namespace.
    let (tiered_live, row_ns) = if flags.bool("named-absent") {
        m.note(format!(
            "mode: HARNESS-VALIDATION forced by --named-absent — rows run against `{ns}` \
             with the tiered split rendered named-absent; no tiered gate row is produced"
        ));
        (false, ns.clone())
    } else {
        match probe_tiered(port, &ns)? {
            None => {
                m.note(
                    "mode: TIERED — the D8 refusal is lifted; rows run against the tiered \
                     namespace",
                );
                (true, ns.clone())
            }
            Some(refusal) if attach_port.is_some() => {
                return Err(format!(
                    "attach mode: namespace `{ns}` is tiered but unwired ({refusal}) — pass a \
                     reachable namespace plus --named-absent for the honest-subset legs"
                ));
            }
            Some(refusal) => {
                // Harness-validation mode: same generator, rows, and
                // report machinery over a memory namespace. Cap the
                // dataset — this mode loads it into plain RAM.
                if dataset_bytes > (4 << 30) {
                    return Err(format!(
                        "harness-validation mode loads the dataset into RAM and {} MiB exceeds \
                         the 4 GiB safety cap — lower --mem-budget-mb/--dataset-multiple, or \
                         wire the data plane (M4-S26) for a real tiered run",
                        dataset_bytes >> 20
                    ));
                }
                m.note(format!(
                    "mode: HARNESS-VALIDATION (named-absent tiered rows) — measured fact: \
                     `{refusal}`; rows run against a memory-mode namespace to validate the \
                     generator, loader, and report machinery; no tiered gate row is produced"
                ));
                control(port, &[b"INF.NS", b"CREATE", b"ycsb-mem", b"MODE", b"memory"])?;
                (false, "ycsb-mem".to_string())
            }
        }
    };

    // Generator self-checks before any row (refusal, not footnote).
    let zipf = Zipf::new(keys, theta);
    m.note(validate_zipf(&zipf, seed, 2_000_000)?);
    let base_spec = RowSpec {
        port,
        ns: row_ns.clone(),
        conns,
        pipeline,
        duration,
        warmup: Duration::from_secs(1),
        keys,
        value_size,
        seed,
    };
    if flags.bool("verify-seed") {
        m.note(verify_seed(&base_spec, &zipf, 100_000)?);
    }

    // Deterministic loader: partitioned exact-once fill, then DBSIZE
    // must equal the key count — the loader's own honesty assert.
    // `--skip-fill` (soak legs after the first) trusts an earlier fill;
    // the DBSIZE assert is skipped with it because later rows insert.
    if flags.bool("skip-fill") {
        m.note("loader: skipped (--skip-fill — an earlier leg of this campaign filled)");
    } else {
        println!("== ycsb: loading {keys} keys ({} MiB) ==", dataset_bytes >> 20);
        let fill_spec = LoadSpec {
            port,
            conns,
            pipeline: 16,
            fill: Some(keys),
            keys,
            key_prefix: KEY_PREFIX.into(),
            key_size: KEY_SIZE,
            value_size,
            seed,
            setup: vec![vec![b"INF.NS".to_vec(), b"USE".to_vec(), row_ns.clone().into_bytes()]],
            ..Default::default()
        };
        // A loader converges, it does not benchmark: sustained pipelined
        // SETs into a durable namespace can draw typed admission
        // refusals (the M1-S07/M2 backpressure design working — observed
        // live: 76/655360 refused on the first S23 soak fill), so short
        // fills re-run bounded, idempotently, with the refusals
        // disclosed rather than silently absorbed.
        let mut fill_report = run_load(&fill_spec)?;
        let mut attempts = 1u32;
        loop {
            let loaded = ns_dbsize(port, &row_ns, cells)?;
            if loaded == keys {
                break;
            }
            if attempts >= 3 {
                return Err(format!(
                    "loader integrity: DBSIZE {loaded} != {keys} after {attempts} fill \
                     passes ({} error replies on the last) — not admission backpressure, \
                     investigate before trusting any row",
                    fill_report.errors
                ));
            }
            m.note(format!(
                "loader: pass {attempts} ended {loaded}/{keys} keys ({} error replies — \
                 admission backpressure is a typed refusal, not silence); refilling",
                fill_report.errors
            ));
            fill_report = run_load(&fill_spec)?;
            attempts += 1;
        }
        m.note(format!(
            "loader: {keys} keys in {:.1}s ({:.0} sets/s, {} passes), DBSIZE == keys asserted",
            fill_report.elapsed_s, fill_report.ops_per_sec, attempts
        ));
        m.raw_section("loader fill", &render(&fill_report));
    }

    // Row set: selected workloads × distributions (uniform variants for
    // the read/update rows only — `latest` and scans keep their shapes).
    // `--fill-only` stops here: the report carries the loader artifact.
    let selected: Vec<&str> = if flags.bool("fill-only") {
        Vec::new()
    } else {
        workloads.split(',').map(str::trim).collect()
    };
    let mut rows: Vec<(String, &Workload, Dist)> = Vec::new();
    for w in &WORKLOADS {
        if !selected.contains(&w.id) {
            continue;
        }
        if distribution != "uniform" {
            rows.push((format!("ycsb-{}-{}", w.id, w.dist.name()), w, w.dist));
        }
        if (distribution == "uniform" || distribution == "both")
            && w.dist == Dist::Zipfian
            && w.scan_pct == 0
        {
            rows.push((format!("ycsb-{}-uniform", w.id), w, Dist::Uniform));
        }
    }

    let mut saturation_done = false;
    let mut mem_hits: Vec<(String, MemHit)> = Vec::new();
    for (name, w, dist) in &rows {
        println!("== ycsb row: {name} ==");
        m.row_open(name);
        let spec = RowSpec { ..base_spec.clone() };
        let w_effective = Workload { dist: *dist, ..**w };
        // Cold-counter baseline *before* the row: the memory-hit
        // derivation needs this row's own cold reads, not the node's
        // lifetime total (a soak leg is row N of hundreds).
        let pre = if tiered_live { Some(scrape_cells(port, cells)?) } else { None };
        let out = run_row(&spec, &w_effective, &zipf)?;
        let mut body = render_row(&out, w, *dist);
        let scrape = scrape_cells(port, cells)?;
        let mem = pre.as_ref().map(|pre| {
            let delta =
                |field: &str| sum_field(&scrape, field).saturating_sub(sum_field(pre, field));
            derive_mem_hit(
                &out,
                delta("cold_reads_issued"),
                delta("tiering_cold_resolves"),
                max_field(&scrape, "tiering_cold_p50_us"),
            )
        });
        body.push_str(&split_section(&scrape, tiered_live, mem.as_ref())?);
        if let Some(mem) = mem {
            mem_hits.push((name.clone(), mem));
        }
        // Tripwires in every row (§19): raw submit grouping.
        let (submits, sqes) = (sum_field(&scrape, "raw_submits"), sum_field(&scrape, "raw_sqes"));
        let grouping = if submits == 0 { 0.0 } else { sqes as f64 / submits as f64 };
        body.push_str(&format!("tripwire sqes/submit = {grouping:.1}\n"));
        m.raw_section(name, &body);
        // WA disposition per row (the S16 debt — real on tiered rows,
        // structurally "none" in harness-validation mode).
        let disposition = writeamp::disposition(&scrape)?;
        if let Some(value) = disposition.gate_value() {
            let worst = m.values.get("wa:write_amp_max").copied().unwrap_or(0.0);
            m.set("wa:write_amp_max", worst.max(value));
        }
        let blob = writeamp::blob_disposition(&scrape)?;
        m.row_write_amp(&format!("{} · blob: {}", disposition.render(), blob.render()));
        // Cold-read gate source (tiered rows only, worst loaded row).
        if tiered_live {
            let cold_p99_us = scrape
                .iter()
                .filter_map(|cells| cells.get("tiering_cold_p99_us"))
                .filter_map(|v| v.parse::<u64>().ok())
                .max()
                .unwrap_or(0);
            let worst = m.values.get("ycsb:cold_read_p99_ms").copied().unwrap_or(0.0);
            m.set("ycsb:cold_read_p99_ms", worst.max(cold_p99_us as f64 / 1000.0));
        }
        // §19 saturation disposition: once per run at the shared config
        // (the mixed-audit probe shape), on the first zipfian row.
        if !saturation_done && *dist == Dist::Zipfian && w.scan_pct == 0 {
            saturation_done = true;
            println!("== ycsb: saturation probe ({name}, +50% conns) ==");
            let probe_spec = RowSpec {
                conns: conns + conns / 2,
                duration: duration / 2,
                seed: seed ^ 0x5A7,
                ..spec.clone()
            };
            let probe = run_row(&probe_spec, &w_effective, &zipf)?;
            let probe_delta = delta_pct(out.ops_per_sec, probe.ops_per_sec);
            m.note(if probe_delta.abs() < 5.0 {
                format!(
                    "saturation ({name}): generator unsaturated at {conns} conns (+50% conns \
                     moved ops/s {probe_delta:+.1}%)"
                )
            } else {
                format!(
                    "saturation ({name}): GENERATOR-LIMITED at {conns} conns (+50% moved ops/s \
                     {probe_delta:+.1}%) — absolutes understate the server; deltas remain valid \
                     at fixed generator config"
                )
            });
        }
    }

    // Hot-set gate rows (tiered mode only): the memory-speed reference is
    // a fully-RAM-resident run through the *same instrument* (ADR-0064 D4),
    // compared on the client-derived memory-hit split (ADR-0071 D2).
    if tiered_live && !mem_hits.is_empty() {
        m.sidecar("mem-hit.tsv", render_mem_hit_tsv(&mem_hits));
        match flags.get("hot-set-reference") {
            None => m.note(
                "hot-set gate rows (ycsb:hot_set_*): PENDING the reference leg — run \
                 `inf-bench ycsb --dataset-multiple 1` in the same campaign and re-run this leg \
                 with `--hot-set-reference <that run's dir or mem-hit.tsv>`; this run publishes \
                 its own memory-hit split in `mem-hit.tsv` for that comparison",
            ),
            Some(path) => {
                let reference = load_mem_hit_tsv(path)?;
                let (note, section) = compare_hot_set(&mut m, &mem_hits, &reference, path);
                m.note(note);
                m.raw_section("hot-set gate: tiered vs RAM-resident reference leg", &section);
            }
        }
    }

    finish_report(
        "m4",
        &gates_list,
        &m,
        env_ok,
        reference_box,
        &artifacts_root,
        &format!(
            "cells: {cells} · conns: {conns} · pipeline: {pipeline} · duration: {}s · \
             dataset: {}× budget · ycsb rows: {} (M4-S22)",
            duration.as_secs(),
            dataset_multiple,
            rows.len()
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn workload_percentages_sum_to_100() {
        for w in &WORKLOADS {
            assert_eq!(
                w.read_pct + w.update_pct + w.insert_pct + w.rmw_pct + w.scan_pct,
                100,
                "workload {} op mix must sum to 100",
                w.id
            );
        }
    }

    #[test]
    fn zipf_hot_share_matches_analytic() {
        // The generator self-check at test scale: θ=0.99 over 100k ranks,
        // 300k draws — the empirical top-1% share lands on the zeta ratio.
        let zipf = Zipf::new(100_000, 0.99);
        let line = validate_zipf(&zipf, 0x1D0C_2026, 300_000).expect("skew within tolerance");
        assert!(line.contains("top-1% share"), "{line}");
    }

    #[test]
    fn zipf_uniform_theta_would_fail_validation() {
        // Negative space: a uniform stream pretending to be zipfian must
        // be refused (the plan's silent-RAM-resident pitfall).
        let zipf = Zipf::new(100_000, 0.99);
        let mut rng = SplitMix64::new(7);
        let mut hot = 0u64;
        let samples = 100_000u64;
        for _ in 0..samples {
            if rng.next_u64() % zipf.n < zipf.n / 100 {
                hot += 1;
            }
        }
        let measured = hot as f64 * 100.0 / samples as f64;
        assert!(
            (measured - zipf.top1_share * 100.0).abs() > 1.5,
            "uniform draws must not pass the zipfian self-check"
        );
    }

    /// A synthetic bimodal row: `mem` ops at `mem_us`, `cold` ops at
    /// `cold_us` — the shape every tiered row has.
    fn bimodal_row(mem: u64, mem_us: u64, cold: u64, cold_us: u64) -> RowOut {
        let mut hist = LogHistogram::new();
        for _ in 0..mem {
            hist.record(mem_us);
        }
        for _ in 0..cold {
            hist.record(cold_us);
        }
        RowOut {
            ops: mem + cold,
            errors: 0,
            nils: 0,
            ops_per_sec: 0.0,
            p50_us: hist.percentile(50.0),
            p99_us: hist.percentile(99.0),
            p999_us: hist.percentile(99.9),
            max_us: hist.max(),
            hot_share_pct: 0.0,
            checksum: 0,
            hist_us: hist,
        }
    }

    #[test]
    fn mem_hit_truncation_recovers_the_memory_population() {
        // 2% of ops cold at 2 ms; the rest memory hits at 80 µs. The
        // blended p99 sits in the cold mode (that is the lie the split
        // exists to prevent); the derived memory-hit p99 must not.
        let out = bimodal_row(196_000, 80, 4_000, 2_000);
        assert!(out.p99_us >= 2_000, "combined p99 is cold-dominated: {}", out.p99_us);
        let mem = derive_mem_hit(&out, 4_000, 9_320, 1_800);
        assert!(mem.eligible.is_ok(), "{:?}", mem.eligible);
        assert!((mem.cold_frac - 0.02).abs() < 1e-6, "cold_frac {}", mem.cold_frac);
        // LogHistogram reports the bucket's upper bound (~3% wide).
        for (label, value) in [("p50", mem.p50_us), ("p99", mem.p99_us), ("p99.9", mem.p999_us)] {
            assert!((80..=83).contains(&value), "memory-hit {label} = {value}, expected ~80 µs");
        }
    }

    #[test]
    fn mem_hit_refuses_when_the_populations_overlap() {
        // A memory population with a tail *slower* than the cold reads:
        // 180k hits at 80 µs, 15k at 900 µs, 5k cold at 600 µs. The
        // truncation cannot tell the slow memory tail from a cold read,
        // so the row renders and is refused a gate value — never quietly
        // published.
        let mut hist = LogHistogram::new();
        for _ in 0..180_000 {
            hist.record(80);
        }
        for _ in 0..15_000 {
            hist.record(900);
        }
        for _ in 0..5_000 {
            hist.record(600);
        }
        let out = RowOut {
            ops: 200_000,
            errors: 0,
            nils: 0,
            ops_per_sec: 0.0,
            p50_us: hist.percentile(50.0),
            p99_us: hist.percentile(99.0),
            p999_us: hist.percentile(99.9),
            max_us: hist.max(),
            hot_share_pct: 0.0,
            checksum: 0,
            hist_us: hist,
        };
        let mem = derive_mem_hit(&out, 5_000, 11_650, 600);
        let reason = mem.eligible.expect_err("overlapping populations must be refused");
        assert!(reason.contains("separation check FAILED"), "{reason}");
    }

    #[test]
    fn mem_hit_accepts_the_ram_resident_reference_leg() {
        // ADR-0071 D6 (readiness F25): `--dataset-multiple 1` demotes
        // nothing, so the server's cold histogram is empty and
        // `tiering_cold_p50_us` scrapes 0. Before D6 that made *every*
        // reference row ineligible, `compare_hot_set` excluded every
        // matched row, and the §7 hot-set gate could not bind at all —
        // phase 4 would have returned "NO gate value" however clean the
        // run was. A row with zero cold reads is unimodal: the
        // separation check is vacuous, not failed.
        let out = bimodal_row(200_000, 80, 0, 2_000);
        let mem = derive_mem_hit(&out, 0, 0, 0);
        assert!(mem.eligible.is_ok(), "{:?}", mem.eligible);
        assert_eq!(mem.cold_frac, 0.0);
        // keep == 1.0, so the truncation is the identity on the client
        // percentiles — the reference leg publishes its own numbers.
        assert_eq!(mem.p50_us, out.p50_us);
        assert_eq!(mem.p99_us, out.p99_us);
        assert_eq!(mem.p999_us, out.p999_us);
        assert!(mem.render().contains("separation: not applicable"), "{}", mem.render());
    }

    #[test]
    fn mem_hit_still_refuses_a_zero_cold_p50_when_cold_reads_happened() {
        // The narrow case the D6 change must NOT swallow: cold reads
        // occurred, so the population *is* bimodal, but the server
        // reported no cold service time. That is a broken instrument and
        // the row must carry no gate value (the F13 class).
        let out = bimodal_row(196_000, 80, 4_000, 2_000);
        let mem = derive_mem_hit(&out, 4_000, 9_320, 0);
        let reason = mem.eligible.expect_err("a cold row with no cold service time is broken");
        assert!(reason.contains("broken instrument"), "{reason}");
    }

    #[test]
    fn mem_hit_refuses_a_cold_dominated_row() {
        let out = bimodal_row(40_000, 80, 160_000, 2_000);
        let mem = derive_mem_hit(&out, 160_000, 320_000, 1_800);
        let reason = mem.eligible.expect_err("a mostly-cold row has no hot set to gate");
        assert!(reason.contains("cold fraction"), "{reason}");
    }

    #[test]
    fn mem_hit_refuses_too_few_ops_for_a_p999() {
        let out = bimodal_row(9_000, 80, 100, 2_000);
        let mem = derive_mem_hit(&out, 100, 233, 1_800);
        let reason = mem.eligible.expect_err("a short row cannot carry a p99.9 gate value");
        assert!(reason.contains("too few"), "{reason}");
    }

    #[test]
    fn mem_hit_tsv_round_trips_through_the_reference_carrier() {
        let rows = vec![
            (
                "ycsb-a-zipfian".to_string(),
                derive_mem_hit(&bimodal_row(196_000, 80, 4_000, 2_000), 4_000, 9_320, 1_800),
            ),
            (
                "ycsb-b-zipfian".to_string(),
                derive_mem_hit(&bimodal_row(40_000, 80, 160_000, 2_000), 160_000, 320_000, 1_800),
            ),
        ];
        let tsv = render_mem_hit_tsv(&rows);
        let dir = std::env::temp_dir().join(format!("inf-memhit-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("temp dir");
        std::fs::write(dir.join("mem-hit.tsv"), &tsv).expect("write");
        let loaded = load_mem_hit_tsv(&dir.to_string_lossy()).expect("load by directory");
        assert_eq!(loaded.len(), 2);
        let a = &loaded["ycsb-a-zipfian"];
        assert!(a.eligible);
        assert_eq!(a.p99_us, rows[0].1.p99_us);
        // The ineligible row round-trips as ineligible: the reference leg
        // must never lend a row its blessing.
        assert!(!loaded["ycsb-b-zipfian"].eligible);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn withdrawn_ram_hit_fields_do_not_refuse_a_tiered_row() {
        // The F17 regression, pinned: a scrape carrying the cold half but
        // none of the withdrawn ram-hit fields must render, not refuse.
        let mut cell = std::collections::BTreeMap::new();
        for (k, v) in [
            ("tiering_cold_p50_us", "1119"),
            ("tiering_cold_p99_us", "14591"),
            ("tiering_cold_p999_us", "56319"),
            ("cold_read_qd_p99", "4"),
            ("coalesce_ratio_milli", "0"),
            ("tiering_ram_hit_split", "unmeasured-iteration-clock"),
            ("tiering_cold_resolves", "3948175"),
        ] {
            cell.insert(k.to_string(), v.to_string());
        }
        let body = split_section(&[cell.clone()], true, None).expect("tiered row must render");
        assert!(body.contains("tiering_cold_p99_us (worst cell) = 14591"), "{body}");
        assert!(body.contains("WITHDRAWN"), "{body}");
        // ...and the cold half is still a hard contract.
        cell.remove("tiering_cold_p99_us");
        let err = split_section(&[cell], true, None).expect_err("missing cold field must refuse");
        assert!(err.contains("cold split"), "{err}");
    }

    #[test]
    fn scan_cursor_parses_and_refuses() {
        let reply = b"*2\r\n$4\r\n1234\r\n*2\r\n$3\r\nabc\r\n$3\r\ndef\r\n";
        assert_eq!(scan_cursor(reply).as_deref(), Some(b"1234".as_slice()));
        assert_eq!(scan_cursor(b"+OK\r\n"), None);
        assert_eq!(scan_cursor(b"$-1\r\n"), None);
        assert_eq!(scan_cursor(b"-ERR nope\r\n"), None);
    }

    #[test]
    fn stream_generation_is_deterministic() {
        let spec = RowSpec {
            port: 0,
            ns: "t".into(),
            conns: 4,
            pipeline: 8,
            duration: Duration::from_secs(1),
            warmup: Duration::ZERO,
            keys: 10_000,
            value_size: 64,
            seed: 0xC0FFEE,
        };
        let zipf = Zipf::new(spec.keys, 0.99);
        let a = verify_seed(&spec, &zipf, 10_000).expect("deterministic");
        let b = verify_seed(&spec, &zipf, 10_000).expect("deterministic");
        assert_eq!(a, b);
    }
}
