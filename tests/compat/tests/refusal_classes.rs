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

/// The matrix's `INF.NS` row quotes the namespace identity space's and the
/// catalog limit's bytes as the table renders them.
#[test]
fn the_matrix_quotes_the_rendered_refusal_bytes() {
    let row = compat::matrixgen::DECLARED.iter().find(|d| d.name == "INF.NS").expect("INF.NS row");
    let text = Refusal::IdentityExhausted(Domain::Namespace).render();
    assert!(row.note.contains(&format!("`-{text}`")), "INF.NS's row does not quote `-{text}`");
    let limit = Refusal::LimitReached(Domain::Catalog).render();
    assert!(row.note.contains(&format!("`-{limit}`")), "INF.NS's row does not quote `-{limit}`");
    let checkpoint = Refusal::IdentityExhausted(Domain::Checkpoint).render();
    for name in ["INF.NS", "INF.CKPT", "BGSAVE"] {
        let row = compat::matrixgen::DECLARED.iter().find(|d| d.name == name).expect("a row");
        assert!(row.note.contains(&format!("`-{checkpoint}`")), "{name} does not quote it");
    }
}
