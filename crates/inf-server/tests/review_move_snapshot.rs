//! ADR-0110: internal move extensions validate before touching a live record.

use inf_foundation::time::Nanos;
use inf_server::{ConnCx, execute_slices};
use inf_store::{Keyspace, StoreConfig};

fn call(store: &mut Keyspace, cx: &mut ConnCx, argv: &[&[u8]]) -> Vec<u8> {
    let mut reply = Vec::new();
    execute_slices(argv, store, cx, Nanos(1), &mut reply);
    reply
}

/// Malformed optional forms never remove a source; legacy forms still work.
#[test]
fn snapshot_validation_and_legacy_forms() {
    let mut store = Keyspace::new(StoreConfig::default());
    // ADR-0115: the program primitives run only under a program context.
    let mut cx = ConnCx { program: true, ..ConnCx::default() };
    assert_eq!(call(&mut store, &mut cx, &[b"SET", b"key", b"value"]), b"+OK\r\n");
    let cases: &[&[&[u8]]] = &[
        &[b"INF.TAKE", b"key", b"IF"],
        &[b"INF.TAKE", b"key", b"IF", b"value"],
        &[b"INF.TAKE", b"key", b"IF", b"value", b"-2"],
        &[b"INF.TAKE", b"key", b"IF", b"value", b"9223372036854775808"],
        &[b"INF.TAKE", b"key", b"IF", b"value", b"-1", b"extra"],
        &[b"INF.TAKE", b"key", b"BAD", b"value", b"-1"],
        &[b"INF.PEEK", b"key", b"BAD"],
        &[b"INF.PEEK", b"key", b"ABS", b"extra"],
        &[b"INF.PEEK", b"key", b"ABS", b"NOSTATS", b"extra"],
    ];
    for argv in cases {
        assert!(call(&mut store, &mut cx, argv).starts_with(b"-ERR "));
        assert_eq!(call(&mut store, &mut cx, &[b"GET", b"key"]), b"$5\r\nvalue\r\n");
    }
    assert_eq!(
        call(&mut store, &mut cx, &[b"INF.PEEK", b"key", b"ABS"]),
        b"*2\r\n$5\r\nvalue\r\n:-1\r\n"
    );
    assert_eq!(
        call(&mut store, &mut cx, &[b"INF.TAKE", b"key", b"IF", b"wrong", b"-1"]),
        b":0\r\n"
    );
    assert_eq!(call(&mut store, &mut cx, &[b"INF.TAKE", b"key", b"IF", b"value", b"1"]), b":0\r\n");
    assert_eq!(call(&mut store, &mut cx, &[b"INF.PEEK", b"key"]), b"*2\r\n$5\r\nvalue\r\n:-1\r\n");
    assert_eq!(call(&mut store, &mut cx, &[b"INF.TAKE", b"key"]), b"*2\r\n$5\r\nvalue\r\n:-1\r\n");
    assert_eq!(call(&mut store, &mut cx, &[b"INF.PEEK", b"key", b"ABS"]), b"*-1\r\n");
    assert_eq!(
        call(&mut store, &mut cx, &[b"INF.TAKE", b"key", b"IF", b"value", b"-1"]),
        b":0\r\n"
    );
}

/// The zero timestamp and quiet read both use the registered execution path.
#[test]
fn zero_deadline_and_quiet_reads() {
    let mut store = Keyspace::new(StoreConfig::default());
    // ADR-0115: the program primitives run only under a program context.
    let mut cx = ConnCx { program: true, ..ConnCx::default() };
    cx.node.wall_anchor.set((60_000, 0));
    assert_eq!(call(&mut store, &mut cx, &[b"SET", b"key", b"v", b"PX", b"60000"]), b"+OK\r\n");
    let before = store.stats();
    assert_eq!(
        call(&mut store, &mut cx, &[b"INF.PEEK", b"key", b"ABS", b"NOSTATS"]),
        b"*2\r\n$1\r\nv\r\n:0\r\n"
    );
    assert_eq!(call(&mut store, &mut cx, &[b"INF.TAKE", b"key", b"IF", b"v", b"0"]), b":1\r\n");
    assert_eq!(store.stats().keyspace_hits, before.keyspace_hits);
    assert_eq!(store.stats().keyspace_misses, before.keyspace_misses);
}

/// Quiet reads retain missing/type checks and never expose document handles.
#[cfg(feature = "doc")]
#[test]
fn quiet_snapshot_preserves_type_and_missing_semantics() {
    let mut store = Keyspace::new(StoreConfig::default());
    // ADR-0115: the program primitives run only under a program context.
    let mut cx = ConnCx { program: true, ..ConnCx::default() };
    let before = store.stats();
    assert_eq!(
        call(&mut store, &mut cx, &[b"INF.PEEK", b"missing", b"ABS", b"NOSTATS"]),
        b"*-1\r\n"
    );
    assert_eq!(call(&mut store, &mut cx, &[b"JSON.SET", b"doc", b"$", b"{}"]), b"+OK\r\n");
    assert!(
        call(&mut store, &mut cx, &[b"INF.PEEK", b"doc", b"ABS", b"NOSTATS"])
            .starts_with(b"-WRONGTYPE ")
    );
    assert_eq!(call(&mut store, &mut cx, &[b"INF.TAKE", b"doc", b"IF", b"{}", b"-1"]), b":0\r\n");
    assert_eq!(store.stats().keyspace_hits, before.keyspace_hits);
    assert_eq!(store.stats().keyspace_misses, before.keyspace_misses);
    assert_eq!(call(&mut store, &mut cx, &[b"TYPE", b"doc"]), b"+ReJSON-RL\r\n");
}

/// Every byte participates in the comparison, including NUL and RESP delimiters.
#[test]
fn snapshot_matches_binary_value_and_deadline() {
    let mut store = Keyspace::new(StoreConfig::default());
    // ADR-0115: the program primitives run only under a program context.
    let mut cx = ConnCx { program: true, ..ConnCx::default() };
    let value = b"\0\xff\r\n$-1\r\n";
    assert_eq!(call(&mut store, &mut cx, &[b"SET", b"key", value, b"PX", b"60000"]), b"+OK\r\n");
    let deadline = call(&mut store, &mut cx, &[b"PEXPIRETIME", b"key"]);
    let deadline = &deadline[1..deadline.len() - 2];
    assert_eq!(
        call(
            &mut store,
            &mut cx,
            &[b"INF.TAKE", b"key", b"IF", &value[..value.len() - 1], deadline]
        ),
        b":0\r\n"
    );
    assert_eq!(
        call(&mut store, &mut cx, &[b"INF.TAKE", b"key", b"if", value, deadline]),
        b":1\r\n"
    );
    assert_eq!(call(&mut store, &mut cx, &[b"GET", b"key"]), b"$-1\r\n");
}
