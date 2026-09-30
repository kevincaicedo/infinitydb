use super::*;

/// A stat line whose fields after the comm are numbered from the state
/// (index 0): every field is its own index, except the ones a test sets.
fn stat_line(comm: &str, state: &str, utime: u64, stime: u64, start: u64) -> String {
    stat_raw(comm, state, &utime.to_string(), &stime.to_string(), &start.to_string())
}

fn stat_raw(comm: &str, state: &str, utime: &str, stime: &str, start: &str) -> String {
    let mut fields: Vec<String> = (0..=STAT_START_INDEX + 5).map(|i| i.to_string()).collect();
    fields[0] = state.to_string();
    fields[STAT_UTIME_INDEX] = utime.to_string();
    fields[STAT_STIME_INDEX] = stime.to_string();
    fields[STAT_START_INDEX] = start.to_string();
    format!("4242 ({comm}) {}\n", fields.join(" "))
}

#[test]
fn stat_parses_after_the_last_paren() {
    let stat = parse_stat(&stat_line("a) b", "S", 7, 5, 99)).expect("parses");
    assert_eq!(stat, Stat { cpu_ticks: 12, start_ticks: 99 });
}

#[test]
fn stat_refuses_exited_truncated_and_garbled_text() {
    assert_eq!(parse_stat(&stat_line("x", "Z", 1, 1, 1)), Err(ProcReadError::Exited));
    assert_eq!(parse_stat(&stat_line("x", "X", 1, 1, 1)), Err(ProcReadError::Exited));
    let line = stat_line("x", "R", 1, 1, 1);
    let cut: String =
        line.split_whitespace().take(2 + STAT_START_INDEX).collect::<Vec<_>>().join(" ");
    assert_eq!(parse_stat(&cut), Err(ProcReadError::Truncated), "ends before starttime");
    assert_eq!(parse_stat("4242 (x) "), Err(ProcReadError::Truncated), "no state");
    assert_eq!(parse_stat("no comm at all"), Err(ProcReadError::Unparsable("comm")));
    let garbled = stat_raw("x", "R", "eleven", "1", "1");
    assert_eq!(parse_stat(&garbled), Err(ProcReadError::Unparsable("utime")));
    let no_start = stat_raw("x", "R", "1", "1", "-1");
    assert_eq!(parse_stat(&no_start), Err(ProcReadError::Unparsable("starttime")));
    let overflow = stat_line("x", "R", u64::MAX, 1, 1);
    assert_eq!(parse_stat(&overflow), Err(ProcReadError::Unparsable("utime+stime")));
}

#[test]
fn a_moved_starttime_is_another_process() {
    let stat = Stat { cpu_ticks: 0, start_ticks: 10 };
    assert_eq!(check_identity(&stat, None), Ok(()));
    assert_eq!(check_identity(&stat, Some(10)), Ok(()));
    assert_eq!(
        check_identity(&stat, Some(9)),
        Err(ProcReadError::ProcessReplaced { expected_start_ticks: 9, found_start_ticks: 10 })
    );
}

#[test]
fn status_needs_rss_and_pin_and_reads_a_zombie_as_exited() {
    let live = "Name:\tinfinityd\nState:\tS (sleeping)\nVmPin:\t  130816 kB\nVmRSS:\t   83212 kB\n";
    assert_eq!(
        parse_status(live),
        Ok(Status { rss_bytes: 83_212 * 1024, pinned_bytes: 130_816 * 1024 })
    );
    let zombie = "Name:\ttrue\nState:\tZ (zombie)\nThreads:\t1\n";
    assert_eq!(parse_status(zombie), Err(ProcReadError::Exited));
    let no_rss = "State:\tS (sleeping)\nVmPin:\t0 kB\n";
    assert_eq!(parse_status(no_rss), Err(ProcReadError::Unparsable("VmRSS")));
    let no_pin = "State:\tS (sleeping)\nVmRSS:\t1 kB\n";
    assert_eq!(parse_status(no_pin), Err(ProcReadError::Unparsable("VmPin")));
    let bad = "State:\tS (sleeping)\nVmRSS:\tlots kB\nVmPin:\t0 kB\n";
    assert_eq!(parse_status(bad), Err(ProcReadError::Unparsable("VmRSS")));
    let unit = "State:\tS (sleeping)\nVmRSS:\t1 MB\nVmPin:\t0 kB\n";
    assert_eq!(parse_status(unit), Err(ProcReadError::Unparsable("VmRSS")));
    let huge = format!("State:\tS\nVmRSS:\t{} kB\nVmPin:\t0 kB\n", u64::MAX);
    assert_eq!(parse_status(&huge), Err(ProcReadError::Unparsable("VmRSS")));
}

#[test]
fn io_needs_read_bytes() {
    assert_eq!(parse_io_read_bytes("rchar: 1\nread_bytes: 4096\n"), Ok(4096));
    assert_eq!(
        parse_io_read_bytes("rchar: 1\nwrite_bytes: 0\n"),
        Err(ProcReadError::Unparsable("read_bytes"))
    );
}

#[test]
fn a_file_over_the_bound_is_truncated_not_a_prefix() {
    let dir = std::env::temp_dir().join(format!("inf-bench-proc-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("fixture dir");
    let path = dir.join("big");
    let at_bound = usize::try_from(PROC_READ_BYTES_MAX).expect("bound fits usize");
    std::fs::write(&path, vec![b'a'; at_bound]).expect("fixture");
    assert_eq!(read_bounded_path(&path).map(|t| t.len()), Ok(at_bound), "the bound itself reads");
    std::fs::write(&path, vec![b'a'; at_bound + 1]).expect("fixture");
    assert_eq!(read_bounded_path(&path), Err(ProcReadError::Truncated));
    assert_eq!(read_bounded_path(&dir.join("absent")), Err(ProcReadError::Missing));
    std::fs::remove_dir_all(&dir).expect("fixture cleanup");
}

#[test]
#[cfg(target_os = "linux")]
fn this_process_reads_and_keeps_its_identity() {
    let pid = std::process::id();
    let first = read_proc(pid, None).expect("self read");
    assert!(first.rss_bytes > 0, "a live process has resident pages");
    let again = read_proc(pid, Some(first.start_ticks)).expect("same process");
    assert_eq!(again.start_ticks, first.start_ticks);
    assert!(again.cpu_ticks >= first.cpu_ticks, "ticks never go back");
    assert!(matches!(
        read_proc(pid, Some(first.start_ticks + 1)),
        Err(ProcReadError::ProcessReplaced { .. })
    ));
    assert!(read_io_bytes(pid, Some(first.start_ticks)).is_ok());
    assert!(read_memlock_limit(pid).is_ok_and(|limit| !limit.is_empty()));
}
