// ADR-0144 A1: this call must be rejected by the pinned stable compiler.
// Compile only; the probe never opens or changes a file.
pub fn probe(path: &std::path::Path) {
    let _ = std::fs::set_times(path, std::fs::FileTimes::new()); // UNSTABLE E0658 fs_set_times std::fs::set_times
}
