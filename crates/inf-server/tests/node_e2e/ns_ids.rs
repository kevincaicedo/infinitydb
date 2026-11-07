//! Namespace ids at the node: an id moves only when the catalog writer
//! accepts a `CREATE`. Ids are never reused within a data directory, so an
//! id a refusal spent is lost, and a client repeating a refused `CREATE`
//! spends the id space at the DDL's pace.

use super::*;

/// `INF.NS INFO name`'s `id` field.
fn ns_id(c: &mut TcpStream, name: &[u8]) -> u32 {
    c.write_all(&cmd(&[b"INF.NS", b"INFO", name])).expect("write");
    let frame = read_frame(c);
    let text = String::from_utf8_lossy(&frame).into_owned();
    let lines: Vec<&str> = text.split("\r\n").collect();
    let at = lines.iter().position(|line| *line == "id").expect("INF.NS INFO names an id");
    lines[at + 2].parse().unwrap_or_else(|_| panic!("a numeric id in {text:?}"))
}

/// Every `INF.NS CREATE` refused before any effect leaves the next id
/// unchanged, so the next accepted `CREATE`'s id is the last accepted one's
/// successor. On two cells: `a` is created, `CREATE a` is refused twice
/// (the name exists), then `b` is created.
#[test]
fn a_refused_create_spends_no_namespace_id() {
    const REFUSED: u32 = 2;
    let dir = temp_data_dir("create-refused-id");
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    let create = |name: &[u8]| cmd(&[b"INF.NS", b"CREATE", name, b"MODE", b"durable"]);
    c.write_all(&create(b"a")).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let accepted = ns_id(&mut c, b"a");
    for _ in 0..REFUSED {
        c.write_all(&create(b"a")).expect("write");
        read_exactly(&mut c, b"-ERR namespace already exists\r\n");
    }
    c.write_all(&create(b"b")).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let next = ns_id(&mut c, b"b");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(
        next,
        accepted + 1,
        "the CREATE after {REFUSED} refused ones got id {next}; the last accepted was {accepted}"
    );
}

/// Boot's counter check (ADR-0148 D4): a data directory whose catalog
/// counter is not past a live id would issue that id again, so its catalog
/// is refused at the load every boot runs before any cell, with the typed
/// error naming both; a counter past it loads.
#[test]
fn a_catalog_counter_not_past_a_live_id_refuses_the_boot() {
    let dir = temp_data_dir("create-ids-low-counter");
    let ahead = inf_store::NsSpec {
        id: inf_store::NsId(20),
        name: b"ahead".to_vec(),
        mode: inf_store::NsMode::Memory,
        fsync: None,
        policy: None,
        maxmemory: None,
        tier: None,
    };
    let mut seed = inf_store::NsCatalog {
        next_id: 18,
        entries: vec![ahead],
        index: inf_store::IndexCatalog::default(),
        dropped: Vec::new(),
    };
    let fs = inf_log::fs::StdSegmentFs;
    inf_log::meta::write_meta(&fs, &dir, &seed.encode()).expect("seed META");
    let refused = inf_server::load_catalog(&dir).expect_err("the boot refuses the catalog");
    assert_eq!(refused.kind(), std::io::ErrorKind::InvalidData);
    assert!(
        refused.to_string().contains("namespace id 20 is at or above the catalog's next id 18"),
        "the refusal names the id and the counter: {refused}"
    );
    seed.next_id = 21;
    inf_log::meta::write_meta(&fs, &dir, &seed.encode()).expect("seed META");
    let loaded = inf_server::load_catalog(&dir).expect("a counter past the id loads");
    assert_eq!(loaded.map(|c| c.next_id), Some(21));
    std::fs::remove_dir_all(&dir).ok();
}

/// The ids oracle, from the wire alone: every `+OK` `CREATE`'s id (its
/// `INF.NS INFO` field) is the last committed id's successor, across
/// refusals and restarts. A `CREATE` commits one id when the catalog
/// writer accepts it: answered `+OK`, or refused after that by a plant the
/// test armed to fire on it (counted by the arming, never read from the
/// reply). Ids are issued up to `u32::MAX − 1`; `u32::MAX` means none is
/// left, so no id follows `u32::MAX − 1`.
struct IdsOracle {
    last_committed: u32,
}

impl IdsOracle {
    /// An oracle whose last committed id is `last` (a fresh data directory:
    /// one below the first named id).
    fn after(last: u32) -> IdsOracle {
        IdsOracle { last_committed: last }
    }

    /// A `CREATE` of `name` answered `+OK` with `id`.
    fn accepted(&mut self, name: &str, id: u32) {
        let last = self.last_committed;
        match last.checked_add(1).filter(|&next| next < u32::MAX) {
            Some(next) => assert_eq!(
                id, next,
                "CREATE {name} answered +OK with id {id} after the last committed {last}: the id \
                 is not its successor"
            ),
            None => panic!(
                "CREATE {name} answered +OK with id {id} after the last committed {last}: no id \
                 follows the last issuable one, so its successor is not an id"
            ),
        }
        self.last_committed = id;
    }

    /// A `CREATE` the writer accepted and a plant refused after: it
    /// committed the next id.
    fn committed_unanswered(&mut self) {
        self.last_committed += 1;
    }
}

/// `INF.NS CREATE name MODE durable` and its reply line.
fn create_durable(c: &mut TcpStream, name: &[u8]) -> Vec<u8> {
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", name, b"MODE", b"durable"])).expect("write");
    read_line(c)
}

/// The ids oracle across every refusal the origin and the writer answer
/// before `Accepted` (parse, a taken name, the reserved-address admission),
/// two refused after it (the origin's store, a peer's leg: each commits one
/// id), and a restart: each `+OK`'s id is the last committed id's
/// successor. On two cells; every refusal's own reply is its engagement.
#[test]
fn every_accepted_create_takes_the_last_committed_id_plus_one() {
    use inf_foundation::fault::FaultSpec;
    use inf_server::fault::{NS_CREATE_FAN_REFUSED, NS_CREATE_ORIGIN_REFUSED};
    let dir = temp_data_dir("create-ids-oracle");
    // The origin's second `CREATE` apply (`p1`) and the peer's second
    // `CREATE` leg (`p2`'s; `p1` sends none) are refused.
    let faults = vec![
        (NS_CREATE_ORIGIN_REFUSED, FaultSpec::Nth(2)),
        (NS_CREATE_FAN_REFUSED, FaultSpec::Nth(2)),
    ];
    let node = Node::start_durable_with_faults(2, &dir, faults);
    let mut oracle = IdsOracle::after(inf_store::FIRST_NAMED_NS_ID - 1);
    let mut c = node.connect();
    assert_eq!(create_durable(&mut c, b"a"), b"+OK\r\n");
    oracle.accepted("a", ns_id(&mut c, b"a"));
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"b", b"MODE", b"bogus"])).expect("write");
    let parse = read_line(&mut c);
    assert!(parse.starts_with(b"-ERR"), "the parse refusal: {:?}", String::from_utf8_lossy(&parse));
    assert_eq!(create_durable(&mut c, b"a"), b"-ERR namespace already exists\r\n");
    let big = [&b"INF.NS"[..], b"CREATE", b"big", b"MODE", b"durable", b"MEM-BUDGET", b"200gb"];
    c.write_all(&cmd(&big)).expect("write");
    let va = read_line(&mut c);
    assert!(
        va.starts_with(b"-ERR tiered namespace would exceed the node's reserved-VA limit"),
        "the reserved-address admission: {:?}",
        String::from_utf8_lossy(&va)
    );
    assert_eq!(create_durable(&mut c, b"p1"), b"-ERR fault: ns_create_origin_refused\r\n");
    oracle.committed_unanswered();
    assert_eq!(create_durable(&mut c, b"p2"), b"-ERR fault: ns_create_fan_refused\r\n");
    oracle.committed_unanswered();
    assert_eq!(create_durable(&mut c, b"c1"), b"+OK\r\n");
    oracle.accepted("c1", ns_id(&mut c, b"c1"));
    drop(c);
    node.stop();
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    assert_eq!(create_durable(&mut c, b"c2"), b"+OK\r\n");
    oracle.accepted("c2", ns_id(&mut c, b"c2"));
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}

/// The id space's last ids, from a catalog whose counter is `u32::MAX − 2`:
/// two `CREATE`s take `u32::MAX − 2` and `u32::MAX − 1` (a restart between),
/// then every `CREATE` answers `-ERR namespace identity space exhausted`
/// before any effect, also after a restart; no id repeats or wraps.
#[test]
fn the_last_namespace_id_is_issued_once_and_then_the_space_refuses() {
    let dir = temp_data_dir("create-ids-exhausted");
    let seed = inf_store::NsCatalog {
        next_id: u32::MAX - 2,
        entries: Vec::new(),
        index: inf_store::IndexCatalog::default(),
        dropped: Vec::new(),
    };
    inf_log::meta::write_meta(&inf_log::fs::StdSegmentFs, &dir, &seed.encode()).expect("seed META");
    let mut oracle = IdsOracle::after(u32::MAX - 3);
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    assert_eq!(create_durable(&mut c, b"x"), b"+OK\r\n");
    oracle.accepted("x", ns_id(&mut c, b"x"));
    drop(c);
    node.stop();
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    assert_eq!(create_durable(&mut c, b"y"), b"+OK\r\n");
    oracle.accepted("y", ns_id(&mut c, b"y"));
    for name in [&b"z"[..], b"z"] {
        let reply = create_durable(&mut c, name);
        if reply == b"+OK\r\n" {
            oracle.accepted("z", ns_id(&mut c, name));
        }
        assert_eq!(reply, b"-ERR namespace identity space exhausted\r\n", "the spent space");
    }
    drop(c);
    node.stop();
    let node = Node::start_durable(2, &dir);
    let mut c = node.connect();
    assert_eq!(create_durable(&mut c, b"w"), b"-ERR namespace identity space exhausted\r\n");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}
