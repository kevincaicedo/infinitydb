//! Included only by carrying test targets; no product dependency edge.

/// Emit only after the fault/cut and verdict assertions have passed.
pub fn verified(point: &str, verdict: &str) {
    #[allow(clippy::disallowed_methods, reason = "test support: the harness names the receipt")]
    let token = std::env::var("INF_CRASH_MATRIX_RECEIPT");
    if let Ok(token) = token {
        eprintln!("CRASH_MATRIX_VERIFIED\t{token}\t{point}\t{verdict}");
    }
}
