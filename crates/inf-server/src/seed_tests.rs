//! The plane builds the keyspace it serves from its seed, on the census it
//! builds first (ADR-0189 D4, ADR-0163 D2).

use super::*;
use inf_fabric::{Mesh, MeshConfig};
use inf_store::{NsCatalog, NsError, NsSpec, StoreConfig, TierSpec};

/// A memory namespace's spec under `name`.
fn memory(id: u32, name: &[u8]) -> NsSpec {
    NsSpec {
        id: NsId(id),
        name: name.to_vec(),
        mode: NsMode::Memory,
        fsync: None,
        policy: None,
        maxmemory: None,
        tier: None,
    }
}

fn plane_of(
    seed: KeyspaceSeed,
    node: &Rc<NodeInfo>,
) -> Result<(ServerPlane, ServeMark, TimerSet), PlaneBuildError> {
    let fabric = Mesh::new(2, MeshConfig::default()).into_iter().nth(1).expect("cell 1");
    ServerPlane::new(CellId(1), 2, -1, seed, fabric, Rc::clone(node), NoopObserver, false)
}

/// A seed the keyspace's build refuses builds no plane: a catalog with two
/// entries of one name is the seeding's refusal, `PlaneBuildError::Seed`,
/// printed as the boot error printed it, and the node is left unwritten (no
/// census attached, its cell and cell count untouched), so nothing of the
/// refused build is served. The same catalog less the second entry builds.
#[test]
fn a_refused_seed_builds_no_plane_and_leaves_the_node_unwritten() {
    let node = Rc::new(NodeInfo::try_default().expect("fixture cache allocation"));
    let mut catalog = NsCatalog::empty();
    catalog.entries = vec![memory(16, b"twice"), memory(17, b"twice")];
    let seed = KeyspaceSeed::Cataloged { cfg: StoreConfig::default(), catalog: catalog.clone() };
    let refused = plane_of(seed, &node).err().expect("a duplicate name refuses the seed");
    assert_eq!(refused, PlaneBuildError::Seed(NsError::Exists));
    assert_eq!(refused.to_string(), "Exists", "the seed's refusal prints its own text");
    assert!(node.cap_census.borrow().is_none(), "no census attached to the node");
    assert_eq!((node.cell.get(), node.cells.get()), (0, 0), "the node unwritten");
    catalog.entries.pop();
    let seed = KeyspaceSeed::Cataloged { cfg: StoreConfig::default(), catalog };
    let (plane, _serve_mark, _timer_set) = plane_of(seed, &node).expect("one entry builds");
    assert!(plane.shared.store.borrow().ns_get(b"twice").is_some(), "the seed's namespace");
    assert_eq!((node.cell.get(), node.cells.get()), (1, 2));
}

/// The seeded keyspace's tables register on the plane's census: a tiered
/// spec in an uncataloged seed leaves the table's capped holders' rows on
/// the census the plane attaches to its node and its loop marks, and the
/// recovery the plane begins builds on that census too.
#[test]
fn a_seeded_tiered_table_registers_on_the_planes_census() {
    let node = Rc::new(NodeInfo::try_default().expect("fixture cache allocation"));
    let tiered = NsSpec {
        mode: NsMode::Durable,
        fsync: Some(inf_log::FsyncClass::Always),
        tier: Some(TierSpec::for_budget(4 << 20)),
        ..memory(16, b"tiered")
    };
    let seed = KeyspaceSeed::Uncataloged { cfg: StoreConfig::default(), specs: vec![tiered] };
    let (plane, serve_mark, _timer_set) = plane_of(seed, &node).expect("the plane");
    assert!(plane.shared.store.borrow().tiered_store(NsId(16)).is_some(), "the seeded table");
    let attached = node.cap_census.borrow().clone().expect("the plane's census attached");
    assert!(Rc::ptr_eq(&attached, serve_mark.census()), "the census the loop marks");
}
