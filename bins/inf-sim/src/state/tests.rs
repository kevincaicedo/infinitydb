use super::*;
use crate::durable::{DurableScenario, TraceObserver, boot, build_disk};
use inf_foundation::CellId;
use inf_foundation::rng::SplitMix64;
use inf_foundation::time::{Clock, VirtualClock};
use inf_log::fs::{SegmentFile, SegmentFs};
use inf_server::{ExecOrigin, ExecScope, PlaneObserver};
use inf_store::{SetOptions, StoreConfig};
use std::path::Path;

#[test]
fn apply_time_changes_state_without_changing_the_frozen_trace() {
    let mut first = TraceObserver::default();
    let mut second = TraceObserver::default();
    for (observer, time) in [(&mut first, 1), (&mut second, 2)] {
        observer.on_execute(
            CellId(0),
            ExecOrigin::Conn(0, 0),
            ExecScope::Db(0),
            &[b"SET", b"k", b"v"],
            b"+OK\r\n",
            Nanos(time),
        );
    }
    assert_eq!(first.trace_bytes(), second.trace_bytes());
    assert_ne!(first.state_hash(), second.state_hash());
    assert!(verify(7, first.state_hash(), 7, second.state_hash()).is_err());
}

#[test]
fn keyspace_state_covers_values_not_only_entry_counts() {
    let mut first = Keyspace::new(StoreConfig::default());
    let mut second = Keyspace::new(StoreConfig::default());
    for (store, value) in [(&mut first, b"a"), (&mut second, b"b")] {
        store.db_mut(0).set(b"key", value, SetOptions::default(), Nanos(1)).unwrap();
    }
    let mut a = StateHash::default();
    let mut b = StateHash::default();
    a.keyspace(&first, Nanos(1));
    b.keyspace(&second, Nanos(1));
    assert_eq!(first.state_digest(Nanos(1)).entries, second.state_digest(Nanos(1)).entries);
    assert_ne!(a.value(), b.value());
}

fn write(disk: &SimDisk, name: &str, bytes: &[u8]) {
    disk.create_dir_all(Path::new(name).parent().unwrap()).unwrap();
    let mut file = match disk.create_meta(Path::new(name)) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
            disk.open_write(Path::new(name)).unwrap()
        }
        Err(error) => panic!("fixture create: {error}"),
    };
    file.write_at(0, bytes).unwrap();
}

fn disk_hash(disk: &SimDisk) -> u64 {
    let mut hash = StateHash::default();
    hash.disk(disk);
    hash.value()
}

#[test]
fn disk_state_covers_paths_bytes_lengths_and_checkpoint_manifest_membership() {
    let disk = SimDisk::new();
    write(&disk, "node/segment", b"abc");
    write(&disk, "node/MANIFEST", b"one");
    let original = disk_hash(&disk);
    let reordered = SimDisk::new();
    write(&reordered, "node/MANIFEST", b"one");
    write(&reordered, "node/segment", b"abc");
    assert_eq!(original, disk_hash(&reordered), "creation order is not file order");
    for (name, bytes) in [("node/segment", b"abd".as_slice()), ("node/MANIFEST", b"two")] {
        write(&reordered, name, bytes);
        assert_ne!(original, disk_hash(&reordered));
        write(&reordered, "node/segment", b"abc");
        write(&reordered, "node/MANIFEST", b"one");
        assert_eq!(original, disk_hash(&reordered));
    }
    disk.rename(Path::new("node/segment"), Path::new("node/segment-2")).unwrap();
    assert_ne!(original, disk_hash(&disk));
    let renamed = disk_hash(&disk);
    write(&disk, "node/checkpoint.ick", b"");
    assert_ne!(renamed, disk_hash(&disk));
    let checkpoint = disk_hash(&disk);
    write(&disk, "node/segment-2", b"abcd");
    assert_ne!(checkpoint, disk_hash(&disk));
}

fn node_state(change_time: bool, change_disk: bool, extra_boot: bool) -> (Vec<u8>, u64) {
    let clock = Rc::new(VirtualClock::new(Nanos(1)));
    let disk = build_disk(19, Some(&inf_log::fs::sim::StallConfig::write_reorder()));
    let observer = TraceObserver::default();
    let mut scenario = DurableScenario::m2_durable(19);
    scenario.cells = 1;
    let mut rng = SplitMix64::new(19);
    let mut node = boot(&scenario, "node".into(), &disk, &clock, &observer).unwrap();
    for _ in 0..1000 {
        if node.ready() {
            break;
        }
        node.step(&mut rng, &clock, &disk, scenario.step_ns_max).unwrap();
    }
    assert!(node.ready());
    if change_time {
        clock.advance(Nanos(17));
    }
    if change_disk {
        write(&disk, "node/extra-segment", b"residue");
    }
    drop(node);
    if extra_boot {
        drop(boot(&scenario, "node".into(), &disk, &clock, &observer).unwrap());
    }
    assert!(clock.now().0 > 1);
    (observer.trace_bytes(), observer.state_hash())
}

#[test]
fn real_node_lifecycle_records_final_clock_disk_and_boots_without_apply_events() {
    let baseline = node_state(false, false, false);
    assert!(baseline.0.is_empty());
    assert_eq!(baseline, node_state(false, false, false));
    for changed in [
        node_state(true, false, false),
        node_state(false, true, false),
        node_state(false, false, true),
    ] {
        assert_eq!(baseline.0, changed.0);
        assert_ne!(baseline.1, changed.1);
    }
}

#[test]
fn final_cut_is_recorded_without_a_following_boot() {
    let at = |now| {
        let observer = TraceObserver::default();
        let disk = SimDisk::new();
        write(&disk, "node/segment", b"unsynced");
        let before = observer.state_hash();
        observer.power_cut(&disk, Nanos(now), 17);
        assert_ne!(before, observer.state_hash());
        assert!(observer.trace_bytes().is_empty());
        observer.state_hash()
    };
    assert_eq!(at(1), at(1));
    assert_ne!(at(1), at(2));
}

#[test]
fn comparator_rejects_either_hash_and_accepts_only_the_matching_pair() {
    assert!(verify(1, 2, 1, 2).is_ok());
    assert!(verify(1, 2, 3, 2).unwrap_err().contains("trace_hash"));
    assert!(verify(1, 2, 1, 3).unwrap_err().contains("state_hash"));
}
