//! The refusal classes against real Redis (ADR-0172 D1, D2): every line
//! `inf_wire::Refusal` renders is the error line Redis itself emits for that
//! text (`redis.error_reply` through `EVAL`, the oracle's own bytes), and its
//! prefix is one of the error codes Redis replies with (`TRYAGAIN`, `ERR`,
//! `OOM`), so a client reads InfinityDB's refusal by the code it reads
//! Redis's. The compatibility matrix's hand-written refusal rows quote the
//! table's bytes, checked here, so the two cannot drift.

use std::io::Write;

use compat::harness::{oracle, read_frames};
use compat::resp::encode_command;
use inf_wire::{Domain, Refusal};

#[test]
fn every_refusal_line_is_the_error_line_redis_emits() {
    let (_guard, mut oracle) = oracle();
    let mut buf = Vec::new();
    let mut lines = 0usize;
    for refusal in Refusal::every() {
        let text = refusal.render();
        let code = text.split(' ').next().unwrap_or_default();
        assert!(["TRYAGAIN", "ERR", "OOM"].contains(&code), "{refusal:?}: code {code:?}");
        let argv: Vec<String> =
            ["EVAL", "return redis.error_reply(ARGV[1])", "0", text].map(String::from).to_vec();
        oracle.write_all(&encode_command(&argv)).expect("write to the oracle");
        let reply = read_frames(&mut oracle, &mut buf, 1);
        assert_eq!(
            String::from_utf8_lossy(&reply),
            format!("-{text}\r\n"),
            "{refusal:?}: the oracle emits another line for its text"
        );
        lines += 1;
    }
    assert_eq!(lines, 4 * Domain::ALL.len(), "VACUOUS: the table rendered {lines} lines");
}

/// The class suffixes a refusal line ends in (ADR-0172 D1).
const CLASS_SUFFIXES: [&str; 4] =
    ["capacity in use", "limit reached", "reservation refused", "identity space exhausted"];

/// Every refusal line the matrix quotes is a line the table renders: each
/// row's backquoted `-…` text that ends in a class suffix is one of
/// `Refusal::every()`'s, so a mistyped line in any row is red and no row
/// joins a list to be checked. The rows that answer a refusal quote it:
/// `INF.NS` the namespace identity space's, the catalog limit's and both
/// tiered lifetime lines, and the three checkpoint rows the checkpoint
/// identity space's.
#[test]
fn the_matrix_quotes_the_rendered_refusal_bytes() {
    let rendered: Vec<String> = Refusal::every().map(|r| format!("-{}", r.render())).collect();
    let mut quoted = 0;
    for row in compat::matrixgen::DECLARED {
        for quote in row.note.split('`').skip(1).step_by(2) {
            if quote.starts_with('-') && CLASS_SUFFIXES.iter().any(|s| quote.ends_with(s)) {
                assert!(rendered.iter().any(|line| line == quote), "{}: `{quote}`", row.name);
                quoted += 1;
            }
        }
    }
    assert!(quoted >= 7, "VACUOUS: the matrix quotes {quoted} refusal lines");
    let quotes = |name: &str, refusal: Refusal| {
        let row = compat::matrixgen::DECLARED.iter().find(|d| d.name == name).expect("a row");
        let text = format!("`-{}`", refusal.render());
        assert!(row.note.contains(&text), "{name}'s row does not quote {text}");
    };
    quotes("INF.NS", Refusal::IdentityExhausted(Domain::Namespace));
    quotes("INF.NS", Refusal::LimitReached(Domain::Catalog));
    quotes("INF.NS", Refusal::CapacityBusy(Domain::TieredNamespace));
    quotes("INF.NS", Refusal::LimitReached(Domain::TieredNamespace));
    for name in ["INF.NS", "INF.CKPT", "BGSAVE"] {
        quotes(name, Refusal::IdentityExhausted(Domain::Checkpoint));
    }
}
