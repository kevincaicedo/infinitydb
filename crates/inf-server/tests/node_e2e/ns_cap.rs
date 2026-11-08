//! The tombstone cap at the node (ADR-0189 D6): a durable `DROP` or a
//! durable `CREATE` meets the catalog writer's verdict on the node's live
//! tombstones, never a count the program read before it asked.

use super::*;

/// The first tombstoned id of a seeded catalog.
const FIRST_DROPPED: u32 = 16;

/// A durable namespace's catalog entry.
fn durable_entry(id: u32, name: &[u8]) -> inf_store::NsSpec {
    inf_store::NsSpec {
        id: inf_store::NsId(id),
        name: name.to_vec(),
        mode: inf_store::NsMode::Durable,
        fsync: Some(inf_store::FsyncClass::Everysec),
        policy: None,
        maxmemory: None,
        tier: None,
    }
}

/// Writes a `META` holding `count` tombstones (ids 16 up) and the durable
/// namespaces `names` after them, its counter past every id; returns the
/// entries.
fn seed_tombstoned_meta(
    dir: &std::path::Path,
    count: u32,
    names: &[&[u8]],
) -> Vec<inf_store::NsSpec> {
    let first_entry = FIRST_DROPPED + count;
    let entries: Vec<inf_store::NsSpec> =
        (first_entry..).zip(names).map(|(id, name)| durable_entry(id, name)).collect();
    let next_id = first_entry + u32::try_from(entries.len()).expect("a few entries");
    let seed = inf_store::NsCatalog {
        next_id,
        entries: entries.clone(),
        index: inf_store::IndexCatalog::default(),
        dropped: (FIRST_DROPPED..first_entry).collect(),
    };
    inf_log::meta::write_meta(&inf_log::fs::StdSegmentFs, dir, &seed.encode()).expect("seed META");
    entries
}

/// Waits until every cell has published a checkpoint at or past `epoch`.
fn wait_all_published(node: &Node, control: &inf_server::ControlHandle, epoch: u64) {
    let board = control.ckpt_board();
    let deadline = Instant::now() + Duration::from_secs(30);
    while (0..node.cells).any(|cell| board.slot(cell).published() < epoch) {
        assert!(Instant::now() < deadline, "the cells did not publish epoch {epoch} in 30 s");
        std::thread::sleep(Duration::from_millis(1));
    }
}

/// The stale fold (ADR-0189 D6, N14): the cells publish past every
/// tombstone's stamp before the next DDL, so every one of the node's 256
/// tombstones is ready, and a durable `DROP` at the cap is accepted with no
/// pacing checkpoint: only its cleanup checkpoint is issued. A program that
/// paces on the tombstone count before it asks issues one more epoch.
#[test]
fn a_drop_at_the_cap_with_every_tombstone_ready_issues_no_pacing_checkpoint() {
    let dir = temp_data_dir("cap-stale-fold");
    let cap = u32::try_from(inf_server::DROPPED_NS_MAX).expect("the cap fits u32");
    let entries = seed_tombstoned_meta(&dir, cap, &[b"victim"]);
    let node = Node::start_durable(2, &dir);
    let control = Arc::clone(node.control.as_ref().expect("a durable node"));
    // The boot re-stamped every tombstone with one all-cell checkpoint.
    let stamp = control.ckpt_last_issued();
    wait_all_published(&node, &control, stamp);
    let held = inf_server::load_catalog(&dir).expect("META").expect("META").dropped.len();
    assert_eq!(held, inf_server::DROPPED_NS_MAX, "VACUOUS: the node is not at the cap");
    let issued = control.ckpt_last_issued();
    let mut c = conn_on_cell(&node, 0);
    c.write_all(&cmd(&[b"INF.NS", b"DROP", b"victim"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let by_drop = control.ckpt_last_issued() - issued;
    let after = inf_server::load_catalog(&dir).expect("META").expect("META");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        by_drop, 1,
        "the DROP at the cap issued {by_drop} checkpoint epochs with every tombstone ready: a \
         pacing checkpoint beside its cleanup"
    );
    assert_eq!(after.dropped, vec![entries[0].id.0], "every ready tombstone retired");
}

/// Every tombstone blocked (ADR-0189 D6): 256 durable `DROP`s, each stopped
/// after the origin's apply (`ns_drop_after_apply`, the node running on),
/// leave 256 unstamped tombstones no pacing checkpoint can ready. A durable
/// `DROP` and a durable `CREATE` are then refused `-ERR catalog limit
/// reached` before any effect (`META`'s bytes and the checkpoint clock
/// unchanged), and a memory `CREATE` proceeds. A restart re-stamps them:
/// once every cell publishes past the stamp, a durable `CREATE` is accepted
/// and every tombstone retires.
#[test]
fn every_tombstone_blocked_refuses_a_durable_ddl_until_a_restart_re_stamps_them() {
    let dir = temp_data_dir("cap-all-blocked");
    let cut = (inf_server::fault::NS_DROP_AFTER_APPLY, inf_foundation::fault::FaultSpec::Always);
    let node = Node::start_durable_with_faults(2, &dir, vec![cut]);
    let control = Arc::clone(node.control.as_ref().expect("a durable node"));
    let mut c = conn_on_cell(&node, 0);
    let create = |c: &mut TcpStream, name: &[u8], mode: &[u8]| {
        c.write_all(&cmd(&[b"INF.NS", b"CREATE", name, b"MODE", mode])).expect("write");
        read_line(c)
    };
    assert_eq!(create(&mut c, b"extra", b"durable"), b"+OK\r\n");
    for index in 0..inf_server::DROPPED_NS_MAX {
        let name = format!("held{index}");
        assert_eq!(create(&mut c, name.as_bytes(), b"durable"), b"+OK\r\n");
        c.write_all(&cmd(&[b"INF.NS", b"DROP", name.as_bytes()])).expect("write");
        read_exactly(&mut c, b"-ERR fault: ns_drop_after_apply\r\n");
    }
    let gauges = control.tombstone_gauges();
    assert_eq!(usize::from(gauges.live), inf_server::DROPPED_NS_MAX, "VACUOUS: {gauges:?}");
    let meta_before = std::fs::read(dir.join(inf_log::meta::META_FILE)).expect("META");
    let issued = control.ckpt_last_issued();
    let limit = b"-ERR catalog limit reached\r\n";
    c.write_all(&cmd(&[b"INF.NS", b"DROP", b"extra"])).expect("write");
    read_exactly(&mut c, limit);
    assert_eq!(create(&mut c, b"more", b"durable"), limit);
    let meta_after = std::fs::read(dir.join(inf_log::meta::META_FILE)).expect("META");
    assert_eq!(meta_after, meta_before, "a refusal at the cap wrote META");
    assert_eq!(control.ckpt_last_issued(), issued, "a refusal at the cap issued a checkpoint");
    assert_eq!(control.drop_refused_capacity(), (0, 2), "both refusals counted as limits");
    assert_eq!(control.ddl_paced(), 0, "no blocked tombstone is paced");
    assert_eq!(create(&mut c, b"scratch", b"memory"), b"+OK\r\n", "a memory CREATE proceeds");
    drop(c);
    node.stop();
    let node = Node::start_durable(2, &dir);
    let mut c = conn_on_cell(&node, 0);
    c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    assert_eq!(create(&mut c, b"more", b"durable"), b"+OK\r\n", "the re-stamped book drained");
    let after = inf_server::load_catalog(&dir).expect("META").expect("META");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
    assert!(after.dropped.is_empty(), "every re-stamped tombstone retired: {:?}", after.dropped);
}

/// Paceable tombstones (ADR-0189 D6, "one pace suffices"): a boot re-stamps
/// the 256 tombstones its `META` carries with one checkpoint, and every
/// cell's clock is held while that checkpoint walks a namespace's data, so
/// none is ready. A durable `DROP` is answered `Paceable` by the writer;
/// the program issues its one pacing checkpoint, the clocks open, every
/// cell publishes both, and the `DROP` asks again and is accepted: two
/// checkpoint epochs (the pace and the cleanup), one pace counted, and
/// every one of the 256 retires.
#[test]
fn a_durable_drop_at_the_cap_paces_once_then_is_accepted() {
    let dir = temp_data_dir("cap-paceable");
    seed_drop_wait_namespace(DropWaitNs::Durable, &dir);
    let mut seed = inf_server::load_catalog(&dir).expect("META").expect("META");
    let cap = u32::try_from(inf_server::DROPPED_NS_MAX).expect("the cap fits u32");
    let first = seed.next_id;
    seed.dropped = (first..first + cap).collect();
    seed.entries.push(durable_entry(first + cap, b"victim"));
    seed.next_id = first + cap + 1;
    inf_log::meta::write_meta(&inf_log::fs::StdSegmentFs, &dir, &seed.encode()).expect("META");
    let trigger = CkptTrigger::Paced { slice_bytes: 4 << 10, stream_bytes_per_sec: 256 << 10 };
    let node = Node::start_with(2, Some(dir.clone()), trigger);
    node.clock_gates.iter().for_each(|gate| gate.hold());
    let control = Arc::clone(node.control.as_ref().expect("a durable node"));
    let stamp = control.ckpt_last_issued();
    let board = control.ckpt_board();
    let unpublished = (0..node.cells).any(|cell| board.slot(cell).published() < stamp);
    assert!(unpublished, "VACUOUS: every cell published the boot's stamp before the clocks held");
    let mut c = conn_on_cell(&node, 0);
    c.write_all(&cmd(&[b"INF.NS", b"DROP", b"victim"])).expect("write");
    let deadline = Instant::now() + Duration::from_secs(10);
    while control.ddl_paced() == 0 {
        assert!(Instant::now() < deadline, "the DROP at the cap never paced");
        std::thread::sleep(Duration::from_millis(1));
    }
    node.clock_gates.iter().for_each(|gate| gate.open());
    read_exactly(&mut c, b"+OK\r\n");
    let issued = control.ckpt_last_issued() - stamp;
    let (paced, repaced) = (control.ddl_paced(), control.ddl_repaced());
    let after = inf_server::load_catalog(&dir).expect("META").expect("META");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!((paced, repaced), (1, 0), "one pace, and the second answer accepted");
    assert_eq!(issued, 2, "the pace and the cleanup, no other checkpoint");
    assert_eq!(after.dropped, vec![first + cap], "the 256 retired; the victim's tombstone stays");
}

/// A boot over the cap (ADR-0189 D7): a `META` with 4,096 tombstones, the
/// decoder's bound, boots and serves on two cells; a memory `CREATE`
/// proceeds; once every cell publishes past the boot's stamp, a durable
/// `CREATE` is accepted, every tombstone retires, and (I) never fires.
#[test]
fn a_boot_with_4096_tombstones_serves_and_drains() {
    let dir = temp_data_dir("cap-over-boot");
    let entries = seed_tombstoned_meta(&dir, 4096, &[]);
    assert!(entries.is_empty());
    let node = Node::start_durable(2, &dir);
    let control = Arc::clone(node.control.as_ref().expect("a durable node"));
    assert_eq!(control.drop_tombstones(), 4096, "the book holds every tombstone");
    let mut c = conn_on_cell(&node, 0);
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"scratch", b"MODE", b"memory"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.CKPT", b"WAIT"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"fresh", b"MODE", b"durable"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let after = inf_server::load_catalog(&dir).expect("META").expect("META");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
    assert!(after.dropped.is_empty(), "{} tombstones left", after.dropped.len());
}
