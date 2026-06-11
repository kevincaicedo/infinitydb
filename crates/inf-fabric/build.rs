fn main() {
    // `--cfg loom` is injected via RUSTFLAGS for the loom test runs; register
    // it so `unexpected_cfgs` stays clean under `-D warnings`.
    println!("cargo::rustc-check-cfg=cfg(loom)");
}
