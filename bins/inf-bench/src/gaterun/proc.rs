//! The harness's one `/proc/<pid>` reader (Linux). A read that fails, is
//! cut short, finds an exited process, or finds another process under the
//! same pid is an error every caller handles: a failed read never becomes
//! a finite value that passes a `<=` row.

use std::io::Read as _;

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

/// One read of a live process.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ProcSample {
    /// `VmRSS`.
    pub(crate) rss_bytes: u64,
    /// `VmPin`: pages pinned by io_uring fixed-buffer registration.
    pub(crate) pinned_bytes: u64,
    /// `utime + stime`, in [`CLOCK_TICKS_PER_S`] ticks.
    pub(crate) cpu_ticks: u64,
    /// `starttime`: the process's identity beside its pid.
    pub(crate) start_ticks: u64,
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
