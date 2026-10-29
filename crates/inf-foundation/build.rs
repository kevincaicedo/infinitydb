fn main() {
    // `--cfg loom` is injected via RUSTFLAGS for the Loom runs (ADR-0159
    // A1.7); register it so `unexpected_cfgs` stays clean under `-D warnings`.
    println!("cargo::rustc-check-cfg=cfg(loom)");
}
