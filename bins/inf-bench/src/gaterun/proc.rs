//! The harness's one `/proc/<pid>` reader (Linux). A read that fails, is
//! cut short, finds an exited process, or finds another process under the
//! same pid is an error every caller handles: a failed read never becomes
//! a finite value that passes a `<=` row.

use std::io::Read as _;
use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

/// Linux `USER_HZ`: the tick of `utime`/`stime` in `/proc/<pid>/stat`.
pub(crate) const CLOCK_TICKS_PER_S: u64 = 100;

/// The longest `/proc/<pid>` text this reader accepts, in bytes. Crossing:
/// `ProcReadError::Truncated` — a longer file is not a shape the parsers
/// know, so it is refused rather than parsed from a prefix.
pub(crate) const PROC_READ_BYTES_MAX: u64 = 64 * 1024;

/// `/proc/<pid>/status` reports memory in `kB`, which is KiB.
const STATUS_UNIT_BYTES: u64 = 1024;

/// Field positions in `/proc/<pid>/stat` after the last `)`, counted from
/// the state (field 3) at index 0: `utime` is field 14, `stime` field 15,
/// `starttime` field 22.
const STAT_UTIME_INDEX: usize = 11;
const STAT_STIME_INDEX: usize = 12;
const STAT_START_INDEX: usize = 19;

/// One read of a live process. The fields are private and [`read_proc`]
/// is the only constructor outside tests, so no module can prove a pin or
/// publish an RSS from a sample no `/proc` read produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcSample {
    rss_bytes: u64,
    pinned_bytes: u64,
    cpu_ticks: u64,
    start_ticks: u64,
}

impl ProcSample {
    /// `VmRSS`.
    pub(crate) fn rss_bytes(&self) -> u64 {
        self.rss_bytes
    }

    /// `VmPin`: pages pinned by io_uring fixed-buffer registration.
    pub(crate) fn pinned_bytes(&self) -> u64 {
        self.pinned_bytes
    }

    /// `utime + stime`, in [`CLOCK_TICKS_PER_S`] ticks.
    pub(crate) fn cpu_ticks(&self) -> u64 {
        self.cpu_ticks
    }

    /// `starttime`: the process's identity beside its pid.
    pub(crate) fn start_ticks(&self) -> u64 {
        self.start_ticks
    }

    /// A sample no `/proc` read produced: test fixtures only.
    #[cfg(test)]
    pub(crate) fn fixture(
        rss_bytes: u64,
        pinned_bytes: u64,
        cpu_ticks: u64,
        start_ticks: u64,
    ) -> ProcSample {
        ProcSample { rss_bytes, pinned_bytes, cpu_ticks, start_ticks }
    }
}

/// The largest `VmRSS` a sampler thread read from one process while a
/// load ran. Estimator: the maximum over the caller's polls, so its
/// resolution is the poll interval (20–100 ms at the call sites) — a rise
/// and fall inside one interval is not seen, and the last poll precedes
/// the window's end by up to one interval. Every read raises the maximum
/// or counts a failure, and [`finish`](Self::finish) is the only way to
/// it: one failed read fails it, and a window with no read at all has
/// none, so a skipped failure or a sampler that never ran cannot lower a
/// `<=` row.
#[derive(Debug)]
pub(crate) struct PeakRssSampler {
    pid: u32,
    /// The first read's `starttime`: every later read must be the same
    /// process.
    start_ticks: OnceLock<u64>,
    peak_bytes: AtomicU64,
    reads: AtomicU64,
    read_failures: AtomicU64,
    first_error: OnceLock<ProcReadError>,
}

/// Why a sampled window has no peak.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum PeakRssError {
    /// `failures` of `attempts` reads failed; `first` is the first error.
    Failed { failures: u64, attempts: u64, first: ProcReadError },
    /// No read ran in the window.
    NoRead,
}

impl std::fmt::Display for PeakRssError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PeakRssError::Failed { failures, attempts, first } => {
                write!(f, "{failures} of {attempts} VmRSS read(s) failed (first: {first})")
            }
            PeakRssError::NoRead => write!(f, "no VmRSS read ran in the window"),
        }
    }
}

impl PeakRssSampler {
    pub(crate) fn new(pid: u32) -> PeakRssSampler {
        PeakRssSampler {
            pid,
            start_ticks: OnceLock::new(),
            peak_bytes: AtomicU64::new(0),
            reads: AtomicU64::new(0),
            read_failures: AtomicU64::new(0),
            first_error: OnceLock::new(),
        }
    }

    /// One read: raises the peak and returns the reading, or counts the
    /// failure and returns it.
    pub(crate) fn sample(&self) -> Result<u64, ProcReadError> {
        match read_proc(self.pid, self.start_ticks.get().copied()) {
            Ok(sample) => {
                self.start_ticks.get_or_init(|| sample.start_ticks);
                self.peak_bytes.fetch_max(sample.rss_bytes, Ordering::Relaxed);
                self.reads.fetch_add(1, Ordering::Relaxed);
                Ok(sample.rss_bytes)
            }
            Err(error) => {
                self.read_failures.fetch_add(1, Ordering::Relaxed);
                let _first_wins = self.first_error.set(error.clone());
                Err(error)
            }
        }
    }

    /// One read whose value the caller does not use: a failure is still
    /// counted and fails [`finish`](Self::finish).
    pub(crate) fn record(&self) {
        let _counted_for_finish = self.sample();
    }

    /// The window's peak: `Err` when any read failed or none ran. Every
    /// failed read sets the first error, so its presence is the failure.
    pub(crate) fn finish(self) -> Result<u64, PeakRssError> {
        let reads = self.reads.into_inner();
        if let Some(first) = self.first_error.into_inner() {
            let failures = self.read_failures.into_inner();
            let attempts = reads.saturating_add(failures);
            return Err(PeakRssError::Failed { failures, attempts, first });
        }
        if reads == 0 {
            return Err(PeakRssError::NoRead);
        }
        Ok(self.peak_bytes.into_inner())
    }
}

/// Why a `/proc` read produced no sample.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ProcReadError {
    /// The read failed for a reason other than absence.
    Io(std::io::ErrorKind),
    /// No process has this pid.
    Missing,
    /// The process exited: state `Z` (unreaped) or `X` (dead).
    Exited,
    /// The named field is absent or not a number.
    Unparsable(&'static str),
    /// The text ends before a field the reader needs, or is longer than
    /// [`PROC_READ_BYTES_MAX`].
    Truncated,
    /// `starttime` moved: the pid names another process (pid reuse).
    ProcessReplaced { expected_start_ticks: u64, found_start_ticks: u64 },
}

impl std::fmt::Display for ProcReadError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ProcReadError::Io(kind) => write!(f, "/proc read failed ({kind})"),
            ProcReadError::Missing => write!(f, "no such process"),
            ProcReadError::Exited => write!(f, "the process exited (state Z or X)"),
            ProcReadError::Unparsable(field) => write!(f, "{field} absent or unparsable"),
            ProcReadError::Truncated => write!(f, "/proc text truncated or over its bound"),
            ProcReadError::ProcessReplaced { expected_start_ticks, found_start_ticks } => write!(
                f,
                "pid reused: starttime {found_start_ticks}, expected {expected_start_ticks}"
            ),
        }
    }
}

/// Reads `pid`'s stat (its state first) and then its status.
/// `expected_start` is the `start_ticks` of an earlier sample of the same
/// process, or `None` for the first read, which establishes it.
pub(crate) fn read_proc(
    pid: u32,
    expected_start: Option<u64>,
) -> Result<ProcSample, ProcReadError> {
    let stat = parse_stat(&read_bounded(pid, "stat")?)?;
    check_identity(&stat, expected_start)?;
    let status = parse_status(&read_bounded(pid, "status")?)?;
    Ok(ProcSample {
        rss_bytes: status.rss_bytes,
        pinned_bytes: status.pinned_bytes,
        cpu_ticks: stat.cpu_ticks,
        start_ticks: stat.start_ticks,
    })
}

/// `read_bytes` of `/proc/<pid>/io`: the bytes the process caused to be
/// fetched from the storage layer. The stat read before it proves the
/// process is alive and, with `expected_start`, the same process.
pub(crate) fn read_io_bytes(pid: u32, expected_start: Option<u64>) -> Result<u64, ProcReadError> {
    let stat = parse_stat(&read_bounded(pid, "stat")?)?;
    check_identity(&stat, expected_start)?;
    parse_io_read_bytes(&read_bounded(pid, "io")?)
}

/// The `Max locked memory` limits of `/proc/<pid>/limits` (soft, hard,
/// unit): the fixed-buffer registration's precondition, for report notes.
pub(crate) fn read_memlock_limit(pid: u32) -> Result<String, ProcReadError> {
    let text = read_bounded(pid, "limits")?;
    let line = text
        .lines()
        .find_map(|line| line.strip_prefix("Max locked memory"))
        .ok_or(ProcReadError::Unparsable("Max locked memory"))?;
    Ok(line.split_whitespace().collect::<Vec<_>>().join(" "))
}

/// The parsed facts of one stat read.
#[derive(Debug, PartialEq, Eq)]
struct Stat {
    cpu_ticks: u64,
    start_ticks: u64,
}

/// The parsed facts of one status read.
#[derive(Debug, PartialEq, Eq)]
struct Status {
    rss_bytes: u64,
    pinned_bytes: u64,
}

fn read_bounded(pid: u32, file: &str) -> Result<String, ProcReadError> {
    read_bounded_path(std::path::Path::new(&format!("/proc/{pid}/{file}")))
}

/// Reads at most [`PROC_READ_BYTES_MAX`] bytes; one more is `Truncated`.
/// `comm` may hold any bytes, so the text is decoded lossily: the fields
/// the parsers read are ASCII after the last `)`.
fn read_bounded_path(path: &std::path::Path) -> Result<String, ProcReadError> {
    let file = std::fs::File::open(path).map_err(|e| classify(&e))?;
    let mut bytes = Vec::new();
    file.take(PROC_READ_BYTES_MAX + 1).read_to_end(&mut bytes).map_err(|e| classify(&e))?;
    let length_bytes = u64::try_from(bytes.len()).map_err(|_| ProcReadError::Truncated)?;
    if length_bytes > PROC_READ_BYTES_MAX {
        return Err(ProcReadError::Truncated);
    }
    Ok(String::from_utf8_lossy(&bytes).into_owned())
}

/// Classifies the errno, not the call: a pid that is gone reads `ENOENT`
/// at open and `ESRCH` when it vanishes between open and read.
fn classify(error: &std::io::Error) -> ProcReadError {
    const ESRCH: i32 = 3;
    if error.kind() == std::io::ErrorKind::NotFound || error.raw_os_error() == Some(ESRCH) {
        ProcReadError::Missing
    } else {
        ProcReadError::Io(error.kind())
    }
}

fn is_exited_state(state: &str) -> bool {
    matches!(state, "Z" | "X" | "x")
}

fn parse_stat(text: &str) -> Result<Stat, ProcReadError> {
    let (_, rest) = text.rsplit_once(')').ok_or(ProcReadError::Unparsable("comm"))?;
    let fields: Vec<&str> = rest.split_whitespace().take(STAT_START_INDEX + 1).collect();
    let state = fields.first().ok_or(ProcReadError::Truncated)?;
    if is_exited_state(state) {
        return Err(ProcReadError::Exited);
    }
    let field = |index: usize, name: &'static str| -> Result<u64, ProcReadError> {
        let raw = fields.get(index).ok_or(ProcReadError::Truncated)?;
        raw.parse().map_err(|_| ProcReadError::Unparsable(name))
    };
    let utime = field(STAT_UTIME_INDEX, "utime")?;
    let stime = field(STAT_STIME_INDEX, "stime")?;
    let cpu_ticks = utime.checked_add(stime).ok_or(ProcReadError::Unparsable("utime+stime"))?;
    Ok(Stat { cpu_ticks, start_ticks: field(STAT_START_INDEX, "starttime")? })
}

fn check_identity(stat: &Stat, expected_start: Option<u64>) -> Result<(), ProcReadError> {
    match expected_start {
        Some(expected) if expected != stat.start_ticks => Err(ProcReadError::ProcessReplaced {
            expected_start_ticks: expected,
            found_start_ticks: stat.start_ticks,
        }),
        Some(_) | None => Ok(()),
    }
}

/// A process that exits between the stat read and this one has a status
/// with `State: Z` and no `VmRSS`; the state is checked on every line so
/// that race reads `Exited`, never an absent field.
fn parse_status(text: &str) -> Result<Status, ProcReadError> {
    let mut rss_bytes = None;
    let mut pinned_bytes = None;
    for line in text.lines() {
        if let Some(state) = line.strip_prefix("State:") {
            if state.split_whitespace().next().is_some_and(is_exited_state) {
                return Err(ProcReadError::Exited);
            }
        } else if let Some(value) = line.strip_prefix("VmRSS:") {
            rss_bytes = Some(parse_kib(value, "VmRSS")?);
        } else if let Some(value) = line.strip_prefix("VmPin:") {
            pinned_bytes = Some(parse_kib(value, "VmPin")?);
        }
    }
    Ok(Status {
        rss_bytes: rss_bytes.ok_or(ProcReadError::Unparsable("VmRSS"))?,
        pinned_bytes: pinned_bytes.ok_or(ProcReadError::Unparsable("VmPin"))?,
    })
}

fn parse_kib(value: &str, name: &'static str) -> Result<u64, ProcReadError> {
    let mut words = value.split_whitespace();
    let count: u64 =
        words.next().and_then(|n| n.parse().ok()).ok_or(ProcReadError::Unparsable(name))?;
    if words.next() != Some("kB") {
        return Err(ProcReadError::Unparsable(name));
    }
    count.checked_mul(STATUS_UNIT_BYTES).ok_or(ProcReadError::Unparsable(name))
}

fn parse_io_read_bytes(text: &str) -> Result<u64, ProcReadError> {
    text.lines()
        .find_map(|line| line.strip_prefix("read_bytes:"))
        .and_then(|value| value.trim().parse().ok())
        .ok_or(ProcReadError::Unparsable("read_bytes"))
}

#[cfg(test)]
mod tests;
