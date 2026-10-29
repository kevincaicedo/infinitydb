//! One wheel node per key hash, from the wire (ADR-0008 A1, M1-S04 AC 1):
//! every `PEXPIRE` on one key runs the real `execute` path, and the
//! wheel's resident bytes — the `wheel_bytes` tripwire `INFO` renders —
//! must stay one node plus the fixed tables, however often the deadline
//! moves: a deadline change keeps or moves the key's one node, never
//! arms a second.

use inf_foundation::time::Nanos;
use inf_server::{ConnCx, execute_slices};
use inf_store::{Keyspace, StoreConfig};

/// Deadline rewrites on one key: 4,096 nodes at 16 B would be 64 KiB.
const REWRITES: u64 = 4_096;

/// One node plus the membership table and the fixed slot tables stays
/// under this; 4,096 nodes cannot.
const WHEEL_BYTES_MAX: u64 = 24 << 10;

fn run(ks: &mut Keyspace, cx: &mut ConnCx, argv: &[&[u8]]) -> Vec<u8> {
    let mut reply = Vec::new();
    execute_slices(argv, ks, cx, Nanos(1_000_000), &mut reply);
    reply
}

fn info_field(info: &[u8], field: &str) -> u64 {
    let text = String::from_utf8_lossy(info);
    let prefix = format!("{field}:");
    text.lines()
        .find_map(|line| line.strip_prefix(&prefix))
        .unwrap_or_else(|| panic!("INFO has no {field} line"))
        .trim()
        .parse()
        .expect("a decimal gauge")
}

#[test]
fn pexpire_rewrites_on_one_key_keep_one_wheel_node() {
    let mut ks = Keyspace::new(StoreConfig::default());
    let mut cx = ConnCx::try_default().expect("fixture cache allocation");
    assert_eq!(run(&mut ks, &mut cx, &[b"SET", b"k", b"v"]), b"+OK\r\n");
    for i in 0..REWRITES {
        let ttl = (100_000 + i).to_string();
        assert_eq!(run(&mut ks, &mut cx, &[b"PEXPIRE", b"k", ttl.as_bytes()]), b":1\r\n");
    }
    let info = run(&mut ks, &mut cx, &[b"INFO", b"tripwires"]);
    let wheel_bytes = info_field(&info, "wheel_bytes");
    assert!(
        wheel_bytes < WHEEL_BYTES_MAX,
        "wheel_bytes {wheel_bytes} after {REWRITES} PEXPIREs on one key (one node expected)"
    );
    assert_eq!(info_field(&info, "wheel_fallback"), 0);
}
