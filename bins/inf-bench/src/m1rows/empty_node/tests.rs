use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

use super::walk::{
    DIR_PLANT_FILLER_MIN, DIR_STEP_BYTES, EMPTY_DIR_WALK_ENTRIES_MAX, WalkError, dir_plant,
    filler_bytes,
};
use super::*;
use crate::gaterun::serving::tests::{Step, fake_node};

fn row_gate(row: Row, threshold: f64) -> Gate {
    Gate {
        id: format!("{row:?}"),
        name: format!("{row:?}"),
        threshold,
        comparator: "<=".into(),
        unit: "unit".into(),
        tier: "linux-reference-box".into(),
        source: row.key().into(),
        informational: true,
    }
}

fn sample(rss_bytes: u64, pinned_bytes: u64, cpu_ticks: u64, start_ticks: u64) -> ProcSample {
    ProcSample { rss_bytes, pinned_bytes, cpu_ticks, start_ticks }
}

/// A scratch directory for one test, removed on drop.
struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let path = std::env::temp_dir()
            .join(format!("inf-bench-empty-node-{}-{name}", std::process::id()));
        if path.exists() {
            std::fs::remove_dir_all(&path).expect("stale scratch");
        }
        std::fs::create_dir_all(&path).expect("scratch");
        Scratch(path)
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        if self.0.exists() {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }
}

/// A one-cell node directory holding every file the bill names: the four
/// root files, the shard's `MANIFEST`, a sparse segment with one written
/// block, and a checkpoint.
fn node_fixture(root: &Path) {
    let write = |relative: &str, bytes: &[u8]| {
        std::fs::write(root.join(relative), bytes).expect("fixture file");
    };
    std::fs::create_dir_all(root.join("shard-0/log")).expect("log dir");
    std::fs::create_dir_all(root.join("shard-0/ckpt")).expect("ckpt dir");
    write("LOCK", b"");
    write("key-hash.toml", b"function = \"siphash13\"\n");
    write("topology.toml", b"cells = 1\n");
    write("io-properties.toml", b"barrier_class = \"flush\"\n");
    write("shard-0/MANIFEST", &[7u8; 64]);
    write("shard-0/ckpt/ckpt-000001.ick", &[1u8; 8192]);
    let segment = root.join("shard-0/log/seg-000000.ilog");
    std::fs::write(&segment, [9u8; 4096]).expect("segment frame");
    let file = std::fs::OpenOptions::new().write(true).open(&segment).expect("segment");
    file.set_len(64 << 20).expect("sparse segment");
}

fn allocated(root: &Path) -> u64 {
    walk_allocated(root, 1).expect("fixture walks").allocated_bytes
}

#[test]
fn the_row_table_names_every_checked_in_row() {
    let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../docs/milestones/m1-gates.toml");
    let gates = crate::gates::load(path).expect("m1 gates file parses");
    for row in Row::ALL {
        let gate = row.gate(&gates).unwrap_or_else(|| panic!("{row:?} has a row"));
        assert!(gate.informational, "{row:?} is a measured baseline");
        assert_eq!(gate.comparator, "<=", "{row:?}");
        assert!(row.resolution() > 0.0 && row.spread_budget() > 0.0, "{row:?}");
    }
    assert!(Row::IdleRss.gate(&[]).is_none(), "no default row");
}

/// Another cell count is a declared withholding: a note, no spawn, no row.
#[test]
fn another_topology_withholds_every_row_without_a_spawn() {
    let (bools, values) = crate::gaterun::GATE_RUN_FLAGS;
    let flags = Flags::parse(&[], bools, values).expect("flags");
    let mut m = Measurements::new();
    run(&flags, &[], &mut m, "/nonexistent/infinityd", 8, true).expect("withheld, not refused");
    assert!(m.values.is_empty());
    assert!(m.notes.iter().any(|note| note.contains("withheld: --cells 8")), "{:?}", m.notes);
}

#[test]
fn the_pin_floor_proves_every_cell() {
    let floor = pin_proven_floor(4, RECEIVE_BUFFERS_DEFAULT).expect("fits");
    assert_eq!(floor, 2 * 3 * 4096 * 4096, "2 (N − 1) · b · s");
    let prove = |pinned| PinnedRss::prove(&sample(1, pinned, 0, 0), 4, RECEIVE_BUFFERS_DEFAULT);
    assert!(prove(floor - 1).is_err());
    assert_eq!(
        prove(floor).map(|rss| rss.rss_bytes),
        Err(PinUnproven { pinned_bytes: floor, floor_bytes: floor })
    );
    assert_eq!(prove(floor + 1).map(|rss| rss.rss_bytes), Ok(1));
    assert_eq!(pin_proven_floor(0, 1), None, "no cell proves nothing");
    assert!(PinnedRss::prove(&sample(1, u64::MAX, 0, 0), 0, 1).is_err());
}

#[test]
fn the_idle_cpu_is_ticks_over_the_window() {
    let window = |first, last, seconds| IdleWindow {
        first: sample(0, 0, first, 7),
        last: sample(0, 0, last, 7),
        elapsed: Duration::from_secs(seconds),
    };
    let pct = window(100, 150, 10).cpu_pct().expect("valid");
    assert!((pct - 5.0).abs() < 1e-9, "{pct}");
    assert!(window(150, 100, 10).cpu_pct().is_err(), "ticks never go back");
    assert!(window(1, 2, 0).cpu_pct().is_err(), "an empty window");
}

/// The node's one idle client carries nothing from the settle start to
/// the window's end; the second sample asks for the first's identity.
#[test]
fn the_idle_window_sends_nothing_and_keeps_the_identity() {
    let (port, node) = fake_node(vec![Step::Reply(b"+PONG\r\n"), Step::Reply(b":0\r\n")]);
    let serving = wait_pong(port, Instant::now(), Duration::from_secs(5), || None).expect("serves");
    let mut asked: Vec<Option<u64>> = Vec::new();
    let settle = Duration::from_millis(20);
    let idle = idle_window(&serving, settle, Duration::from_millis(30), |expected| {
        asked.push(expected);
        Ok(sample(1, 1, 0, 42))
    })
    .expect("window");
    assert!(idle.elapsed >= Duration::from_millis(30));
    assert_eq!(asked, vec![None, Some(42)]);
    drop(serving);
    assert_eq!(node.join().expect("fake node"), 0, "bytes sent during the idle window");
    let failed = |_: Option<u64>| Err(ProcReadError::Exited);
    let (port, _node) = fake_node(vec![Step::Reply(b"+PONG\r\n"), Step::Reply(b":0\r\n")]);
    let serving = wait_pong(port, Instant::now(), Duration::from_secs(5), || None).expect("serves");
    let result = idle_window(&serving, Duration::ZERO, Duration::ZERO, failed);
    assert_eq!(result.map(|w| w.elapsed), Err(ProcReadError::Exited));
}

#[test]
fn the_control_compares_in_resolution_units_at_zero() {
    let red = PlantVerdict::Red("planted".into());
    let check = |row, a: &[f64], a_prime: &[f64]| {
        let input = RowInput::Measured { a: a.to_vec(), a_prime: a_prime.to_vec() };
        ControlChecked::check(row, input, &red).map(|checked| checked.value)
    };
    assert_eq!(check(Row::IdleCpu, &[0.0, 0.0, 0.0], &[0.1, 0.0, 0.1]).ok(), Some(0.0));
    assert!(matches!(check(Row::IdleCpu, &[0.0; 3], &[0.2; 3]), Err(Refusal::Spread(_))));
    assert_eq!(check(Row::IdleRss, &[100.0; 3], &[101.9; 3]).ok(), Some(100.0));
    assert!(matches!(check(Row::IdleRss, &[100.0; 3], &[103.0; 3]), Err(Refusal::Spread(_))));
    assert!(matches!(check(Row::WarmBoot, &[], &[1.0]), Err(Refusal::Spread(_))));
    assert!(matches!(check(Row::DataDir, &[f64::NAN], &[128.0]), Err(Refusal::Spread(_))));
}

#[test]
fn a_green_or_unengaged_plant_keeps_its_row_unset() {
    let measured = || RowInput::Measured { a: vec![128.0], a_prime: vec![128.0] };
    for plant in [PlantVerdict::Green("g".into()), PlantVerdict::Unengaged("u".into())] {
        let result = ControlChecked::check(Row::DataDir, measured(), &plant);
        assert!(matches!(result, Err(Refusal::Vacuous(_))), "{plant}");
    }
    let red = PlantVerdict::Red("r".into());
    let withheld = RowInput::Withheld { reason: "declared".into(), precondition: false };
    let result = ControlChecked::check(Row::DataDir, withheld, &red);
    assert!(matches!(result, Err(Refusal::Withheld { precondition: false, .. })));
    let checked = ControlChecked::check(Row::DataDir, measured(), &red).expect("publishes");
    assert_eq!((checked.row, checked.value), (Row::DataDir, 128.0));
}

#[test]
fn a_plant_needs_both_its_rise_and_the_rows_comparator() {
    let gate = row_gate(Row::IdleCpu, 5.0);
    assert!(matches!(judge(Some(&gate), 92.4, 3.0, 19.2), PlantVerdict::Red(_)));
    assert!(matches!(judge(Some(&gate), 10.0, 3.0, 19.2), PlantVerdict::Green(_)), "no rise");
    assert!(matches!(judge(Some(&gate), 4.0, -20.0, 19.2), PlantVerdict::Green(_)), "in bill");
    assert!(matches!(judge(Some(&gate), f64::NAN, 3.0, 19.2), PlantVerdict::Green(_)));
    assert!(matches!(judge(None, 92.4, 3.0, 19.2), PlantVerdict::Unengaged(_)));
    assert!((cpu_plant_bill_pct() - 96.0).abs() < 1e-9, "N × 48 000 wakes/s × 5 µs");
    assert!((CPU_PLANT_RISE_FRACTION * cpu_plant_bill_pct() - 19.2).abs() < 1e-9);
}

#[test]
fn io_properties_must_be_byte_equal() {
    let scratch = Scratch::new("io-properties");
    let (d, d_prime) = (scratch.0.join("d"), scratch.0.join("d-prime"));
    std::fs::create_dir_all(&d).expect("d");
    std::fs::create_dir_all(&d_prime).expect("d′");
    assert!(SameIoProperties::compare(&d, &d_prime).is_err(), "missing files prove nothing");
    std::fs::write(d.join(IO_PROPERTIES_FILE), b"barrier_class = \"fua\"\n").expect("d file");
    std::fs::write(d_prime.join(IO_PROPERTIES_FILE), b"barrier_class = \"flush\"\n").expect("d′");
    assert!(SameIoProperties::compare(&d, &d_prime).is_err(), "a reprobe differs");
    std::fs::copy(d.join(IO_PROPERTIES_FILE), d_prime.join(IO_PROPERTIES_FILE)).expect("seed");
    assert!(SameIoProperties::compare(&d, &d_prime).is_ok());
}

fn memory_leg_fixture(rss: Result<PinnedRss, PinUnproven>, cpu_pct: f64) -> MemoryLeg {
    MemoryLeg {
        rss,
        rss_bytes: 0,
        pinned_bytes: 0,
        cpu_pct,
        boot: Duration::ZERO,
        loading_replies: 0,
        memlock: String::new(),
        attribution: String::new(),
    }
}

fn warm_leg_fixture(boot_ms: f64, allocated_bytes: u64, block_bytes_max: u64) -> WarmLeg {
    let walk = DirWalk { allocated_bytes, block_bytes_max, entries: Vec::new() };
    WarmLeg { boot_ms, walk }
}

fn outcome_fixture(rss: Result<PinnedRss, PinUnproven>, block_bytes_max: u64) -> Outcome {
    let set = || LegSet {
        memory: vec![memory_leg_fixture(rss, 2.0)],
        warm: vec![warm_leg_fixture(15.0, 131_072, block_bytes_max)],
    };
    let red = || PlantVerdict::Red("planted".into());
    Outcome {
        a: set(),
        a_prime: set(),
        durable: Durable::Ran { io: Ok(SameIoProperties(())) },
        plants: Plants { rss: red(), cpu: red(), boot: red(), dir: red() },
    }
}

#[test]
fn publication_sets_exactly_the_checked_rows() {
    let pinned = Ok(PinnedRss { rss_bytes: 80 << 20 });
    let mut m = Measurements::new();
    publish_rows(&mut m, Tier::Binding, &outcome_fixture(pinned, 4096));
    assert_eq!(m.values.get(Row::IdleRss.key()), Some(&80.0));
    assert_eq!(m.values.get(Row::DataDir.key()), Some(&128.0));
    assert_eq!(m.values.get(Row::WarmBoot.key()), Some(&15.0));
    assert_eq!(m.values.get(Row::IdleCpu.key()), Some(&2.0));

    let mut outcome = outcome_fixture(pinned, 4096);
    outcome.plants.dir = PlantVerdict::Green("walk blind".into());
    let mut m = Measurements::new();
    publish_rows(&mut m, Tier::Dev, &outcome);
    assert_eq!(m.values.get(Row::DataDir.key()), None, "a green plant keeps the row unset");
    assert_eq!(m.values.len(), 3);
}

#[test]
fn unproven_preconditions_withhold_their_rows() {
    let unproven = Err(PinUnproven { pinned_bytes: 1, floor_bytes: 2 });
    let mut m = Measurements::new();
    publish_rows(&mut m, Tier::Dev, &outcome_fixture(unproven, 65_536));
    assert_eq!(m.values.get(Row::IdleRss.key()), None, "an unproven pin withholds RSS");
    assert_eq!(m.values.get(Row::DataDir.key()), None, "a 64 KiB block withholds the dir row");
    assert_eq!(m.values.get(Row::WarmBoot.key()), Some(&15.0));
    assert_eq!(m.notes.iter().filter(|note| note.contains("withheld")).count(), 2);

    let mut binding = Measurements::new();
    publish_rows(&mut binding, Tier::Binding, &outcome_fixture(unproven, 4096));
    assert_eq!(binding.values.get(Row::IdleRss.key()), None);
    assert!(!binding.notes.iter().any(|note| note.contains("withheld")), "binding: a failure");

    let mut outcome = outcome_fixture(Ok(PinnedRss { rss_bytes: 1 }), 4096);
    outcome.durable = Durable::Ran { io: Err(IoPropertiesDiffer("reprobed".into())) };
    let input = row_input(Row::WarmBoot, &outcome.a, &outcome.a_prime, &outcome.durable);
    assert!(matches!(input, RowInput::Withheld { precondition: true, .. }));
}

// ---- the walk -----------------------------------------------------------

#[test]
fn the_walk_counts_allocated_blocks_once_and_never_follows_links() {
    let scratch = Scratch::new("walk-links");
    let root = scratch.0.join("node");
    node_fixture(&root);
    let base = allocated(&root);
    assert!(base < 64 << 20, "the sparse segment counts its written block only: {base}");
    std::fs::hard_link(root.join("topology.toml"), root.join("topology.link")).expect("link");
    assert!(allocated(&root) - base < DIR_STEP_BYTES, "a hard link is its inode once");
    let big = scratch.0.join("big");
    std::fs::write(&big, vec![5u8; 1 << 20]).expect("big file");
    std::os::unix::fs::symlink("../big", root.join("big.link")).expect("symlink");
    assert!(allocated(&root) - base < DIR_STEP_BYTES, "a symlink is never followed");
    let sparse = std::fs::File::create(root.join("sparse")).expect("sparse");
    sparse.set_len(1 << 30).expect("set_len");
    assert!(allocated(&root) - base < DIR_STEP_BYTES, "a sparse file allocates nothing");
}

#[test]
fn the_walk_refuses_a_tree_over_its_cap() {
    let scratch = Scratch::new("walk-cap");
    let root = scratch.0.join("node");
    node_fixture(&root);
    let fixture_entries = walk_allocated(&root, 1).expect("walks").entries.len();
    let extra = root.join("extra");
    std::fs::create_dir(&extra).expect("extra dir");
    for index in 0..EMPTY_DIR_WALK_ENTRIES_MAX - fixture_entries - 1 {
        std::fs::write(extra.join(index.to_string()), b"").expect("entry");
    }
    let at_cap = walk_allocated(&root, 1).expect("the cap itself walks");
    assert_eq!(at_cap.entries.len(), EMPTY_DIR_WALK_ENTRIES_MAX);
    std::fs::write(extra.join("one-more"), b"").expect("entry");
    assert_eq!(walk_allocated(&root, 1).map(|w| w.entries.len()), Err(WalkError::OverCap));
}

#[test]
fn the_walk_needs_every_file_the_bill_names() {
    let scratch = Scratch::new("walk-skeleton");
    let root = scratch.0.join("node");
    let cases = [
        ("key-hash.toml", "key-hash.toml"),
        ("shard-0/MANIFEST", "shard-0/MANIFEST"),
        ("shard-0/log/seg-000000.ilog", "shard-0/log/seg-*.ilog"),
        ("shard-0/ckpt/ckpt-000001.ick", "shard-0/ckpt/ckpt-*.ick"),
    ];
    for (removed, missing) in cases {
        node_fixture(&root);
        std::fs::remove_file(root.join(removed)).expect("remove");
        let result = walk_allocated(&root, 1).map(|w| w.allocated_bytes);
        assert_eq!(result, Err(WalkError::NotANodeDir(missing.into())), "{removed}");
    }
    node_fixture(&root);
    let result = walk_allocated(&root, 2).map(|w| w.allocated_bytes);
    assert_eq!(result, Err(WalkError::NotANodeDir("shard-1/MANIFEST".into())), "every cell");
}

#[test]
fn an_unreadable_directory_is_a_walk_error() {
    let owner = std::fs::metadata("/proc/self").expect("/proc/self").uid();
    if owner == 0 {
        eprintln!("running as root: a mode-000 directory stays readable, the case cannot plant");
        return;
    }
    let scratch = Scratch::new("walk-unreadable");
    let root = scratch.0.join("node");
    node_fixture(&root);
    let locked = root.join("shard-0/ckpt");
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o000)).expect("chmod");
    let result = walk_allocated(&root, 1).map(|w| w.allocated_bytes);
    std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    assert!(
        matches!(&result, Err(WalkError::Io { kind: std::io::ErrorKind::PermissionDenied, .. })),
        "{result:?}"
    );
}

// ---- the dir plant --------------------------------------------------------

#[test]
fn the_filler_always_writes_and_never_underflows() {
    let bill = 128 * 1024;
    assert_eq!(filler_bytes(bill, 0), Some(132 * 1024), "W0 = 0: the bill plus one step");
    let meet = bill + DIR_STEP_BYTES - DIR_PLANT_FILLER_MIN;
    assert_eq!(filler_bytes(bill, meet), Some(DIR_PLANT_FILLER_MIN), "the floor meets it");
    assert_eq!(filler_bytes(bill, meet + 1), Some(DIR_PLANT_FILLER_MIN));
    assert_eq!(filler_bytes(bill, meet - 1), Some(DIR_PLANT_FILLER_MIN + DIR_STEP_BYTES));
    let residue = bill + DIR_STEP_BYTES + 147_456;
    assert_eq!(filler_bytes(bill, residue), Some(DIR_PLANT_FILLER_MIN), "no underflow");
    assert_eq!(filler_bytes(bill, u64::MAX), Some(DIR_PLANT_FILLER_MIN));
    assert_eq!(filler_bytes(bill, 1000), Some(132 * 1024), "rounded up to a whole step");
    assert_eq!(filler_bytes(u64::MAX, 0), None, "rounding past u64::MAX");
}

/// A stand-in walk that counts only entries at depth ≤ `depth_max`.
fn walk_to_depth(root: &Path, depth_max: usize) -> Result<u64, WalkError> {
    let mut total = 0u64;
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("fixture dir") {
            let path = entry.expect("entry").path();
            let meta = std::fs::symlink_metadata(&path).expect("lstat");
            if depth < depth_max {
                total += meta.blocks() * 512;
                if meta.is_dir() {
                    stack.push((path, depth + 1));
                }
            }
        }
    }
    Ok(total)
}

fn apparent_size(root: &Path) -> Result<u64, WalkError> {
    let mut total = 0u64;
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("fixture dir") {
            let path = entry.expect("entry").path();
            let meta = std::fs::symlink_metadata(&path).expect("lstat");
            total += meta.len();
            if meta.is_dir() {
                stack.push(path);
            }
        }
    }
    Ok(total)
}

#[test]
fn the_dir_plant_is_red_on_a_node_directory_and_cleans_up() {
    let scratch = Scratch::new("plant-red");
    let root = scratch.0.join("node");
    node_fixture(&root);
    let gate = row_gate(Row::DataDir, 128.0);
    let verdict = dir_plant(&root, Some(&gate), |path| {
        walk_allocated(path, 1).map(|walk| walk.allocated_bytes)
    });
    assert!(matches!(verdict, PlantVerdict::Red(_)), "{verdict}");
    assert!(!root.join("shard-0/log/plant-filler").exists(), "the filler is removed");
    assert!(!root.join("shard-0/log/plant-sparse").exists(), "the sparse file is removed");
}

#[test]
fn a_blind_walk_reads_the_dir_plant_vacuous() {
    let scratch = Scratch::new("plant-blind");
    let root = scratch.0.join("node");
    node_fixture(&root);
    let gate = row_gate(Row::DataDir, 128.0);
    let depth_two = dir_plant(&root, Some(&gate), |path| walk_to_depth(path, 2));
    assert!(matches!(depth_two, PlantVerdict::Green(_)), "depth 2: {depth_two}");
    let constant = dir_plant(&root, Some(&gate), |_| Ok(4096));
    assert!(matches!(constant, PlantVerdict::Green(_)), "constant: {constant}");
    let apparent = dir_plant(&root, Some(&gate), apparent_size);
    assert!(matches!(apparent, PlantVerdict::Green(_)), "apparent size: {apparent}");
    let no_row = dir_plant(&root, None, |path| walk_to_depth(path, 8));
    assert!(matches!(no_row, PlantVerdict::Unengaged(_)), "no row: {no_row}");
    let failing = dir_plant(&root, Some(&gate), |_| Err(WalkError::OverCap));
    assert!(matches!(failing, PlantVerdict::Unengaged(_)), "walk error: {failing}");
}
