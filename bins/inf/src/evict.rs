//! `inf cache-evict <data-dir>` (M4.5-S34, the C38b replay clause): the
//! CLI face of [`inf_probe::evict_page_cache`] — sync + `DONTNEED` every
//! file under a data directory so the next boot reads it from the device
//! (a power-loss-shaped recovery row without `drop_caches`). Prints one
//! machine-readable line the `inf-bench` S39d row parses.

use std::io;
use std::path::PathBuf;

/// Entry point: `cache-evict <data-dir>`.
pub(crate) fn run(args: &[String]) -> io::Result<()> {
    let [dir] = args else {
        return Err(io::Error::other("usage: inf cache-evict <data-dir>"));
    };
    let dir = PathBuf::from(dir);
    let report = inf_probe::evict_page_cache(&dir)?;
    println!(
        "cache-evict dir={} files={} bytes={} dirs={}",
        dir.display(),
        report.files,
        report.bytes,
        report.dirs
    );
    Ok(())
}
