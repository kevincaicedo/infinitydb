//! A `CREATE` its origin's own store refuses after the catalog swap
//! (ADR-0148 D5). The catalog writer has accepted the `CREATE` and its
//! `META` swap has returned when the origin applies it; the origin's
//! refusal there (`ns_create_origin_refused`, in place of its
//! `ns_create`) is answered to the client. A refused `CREATE` must leave
//! the namespace on no cell and in no durable `META`, before and after a
//! cut, so the origin withdraws the definition durably before it answers.

use super::*;

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
