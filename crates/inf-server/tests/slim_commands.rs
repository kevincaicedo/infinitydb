//! L11 / L20-09: the docless server retains strings and refuses documents.
#![cfg(not(feature = "doc"))]

use inf_foundation::time::Nanos;
use inf_server::{ConnCx, execute_slices};
use inf_store::{Keyspace, StoreConfig};

#[test]
fn slim_serves_strings_and_refuses_documents() {
    let mut store = Keyspace::new(StoreConfig::default());
    let mut cx = ConnCx::default();
    let mut run = |argv: &[&[u8]]| {
        let mut reply = Vec::new();
        execute_slices(argv, &mut store, &mut cx, Nanos(1), &mut reply);
        reply
    };
    assert_eq!(run(&[b"SET", b"k", b"v"]), b"+OK\r\n");
    assert_eq!(run(&[b"GET", b"k"]), b"$1\r\nv\r\n");
    assert!(run(&[b"JSON.SET", b"k", b"$", b"{}"]).starts_with(b"-ERR unknown command"));
    assert!(run(&[b"JSON.GET", b"k"]).starts_with(b"-ERR unknown command"));
    assert_eq!(run(&[b"GET", b"k"]), b"$1\r\nv\r\n");
}
