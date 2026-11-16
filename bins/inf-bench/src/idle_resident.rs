//! `inf-bench idle-resident`: the idle node's resident memory, one build
//! against another, in bytes a cell, judged by the resident-delta verdict
//! ([`crate::resident_delta`]).
//!
//! Estimator and scope: each boot is `empty_idle_rss`'s memory leg (4 cells
//! at the shipped defaults, the first `+PONG`, 5 s settle, 10 s window),
//! VmRSS at the window's end divided by the cells (floor: every cell builds
//! the same reserve). An arm's value is the median of its three boots, and
//! the reading is median(candidate) − median(base), signed. Resolution: one
//! 4 KiB page at the node, 1 KiB a cell. Control: the row's
//! `--control-bytes-cell`, which each arm's three boots must lie within;
//! the arms interleave in [`LEG_ORDER`], so both share the box's drift.
//! Liveness, checked in this order before the judge: every boot serves with
//! its census `serving` on every cell; every boot's receive pools
//! registered (VmPin over the floor `PinnedRss::prove` names); every named
//! census row on every cell of every candidate boot; VmPin within a page
//! inside an arm, and across the arms within the control of the declared
//! `--pinned-bytes-cell`; in plant mode, every `--plant-row`'s `entries_max`
//! moved on every cell. A boot is one node at a time; a hung boot ends at
//! the boot deadline and its guard kills and reaps it.

use std::collections::BTreeMap;

use crate::cli::Flags;
use crate::m1rows::{EMPTY_NODE_CELLS, MemoryLeg, PinUnproven, PinnedRss, idle_leg};
use crate::resident_delta::{
    Control, ControlSpreads, Ended, Mode, NoVerdictReason, PlantThreshold, Reading,
    ResidentVerdict, judge, preflight,
};

/// The verdict line's token: `IDLE-RESIDENT-OVER:`, `IDLE-RESIDENT-WITHIN:`.
const TOKEN: &str = "IDLE-RESIDENT";

/// The page the kernel counts resident and pinned memory in, bytes.
const PAGE_BYTES: u64 = 4096;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Arm {
    Base,
    Candidate,
}

impl Arm {
    fn name(self) -> &'static str {
        match self {
            Arm::Base => "base",
            Arm::Candidate => "candidate",
        }
    }
}

/// The boots, one node at a time, A B B A A B (A the base, B the
/// candidate): three an arm, each arm's first and last apart, so a drift of
/// the box over the run falls on both arms alike.
const LEG_ORDER: [Arm; 6] =
    [Arm::Base, Arm::Candidate, Arm::Candidate, Arm::Base, Arm::Base, Arm::Candidate];

const FLAGS: [&str; 9] = [
    "preflight",
    "base",
    "candidate",
    "budget-bytes-cell",
    "plant-bytes-cell",
    "control-bytes-cell",
    "caps-row",
    "plant-row",
    "pinned-bytes-cell",
];

/// The parsed arguments; every one was checked before any boot.
#[derive(Debug)]
struct Options {
    base: String,
    candidate: String,
    mode: Mode,
    control: Control,
    /// Census rows every candidate cell must carry (`cap_row_<name>`).
    caps_rows: Vec<String>,
    /// Census rows the plant scales: each must carry another `entries_max`
    /// on the candidate than on the base, on every cell.
    plant_rows: Vec<String>,
    /// The pinned bytes a cell the candidate adds, declared by the row.
    pinned_bytes_cell: i64,
}

impl Options {
    fn census_rows(&self) -> Vec<String> {
        self.caps_rows.iter().chain(&self.plant_rows).cloned().collect()
    }
}

/// The subcommand. `--preflight` alone runs the host's admission only.
pub(crate) fn cmd_idle_resident(args: &[String]) -> Ended {
    let host = match preflight() {
        Ok(host) => host,
        Err(reason) => return finish(ResidentVerdict::NoVerdict(reason)),
    };
    println!("{host}");
    if args.iter().any(|arg| arg == "--preflight") {
        if args.len() == 1 {
            return Ended::Admitted;
        }
        let usage = NoVerdictReason::Usage("--preflight runs alone".into());
        return finish(ResidentVerdict::NoVerdict(usage));
    }
    let options = match parse(args) {
        Ok(options) => options,
        Err(reason) => return finish(ResidentVerdict::NoVerdict(reason)),
    };
    let names = options.census_rows();
    let arms = boot_arms(|arm, index| boot(&options, &names, arm, index));
    finish(match arms {
        Ok(arms) => {
            if let Some(spreads) = control_spreads(&arms, options.control) {
                println!("{}", spreads.line());
            }
            evaluate(&arms, &options)
        }
        Err(reason) => ResidentVerdict::NoVerdict(reason),
    })
}

fn finish(verdict: ResidentVerdict) -> Ended {
    println!("{}", verdict.line(TOKEN));
    Ended::Judged(verdict)
}

/// The arguments, refused before any boot: a usage error first, then a
/// plant that does not separate from its control.
fn parse(args: &[String]) -> Result<Options, NoVerdictReason> {
    let flags = Flags::parse(args, &[], &FLAGS).map_err(NoVerdictReason::Usage)?;
    let base = executable(&flags, "base")?;
    let candidate = executable(&flags, "candidate")?;
    let control = number(&flags, "control-bytes-cell")?.ok_or_else(|| {
        NoVerdictReason::Usage("--control-bytes-cell is required: no verdict without one".into())
    })?;
    let control = Control::of_bytes_cell(control);
    let caps_rows = names(&flags, "caps-row")?;
    let plant_rows = names(&flags, "plant-row")?;
    let pinned_bytes_cell = match flags.get("pinned-bytes-cell") {
        None => 0,
        Some(raw) => raw.parse::<i64>().map_err(|_| {
            NoVerdictReason::Usage(format!("--pinned-bytes-cell `{raw}` is not a number"))
        })?,
    };
    let mode = match (number(&flags, "budget-bytes-cell")?, number(&flags, "plant-bytes-cell")?) {
        (Some(budget_bytes_cell), None) if plant_rows.is_empty() => {
            Mode::Budget { budget_bytes_cell }
        }
        (Some(_), None) => {
            let why = "--plant-row names a plant's census rows, and a budget run reads none";
            return Err(NoVerdictReason::Usage(why.into()));
        }
        (None, Some(_)) if plant_rows.is_empty() => {
            return Err(NoVerdictReason::Usage("a plant needs its --plant-row".into()));
        }
        (None, Some(plant_bytes_cell)) => {
            Mode::Plant(PlantThreshold::new(plant_bytes_cell, control)?)
        }
        (None, None) | (Some(_), Some(_)) => {
            return Err(NoVerdictReason::Usage(
                "exactly one of --budget-bytes-cell and --plant-bytes-cell".into(),
            ));
        }
    };
    Ok(Options { base, candidate, mode, control, caps_rows, plant_rows, pinned_bytes_cell })
}

/// A path that is a file with an execute bit.
fn executable(flags: &Flags, name: &str) -> Result<String, NoVerdictReason> {
    use std::os::unix::fs::PermissionsExt as _;
    let path =
        flags.get(name).ok_or_else(|| NoVerdictReason::Usage(format!("--{name} is required")))?;
    let runs = std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0);
    if runs {
        Ok(path.to_string())
    } else {
        Err(NoVerdictReason::Usage(format!("--{name} {path} is not an executable file")))
    }
}

fn number(flags: &Flags, name: &str) -> Result<Option<u64>, NoVerdictReason> {
    flags
        .get(name)
        .map(|raw| {
            raw.parse::<u64>()
                .map_err(|_| NoVerdictReason::Usage(format!("--{name} `{raw}` is not a number")))
        })
        .transpose()
}

/// A comma-separated list of census row names, each of letters, digits,
/// `_` and `-` (the census's spelling: `cap_row_timer-owners`).
fn names(flags: &Flags, name: &str) -> Result<Vec<String>, NoVerdictReason> {
    let Some(raw) = flags.get(name) else { return Ok(Vec::new()) };
    raw.split(',')
        .map(|row| {
            let spelled = !row.is_empty()
                && row.bytes().all(|byte| byte.is_ascii_alphanumeric() || b"_-".contains(&byte));
            if spelled {
                Ok(row.to_string())
            } else {
                Err(NoVerdictReason::Usage(format!("--{name} `{raw}`: a row `{row}`")))
            }
        })
        .collect()
}

/// One boot's facts, the only input the checks below read.
#[derive(Debug)]
struct Boot {
    rss: Result<PinnedRss, PinUnproven>,
    pinned_bytes: u64,
    memlock: String,
    /// Per cell, in cell order: each named census row's `entries_max`;
    /// a row the cell does not carry, or carries in another shape, is
    /// absent.
    rows: Vec<BTreeMap<String, u64>>,
}

impl Boot {
    /// The leg's facts; a census not `serving` on every cell is a failed
    /// boot.
    fn from_leg(leg: &MemoryLeg, names: &[String]) -> Result<Boot, NoVerdictReason> {
        let cells =
            leg.cells().as_ref().map_err(|e| NoVerdictReason::Boot(format!("INFO: {e}")))?;
        let mut rows = Vec::with_capacity(cells.len());
        for (index, info) in cells.iter().enumerate() {
            let phase = info.get("cap_census_phase").map_or("absent", String::as_str);
            if phase != "serving" {
                let why = format!("cell {index}'s cap_census_phase is {phase}, not serving");
                return Err(NoVerdictReason::Boot(why));
            }
            let read = |name: &String| {
                let value = info.get(&format!("cap_row_{name}"))?;
                Some((name.clone(), entries_max(value)?))
            };
            rows.push(names.iter().filter_map(read).collect());
        }
        Ok(Boot {
            rss: leg.rss(),
            pinned_bytes: leg.pinned_bytes(),
            memlock: leg.memlock().to_string(),
            rows,
        })
    }

    /// The boot's bytes a cell; `None` with the registration unproven.
    fn rss_bytes_cell(&self) -> Option<u64> {
        self.rss.ok().map(bytes_cell)
    }

    fn line(&self, arm: Arm, index: usize) -> String {
        let registration = match self.rss {
            Ok(_) => "registration proven".to_string(),
            Err(unproven) => unproven.to_string(),
        };
        let bytes_cell = self.rss_bytes_cell().map_or_else(|| "no".into(), |b| b.to_string());
        format!(
            "boot {}/{} {}: {bytes_cell} B a cell, VmPin {} B, {registration}",
            index + 1,
            LEG_ORDER.len(),
            arm.name(),
            self.pinned_bytes
        )
    }
}

/// VmRSS divided by the cells, floor: every cell builds the same reserve.
fn bytes_cell(rss: PinnedRss) -> u64 {
    rss.rss_bytes() / u64::from(EMPTY_NODE_CELLS)
}

/// `entries_max=<n>,fill=…`: the census row's cap.
fn entries_max(value: &str) -> Option<u64> {
    value.split(',').find_map(|field| field.strip_prefix("entries_max="))?.parse().ok()
}

/// Boots one leg of `arm` and prints its line.
fn boot(
    options: &Options,
    names: &[String],
    arm: Arm,
    index: usize,
) -> Result<Boot, NoVerdictReason> {
    let binary = match arm {
        Arm::Base => &options.base,
        Arm::Candidate => &options.candidate,
    };
    let leg = idle_leg(binary).map_err(NoVerdictReason::Boot)?;
    let facts = Boot::from_leg(&leg, names)?;
    println!("{}", facts.line(arm, index));
    Ok(facts)
}

/// Both arms' boots, in the order they ran.
#[derive(Debug, Default)]
struct Arms {
    base: Vec<Boot>,
    candidate: Vec<Boot>,
}

/// Boots in [`LEG_ORDER`]. A failed boot ends the run: it decides first.
fn boot_arms<F>(mut boot: F) -> Result<Arms, NoVerdictReason>
where
    F: FnMut(Arm, usize) -> Result<Boot, NoVerdictReason>,
{
    let mut arms = Arms::default();
    for (index, arm) in LEG_ORDER.into_iter().enumerate() {
        let facts = boot(arm, index)?;
        match arm {
            Arm::Base => arms.base.push(facts),
            Arm::Candidate => arms.candidate.push(facts),
        }
    }
    Ok(arms)
}

/// The checks in their order, then the judge on the reading.
fn evaluate(arms: &Arms, options: &Options) -> ResidentVerdict {
    match checked(arms, options) {
        Ok((reading, spreads)) => judge(reading, &spreads, options.mode),
        Err(reason) => ResidentVerdict::NoVerdict(reason),
    }
}

fn checked(arms: &Arms, options: &Options) -> Result<(Reading, ControlSpreads), NoVerdictReason> {
    let base = registered(&arms.base)?;
    let candidate = registered(&arms.candidate)?;
    rows_present(&arms.candidate, &options.census_rows())?;
    pin_shape(arms, options)?;
    if let Mode::Plant(_) = options.mode {
        engaged(arms, &options.plant_rows)?;
    }
    let spreads = control_spreads(arms, options.control).ok_or_else(|| {
        NoVerdictReason::Vacuous("no boot of either arm: no same-binary control".into())
    })?;
    let (Some(base_bytes_cell), Some(candidate_bytes_cell)) = (median(&base), median(&candidate))
    else {
        return Err(NoVerdictReason::Vacuous("an arm with no boot".into()));
    };
    Ok((Reading { base_bytes_cell, candidate_bytes_cell }, spreads))
}

/// Every boot's bytes a cell, or the first boot whose registration is
/// unproven: its pools were never touched, so its resident memory is not
/// the node's.
fn registered(boots: &[Boot]) -> Result<Vec<u64>, NoVerdictReason> {
    boots
        .iter()
        .map(|boot| match boot.rss {
            Ok(rss) => Ok(bytes_cell(rss)),
            Err(unproven) => Err(NoVerdictReason::Withheld(format!(
                "registration unproven (VmPin {}, floor {}, memlock {})",
                unproven.pinned_bytes(),
                unproven.floor_bytes(),
                boot.memlock
            ))),
        })
        .collect()
}

/// Every named row on every cell of every candidate boot.
fn rows_present(candidate: &[Boot], names: &[String]) -> Result<(), NoVerdictReason> {
    for (boot, cells) in candidate.iter().enumerate() {
        for (cell, rows) in cells.rows.iter().enumerate() {
            if let Some(name) = names.iter().find(|name| !rows.contains_key(*name)) {
                let why = format!(
                    "census row {name} absent on cell {cell} of candidate boot {}",
                    boot + 1
                );
                return Err(NoVerdictReason::Vacuous(why));
            }
        }
    }
    Ok(())
}

/// VmPin is one value a binary within a page, and the candidate's differs
/// from the base's by the declared pinned bytes, within the control.
fn pin_shape(arms: &Arms, options: &Options) -> Result<(), NoVerdictReason> {
    let pins = |boots: &[Boot]| boots.iter().map(|boot| boot.pinned_bytes).collect::<Vec<_>>();
    let (base, candidate) = (pins(&arms.base), pins(&arms.candidate));
    for (arm, values) in [(Arm::Base, &base), (Arm::Candidate, &candidate)] {
        let apart = spread(values);
        if apart > PAGE_BYTES {
            let why = format!("VmPin of the {} arm spreads {apart} B, over one page", arm.name());
            return Err(NoVerdictReason::ShapeDiffers(why));
        }
    }
    let (Some(base_pin), Some(candidate_pin)) = (median(&base), median(&candidate)) else {
        return Ok(());
    };
    let cells = i128::from(EMPTY_NODE_CELLS);
    let delta = i128::from(candidate_pin) - i128::from(base_pin);
    let off = (delta - i128::from(options.pinned_bytes_cell) * cells).abs();
    if off > i128::from(options.control.bytes_cell()) * cells {
        let why = format!(
            "VmPin {candidate_pin} B (candidate) against {base_pin} B (base): {delta} B at the \
             node, declared {} B a cell",
            options.pinned_bytes_cell
        );
        return Err(NoVerdictReason::ShapeDiffers(why));
    }
    Ok(())
}

/// Every plant row's `entries_max` differs between every base boot and
/// every candidate boot, on every cell: a plant that moved nothing in the
/// census never engaged, whatever the pages read.
fn engaged(arms: &Arms, plant_rows: &[String]) -> Result<(), NoVerdictReason> {
    let pairs = arms
        .base
        .iter()
        .flat_map(|base| arms.candidate.iter().map(move |candidate| (base, candidate)));
    for (base, candidate) in pairs {
        if let Some((name, cell, cap)) = same_cap(base, candidate, plant_rows) {
            let why =
                format!("plant row {name} reads entries_max {cap} on cell {cell} of both arms");
            return Err(NoVerdictReason::Unengaged(why));
        }
    }
    Ok(())
}

/// The first plant row whose `entries_max` reads the same on one cell of
/// both boots: `(row, cell, entries_max)`.
fn same_cap<'a>(
    base: &Boot,
    candidate: &Boot,
    plant_rows: &'a [String],
) -> Option<(&'a str, usize, u64)> {
    for (cell, (base_rows, candidate_rows)) in base.rows.iter().zip(&candidate.rows).enumerate() {
        for name in plant_rows {
            if let (Some(cap), Some(other)) = (base_rows.get(name), candidate_rows.get(name))
                && cap == other
            {
                return Some((name, cell, *cap));
            }
        }
    }
    None
}

/// Each arm's spread over its registered boots, beside the control.
fn control_spreads(arms: &Arms, control: Control) -> Option<ControlSpreads> {
    let values = |boots: &[Boot]| boots.iter().filter_map(Boot::rss_bytes_cell).collect::<Vec<_>>();
    let mut spreads = Vec::new();
    for (arm, boots) in [(Arm::Base, &arms.base), (Arm::Candidate, &arms.candidate)] {
        let values = values(boots);
        if !values.is_empty() {
            spreads.push((arm.name(), spread(&values)));
        }
    }
    ControlSpreads::new(spreads, control)
}

/// The largest less the smallest; 0 for no value.
fn spread(values: &[u64]) -> u64 {
    let (Some(max), Some(min)) = (values.iter().max(), values.iter().min()) else { return 0 };
    max - min
}

/// The upper median, the harness's convention (the middle of three).
fn median(values: &[u64]) -> Option<u64> {
    let mut sorted = values.to_vec();
    sorted.sort_unstable();
    sorted.get(sorted.len() / 2).copied()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::gaterun::ProcSample;
    use crate::m1rows::RECEIVE_BUFFERS_DEFAULT;

    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const CONTROL: u64 = 256 * KIB;
    const CELLS: u64 = EMPTY_NODE_CELLS as u64;
    /// `PinnedRss::prove`'s floor at the default pools: 2 (N − 1) × 4096 ×
    /// 4 KiB.
    const PIN_FLOOR: u64 = 2 * (CELLS - 1) * RECEIVE_BUFFERS_DEFAULT * 4096;

    fn options(mode: Mode) -> Options {
        Options {
            base: "base".into(),
            candidate: "candidate".into(),
            mode,
            control: Control::of_bytes_cell(CONTROL),
            caps_rows: vec!["caps_row".into()],
            plant_rows: match mode {
                Mode::Plant(_) => vec!["plant_row".into()],
                Mode::Budget { .. } => Vec::new(),
            },
            pinned_bytes_cell: 0,
        }
    }

    fn plant(n: u64) -> Mode {
        Mode::Plant(PlantThreshold::new(n, Control::of_bytes_cell(CONTROL)).expect("separated"))
    }

    fn budget() -> Mode {
        Mode::Budget { budget_bytes_cell: 4 * MIB }
    }

    /// A boot of `rss_bytes_cell` a cell, VmPin `pinned` at the node, and
    /// each cell carrying `caps_row` and `plant_row` at `entries_max`.
    fn boot_of(rss_bytes_cell: u64, pinned: u64, entries_max: u64) -> Boot {
        let sample = ProcSample::fixture(rss_bytes_cell * CELLS, pinned, 0, 0);
        let row = |name: &str| (name.to_string(), entries_max);
        Boot {
            rss: PinnedRss::prove(&sample, CELLS, RECEIVE_BUFFERS_DEFAULT),
            pinned_bytes: pinned,
            memlock: "8589934592 8589934592 bytes".into(),
            rows: (0..CELLS).map(|_| [row("caps_row"), row("plant_row")].into()).collect(),
        }
    }

    const PINNED: u64 = PIN_FLOOR + 64 * MIB;

    /// Three base boots at 25 MiB a cell, three candidate boots raised by
    /// `rise`, every boot pinning `pinned`, the plant row at 1 024 on the
    /// base and 2 048 on the candidate.
    fn arms_pinned(rise: u64, pinned: u64) -> Arms {
        let base = 25 * MIB;
        Arms {
            base: (0..3).map(|_| boot_of(base, pinned, 1024)).collect(),
            candidate: (0..3).map(|_| boot_of(base + rise, pinned, 2048)).collect(),
        }
    }

    fn arms(rise: u64) -> Arms {
        arms_pinned(rise, PINNED)
    }

    fn is_reason(verdict: &ResidentVerdict, reason: fn(&NoVerdictReason) -> bool) -> bool {
        matches!(verdict, ResidentVerdict::NoVerdict(found) if reason(found))
    }

    #[test]
    fn a_pin_two_pages_apart_inside_an_arm_differs_in_shape() {
        let shape = |reason: &NoVerdictReason| matches!(reason, NoVerdictReason::ShapeDiffers(_));
        let mut apart = arms(4 * MIB);
        apart.base[1].pinned_bytes += 2 * PAGE_BYTES;
        let verdict = evaluate(&apart, &options(plant(3 * MIB)));
        assert!(is_reason(&verdict, shape), "{verdict:?}");
        let mut one_page = arms(4 * MIB);
        one_page.base[1].pinned_bytes += PAGE_BYTES;
        let verdict = evaluate(&one_page, &options(plant(3 * MIB)));
        assert!(matches!(verdict, ResidentVerdict::Over(_)), "one page is within: {verdict:?}");
    }

    #[test]
    fn a_row_absent_on_one_cell_is_vacuous_and_a_plant_row_equal_on_one_is_unengaged() {
        let vacuous = |reason: &NoVerdictReason| matches!(reason, NoVerdictReason::Vacuous(_));
        let unengaged = |reason: &NoVerdictReason| matches!(reason, NoVerdictReason::Unengaged(_));
        let mut missing = arms(4 * MIB);
        missing.candidate[2].rows[3].remove("caps_row");
        let verdict = evaluate(&missing, &options(plant(3 * MIB)));
        assert!(is_reason(&verdict, vacuous), "{verdict:?}");
        let mut base_missing = arms(4 * MIB);
        base_missing.base[0].rows[1].remove("caps_row");
        let verdict = evaluate(&base_missing, &options(plant(3 * MIB)));
        assert!(matches!(verdict, ResidentVerdict::Over(_)), "the base needs no row: {verdict:?}");
        let mut equal = arms(4 * MIB);
        equal.candidate[1].rows[2].insert("plant_row".into(), 1024);
        let verdict = evaluate(&equal, &options(plant(3 * MIB)));
        assert!(is_reason(&verdict, unengaged), "{verdict:?}");
        let verdict = evaluate(&equal, &options(budget()));
        assert!(matches!(verdict, ResidentVerdict::Within(_)), "a budget reads no plant row");
    }

    #[test]
    fn a_boot_at_the_pin_floor_is_withheld_and_one_page_over_is_judged() {
        let verdict = evaluate(&arms_pinned(4 * MIB, PIN_FLOOR), &options(plant(3 * MIB)));
        let ResidentVerdict::NoVerdict(NoVerdictReason::Withheld(why)) = &verdict else {
            panic!("{verdict:?}");
        };
        assert!(why.starts_with("registration unproven (VmPin "), "{why}");
        assert!(why.contains(&format!("floor {PIN_FLOOR}")), "{why}");
        assert!(verdict.line(TOKEN).starts_with("WITHHELD: registration unproven"));
        let over = arms_pinned(4 * MIB, PIN_FLOOR + PAGE_BYTES);
        let verdict = evaluate(&over, &options(plant(3 * MIB)));
        assert!(matches!(verdict, ResidentVerdict::Over(_)), "{verdict:?}");
    }

    #[test]
    fn a_pin_one_mib_a_cell_apart_across_arms_needs_its_declaration() {
        let shape = |reason: &NoVerdictReason| matches!(reason, NoVerdictReason::ShapeDiffers(_));
        let mut apart = arms(4 * MIB);
        for boot in &mut apart.candidate {
            boot.pinned_bytes += MIB * CELLS;
        }
        let verdict = evaluate(&apart, &options(plant(3 * MIB)));
        assert!(is_reason(&verdict, shape), "{verdict:?}");
        let mut declared = options(plant(3 * MIB));
        declared.pinned_bytes_cell = i64::try_from(MIB).expect("1 MiB");
        let verdict = evaluate(&apart, &declared);
        assert!(matches!(verdict, ResidentVerdict::Over(_)), "{verdict:?}");
    }

    /// The runner boots in A B B A A B, and the judge reads both arms'
    /// spreads: a spread over the control on either arm alone is `Noisy`.
    #[test]
    fn the_runner_boots_in_leg_order_and_the_judge_reads_both_spreads() {
        let mut order = Vec::new();
        let ran = boot_arms(|arm, _| {
            order.push(arm);
            Ok(match arm {
                Arm::Base => boot_of(25 * MIB, PINNED, 1024),
                Arm::Candidate => boot_of(29 * MIB, PINNED, 2048),
            })
        })
        .expect("every boot served");
        assert_eq!(order, LEG_ORDER);
        assert!(matches!(evaluate(&ran, &options(plant(3 * MIB))), ResidentVerdict::Over(_)));
        for wide in [Arm::Base, Arm::Candidate] {
            let mut noisy = arms(4 * MIB);
            let (boots, cap) = match wide {
                Arm::Base => (&mut noisy.base, 1024),
                Arm::Candidate => (&mut noisy.candidate, 2048),
            };
            let last = boots[2].rss_bytes_cell().expect("registered");
            boots[2] = boot_of(last + CONTROL + KIB, PINNED, cap);
            let verdict = evaluate(&noisy, &options(plant(3 * MIB)));
            let ResidentVerdict::NoVerdict(NoVerdictReason::Noisy { arm, .. }) = verdict else {
                panic!("{wide:?}: {verdict:?}");
            };
            assert_eq!(arm, wide.name());
        }
        let mut booted = 0;
        let failed = boot_arms(|_, index| {
            booted += 1;
            if index == 1 {
                return Err(NoVerdictReason::Boot("no +PONG".into()));
            }
            Ok(boot_of(25 * MIB, PINNED, 1024))
        });
        assert!(matches!(failed, Err(NoVerdictReason::Boot(_))), "a failed boot decides");
        assert_eq!(booted, 2, "and ends the run");
    }

    #[test]
    fn the_census_row_reads_its_cap() {
        let value = "entries_max=4096,fill=serving,high_water=12,crossings=0,capacity_now=4096";
        assert_eq!(entries_max(value), Some(4096));
        assert_eq!(entries_max("fill=serving"), None);
        assert_eq!(entries_max("entries_max=x"), None);
    }

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn the_arguments_are_refused_before_any_boot() {
        let exe = std::env::current_exe().expect("this test binary");
        let exe = exe.to_str().expect("UTF-8 path");
        let base = ["--base", exe, "--candidate", exe, "--control-bytes-cell", "262144"];
        let with = |more: &[&str]| parse(&args(&[&base[..], more].concat()));
        let usage = |result: Result<Options, NoVerdictReason>| {
            matches!(result, Err(NoVerdictReason::Usage(_)))
        };
        assert!(usage(with(&[])), "neither mode");
        assert!(usage(with(&["--budget-bytes-cell", "1", "--plant-bytes-cell", "9999999"])));
        assert!(usage(with(&["--plant-bytes-cell", "9999999"])), "a plant with no row");
        assert!(usage(with(&["--budget-bytes-cell", "1", "--plant-row", "row"])));
        assert!(usage(with(&["--budget-bytes-cell", "1", "--caps-row", "a,,b"])));
        assert!(usage(parse(&args(&["--base", "/nonexistent", "--candidate", exe]))));
        let no_control = ["--base", exe, "--candidate", exe, "--budget-bytes-cell", "1"];
        assert!(usage(parse(&args(&no_control))), "no control");
        let below = with(&["--plant-bytes-cell", "2621440", "--plant-row", "row"]);
        assert!(matches!(below, Err(NoVerdictReason::BelowSeparation { .. })), "{below:?}");
        let plant =
            with(&["--plant-bytes-cell", "2622464", "--plant-row", "a,b", "--caps-row", "c-d"])
                .expect("a separated plant");
        assert_eq!(plant.census_rows(), ["c-d", "a", "b"]);
        let pinned =
            with(&["--budget-bytes-cell", "1", "--pinned-bytes-cell", "-4096"]).expect("ok");
        assert_eq!(pinned.pinned_bytes_cell, -4096);
    }
}
