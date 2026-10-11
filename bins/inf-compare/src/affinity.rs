//! Process-wide Linux CPU allowances for servers and external generators.

use std::fmt;
use std::process::Command;

use crate::cli::Flags;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CpuRange {
    start: usize,
    end: usize,
}

impl CpuRange {
    pub fn new(start: usize, count: u16) -> Result<Self, String> {
        let last = count.checked_sub(1).ok_or("CPU range must contain at least one CPU")?;
        let end = start.checked_add(usize::from(last)).ok_or("CPU range endpoint overflow")?;
        if end > usize::from(u16::MAX) {
            return Err("CPU IDs must be in 0..=65535 (bounded taskset mask)".into());
        }
        Ok(Self { start, end })
    }

    pub fn start(self) -> usize {
        self.start
    }

    fn overlaps(self, other: Self) -> bool {
        self.start <= other.end && other.start <= self.end
    }

    fn verify(self) -> Result<(), String> {
        // taskset may succeed after the kernel trims unavailable CPUs. Require the whole range.
        let output = command("cat", Some(self))
            .arg("/proc/self/status")
            .output()
            .map_err(|error| format!("verify CPU range {self} with taskset: {error}"))?;
        if !output.status.success() {
            return Err(format!(
                "verify CPU range {self}: taskset exited {}: {}",
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            ));
        }
        let status = String::from_utf8_lossy(&output.stdout);
        let actual = status.lines().find_map(|line| line.strip_prefix("Cpus_allowed_list:"));
        if actual.map(str::trim) != Some(self.to_string().as_str()) {
            return Err(format!(
                "CPU range {self} was not fully applied (effective {})",
                actual.map_or("unavailable", str::trim)
            ));
        }
        Ok(())
    }
}

impl fmt::Display for CpuRange {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.start == self.end {
            write!(formatter, "{}", self.start)
        } else {
            write!(formatter, "{}-{}", self.start, self.end)
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct Placement {
    pub server: CpuRange,
    pub load: CpuRange,
}

pub fn placement(
    flags: &Flags,
    threads: u16,
    docker: bool,
    attached: bool,
) -> Result<Option<Placement>, String> {
    if threads == 0 {
        return Err("--threads must be greater than zero".into());
    }
    let server_start = flags.opt_usize("pin-start")?;
    let load_start = flags.opt_usize("load-pin-start")?;
    let load_count = flags.u16_or("load-cpus", threads)?;
    if load_count == 0 {
        return Err("--load-cpus must be greater than zero".into());
    }
    let (server_start, load_start) = match (server_start, load_start) {
        (Some(server), Some(load)) => (server, load),
        (None, None) if !flags.bool("reference-box") && flags.get("load-cpus").is_none() => {
            return Ok(None);
        }
        _ => return Err("CPU isolation requires both --pin-start and --load-pin-start".into()),
    };
    if !cfg!(target_os = "linux") || docker || attached {
        return Err(
            "CPU pinning requires Linux host launches; --docker/--attach are unverified".into()
        );
    }
    let placement = Placement {
        server: CpuRange::new(server_start, threads)?,
        load: CpuRange::new(load_start, load_count)?,
    };
    if placement.server.overlaps(placement.load) {
        return Err("server and load-generator CPU ranges must be disjoint".into());
    }
    placement.server.verify()?;
    placement.load.verify()?;
    Ok(Some(placement))
}

pub fn wrap(program: String, args: Vec<String>, cpus: Option<CpuRange>) -> (String, Vec<String>) {
    let Some(cpus) = cpus else { return (program, args) };
    let mut wrapped = vec!["-c".to_string(), cpus.to_string(), program];
    wrapped.extend(args);
    ("taskset".into(), wrapped)
}

pub fn command(program: &str, cpus: Option<CpuRange>) -> Command {
    let (program, args) = wrap(program.into(), Vec::new(), cpus);
    let mut command = Command::new(program);
    command.args(args);
    command
}

pub fn description(placement: Option<Placement>) -> String {
    placement.map_or_else(
        || "CPU isolation unverified: server and generator affinity is not controlled".into(),
        |placement| {
            format!(
                "server CPUs {} (all process threads/children); generator CPUs {} \
                 (memtier, preload, redis-benchmark); disjoint logical CPU ranges",
                placement.server, placement.load
            )
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_reject_empty_overflow_and_shared_endpoints() {
        assert!(CpuRange::new(0, 0).is_err());
        assert!(CpuRange::new(usize::MAX, 2).is_err());
        assert!(CpuRange::new(65_536, 1).is_err());
        assert!(CpuRange::new(65_535, 1).is_ok());
        assert_eq!(CpuRange::new(7, 1).unwrap().to_string(), "7");
        let range = CpuRange::new(7, 2).unwrap();
        assert_eq!(range.to_string(), "7-8");
        assert!(range.overlaps(CpuRange::new(8, 2).unwrap()));
        assert!(CpuRange::new(8, 2).unwrap().overlaps(range));
        assert!(!range.overlaps(CpuRange::new(9, 1).unwrap()));
    }
}
