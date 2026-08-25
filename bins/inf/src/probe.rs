//! `inf probe-device <data-dir> [--seconds N]` (M4.5-S34, ADR-0086 D7;
//! shared with `infinityd --device-probe auto` since M4.5-S42, ADR-0091):
//! the CLI face of [`inf_probe`] — argument parsing and the operator's
//! row lines; the measurement and the file live in the crate.

use std::io;
use std::path::PathBuf;

/// Entry point: `probe-device <data-dir> [--seconds N]`.
pub(crate) fn run(args: &[String]) -> io::Result<()> {
    let mut dir: Option<PathBuf> = None;
    let mut opts = inf_probe::ProbeOptions::CLI;
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--seconds" => {
                opts.seconds_per_row = it
                    .next()
                    .and_then(|v| v.parse().ok())
                    .filter(|s| inf_probe::SECONDS_PER_ROW_RANGE.contains(s))
                    .ok_or_else(|| io::Error::other("--seconds wants 1..=60"))?;
            }
            other if dir.is_none() => dir = Some(PathBuf::from(other)),
            other => return Err(io::Error::other(format!("unexpected argument {other}"))),
        }
    }
    let dir = dir.ok_or_else(|| io::Error::other("usage: inf probe-device <data-dir>"))?;
    let (path, report) = inf_probe::run_and_write(&dir, opts)?;
    for line in report.row_lines() {
        eprintln!("{line}");
    }
    eprintln!(
        "wrote {} — barrier_class = \"{}\", fua_max_frame_bytes = {} (schema {}, identity {} {} {}, {:.1} s)",
        path.display(),
        report.verdict.class.name(),
        report.verdict.fua_max_frame_bytes,
        inf_probe::PROBE_SCHEMA,
        if report.identity.fs_type.is_empty() { "?" } else { &report.identity.fs_type },
        if report.identity.device_path.is_empty() { "?" } else { &report.identity.device_path },
        if report.identity.fs_uuid.is_empty() { "(no uuid)" } else { &report.identity.fs_uuid },
        report.elapsed.as_secs_f64()
    );
    Ok(())
}
