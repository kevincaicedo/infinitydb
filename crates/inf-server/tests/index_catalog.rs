//! M4.5-S03 (ADR-0075): the index declaration lifecycle across the
//! control plane — the 8-cell propagation race (no planning before the
//! last owning cell reports ready), drop-during-query typed binding
//! failure, and the restart path through the real `META` swap (catalog
//! persist → load → seed, the ADR-0075 D4 semantics over actual files).

#![cfg(feature = "doc")]

use inf_server::{ControlHandle, StdSegmentFs, load_catalog};
use inf_store::{
    FsyncClass, IndexBindError, IndexId, IndexSpec, IndexState, Keyspace, NsId, NsMode, NsSpec,
    StoreConfig,
};

const CELLS: u16 = 8;

fn ledger_spec() -> NsSpec {
    NsSpec {
        id: NsId(16),
        name: b"ledger".to_vec(),
        mode: NsMode::Durable,
        fsync: Some(FsyncClass::Everysec),
        policy: None,
        maxmemory: None,
        tier: None,
    }
}

fn idx_spec(id: u32, generation: u64, name: &[u8]) -> IndexSpec {
    IndexSpec {
        id: IndexId(id),
        generation,
        ns: NsId(16),
        name: name.to_vec(),
        program: inf_doc::path::compile(b"$.price").expect("valid path").as_bytes().to_vec(),
        key_type: inf_store::IndexKeyType::F64,
        state: IndexState::Declared,
    }
}

/// Eight cells' registries plus the control handle — the DDL fan's
/// destination set, driven directly (S10 wires the wire-level verbs).
fn fleet() -> (Vec<Keyspace>, std::sync::Arc<ControlHandle>, inf_server::ControlInbox) {
    let (control, inbox) = ControlHandle::detached(CELLS, 0);
    let mut cells = Vec::new();
    for _ in 0..CELLS {
        let mut ks = Keyspace::new(StoreConfig::default());
        ks.ns_create(ledger_spec()).expect("ns create");
        cells.push(ks);
    }
    (cells, control, inbox)
}

/// The S03 propagation race AC: create on an 8-cell node — the catalog
/// must not flip to `ready` (and planning must refuse) until the last
/// owning cell reports ready at the exact generation.
#[test]
fn no_planning_until_the_last_cell_reports_ready() {
    let (mut cells, control, _inbox) = fleet();
    let id = IndexId(control.alloc_index_id());
    let generation = control.alloc_index_generation();
    let slot = 0usize;
    // Fan: declare on every cell, then start backfill everywhere.
    for ks in &mut cells {
        ks.idx_create(idx_spec(id.0, generation, b"by-price")).expect("fan apply");
        ks.idx_registry_mut().set_catalog_state(id, IndexState::Backfilling).expect("edge");
        ks.idx_registry_mut().set_cell_state(id, IndexState::Backfilling).expect("edge");
    }
    let board = control.index_board();
    // Cells complete their walks one at a time; before the last report
    // the fleet check must hold the catalog in `backfilling` and every
    // compile check must refuse typed.
    for cell in 0..CELLS {
        assert!(!board.fleet_ready(slot, generation), "cell {cell} has not reported yet");
        for ks in &cells {
            assert_eq!(
                ks.idx_registry().validate_binding(NsId(16), id, generation),
                Err(IndexBindError::NotReady(IndexState::Backfilling)),
                "planning refused while any cell is behind"
            );
        }
        cells[usize::from(cell)]
            .idx_registry_mut()
            .set_cell_state(id, IndexState::Ready)
            .expect("edge");
        board.publish_ready(cell, slot, generation);
    }
    assert!(board.fleet_ready(slot, generation), "all cells reported");
    // A stale-generation report is never ready (the rebuild ABA guard).
    assert!(!board.fleet_ready(slot, generation + 1));
    // The catalog flip fans; planning opens on every cell.
    for ks in &mut cells {
        ks.idx_registry_mut().set_catalog_state(id, IndexState::Ready).expect("edge");
        ks.idx_registry().validate_binding(NsId(16), id, generation).expect("plans");
    }
}

/// Drop-during-query: a cursor bound to `{id, generation}` fails typed
/// the moment the catalog mirrors through `dropping`, and stays failed
/// (UnknownIndex) after teardown — never a wrong page.
#[test]
fn drop_during_query_fails_the_binding_typed() {
    let (mut cells, control, _inbox) = fleet();
    let id = IndexId(control.alloc_index_id());
    let generation = control.alloc_index_generation();
    for ks in &mut cells {
        ks.idx_create(idx_spec(id.0, generation, b"by-price")).expect("fan apply");
        let reg = ks.idx_registry_mut();
        reg.set_catalog_state(id, IndexState::Backfilling).expect("edge");
        reg.set_catalog_state(id, IndexState::Ready).expect("edge");
    }
    // A standing cursor binds {ns, id, generation}; the drop mirrors
    // through `dropping` first (§3.1), then cells tear down.
    cells[0].idx_registry().validate_binding(NsId(16), id, generation).expect("bound");
    for ks in &mut cells {
        ks.idx_registry_mut().set_catalog_state(id, IndexState::Dropping).expect("edge");
    }
    assert_eq!(
        cells[0].idx_registry().validate_binding(NsId(16), id, generation),
        Err(IndexBindError::NotReady(IndexState::Dropping))
    );
    for (cell, ks) in cells.iter_mut().enumerate() {
        control.index_board().clear(cell as u16, 0);
        ks.idx_drop_finish(id).expect("teardown");
    }
    assert_eq!(
        cells[0].idx_registry().validate_binding(NsId(16), id, generation),
        Err(IndexBindError::UnknownIndex)
    );
    // Re-creating under the same name takes a fresh id + generation —
    // the old binding can never resolve against the successor.
    let id2 = IndexId(control.alloc_index_id());
    let generation2 = control.alloc_index_generation();
    assert!(id2 != id, "ids are never reused");
    cells[0].idx_create(idx_spec(id2.0, generation2, b"by-price")).expect("recreate");
    assert_eq!(
        cells[0].idx_registry().validate_binding(NsId(16), id, generation),
        Err(IndexBindError::UnknownIndex)
    );
}

/// The restart AC over the real control-plane store: persist through
/// the `META` swap (the inbox drain — the exact control-thread code
/// path), load, seed — declarations survive, states map per ADR-0075
/// D4, allocator counters never regress, and driving the rebuild to
/// completion restores `ready`.
#[test]
fn declarations_survive_restart_through_the_meta_swap() {
    let dir =
        std::env::temp_dir().join(format!("inf-m45-s03-meta-{}-{}", std::process::id(), line!()));
    std::fs::create_dir_all(&dir).expect("mkdir");
    let (mut cells, control, mut inbox) = fleet();
    let ready_id = IndexId(control.alloc_index_id());
    let ready_generation = control.alloc_index_generation();
    let dropping_id = IndexId(control.alloc_index_id());
    let dropping_generation = control.alloc_index_generation();
    let origin = &mut cells[0];
    origin.idx_create(idx_spec(ready_id.0, ready_generation, b"was-ready")).expect("create");
    origin.idx_create(idx_spec(dropping_id.0, dropping_generation, b"mid-drop")).expect("create");
    let reg = origin.idx_registry_mut();
    reg.set_catalog_state(ready_id, IndexState::Backfilling).expect("edge");
    reg.set_catalog_state(ready_id, IndexState::Ready).expect("edge");
    reg.set_catalog_state(dropping_id, IndexState::Dropping).expect("edge");
    // Persist exactly like the DDL program: export + counters, swap
    // through the (detached) control plane, epoch published.
    let epoch = control.request_persist(origin.export_catalog(
        control.next_ns_id(),
        control.next_index_id(),
        control.next_index_generation(),
    ));
    inbox.drain(&StdSegmentFs, &dir).expect("META swap");
    assert!(control.persisted(epoch), "epoch published after the swap");

    // "Reboot": load the catalog, re-seed control plane + a fresh cell.
    let loaded = load_catalog(&dir).expect("readable").expect("present");
    let (control2, _inbox2) = ControlHandle::detached_with_catalog(Some(&loaded), CELLS, 0);
    assert!(control2.next_index_id() > dropping_id.0, "ids never regress");
    assert!(control2.next_index_generation() > dropping_generation, "generations never regress");
    let mut fresh = Keyspace::new(StoreConfig::default());
    fresh.seed_catalog(&loaded).expect("seed");
    let reg = fresh.idx_registry();
    assert!(reg.get_by_id(dropping_id).is_none(), "dropping resumed its drop");
    let survivor = reg.get_by_id(ready_id).expect("declaration survives");
    assert_eq!(survivor.state, IndexState::Backfilling, "readiness regresses at boot (D4)");
    assert_eq!(survivor.generation, ready_generation, "no boot generation bump (D4)");
    assert_eq!(reg.was_ready(ready_id), Some(true), "the S06 sidecar hint");
    // Rebuild completes → the pre-crash-ready index serves again.
    fresh.idx_registry_mut().set_catalog_state(ready_id, IndexState::Ready).expect("edge");
    fresh
        .idx_registry()
        .validate_binding(NsId(16), ready_id, ready_generation)
        .expect("ready again after rebuild");
    std::fs::remove_dir_all(&dir).ok();
}
