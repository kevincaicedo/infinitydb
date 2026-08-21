//! `inf probe-device` (M4.5-S34, ADR-0086 D7 — the ScyllaDB `iotune`
//! precedent): measure the two log barrier classes on the data
//! directory's device and write `<data-dir>/io-properties.toml`.
//!
//! Two policies, on a pre-written scratch file (written extents — the
//! `fallocate`d-but-unwritten trap of ADR-0086 D4 would make the FUA
//! rows lie):
//!
//! - **flush** — buffered `pwrite` + `fdatasync`: today's production
//!   barrier (ADR-0013 D1), ending in a device-wide cache FLUSH.
//! - **fua** — `O_DIRECT | O_DSYNC` `pwrite` from a 4 KiB-aligned buffer:
//!   the kernel's write-through path, a FUA-flagged write where the
//!   device supports it.
//!
//! Sizes 4 KiB, 64 KiB, 256 KiB, 1 MiB — the frame sizes the group-commit
//! path produces from one record per iteration up to the `always`
//! saturation shape. Sequential offsets cycling through the scratch file,
//! one writer (the per-cell shape; concurrency multiplies FLUSH latency
//! and leaves FUA alone — ADR-0086 Context — which is the class's point,
//! not something a single probe needs to re-measure).
//!
//! **Recommendation rule** (written into the file's comment too): `fua`
//! iff its 4 KiB p50 ≤ 0.75 × flush's **and** its 4 KiB p99 ≤ flush's
//! p99; `fua_max_frame_bytes` = the largest probed size whose fua p50
//! still beats flush's **and** whose fua p99 stays within 2× flush's p99
//! (the reference device's 1 MiB FUA tail — 45 ms vs 8.5 — is exactly
//! the shape this guard exists for). Otherwise `flush` — today's path,
//! byte-for-byte.
//!
//! **Schema 2 (M4.5-S36, ADR-0088 D6)** adds the device model the
//! per-cell budget spends — `write_bytes_per_s_256k` and
//! `write_ops_per_s_4k` are the direct rows' own throughput (measured
//! here since S34 and previously written only as a comment) — and one
//! new row, `write_ops_per_s_4k_qd4`: the direct 4 KiB barrier rate at
//! **four concurrent writers** on disjoint regions, the concurrency the
//! frame-seal pacer (ADR-0088 D2b) needs (the single-writer rate
//! under-states a FUA device's aggregate 3–4×). Read rows are declared
//! and left at 0 (a QD-1 read number is not an IOPS number; a probe
//! story with concurrency owns them).
//!
//! Dev tool: `Instant::now()` is fine here (not cell code); the output is
//! a boot input, never a claim (L10 — the A/B is the claim).

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Scratch file size: one log segment — large enough that sequential
/// writes do not revisit a block within one row at any probed size, and
/// the same span the C probe of the S34 diagnosis used.
const SCRATCH_BYTES: u64 = 256 << 20;
const ALIGN: usize = 4096;
const SIZES: [usize; 4] = [4 << 10, 64 << 10, 256 << 10, 1 << 20];
const DEFAULT_SECONDS_PER_ROW: u64 = 2;
/// Writers of the concurrent barrier-rate row (ADR-0088 D2b/D6): four,
/// not the cell count the probe cannot know — the number is divided by
/// `cells` at boot, conservative in the direction that batches more.
const QD_WRITERS: usize = 4;

#[derive(Copy, Clone, PartialEq, Eq, Debug)]
enum Policy {
    Flush,
    Fua,
}

impl Policy {
    fn name(self) -> &'static str {
        match self {
            Policy::Flush => "flush",
            Policy::Fua => "fua",
        }
    }
}

struct Row {
    policy: Policy,
    bytes: usize,
    p50_us: u64,
    p99_us: u64,
    barriers_per_sec: u64,
}

/// Entry point: `probe-device <data-dir> [--seconds N]`.
pub(crate) fn run(args: &[String]) -> io::Result<()> {
    let mut dir: Option<PathBuf> = None;
    let mut seconds = DEFAULT_SECONDS_PER_ROW;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seconds" => {
                seconds = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|s| (1..=60).contains(s))
                    .ok_or_else(|| io::Error::other("--seconds wants 1..=60"))?;
            }
            other if dir.is_none() => dir = Some(PathBuf::from(other)),
            other => return Err(io::Error::other(format!("unexpected argument {other}"))),
        }
    }
    let dir = dir.ok_or_else(|| io::Error::other("usage: inf probe-device <data-dir>"))?;
    std::fs::create_dir_all(&dir)?;
    let scratch = dir.join(".io-probe.scratch");
    let result = probe(&dir, &scratch, Duration::from_secs(seconds));
    let _ = std::fs::remove_file(&scratch);
    result
}

fn probe(dir: &Path, scratch: &Path, per_row: Duration) -> io::Result<()> {
    prewrite(scratch)?;
    let mut rows = Vec::new();
    for &bytes in &SIZES {
        for policy in [Policy::Flush, Policy::Fua] {
            let row = measure(scratch, policy, bytes, per_row)?;
            eprintln!(
                "{:<5} bytes={:<8} p50={:>6} us  p99={:>6} us  barriers/s={:>6}",
                row.policy.name(),
                row.bytes,
                row.p50_us,
                row.p99_us,
                row.barriers_per_sec
            );
            rows.push(row);
        }
    }
    let qd4 = measure_concurrent(scratch, SIZES[0], per_row, QD_WRITERS)?;
    eprintln!("fua   bytes={:<8} writers={QD_WRITERS} barriers/s={qd4:>6} (aggregate)", SIZES[0]);
    let verdict = recommend(&rows, qd4);
    let text = render(&rows, &verdict);
    let path = dir.join("io-properties.toml");
    let tmp = dir.join("io-properties.toml.new");
    {
        let mut file = File::create(&tmp)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()?;
    }
    std::fs::rename(&tmp, &path)?;
    File::open(dir)?.sync_all()?;
    eprintln!(
        "wrote {} — barrier_class = \"{}\", fua_max_frame_bytes = {}",
        path.display(),
        verdict.class.name(),
        verdict.fua_max_frame_bytes
    );
    Ok(())
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

struct Verdict {
    class: Policy,
    fua_max_frame_bytes: usize,
    fua_p50_us_4k: u64,
    flush_p50_us_4k: u64,
    /// Schema 2 (ADR-0088 D6): the device model rows.
    write_bytes_per_s_256k: u64,
    write_ops_per_s_4k: u64,
    write_ops_per_s_4k_qd4: u64,
}

fn row(rows: &[Row], policy: Policy, bytes: usize) -> Option<&Row> {
    rows.iter().find(|r| r.policy == policy && r.bytes == bytes)
}

/// The recommendation rule (module docs).
fn recommend(rows: &[Row], write_ops_per_s_4k_qd4: u64) -> Verdict {
    let flush_4k = row(rows, Policy::Flush, SIZES[0]);
    let fua_4k = row(rows, Policy::Fua, SIZES[0]);
    // The model rows (ADR-0088 D6): the direct 256 KiB row's throughput
    // and the direct 4 KiB row's barrier rate — measured every run since
    // S34, now written as keys instead of a comment.
    let write_bytes_per_s_256k =
        row(rows, Policy::Fua, SIZES[2]).map_or(0, |r| r.barriers_per_sec * r.bytes as u64);
    let write_ops_per_s_4k = fua_4k.map_or(0, |r| r.barriers_per_sec);
    let (Some(flush), Some(fua)) = (flush_4k, fua_4k) else {
        return Verdict {
            class: Policy::Flush,
            fua_max_frame_bytes: 0,
            fua_p50_us_4k: 0,
            flush_p50_us_4k: 0,
            write_bytes_per_s_256k,
            write_ops_per_s_4k,
            write_ops_per_s_4k_qd4,
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
    }
}

fn render(rows: &[Row], verdict: &Verdict) -> String {
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
    out.push_str("# absent file => flush (today's path). `infinityd --barrier-class` overrides.\n");
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
    out.push_str("probe_schema = 2\n");
    out.push_str(&format!("write_bytes_per_s_256k = {}\n", verdict.write_bytes_per_s_256k));
    out.push_str(&format!("write_ops_per_s_4k = {}\n", verdict.write_ops_per_s_4k));
    out.push_str(&format!("write_ops_per_s_4k_qd4 = {}\n", verdict.write_ops_per_s_4k_qd4));
    out.push_str("read_bytes_per_s_256k = 0\n");
    out.push_str("read_ops_per_s_4k = 0\n");
    out
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
        let text = render(&rows, &v);
        assert!(text.contains("probe_schema = 2\n"));
        assert!(text.contains(&format!("write_bytes_per_s_256k = {}\n", 1_949 * (256 << 10))));
        assert!(text.contains("write_ops_per_s_4k_qd4 = 9918\n"));
        assert!(text.contains("read_ops_per_s_4k = 0\n"));
    }

    #[test]
    fn rendered_file_parses_as_the_flat_subset() {
        let rows = vec![r(Policy::Flush, 4 << 10, 900, 1200), r(Policy::Fua, 4 << 10, 300, 600)];
        let text = render(&rows, &recommend(&rows, 0));
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
            ]
        );
        assert!(text.contains("barrier_class = \"fua\""));
    }
}
