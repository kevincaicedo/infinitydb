//! Included only by carrying test targets; no product dependency edge.

/// Emit only after the fault/cut and verdict assertions have passed.
pub fn verified(point: &str, verdict: &str) {
    if let Ok(token) = std::env::var("INF_CRASH_MATRIX_RECEIPT") {
        eprintln!("CRASH_MATRIX_VERIFIED\t{token}\t{point}\t{verdict}");
    }
}
