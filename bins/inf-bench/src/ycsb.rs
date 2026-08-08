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
//! whose INFO scrape lacks the split-histogram fields fails loudly — the
//! split is the deliverable, not an optional extra.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::TcpStream;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use inf_foundation::LogHistogram;
use inf_foundation::rng::{Entropy, SplitMix64};

use crate::cli::Flags;
use crate::gaterun::{
    Measurements, ServerGuard, env_gate, finish_report, load_gates, scrape_cells, spawn_infinityd,
    sum_field,
};
use crate::load::{LoadSpec, make_key, render, run as run_load};
use crate::m2rows::delta_pct;
use crate::resp::{connect, encode_command, reply_len, request};
use crate::writeamp;

/// One deterministic-from-seed key shape shared by the loader and every
/// workload generator (`load::make_key` is the single source of truth).
const KEY_PREFIX: &str = "y:";
const KEY_SIZE: usize = 16;

/// The served-from split fields the report consumes from `INFO tiering`.
/// This array is the harness side of the M4-S26 contract: command wiring
/// must emit exactly these (µs percentiles from the resolver-tagged
/// completion histograms; `coalesce_ratio_milli` is the S10 tripwire).
/// A tiered run missing any of them fails — see [`split_section`].
const SPLIT_FIELDS: [&str; 8] = [
    "tiering_ram_hit_p50_us",
    "tiering_ram_hit_p99_us",
    "tiering_ram_hit_p999_us",
    "tiering_cold_p50_us",
    "tiering_cold_p99_us",
    "tiering_cold_p999_us",
    "cold_read_qd_p99",
    "coalesce_ratio_milli",
];

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

/// Renders the served-from split for one row. Tiered rows **must** find
/// every [`SPLIT_FIELDS`] entry in the scrape (max across cells binds —
/// per-cell histograms cannot merge from percentiles, disclosed); in
/// harness-validation mode the section is one named-absent contract line.
fn split_section(
    scrape: &[std::collections::BTreeMap<String, String>],
    tiered_live: bool,
) -> Result<String, String> {
    if !tiered_live {
        return Ok("memory-hit / cold split: NAMED-ABSENT — the tiered data plane is behind the \
                   ADR-0062 D8 refusal; M4-S26 emits the split service histograms \
                   (resolver-tagged {mutable, ro, cold}) under the SPLIT_FIELDS names\n"
            .into());
    }
    let mut lines = String::new();
    for field in SPLIT_FIELDS {
        let present = scrape.iter().any(|cells| cells.contains_key(field));
        if !present {
            return Err(format!(
                "tiered row ran but `{field}` is missing from INFO tiering — the split \
                 histogram contract (M4-S26 / SPLIT_FIELDS) is not met; a tiered row \
                 without the split is invalid by construction (§18/§19)"
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
    let cold_resolves = sum_field(scrape, "tiering_cold_resolves");
    lines.push_str(&format!("tiering_cold_resolves = {cold_resolves}\n"));
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
    for (name, w, dist) in &rows {
        println!("== ycsb row: {name} ==");
        m.row_open(name);
        let spec = RowSpec { ..base_spec.clone() };
        let w_effective = Workload { dist: *dist, ..**w };
        let out = run_row(&spec, &w_effective, &zipf)?;
        let mut body = render_row(&out, w, *dist);
        let scrape = scrape_cells(port, cells)?;
        body.push_str(&split_section(&scrape, tiered_live)?);
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
    // a fully-RAM-resident run through the *same instrument* — the S22
    // plan-interpretation note; produced only when the split fields exist.
    if tiered_live {
        m.note(
            "hot-set gate rows (ycsb:hot_set_*) require the reference leg — run \
             `inf-bench ycsb --dataset-multiple 1` in the same campaign and compare the \
             memory-hit split percentiles (S24 runbook step); this run reports the tiered \
             side only",
        );
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
