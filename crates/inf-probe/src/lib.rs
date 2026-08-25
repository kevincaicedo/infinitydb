//! The device probe behind `io-properties.toml` (M4.5-S34, ADR-0086 D7 —
//! the ScyllaDB `iotune` precedent; M4.5-S36, ADR-0088 D6 — the device
//! model; M4.5-S42, ADR-0091 — the lifecycle): measure the two log
//! barrier classes on a data directory's device and write the file
//! `infinityd` reads at boot. Shared by `inf probe-device` and by
//! `infinityd --device-probe auto`, which runs it at the first boot of a
//! data directory (before any cell starts) and whenever the file's
//! identity no longer matches the device underneath.
//!
//! Two policies, on a pre-written scratch file (written extents — the
//! `fallocate`d-but-unwritten trap of ADR-0086 D4 would make the FUA
//! rows lie):
//!
//! - **flush** — buffered `pwrite` + `fdatasync`: the FLUSH-class
//!   barrier (ADR-0013 D1), ending in a device-wide cache FLUSH.
//! - **fua** — `O_DIRECT | O_DSYNC` `pwrite` from a 4 KiB-aligned buffer:
//!   the kernel's write-through path, a FUA-flagged write where the
//!   device supports it.
//!
//! Sizes 4 KiB, 64 KiB, 256 KiB, 1 MiB — the frame sizes the group-commit
//! path produces from one record per iteration up to the `always`
//! saturation shape. Sequential offsets cycling through the scratch file,
//! one writer (the per-cell shape; concurrency multiplies FLUSH latency
//! and leaves FUA alone — ADR-0086 Context — which is the class's point).
//!
//! **Recommendation rule** (written into the file's comment too): `fua`
//! iff its 4 KiB p50 ≤ 0.75 × flush's **and** its 4 KiB p99 ≤ flush's
//! p99; `fua_max_frame_bytes` = the largest probed size whose fua p50
//! still beats flush's **and** whose fua p99 stays within 2× flush's p99
//! (the reference device's 1 MiB FUA tail — 45 ms vs 8.5 — is exactly
//! the shape this guard exists for). Otherwise `flush`.
//!
//! **Schema 2** (ADR-0088 D6) adds the device model the per-cell budget
//! spends — `write_bytes_per_s_256k`, `write_ops_per_s_4k` (the direct
//! rows' own throughput) and `write_ops_per_s_4k_qd4` (the direct 4 KiB
//! barrier rate at four concurrent writers on disjoint regions). Read
//! rows are declared and left at 0.
//!
//! **Schema 3** (ADR-0091 D2/D3) adds the identity of the device the
//! model describes ([`DeviceIdentity`]: filesystem type + UUID, mount
//! source, block sizes — safe text reads of `/proc/self/mountinfo`,
//! `/sys/dev/block` and `/dev/disk/by-uuid`), the reason when the direct
//! class is unavailable (`fua_unsupported` — tmpfs and friends yield a
//! `flush` verdict, never a failed probe), and one informational row at
//! the logical block size (`fua_p50_us_512`) for S39c's sub-block
//! question.
//!
//! Dev tool: `Instant::now()` and `std::thread` are fine here (not cell
//! code — the crate is a leaf the two binaries share, ADR-0091); the
//! output is a boot input, never a claim (L10 — the A/B is the claim).
#![forbid(unsafe_code)]

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub use inf_foundation::{DeviceIdentity, IdentityVerdict};

/// File name under the data directory.
pub const IO_PROPERTIES_FILE: &str = "io-properties.toml";
/// The schema this probe writes (ADR-0091 D2). Readers accept ≤ this.
pub const PROBE_SCHEMA: u64 = 3;
/// The recommendation rule's revision — bumped when the rule changes so
/// an old verdict is recognisable as one.
pub const PROBE_VERSION: u64 = 3;
/// `inf probe-device`'s default seconds per row.
pub const DEFAULT_SECONDS_PER_ROW: u64 = 2;
/// The in-boot probe's default seconds per row (ADR-0091 D1): eight
/// latency rows + one four-writer row + the pre-write ≈ 10 s.
pub const BOOT_SECONDS_PER_ROW: u64 = 1;
/// Bounds of `--seconds` / `--probe-seconds`.
pub const SECONDS_PER_ROW_RANGE: std::ops::RangeInclusive<u64> = 1..=60;

/// Scratch file size: one log segment — large enough that sequential
/// writes do not revisit a block within one row at any probed size, and
/// the same span the C probe of the S34 diagnosis used.
pub const SCRATCH_BYTES: u64 = 256 << 20;
/// Buffer and offset alignment (ADR-0086 D3's `FRAME_ALIGN`).
const ALIGN: usize = 4096;
const SIZES: [usize; 4] = [4 << 10, 64 << 10, 256 << 10, 1 << 20];
/// Writers of the concurrent barrier-rate row (ADR-0088 D2b/D6): four,
/// not the cell count the probe cannot know — the number is divided by
/// `cells` at boot, conservative in the direction that batches more.
const QD_WRITERS: usize = 4;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Policy {
    Flush,
    Fua,
}

impl Policy {
    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Policy::Flush => "flush",
            Policy::Fua => "fua",
        }
    }
}

/// One measured row.
#[derive(Copy, Clone, Debug)]
pub struct Row {
    pub policy: Policy,
    pub bytes: usize,
    pub p50_us: u64,
    pub p99_us: u64,
    pub barriers_per_sec: u64,
}

/// How long each row runs.
#[derive(Copy, Clone, Debug)]
pub struct ProbeOptions {
    pub seconds_per_row: u64,
}

impl ProbeOptions {
    /// `inf probe-device`'s default.
    pub const CLI: ProbeOptions = ProbeOptions { seconds_per_row: DEFAULT_SECONDS_PER_ROW };
    /// The in-boot default (ADR-0091 D1).
    pub const BOOT: ProbeOptions = ProbeOptions { seconds_per_row: BOOT_SECONDS_PER_ROW };
}

/// The probe's verdict and every value the file carries.
#[derive(Clone, Debug)]
pub struct Verdict {
    pub class: Policy,
    pub fua_max_frame_bytes: usize,
    pub fua_p50_us_4k: u64,
    pub flush_p50_us_4k: u64,
    /// Schema 2 (ADR-0088 D6): the device model rows.
    pub write_bytes_per_s_256k: u64,
    pub write_ops_per_s_4k: u64,
    pub write_ops_per_s_4k_qd4: u64,
    /// Schema 3 (ADR-0091 D3): why the direct class was not measured
    /// (`None` = it was), and the logical-block-size write-through row
    /// (`None` = not run: the logical block is ≥ 4 KiB or the class is
    /// unavailable).
    pub fua_unsupported: Option<String>,
    pub fua_512: Option<(u64, u64)>,
}

/// Everything one probe run produced.
#[derive(Clone, Debug)]
pub struct ProbeReport {
    pub rows: Vec<Row>,
    pub verdict: Verdict,
    pub identity: DeviceIdentity,
    pub seconds_per_row: u64,
    pub elapsed: Duration,
}

impl ProbeReport {
    /// One line per measured row, for the operator.
    #[must_use]
    pub fn row_lines(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .rows
            .iter()
            .map(|r| {
                format!(
                    "{:<5} bytes={:<8} p50={:>6} us  p99={:>6} us  barriers/s={:>6}",
                    r.policy.name(),
                    r.bytes,
                    r.p50_us,
                    r.p99_us,
                    r.barriers_per_sec
                )
            })
            .collect();
        if let Some(reason) = &self.verdict.fua_unsupported {
            out.push(format!("fua   unsupported on this filesystem: {reason}"));
        } else {
            out.push(format!(
                "fua   bytes={:<8} writers={QD_WRITERS} barriers/s={:>6} (aggregate)",
                SIZES[0], self.verdict.write_ops_per_s_4k_qd4
            ));
        }
        out
    }

    /// The file's text (schema 3).
    #[must_use]
    pub fn render(&self) -> String {
        render(&self.rows, &self.verdict, &self.identity, self.seconds_per_row)
    }
}

/// Run the probe on `dir`'s device: pre-write the scratch file, measure
/// every row, gather the identity, remove the scratch. Never writes
/// `io-properties.toml` — [`write_io_properties`] does, so a caller can
/// look before it commits.
///
/// # Errors
/// The scratch file cannot be written (space, permissions, I/O) or a
/// measurement fails for a reason other than the direct class being
/// unavailable (that one degrades to a `flush` verdict, ADR-0091 D3).
pub fn probe(dir: &Path, opts: ProbeOptions) -> io::Result<ProbeReport> {
    if !SECONDS_PER_ROW_RANGE.contains(&opts.seconds_per_row) {
        return Err(io::Error::other("seconds per row wants 1..=60"));
    }
    std::fs::create_dir_all(dir)?;
    let scratch = dir.join(".io-probe.scratch");
    let result = probe_rows(dir, &scratch, opts);
    let _ = std::fs::remove_file(&scratch);
    result
}

/// Write `text` as `<dir>/io-properties.toml` — a temporary, synced,
/// renamed into place, the directory synced (the M2.5-S01 shape).
///
/// # Errors
/// Any I/O failure; the previous file, if any, is untouched on failure.
pub fn write_io_properties(dir: &Path, text: &str) -> io::Result<PathBuf> {
    let path = dir.join(IO_PROPERTIES_FILE);
    let tmp = dir.join("io-properties.toml.new");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    File::open(dir)?.sync_all()?;
    Ok(path)
}

/// [`probe`] then [`write_io_properties`].
///
/// # Errors
/// Either step's.
pub fn run_and_write(dir: &Path, opts: ProbeOptions) -> io::Result<(PathBuf, ProbeReport)> {
    let report = probe(dir, opts)?;
    let path = write_io_properties(dir, &report.render())?;
    Ok((path, report))
}

fn probe_rows(dir: &Path, scratch: &Path, opts: ProbeOptions) -> io::Result<ProbeReport> {
    let started = Instant::now();
    let per_row = Duration::from_secs(opts.seconds_per_row);
    let identity = identity_of(dir);
    prewrite(scratch)?;
    let mut rows = Vec::new();
    let mut fua_unsupported = None;
    for &bytes in &SIZES {
        rows.push(measure(scratch, Policy::Flush, bytes, per_row)?);
        if fua_unsupported.is_some() {
            continue;
        }
        match measure(scratch, Policy::Fua, bytes, per_row) {
            Ok(row) => rows.push(row),
            // ADR-0091 D3: a filesystem without the direct class is the
            // FLUSH class's job, never a failed probe.
            Err(err) if is_direct_refusal(&err) => fua_unsupported = Some(err.to_string()),
            Err(err) => return Err(err),
        }
    }
    let qd4 = if fua_unsupported.is_some() {
        0
    } else {
        measure_concurrent(scratch, SIZES[0], per_row, QD_WRITERS)?
    };
    // The logical-block row (ADR-0091 D3): informational, for S39c.
    let fua_512 = match (fua_unsupported.is_none(), identity.block_logical_bytes) {
        (true, logical) if logical > 0 && (logical as usize) < SIZES[0] => {
            match measure(scratch, Policy::Fua, logical as usize, per_row) {
                Ok(row) => Some((row.p50_us, row.p99_us)),
                Err(err) if is_direct_refusal(&err) => None,
                Err(err) => return Err(err),
            }
        }
        _ => None,
    };
    let mut verdict = recommend(&rows, qd4);
    verdict.fua_unsupported = fua_unsupported;
    verdict.fua_512 = fua_512;
    Ok(ProbeReport {
        rows,
        verdict,
        identity,
        seconds_per_row: opts.seconds_per_row,
        elapsed: started.elapsed(),
    })
}

/// `EINVAL` at open or at the first write (tmpfs, overlay, some network
/// mounts) and `Unsupported` off Linux: the direct class is unavailable.
fn is_direct_refusal(err: &io::Error) -> bool {
    matches!(err.kind(), io::ErrorKind::InvalidInput | io::ErrorKind::Unsupported)
}

/// Written extents end to end: buffered zeros, synced, so both policies
/// overwrite allocated storage (ADR-0086 D4).
fn prewrite(scratch: &Path) -> io::Result<()> {
    let file =
        OpenOptions::new().read(true).write(true).create(true).truncate(true).open(scratch)?;
    let zeros = vec![0u8; 1 << 20];
    let mut at = 0u64;
    while at < SCRATCH_BYTES {
        file.write_all_at(&zeros, at)?;
        at += zeros.len() as u64;
    }
    file.sync_all()?;
    Ok(())
}

fn open(scratch: &Path, policy: Policy) -> io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true).write(true);
    if policy == Policy::Fua {
        #[cfg(target_os = "linux")]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_DIRECT | libc::O_DSYNC);
        }
        #[cfg(not(target_os = "linux"))]
        {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "the fua class is Linux-only (ADR-0086 D1)",
            ));
        }
    }
    options.open(scratch)
}

/// One row: sequential `bytes`-sized writes for `per_row`, each followed
/// by the policy's barrier; latency per barrier.
fn measure(scratch: &Path, policy: Policy, bytes: usize, per_row: Duration) -> io::Result<Row> {
    let file = open(scratch, policy)?;
    // Aligned source (the ADR-0054 D2 shape): `O_DIRECT` needs it; the
    // buffered policy is indifferent. A non-zero pattern so the device
    // cannot elide the write.
    let mut raw = vec![0u8; bytes + ALIGN];
    let at = raw.as_ptr().align_offset(ALIGN);
    let buf = &mut raw[at..at + bytes];
    for (i, b) in buf.iter_mut().enumerate() {
        *b = (i % 251) as u8;
    }
    let mut latencies_us: Vec<u64> = Vec::with_capacity(16 << 10);
    let started = Instant::now();
    let mut offset = 0u64;
    while started.elapsed() < per_row {
        let t0 = Instant::now();
        file.write_all_at(buf, offset)?;
        if policy == Policy::Flush {
            file.sync_data()?;
        }
        latencies_us.push(u64::try_from(t0.elapsed().as_micros()).unwrap_or(u64::MAX));
        offset += bytes as u64;
        if offset + bytes as u64 > SCRATCH_BYTES {
            offset = 0;
        }
    }
    let elapsed = started.elapsed().as_secs_f64().max(1e-9);
    latencies_us.sort_unstable();
    let pct = |p: f64| -> u64 {
        if latencies_us.is_empty() {
            return 0;
        }
        let rank = ((p / 100.0) * latencies_us.len() as f64).ceil().max(1.0) as usize;
        latencies_us[rank.min(latencies_us.len()) - 1]
    };
    Ok(Row {
        policy,
        bytes,
        p50_us: pct(50.0),
        p99_us: pct(99.0),
        barriers_per_sec: (latencies_us.len() as f64 / elapsed) as u64,
    })
}

/// The direct 4 KiB barrier rate at `writers` concurrent writers, each on
/// its own quarter of the scratch file (ADR-0088 D6). Aggregate
/// barriers/s — one thread per writer, the same loop as `measure` minus
/// the latency histogram. Not cell code: `std::thread` is fine here.
fn measure_concurrent(
    scratch: &Path,
    bytes: usize,
    per_row: Duration,
    writers: usize,
) -> io::Result<u64> {
    let region = SCRATCH_BYTES / writers as u64;
    let mut handles = Vec::with_capacity(writers);
    for w in 0..writers {
        let scratch = scratch.to_path_buf();
        handles.push(std::thread::spawn(move || -> io::Result<u64> {
            let file = open(&scratch, Policy::Fua)?;
            let mut raw = vec![0u8; bytes + ALIGN];
            let at = raw.as_ptr().align_offset(ALIGN);
            let buf = &mut raw[at..at + bytes];
            for (i, b) in buf.iter_mut().enumerate() {
                *b = ((i + w) % 251) as u8;
            }
            let base = region * w as u64;
            let started = Instant::now();
            let mut offset = base;
            let mut count = 0u64;
            while started.elapsed() < per_row {
                file.write_all_at(buf, offset)?;
                count += 1;
                offset += bytes as u64;
                if offset + bytes as u64 > base + region {
                    offset = base;
                }
            }
            Ok(count)
        }));
    }
    let started = Instant::now();
    let mut total = 0u64;
    for h in handles {
        total += h.join().map_err(|_| io::Error::other("probe writer panicked"))??;
    }
    let elapsed = started.elapsed().max(per_row).as_secs_f64().max(1e-9);
    Ok((total as f64 / elapsed) as u64)
}

fn row(rows: &[Row], policy: Policy, bytes: usize) -> Option<&Row> {
    rows.iter().find(|r| r.policy == policy && r.bytes == bytes)
}

/// The recommendation rule (module docs).
#[must_use]
pub fn recommend(rows: &[Row], write_ops_per_s_4k_qd4: u64) -> Verdict {
    let flush_4k = row(rows, Policy::Flush, SIZES[0]);
    let fua_4k = row(rows, Policy::Fua, SIZES[0]);
    // The model rows (ADR-0088 D6): the direct 256 KiB row's throughput
    // and the direct 4 KiB row's barrier rate.
    let write_bytes_per_s_256k =
        row(rows, Policy::Fua, SIZES[2]).map_or(0, |r| r.barriers_per_sec * r.bytes as u64);
    let write_ops_per_s_4k = fua_4k.map_or(0, |r| r.barriers_per_sec);
    let (Some(flush), Some(fua)) = (flush_4k, fua_4k) else {
        return Verdict {
            class: Policy::Flush,
            fua_max_frame_bytes: SIZES[0],
            fua_p50_us_4k: 0,
            flush_p50_us_4k: flush_4k.map_or(0, |r| r.p50_us),
            write_bytes_per_s_256k,
            write_ops_per_s_4k,
            write_ops_per_s_4k_qd4,
            fua_unsupported: None,
            fua_512: None,
        };
    };
    let wins_p50 = fua.p50_us * 4 <= flush.p50_us * 3;
    let wins_p99 = fua.p99_us <= flush.p99_us;
    let class = if wins_p50 && wins_p99 { Policy::Fua } else { Policy::Flush };
    let mut fua_max = 0;
    for &bytes in &SIZES {
        match (row(rows, Policy::Fua, bytes), row(rows, Policy::Flush, bytes)) {
            (Some(f), Some(b)) if f.p50_us < b.p50_us && f.p99_us <= 2 * b.p99_us => {
                fua_max = bytes;
            }
            _ => break,
        }
    }
    Verdict {
        class,
        fua_max_frame_bytes: fua_max.max(SIZES[0]),
        fua_p50_us_4k: fua.p50_us,
        flush_p50_us_4k: flush.p50_us,
        write_bytes_per_s_256k,
        write_ops_per_s_4k,
        write_ops_per_s_4k_qd4,
        fua_unsupported: None,
        fua_512: None,
    }
}

/// A double-quoted TOML string: the identity fields are kernel-provided
/// names (paths, types, UUIDs) — no control characters, but a quote or
/// backslash is escaped rather than trusted.
fn quoted(value: &str) -> String {
    let mut out = String::with_capacity(value.len() + 2);
    out.push('"');
    for ch in value.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            c if c.is_control() => out.push('?'),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

#[must_use]
fn render(rows: &[Row], verdict: &Verdict, identity: &DeviceIdentity, seconds: u64) -> String {
    let probed_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let mut out = String::new();
    out.push_str("# io-properties — written by `inf probe-device` (ADR-0086 D7)\n");
    out.push_str(
        "# rule: fua iff fua p50 <= 0.75 x flush p50 and fua p99 <= flush p99 at 4 KiB;\n",
    );
    out.push_str("# fua_max_frame_bytes = largest probed size whose fua p50 beats flush's and\n");
    out.push_str("#   whose fua p99 stays within 2x flush's p99 (the large-write tail guard).\n");
    out.push_str(
        "# absent file => flush under `--device-probe off`, probed at boot under `auto`\n",
    );
    out.push_str("#   (ADR-0091). `infinityd --barrier-class` overrides.\n");
    for r in rows {
        out.push_str(&format!(
            "# {:<5} bytes={:<8} p50_us={:<7} p99_us={:<7} barriers_per_sec={}\n",
            r.policy.name(),
            r.bytes,
            r.p50_us,
            r.p99_us,
            r.barriers_per_sec
        ));
    }
    out.push_str(&format!("barrier_class = \"{}\"\n", verdict.class.name()));
    out.push_str(&format!("fua_max_frame_bytes = {}\n", verdict.fua_max_frame_bytes));
    out.push_str(&format!("fua_p50_us_4k = {}\n", verdict.fua_p50_us_4k));
    out.push_str(&format!("flush_p50_us_4k = {}\n", verdict.flush_p50_us_4k));
    out.push_str(&format!("probed_at_unix_s = {probed_at}\n"));
    // Schema 2 (M4.5-S36, ADR-0088 D6): the device model. Read rows are
    // declared at 0 (unbudgeted) until a probe story with concurrency
    // measures them.
    out.push_str("# schema 2 (ADR-0088 D6): the device model the per-cell budget spends;\n");
    out.push_str("#   0 = not probed => that direction is unbudgeted (io_budget_model:absent).\n");
    out.push_str(&format!("probe_schema = {PROBE_SCHEMA}\n"));
    out.push_str(&format!("write_bytes_per_s_256k = {}\n", verdict.write_bytes_per_s_256k));
    out.push_str(&format!("write_ops_per_s_4k = {}\n", verdict.write_ops_per_s_4k));
    out.push_str(&format!("write_ops_per_s_4k_qd4 = {}\n", verdict.write_ops_per_s_4k_qd4));
    out.push_str("read_bytes_per_s_256k = 0\n");
    out.push_str("read_ops_per_s_4k = 0\n");
    // Schema 3 (M4.5-S42, ADR-0091 D2/D3): what the model describes.
    out.push_str("# schema 3 (ADR-0091 D2): the identity of the device this model describes.\n");
    out.push_str("#   A boot compares it to the data directory's device; a mismatch is a\n");
    out.push_str("#   stale model (re-probed under `auto`, refused under `off`). UUID decides\n");
    out.push_str("#   when both sides have one, else device_path + fs_type; empty = unknown.\n");
    out.push_str(&format!("fs_type = {}\n", quoted(&identity.fs_type)));
    out.push_str(&format!("fs_uuid = {}\n", quoted(&identity.fs_uuid)));
    out.push_str(&format!("device_path = {}\n", quoted(&identity.device_path)));
    out.push_str(&format!("device_major_minor = {}\n", quoted(&identity.device_major_minor)));
    out.push_str(&format!("block_logical_bytes = {}\n", identity.block_logical_bytes));
    out.push_str(&format!("block_physical_bytes = {}\n", identity.block_physical_bytes));
    out.push_str(&format!("kernel_release = {}\n", quoted(&identity.kernel_release)));
    out.push_str(&format!("probe_version = {PROBE_VERSION}\n"));
    out.push_str(&format!("probe_seconds_per_row = {seconds}\n"));
    if let Some(reason) = &verdict.fua_unsupported {
        out.push_str("# the direct class (O_DIRECT | O_DSYNC) was refused: flush is the class\n");
        out.push_str(&format!("fua_unsupported = {}\n", quoted(reason)));
    }
    // The logical-block write-through row (ADR-0091 D3): informational,
    // S39c's first discriminator; 0 = not run.
    let (p50, p99) = verdict.fua_512.unwrap_or((0, 0));
    out.push_str(&format!("fua_p50_us_512 = {p50}\n"));
    out.push_str(&format!("fua_p99_us_512 = {p99}\n"));
    out
}

// ---- identity (ADR-0091 D2): safe text reads, never FFI -------------------

/// The identity of the device holding `dir`. Never fails: a field the
/// host does not expose is left empty / 0 (the comparison compares only
/// what both sides carry).
#[must_use]
pub fn identity_of(dir: &Path) -> DeviceIdentity {
    let mut identity = DeviceIdentity::default();
    let Ok(canonical) = std::fs::canonicalize(dir) else { return identity };
    let dev = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&canonical).map(|m| m.dev()).ok()
    };
    if let Some(dev) = dev {
        let (major, minor) = decode_dev(dev);
        identity.device_major_minor = format!("{major}:{minor}");
        let (logical, physical) = block_sizes(major, minor);
        identity.block_logical_bytes = logical;
        identity.block_physical_bytes = physical;
    }
    if let Ok(text) = std::fs::read_to_string("/proc/self/mountinfo")
        && let Some(mount) = mount_for(&text, &canonical, identity.device_major_minor.as_str())
    {
        identity.fs_type = mount.fs_type;
        identity.device_path = mount.source.filter(|s| s.starts_with("/dev/")).unwrap_or_default();
    }
    if !identity.device_path.is_empty() {
        identity.fs_uuid = uuid_of(&identity.device_path);
    }
    identity.kernel_release = std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default();
    identity
}

/// Linux `dev_t` decoding (the `makedev` layout: 12-bit major split
/// across bits 8–19 and 32–43, 20-bit minor split across 0–7 and 20–31).
#[must_use]
pub fn decode_dev(dev: u64) -> (u64, u64) {
    let major = ((dev >> 8) & 0xfff) | ((dev >> 32) & !0xfff);
    let minor = (dev & 0xff) | ((dev >> 12) & !0xff);
    (major, minor)
}

/// `/sys/dev/block/<maj:min>/queue/{logical,physical}_block_size`, or the
/// parent's `queue/` for a partition (which has none of its own).
fn block_sizes(major: u64, minor: u64) -> (u32, u32) {
    let base = PathBuf::from(format!("/sys/dev/block/{major}:{minor}"));
    let read = |dir: &Path, name: &str| -> u32 {
        std::fs::read_to_string(dir.join("queue").join(name))
            .ok()
            .and_then(|s| s.trim().parse().ok())
            .unwrap_or(0)
    };
    for dir in [base.clone(), base.join("..")] {
        let logical = read(&dir, "logical_block_size");
        if logical > 0 {
            return (logical, read(&dir, "physical_block_size"));
        }
    }
    (0, 0)
}

/// One `/proc/self/mountinfo` line's facts the identity needs.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MountRecord {
    pub major_minor: String,
    pub mount_point: String,
    pub fs_type: String,
    pub source: Option<String>,
}

/// Parse one `mountinfo` line: `ID PARENT MAJ:MIN ROOT MOUNTPOINT OPTS
/// [optional…] - FSTYPE SOURCE SUPEROPTS`. Octal escapes (`\040`) in
/// paths are decoded.
#[must_use]
pub fn parse_mountinfo_line(line: &str) -> Option<MountRecord> {
    let (pre, post) = line.split_once(" - ")?;
    let pre: Vec<&str> = pre.split_whitespace().collect();
    let post: Vec<&str> = post.split_whitespace().collect();
    if pre.len() < 5 || post.is_empty() {
        return None;
    }
    let source = post.get(1).map(|s| unescape_octal(s));
    Some(MountRecord {
        major_minor: pre[2].to_owned(),
        mount_point: unescape_octal(pre[4]),
        fs_type: post[0].to_owned(),
        source: source.filter(|s| s != "none"),
    })
}

fn unescape_octal(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        // `\ooo`: a backslash followed by exactly three octal digits.
        if bytes[i] == b'\\'
            && i + 4 <= bytes.len()
            && let Ok(v) = u8::from_str_radix(&s[i + 1..i + 4], 8)
        {
            out.push(v);
            i += 4;
            continue;
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// The mount holding `path`: among the lines whose `MAJ:MIN` equals the
/// directory's device, the longest mount point that prefixes the path;
/// failing that (a bind mount whose device differs from what `stat`
/// reports), the longest prefix over every line.
#[must_use]
pub fn mount_for(mountinfo: &str, path: &Path, major_minor: &str) -> Option<MountRecord> {
    let path = path.to_string_lossy();
    let records: Vec<MountRecord> = mountinfo.lines().filter_map(parse_mountinfo_line).collect();
    let prefixes = |same_dev: bool| {
        records
            .iter()
            .filter(|r| !same_dev || r.major_minor == major_minor)
            .filter(|r| {
                let mp = r.mount_point.as_str();
                path.as_ref() == mp
                    || mp == "/"
                    || path.starts_with(&format!("{}/", mp.trim_end_matches('/')))
            })
            .max_by_key(|r| r.mount_point.len())
            .cloned()
    };
    prefixes(true).or_else(|| prefixes(false))
}

/// The filesystem UUID of `device_path` via `/dev/disk/by-uuid` (a
/// directory of symlinks named by UUID); empty when unavailable.
fn uuid_of(device_path: &str) -> String {
    let Ok(target) = std::fs::canonicalize(device_path) else { return String::new() };
    let Ok(entries) = std::fs::read_dir("/dev/disk/by-uuid") else { return String::new() };
    for entry in entries.flatten() {
        if std::fs::canonicalize(entry.path()).is_ok_and(|p| p == target) {
            return entry.file_name().to_string_lossy().into_owned();
        }
    }
    String::new()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(policy: Policy, bytes: usize, p50_us: u64, p99_us: u64) -> Row {
        Row { policy, bytes, p50_us, p99_us, barriers_per_sec: 0 }
    }

    #[test]
    fn recommendation_follows_the_rule() {
        // The reference device's probe record: fua wins p50 and p99 at
        // 4 KiB, keeps winning through 256 KiB, loses at 1 MiB.
        let rows = vec![
            r(Policy::Flush, 4 << 10, 915, 1272),
            r(Policy::Fua, 4 << 10, 294, 648),
            r(Policy::Flush, 64 << 10, 924, 1849),
            r(Policy::Fua, 64 << 10, 372, 677),
            r(Policy::Flush, 256 << 10, 1400, 2500),
            r(Policy::Fua, 256 << 10, 900, 2536),
            r(Policy::Flush, 1 << 20, 1602, 3412),
            r(Policy::Fua, 1 << 20, 3058, 45311),
        ];
        let v = recommend(&rows, 9_918);
        assert_eq!(v.class, Policy::Fua);
        assert_eq!(v.fua_max_frame_bytes, 256 << 10);
        assert_eq!(v.fua_p50_us_4k, 294);
        assert_eq!(v.write_ops_per_s_4k_qd4, 9_918);
        // A device where fua's p99 is worse stays flush (the falsifier).
        let mut worse = rows;
        worse[1].p99_us = 2000;
        assert_eq!(recommend(&worse, 0).class, Policy::Flush);
    }

    /// ADR-0088 D6: the model rows come from the direct rows' measured
    /// throughput — bytes × barriers/s at 256 KiB, barriers/s at 4 KiB.
    #[test]
    fn schema_2_rows_derive_from_the_direct_rows() {
        let mut rows =
            vec![r(Policy::Flush, 4 << 10, 900, 1200), r(Policy::Fua, 4 << 10, 300, 600)];
        rows[1].barriers_per_sec = 2_898;
        let mut r256 = r(Policy::Fua, 256 << 10, 467, 1413);
        r256.barriers_per_sec = 1_949;
        rows.push(r256);
        let v = recommend(&rows, 9_918);
        assert_eq!(v.write_ops_per_s_4k, 2_898);
        assert_eq!(v.write_bytes_per_s_256k, 1_949 * (256 << 10));
        let text = render(&rows, &v, &DeviceIdentity::default(), 2);
        assert!(text.contains("probe_schema = 3\n"));
        assert!(text.contains(&format!("write_bytes_per_s_256k = {}\n", 1_949 * (256 << 10))));
        assert!(text.contains("write_ops_per_s_4k_qd4 = 9918\n"));
        assert!(text.contains("read_ops_per_s_4k = 0\n"));
    }

    /// The file stays the flat `key = value` subset the inf-server parser
    /// reads, in a fixed key order (schema 3 appends to schema 2).
    #[test]
    fn rendered_file_parses_as_the_flat_subset() {
        let rows = vec![r(Policy::Flush, 4 << 10, 900, 1200), r(Policy::Fua, 4 << 10, 300, 600)];
        let identity = DeviceIdentity {
            fs_type: "ext4".into(),
            fs_uuid: "c97c5418".into(),
            device_path: "/dev/nvme0n1p3".into(),
            device_major_minor: "259:3".into(),
            block_logical_bytes: 512,
            block_physical_bytes: 512,
            kernel_release: "7.0.0".into(),
        };
        let text = render(&rows, &recommend(&rows, 0), &identity, 1);
        let keys: Vec<&str> = text
            .lines()
            .filter(|l| !l.starts_with('#'))
            .map(|l| l.split('=').next().unwrap().trim())
            .collect();
        assert_eq!(
            keys,
            [
                "barrier_class",
                "fua_max_frame_bytes",
                "fua_p50_us_4k",
                "flush_p50_us_4k",
                "probed_at_unix_s",
                "probe_schema",
                "write_bytes_per_s_256k",
                "write_ops_per_s_4k",
                "write_ops_per_s_4k_qd4",
                "read_bytes_per_s_256k",
                "read_ops_per_s_4k",
                "fs_type",
                "fs_uuid",
                "device_path",
                "device_major_minor",
                "block_logical_bytes",
                "block_physical_bytes",
                "kernel_release",
                "probe_version",
                "probe_seconds_per_row",
                "fua_p50_us_512",
                "fua_p99_us_512",
            ]
        );
        assert!(text.contains("barrier_class = \"fua\""));
        assert!(text.contains("fs_uuid = \"c97c5418\"\n"));
        assert!(text.contains("device_path = \"/dev/nvme0n1p3\"\n"));
        assert!(text.contains("block_logical_bytes = 512\n"));
    }

    /// ADR-0091 D3: a refused direct class is a `flush` verdict with the
    /// reason on the record, never a failed probe.
    #[test]
    fn a_refused_direct_class_renders_flush_with_the_reason() {
        let rows = vec![r(Policy::Flush, 4 << 10, 900, 1200)];
        let mut v = recommend(&rows, 0);
        assert_eq!(v.class, Policy::Flush);
        assert_eq!(v.fua_max_frame_bytes, 4 << 10);
        v.fua_unsupported = Some("open: Invalid argument (os error 22)".into());
        let text = render(&rows, &v, &DeviceIdentity::default(), 1);
        assert!(text.contains("barrier_class = \"flush\"\n"));
        assert!(text.contains("fua_unsupported = \"open: Invalid argument (os error 22)\"\n"));
        assert!(text.contains("write_ops_per_s_4k = 0\n"));
        assert!(text.contains("fua_p50_us_512 = 0\n"));
        assert!(is_direct_refusal(&io::Error::from_raw_os_error(22)));
        assert!(!is_direct_refusal(&io::Error::from_raw_os_error(28))); // ENOSPC
    }

    /// The identity readers: a `mountinfo` line, the bind-mount fallback,
    /// and the `dev_t` layout.
    #[test]
    fn identity_readers_parse_the_kernel_formats() {
        let text = "34 44 0:29 / /run rw,nosuid,nodev shared:13 - tmpfs tmpfs rw,size=1k\n\
                    44 1 259:3 / / rw,relatime shared:1 - ext4 /dev/nvme0n1p3 rw\n\
                    99 44 259:3 /home/x /mnt/data\\040dir rw - ext4 /dev/nvme0n1p3 rw\n";
        let root = parse_mountinfo_line(text.lines().nth(1).unwrap()).unwrap();
        assert_eq!(root.fs_type, "ext4");
        assert_eq!(root.source.as_deref(), Some("/dev/nvme0n1p3"));
        assert_eq!(root.mount_point, "/");
        let bind = parse_mountinfo_line(text.lines().nth(2).unwrap()).unwrap();
        assert_eq!(bind.mount_point, "/mnt/data dir");
        // The longest matching mount point on the same device wins.
        let m = mount_for(text, Path::new("/mnt/data dir/infinity"), "259:3").unwrap();
        assert_eq!(m.mount_point, "/mnt/data dir");
        let m = mount_for(text, Path::new("/var/lib/x"), "259:3").unwrap();
        assert_eq!(m.mount_point, "/");
        // A pseudo filesystem has no device path.
        let m = mount_for(text, Path::new("/run/x"), "0:29").unwrap();
        assert_eq!(m.fs_type, "tmpfs");
        assert_eq!(m.source.as_deref(), Some("tmpfs"));
        assert_eq!(decode_dev(0x1_0303), (259, 3));
        assert_eq!(decode_dev(0x0_801), (8, 1));
    }

    /// The live identity of a tmpfs / real directory never fails and
    /// leaves unknown fields empty.
    #[test]
    fn identity_of_never_fails() {
        let identity = identity_of(&std::env::temp_dir());
        assert!(!identity.device_major_minor.is_empty());
        let _ = identity_of(Path::new("/nonexistent/for/sure"));
    }

    /// TOML string quoting escapes what the kernel could never emit but
    /// an operator's copy could.
    #[test]
    fn quoted_escapes() {
        assert_eq!(quoted("a\"b\\c"), "\"a\\\"b\\\\c\"");
        assert_eq!(quoted(""), "\"\"");
    }
}
