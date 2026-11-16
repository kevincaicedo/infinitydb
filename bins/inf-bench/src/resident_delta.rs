//! The resident-delta verdict: one outcome type, one host preflight and one
//! arithmetic for every instrument whose reading is one signed difference,
//! in bytes a cell, between the resident memory of two builds of the node.
//! Each instrument owns its estimator, its boot order and its liveness
//! checks; this module owns the outcome, the separation of a plant from its
//! control, and the judgement, which reads the reading and the control
//! spreads and never a boot.
//!
//! A plant of `n` bytes a cell is caught when the reading `r` rises over
//! [`RISE`] of it, `10 r > 9 n`. When every boot of an arm lies within the
//! control `c` of its median, and both arms share one noise range (what each
//! instrument's interleaved boot order is for), the difference of the
//! medians is off by at most `c`. A fully resident plant then reads
//! `r ≥ n − c`, over `0.9 n` exactly when `n > 10 c`; a plant at most four
//! fifths resident reads `r ≤ 0.8 n + c`, under it; and a build against
//! itself reads at most `c`, under `0.1 n`. So [`PlantThreshold::new`]
//! refuses a plant at or under `10 c` before any boot: there noise, not the
//! plant, would decide. The exit codes are [`exit_code`]'s alone: 0 within,
//! 1 over, 2 no verdict.

use std::fmt;

/// The fraction of a plant's bytes its reading must rise by to be caught,
/// numerator over denominator. The separation is derived from it.
pub(crate) const RISE: (u64, u64) = (9, 10);
const _: () = assert!(RISE.0 > 0);
const _: () = assert!(RISE.0 < RISE.1);

/// The control of one row: the most one build's readings may spread, in
/// bytes a cell. A spread over it is `Noisy`, never a verdict.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Control {
    bytes_cell: u64,
}

impl Control {
    pub(crate) fn of_bytes_cell(bytes_cell: u64) -> Control {
        Control { bytes_cell }
    }

    pub(crate) fn bytes_cell(self) -> u64 {
        self.bytes_cell
    }
}

/// A plant's declared bytes a cell, separated from its control:
/// `n × (den − num) > c × den`, that is `n > 10 c`. [`new`](Self::new) is
/// the only constructor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct PlantThreshold {
    plant_bytes_cell: u64,
}

impl PlantThreshold {
    /// Refuses a plant at or under the separation, before any boot.
    pub(crate) fn new(
        plant_bytes_cell: u64,
        control: Control,
    ) -> Result<PlantThreshold, NoVerdictReason> {
        let (rise, whole) = (u128::from(RISE.0), u128::from(RISE.1));
        // Two u64 factors under 2^64 each: no product overflows a u128.
        let plant = u128::from(plant_bytes_cell) * (whole - rise);
        let noise = u128::from(control.bytes_cell) * whole;
        if plant > noise {
            Ok(PlantThreshold { plant_bytes_cell })
        } else {
            Err(NoVerdictReason::BelowSeparation {
                plant_bytes_cell,
                control_bytes_cell: control.bytes_cell,
            })
        }
    }
}

/// What a reading is judged against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Mode {
    /// A bound on the reading: over it is `Over`.
    Budget { budget_bytes_cell: u64 },
    /// A plant the reading must rise by [`RISE`] of to be `Over`.
    Plant(PlantThreshold),
}

/// The two arms' medians, bytes a cell; the reading is the candidate's
/// less the base's, signed. Named fields, so the arms cannot be swapped.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Reading {
    pub(crate) base_bytes_cell: u64,
    pub(crate) candidate_bytes_cell: u64,
}

impl Reading {
    fn rise_bytes_cell(self) -> i128 {
        i128::from(self.candidate_bytes_cell) - i128::from(self.base_bytes_cell)
    }
}

/// Each arm's spread (the largest less the smallest of one binary's
/// readings, bytes a cell) beside the control it must lie within.
/// [`new`](Self::new) refuses an empty list: no verdict exists without a
/// same-binary control.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ControlSpreads {
    spreads: Vec<(&'static str, u64)>,
    control: Control,
}

impl ControlSpreads {
    /// `spreads` holds `(arm, spread bytes a cell)`; `None` when empty.
    pub(crate) fn new(
        spreads: Vec<(&'static str, u64)>,
        control: Control,
    ) -> Option<ControlSpreads> {
        if spreads.is_empty() { None } else { Some(ControlSpreads { spreads, control }) }
    }

    /// `control: spreads <x>, <y>, within <c> (bytes a cell)`: printed
    /// before the verdict.
    pub(crate) fn line(&self) -> String {
        let spreads: Vec<String> =
            self.spreads.iter().map(|(arm, spread)| format!("{spread} ({arm})")).collect();
        format!(
            "control: spreads {}, within {} (bytes a cell)",
            spreads.join(", "),
            self.control.bytes_cell
        )
    }

    fn first_over(&self) -> Option<NoVerdictReason> {
        let (arm, spread) = self.spreads.iter().find(|(_, s)| *s > self.control.bytes_cell)?;
        Some(NoVerdictReason::Noisy {
            arm,
            spread_bytes_cell: *spread,
            control_bytes_cell: self.control.bytes_cell,
        })
    }
}

/// Why a run gave no verdict. Every one exits 2; `Withheld` alone is the
/// host's or the node's scope (the line begins `WITHHELD:`), which the
/// canary lane reads as judged nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum NoVerdictReason {
    /// Outside the instrument's scope: not Linux, no `/proc`, transparent
    /// huge pages `[always]`, a boot whose registration is unproven.
    Withheld(String),
    /// The arguments.
    Usage(String),
    /// A plant at or under the separation from its control.
    BelowSeparation { plant_bytes_cell: u64, control_bytes_cell: u64 },
    /// A boot failed: its spawn, its first `+PONG`, its census.
    Boot(String),
    /// A named census row absent on a cell of a candidate boot.
    Vacuous(String),
    /// The boots are not of one shape (pinned memory).
    ShapeDiffers(String),
    /// A plant row reads the same on both arms: the plant never engaged.
    Unengaged(String),
    /// A control spread over the control.
    Noisy { arm: &'static str, spread_bytes_cell: u64, control_bytes_cell: u64 },
}

impl fmt::Display for NoVerdictReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            NoVerdictReason::Withheld(why) => write!(f, "WITHHELD: {why}"),
            NoVerdictReason::Usage(why) => write!(f, "NO VERDICT: {why}"),
            NoVerdictReason::BelowSeparation { plant_bytes_cell, control_bytes_cell } => write!(
                f,
                "NO VERDICT: the plant's {plant_bytes_cell} B a cell is not over 10 × the \
                 control's {control_bytes_cell} B a cell: noise could decide, so no boot ran"
            ),
            NoVerdictReason::Boot(why) => write!(f, "NO VERDICT: a boot failed: {why}"),
            NoVerdictReason::Vacuous(why) => write!(f, "NO VERDICT: vacuous: {why}"),
            NoVerdictReason::ShapeDiffers(why) => write!(f, "NO VERDICT: shape differs: {why}"),
            NoVerdictReason::Unengaged(why) => write!(f, "NO VERDICT: unengaged: {why}"),
            NoVerdictReason::Noisy { arm, spread_bytes_cell, control_bytes_cell } => write!(
                f,
                "NO VERDICT: control spread {spread_bytes_cell} B a cell on the {arm} arm, over \
                 the control's {control_bytes_cell} B a cell"
            ),
        }
    }
}

/// A resident-delta verdict: one of three outcomes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum ResidentVerdict {
    /// Within the budget, or under the plant's rise.
    Within(String),
    /// Over the budget, or over the plant's rise.
    Over(String),
    NoVerdict(NoVerdictReason),
}

impl ResidentVerdict {
    /// The verdict's line, `<TOKEN>-OVER: …`, `<TOKEN>-WITHIN: …`,
    /// `WITHHELD: …` or `NO VERDICT: …`: the canary lane counts a row caught
    /// only on a line that begins with its token.
    pub(crate) fn line(&self, token: &str) -> String {
        match self {
            ResidentVerdict::Within(detail) => format!("{token}-WITHIN: {detail}"),
            ResidentVerdict::Over(detail) => format!("{token}-OVER: {detail}"),
            ResidentVerdict::NoVerdict(reason) => reason.to_string(),
        }
    }
}

/// How a resident-delta subcommand ends: its preflight alone, the host
/// admitted; or a verdict.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Ended {
    Admitted,
    Judged(ResidentVerdict),
}

/// The subcommand's exit code, its only one: 0 admitted or within, 1 over,
/// 2 no verdict. A panic is 101, which the canary lane reads as red for
/// another reason.
pub(crate) fn exit_code(ended: &Ended) -> u8 {
    match ended {
        Ended::Admitted | Ended::Judged(ResidentVerdict::Within(_)) => 0,
        Ended::Judged(ResidentVerdict::Over(_)) => 1,
        Ended::Judged(ResidentVerdict::NoVerdict(_)) => 2,
    }
}

/// The verdict on a reading. A spread over the control decides first
/// (`Noisy`); then the budget (`r > b`) or the plant (`10 r > 9 n`).
pub(crate) fn judge(reading: Reading, spreads: &ControlSpreads, mode: Mode) -> ResidentVerdict {
    if let Some(noisy) = spreads.first_over() {
        return ResidentVerdict::NoVerdict(noisy);
    }
    let rise = reading.rise_bytes_cell();
    let medians = format!(
        "reading {rise} B a cell (median {} candidate, {} base)",
        reading.candidate_bytes_cell, reading.base_bytes_cell
    );
    match mode {
        Mode::Budget { budget_bytes_cell } => {
            let detail = format!("{medians}, budget {budget_bytes_cell} B a cell");
            if rise > i128::from(budget_bytes_cell) {
                ResidentVerdict::Over(detail)
            } else {
                ResidentVerdict::Within(detail)
            }
        }
        Mode::Plant(threshold) => {
            let plant = threshold.plant_bytes_cell;
            let detail = format!(
                "{medians}, plant {plant} B a cell, caught over {}/{} of it",
                RISE.0, RISE.1
            );
            // |rise| < 2^64 and RISE.1 is small: no product overflows an i128.
            if rise * i128::from(RISE.1) > i128::from(plant) * i128::from(RISE.0) {
                ResidentVerdict::Over(detail)
            } else {
                ResidentVerdict::Within(detail)
            }
        }
    }
}

/// The host as the instrument reads it, printed before any boot.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Host {
    release: String,
    cpus_online: String,
    thp: String,
    memlock: String,
}

impl fmt::Display for Host {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "host: Linux {}, {} online CPUs, THP {}, memlock {}",
            self.release, self.cpus_online, self.thp, self.memlock
        )
    }
}

/// What the preflight reads from the host: `status` is whether
/// `/proc/self/status` read, `thp` the transparent-huge-page setting's file.
struct HostReads {
    status: Result<(), String>,
    thp: Result<String, String>,
    release: Result<String, String>,
    cpus_online: Result<String, String>,
    memlock: Result<String, String>,
}

/// The host's admission, before any argument is read: Linux, a readable
/// `/proc/self/status` (every boot's reading), transparent huge pages not
/// `[always]` (a huge page makes resident memory move in 2 MiB steps, which
/// no control separates). Anything else is `Withheld`.
pub(crate) fn preflight() -> Result<Host, NoVerdictReason> {
    if !cfg!(target_os = "linux") {
        return Err(NoVerdictReason::Withheld(
            "the instrument reads /proc and the shipped backend: Linux only".into(),
        ));
    }
    let read = |path: &str| std::fs::read_to_string(path).map_err(|e| format!("{path}: {e}"));
    admit(HostReads {
        status: read("/proc/self/status").map(|_| ()),
        thp: read("/sys/kernel/mm/transparent_hugepage/enabled"),
        release: read("/proc/sys/kernel/osrelease"),
        cpus_online: read("/sys/devices/system/cpu/online"),
        memlock: crate::gaterun::proc::read_memlock_limit(std::process::id())
            .map_err(|e| e.to_string()),
    })
}

fn admit(reads: HostReads) -> Result<Host, NoVerdictReason> {
    if let Err(why) = reads.status {
        return Err(NoVerdictReason::Withheld(format!("no resident reading: {why}")));
    }
    let thp = match reads.thp {
        Ok(text) => thp_mode(&text),
        // A kernel built without transparent huge pages has no such file.
        Err(why) => format!("absent ({why})"),
    };
    if thp == "always" {
        return Err(NoVerdictReason::Withheld(
            "transparent huge pages [always]: resident memory moves in huge pages".into(),
        ));
    }
    let known = |read: Result<String, String>| read.unwrap_or_else(|e| format!("unknown ({e})"));
    Ok(Host {
        release: known(reads.release.map(|text| text.trim().to_string())),
        cpus_online: known(reads.cpus_online.and_then(|list| count_cpu_list(list.trim()))),
        thp,
        memlock: known(reads.memlock),
    })
}

/// The bracketed word of `always [madvise] never`, or the text itself.
fn thp_mode(text: &str) -> String {
    text.split_whitespace()
        .find_map(|word| word.strip_prefix('[').and_then(|w| w.strip_suffix(']')))
        .unwrap_or(text.trim())
        .to_string()
}

/// The CPUs a list like `0-3,8,10-11` names (the kernel's `online` file).
fn count_cpu_list(list: &str) -> Result<String, String> {
    let mut count: u64 = 0;
    for span in list.split(',') {
        let (first, last) = span.split_once('-').unwrap_or((span, span));
        let parse = |n: &str| n.parse::<u64>().map_err(|_| format!("online CPUs `{list}`"));
        let (first, last) = (parse(first)?, parse(last)?);
        let width = last.checked_sub(first).ok_or_else(|| format!("online CPUs `{list}`"))?;
        count = count.saturating_add(width.saturating_add(1));
    }
    Ok(count.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    const KIB: u64 = 1024;
    const CONTROL: u64 = 256 * KIB;

    fn control() -> Control {
        Control::of_bytes_cell(CONTROL)
    }

    fn spreads(base: u64, candidate: u64) -> ControlSpreads {
        ControlSpreads::new(vec![("base", base), ("candidate", candidate)], control())
            .expect("two arms")
    }

    fn plant(n: u64) -> Mode {
        Mode::Plant(PlantThreshold::new(n, control()).expect("separated"))
    }

    fn reading(base: u64, rise: u64) -> Reading {
        Reading { base_bytes_cell: base, candidate_bytes_cell: base + rise }
    }

    fn is_over(verdict: &ResidentVerdict) -> bool {
        matches!(verdict, ResidentVerdict::Over(_))
    }

    fn is_within(verdict: &ResidentVerdict) -> bool {
        matches!(verdict, ResidentVerdict::Within(_))
    }

    /// The worst fully resident reading the separation promises to catch:
    /// the plant at the least separated size, read a whole control low.
    #[test]
    fn a_fully_resident_plant_read_a_control_low_is_over() {
        let n = 10 * CONTROL + KIB;
        let verdict = judge(reading(25 << 20, n - CONTROL), &spreads(CONTROL, CONTROL), plant(n));
        assert!(is_over(&verdict), "{verdict:?}");
        assert!(verdict.line("T").starts_with("T-OVER: "), "{}", verdict.line("T"));
    }

    /// Four fifths resident and read a whole control high stays under 0.9 n.
    #[test]
    fn a_plant_four_fifths_resident_plus_the_control_is_within() {
        let n = 10 * CONTROL + KIB;
        let verdict = judge(reading(25 << 20, n * 4 / 5 + CONTROL), &spreads(0, 0), plant(n));
        assert!(is_within(&verdict), "{verdict:?}");
        assert!(verdict.line("T").starts_with("T-WITHIN: "), "{}", verdict.line("T"));
    }

    #[test]
    fn the_same_readings_on_both_arms_are_within() {
        let n = 10 * CONTROL + KIB;
        assert!(is_within(&judge(reading(25 << 20, 0), &spreads(KIB, KIB), plant(n))));
        let budget = Mode::Budget { budget_bytes_cell: 0 };
        assert!(is_within(&judge(reading(25 << 20, 0), &spreads(KIB, KIB), budget)));
    }

    #[test]
    fn a_spread_over_the_control_on_either_arm_is_noisy() {
        let n = 10 * CONTROL + KIB;
        for (base, candidate, arm) in [(CONTROL + KIB, 0, "base"), (0, CONTROL + KIB, "candidate")]
        {
            let verdict = judge(reading(25 << 20, n), &spreads(base, candidate), plant(n));
            let expected = NoVerdictReason::Noisy {
                arm,
                spread_bytes_cell: CONTROL + KIB,
                control_bytes_cell: CONTROL,
            };
            assert_eq!(verdict, ResidentVerdict::NoVerdict(expected));
            assert!(verdict.line("T").starts_with("NO VERDICT: control spread "));
        }
        let at_control = judge(reading(25 << 20, n), &spreads(CONTROL, CONTROL), plant(n));
        assert!(is_over(&at_control), "a spread at the control is within it");
    }

    #[test]
    fn a_plant_at_ten_controls_is_refused_and_one_byte_over_is_not() {
        let refused = PlantThreshold::new(10 * CONTROL, control());
        assert_eq!(
            refused,
            Err(NoVerdictReason::BelowSeparation {
                plant_bytes_cell: 10 * CONTROL,
                control_bytes_cell: CONTROL
            })
        );
        assert!(PlantThreshold::new(10 * CONTROL + 1, control()).is_ok());
        assert!(PlantThreshold::new(0, Control::of_bytes_cell(0)).is_err());
        assert!(PlantThreshold::new(u64::MAX, Control::of_bytes_cell(u64::MAX / 10)).is_ok());
        assert!(PlantThreshold::new(u64::MAX, Control::of_bytes_cell(u64::MAX)).is_err());
    }

    #[test]
    fn a_budget_is_within_at_its_bound_and_over_one_byte_past() {
        let budget = Mode::Budget { budget_bytes_cell: 3 << 20 };
        assert!(is_within(&judge(reading(25 << 20, 3 << 20), &spreads(0, 0), budget)));
        assert!(is_over(&judge(reading(25 << 20, (3 << 20) + 1), &spreads(0, 0), budget)));
        let fall = Reading { base_bytes_cell: 25 << 20, candidate_bytes_cell: 0 };
        assert!(is_within(&judge(fall, &spreads(0, 0), Mode::Budget { budget_bytes_cell: 0 })));
    }

    #[test]
    fn the_exit_codes_are_zero_one_and_two() {
        let within = ResidentVerdict::Within(String::new());
        let over = ResidentVerdict::Over(String::new());
        let none = ResidentVerdict::NoVerdict(NoVerdictReason::Usage(String::new()));
        assert_eq!(exit_code(&Ended::Admitted), 0);
        assert_eq!(exit_code(&Ended::Judged(within)), 0);
        assert_eq!(exit_code(&Ended::Judged(over)), 1);
        assert_eq!(exit_code(&Ended::Judged(none)), 2);
    }

    /// The subcommand's dispatch returns `exit_code`'s value and nothing
    /// else: `inf-bench`'s generic failure (exit 1) would read as over.
    #[test]
    fn the_dispatch_returns_the_verdicts_exit_code_alone() {
        let main = include_str!("main.rs");
        let (_, after) = main.split_once("if cmd == \"idle-resident\" {").expect("the dispatch");
        let (block, _) = after.split_once('}').expect("its end");
        let code: Vec<&str> = block
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && !line.starts_with("//"))
            .collect();
        assert_eq!(
            code,
            [
                "let ended = idle_resident::cmd_idle_resident(rest);",
                "return ExitCode::from(resident_delta::exit_code(&ended));"
            ]
        );
    }

    #[test]
    fn a_control_needs_at_least_one_spread() {
        assert_eq!(ControlSpreads::new(Vec::new(), control()), None);
        let one = ControlSpreads::new(vec![("base", 7)], control()).expect("one arm");
        assert_eq!(one.line(), "control: spreads 7 (base), within 262144 (bytes a cell)");
    }

    fn reads(thp: &str) -> HostReads {
        HostReads {
            status: Ok(()),
            thp: Ok(thp.to_string()),
            release: Ok("7.0.0-43-generic\n".into()),
            cpus_online: Ok("0-23\n".into()),
            memlock: Ok("8589934592 8589934592 bytes".into()),
        }
    }

    #[test]
    fn the_preflight_withholds_huge_pages_always_and_an_unreadable_status() {
        assert!(matches!(
            admit(reads("[always] madvise never\n")),
            Err(NoVerdictReason::Withheld(_))
        ));
        let host = admit(reads("always [madvise] never\n")).expect("admitted");
        assert_eq!(
            host.to_string(),
            "host: Linux 7.0.0-43-generic, 24 online CPUs, THP madvise, memlock 8589934592 \
             8589934592 bytes"
        );
        let mut unreadable = reads("never");
        unreadable.status = Err("/proc/self/status: denied".into());
        let withheld = admit(unreadable).expect_err("no reading");
        assert!(withheld.to_string().starts_with("WITHHELD: no resident reading"));
        let mut no_thp = reads("");
        no_thp.thp = Err("absent".into());
        assert!(admit(no_thp).is_ok(), "a kernel without huge pages is admitted");
    }

    #[test]
    fn the_online_cpu_list_counts_every_span() {
        assert_eq!(count_cpu_list("0-23"), Ok("24".into()));
        assert_eq!(count_cpu_list("0,2-3,8"), Ok("4".into()));
        assert!(count_cpu_list("3-1").is_err());
        assert!(count_cpu_list("").is_err());
    }
}
