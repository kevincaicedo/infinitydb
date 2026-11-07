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
