//! A `CREATE` its origin's own store refuses after the catalog swap
//! (ADR-0148 D5). The catalog writer has accepted the `CREATE` and its
//! `META` swap has returned when the origin applies it; the origin's
//! refusal there (`ns_create_origin_refused`, in place of its
//! `ns_create`) is answered to the client. A refused `CREATE` must leave
//! the namespace on no cell and in no durable `META`, before and after a
//! cut, so the origin withdraws the definition durably before it answers.

// nextest-stall-default: each tiered `CREATE` here is refused or accepted, and
// no tiered namespace is written: no write reaches a window (ADR-0053 D4), so
// the stall bound is never read.

use super::*;

/// Whether the `META` on disk names a namespace called `name`, and its
/// `next_id`.
fn meta_names(dir: &std::path::Path, name: &[u8]) -> (bool, u32) {
    let catalog = inf_server::load_catalog(dir).expect("META decodes").expect("META exists");
    (catalog.entries.iter().any(|e| e.name == name), catalog.next_id)
}

/// The crash-matrix carrier of `ns_create_origin_refused`: on two cells,
/// the origin's store refuses `refused` after the swap. The refusal is the
/// reply; no cell serves `refused`; the name creates again, and a restart
/// serves the second definition.
#[test]
fn an_origin_refused_create_answers_and_its_name_creates_again() {
    use inf_foundation::fault::FaultSpec;
    let dir = temp_data_dir("create-origin-refused");
    let faults = vec![(inf_server::fault::NS_CREATE_ORIGIN_REFUSED, FaultSpec::Nth(1))];
    let node = Node::start_durable_with_faults(2, &dir, faults);
    let mut c = node.connect();
    let create = cmd(&[b"INF.NS", b"CREATE", b"refused", b"MODE", b"durable"]);
    c.write_all(&create).expect("write");
    read_exactly(&mut c, b"-ERR fault: ns_create_origin_refused\r\n");
    for cell in 0..2u16 {
        let mut c = conn_on_cell(&node, cell);
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"refused"])).expect("write");
        let reply = read_line(&mut c);
        assert!(
            reply.starts_with(b"-ERR namespace 'refused' not found"),
            "cell {cell} serves the namespace its origin refused: {:?}",
            String::from_utf8_lossy(&reply)
        );
    }
    // The point fires at the origin's first apply only: the second applies.
    c.write_all(&create).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    drop(c);
    node.stop();
    let node = Node::start_durable(2, &dir);
    for cell in 0..2u16 {
        let mut c = conn_on_cell_use(&node, cell, b"refused");
        c.write_all(&cmd(&[b"SET", &key_for_cell(2, cell), b"after"])).expect("write");
        read_exactly(&mut c, b"+OK\r\n");
    }
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
    receipt::verified("ns_create_origin_refused", "origin-refusal-answered");
}

/// The narrowest tier of the origin's refusal: once its refusal is
/// answered, the `META` on disk no longer names the namespace. The swap
/// itself is read from the disk (`META`'s counter past the refused id), so
/// a refusal that fired before the swap reads as `VACUOUS`, never green.
#[test]
fn an_origin_refused_create_leaves_no_meta_naming_it_once_answered() {
    use inf_foundation::fault::FaultSpec;
    let dir = temp_data_dir("create-origin-refused-meta");
    // The point fires at the origin's second `CREATE` apply: `before`
    // applies, and its `META` holds the counter the refused one moves past.
    let faults = vec![(inf_server::fault::NS_CREATE_ORIGIN_REFUSED, FaultSpec::Nth(2))];
    let node = Node::start_durable_with_faults(2, &dir, faults);
    let mut c = node.connect();
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"before", b"MODE", b"durable"])).expect("write");
    read_exactly(&mut c, b"+OK\r\n");
    let (_, counter_before) = meta_names(&dir, b"before");
    c.write_all(&cmd(&[b"INF.NS", b"CREATE", b"refused", b"MODE", b"durable"])).expect("write");
    let reply = read_line(&mut c);
    let (named, counter_after) = meta_names(&dir, b"refused");
    drop(c);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
    assert_eq!(reply, b"-ERR fault: ns_create_origin_refused\r\n", "the plant's refusal");
    assert!(
        counter_after > counter_before,
        "VACUOUS: META's counter is {counter_after}, as before the refused CREATE: its swap never \
         landed"
    );
    assert!(!named, "META still names the namespace its origin refused after its refusal");
}

/// The client-reachable route to the origin's refusal after the swap: a
/// `CONFIG SET tiered-reserved-va-limit` on the origin's cell, made while
/// its tiered `CREATE` waits for the catalog writer, lowers the
/// reserved-address admission the store's apply runs again. The writer
/// accepts (the limit is the cell's, not the writer's); the origin's apply
/// is refused; the `CREATE` rolls back before it answers, so `META` names
/// nothing of it and no cell serves it; and its grant returns: with the
/// limit restored, the same tiered `CREATE` is accepted (ADR-0148 D2, D5).
#[test]
fn a_reserved_address_limit_lowered_during_the_wait_rolls_the_create_back() {
    let dir = temp_data_dir("create-va-lowered");
    let hold = Arc::new(AtomicBool::new(true));
    let node =
        Node::start_durable_with_held_catalog(2, &dir, Arc::clone(&hold), CkptTrigger::Manual);
    let create = tiered_ns_create(b"lowered", &[b"MEM-BUDGET", b"3mb"]);
    let control = node.control.as_ref().expect("a durable node");
    let sent = control.persist_requests_sent();
    let mut creator = conn_on_cell(&node, 0);
    creator.write_all(&create).expect("write");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while control.persist_requests_sent() == sent {
        assert!(std::time::Instant::now() < deadline, "the CREATE never sent its request");
        std::thread::yield_now();
    }
    let mut admin = conn_on_cell(&node, 0);
    let lower = cmd(&[b"CONFIG", b"SET", b"tiered-reserved-va-limit", b"1048576"]);
    admin.write_all(&lower).expect("write");
    read_exactly(&mut admin, b"+OK\r\n");
    hold.store(false, Ordering::Release);
    let refusal = read_line(&mut creator);
    assert!(
        refusal.starts_with(b"-ERR tiered namespace would exceed the node's reserved-VA limit"),
        "engagement: the origin's apply refused at the lowered limit: {:?}",
        String::from_utf8_lossy(&refusal)
    );
    let (named, _) = meta_names(&dir, b"lowered");
    assert!(!named, "META names the namespace its origin refused once the refusal is answered");
    for cell in 0..2u16 {
        let mut c = conn_on_cell(&node, cell);
        c.write_all(&cmd(&[b"INF.NS", b"USE", b"lowered"])).expect("write");
        let reply = read_line(&mut c);
        assert!(reply.starts_with(b"-ERR namespace 'lowered' not found"), "cell {cell}: {reply:?}");
    }
    let restore = cmd(&[b"CONFIG", b"SET", b"tiered-reserved-va-limit", b"274877906944"]);
    admin.write_all(&restore).expect("write");
    read_exactly(&mut admin, b"+OK\r\n");
    // The grant returns once every cell's receipt names the refused id.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        creator.write_all(&create).expect("write");
        let reply = read_line(&mut creator);
        if reply == b"+OK\r\n" {
            break;
        }
        assert_eq!(
            reply,
            b"-TRYAGAIN tiered namespace capacity in use\r\n".to_vec(),
            "only a Retiring grant refuses it meanwhile"
        );
        assert!(std::time::Instant::now() < deadline, "the refused CREATE's grant never returned");
        #[allow(clippy::disallowed_methods)] // test harness thread, not cell code
        std::thread::sleep(Duration::from_millis(20));
    }
    drop(creator);
    drop(admin);
    node.stop();
    std::fs::remove_dir_all(&dir).ok();
}
