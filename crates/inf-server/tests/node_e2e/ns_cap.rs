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
#[ignore = "red until the catalog writer, not a count the program reads, decides the tombstone \
            cap (ADR-0189 D6)"]
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
