//! Placement errors refuse before creating durable state or starting cells.

#[test]
fn invalid_stride_and_cpu_overflow_are_usage_errors() {
    let maximum = usize::MAX.to_string();
    for (args, reason) in [
        (vec!["--pin-stride", "0"], "--pin-stride must be >= 1"),
        (vec!["--pin-start", &maximum, "--cells", "2"], "cell CPU overflow"),
    ] {
        let output = std::process::Command::new(env!("CARGO_BIN_EXE_infinityd"))
            .args(args)
            .output()
            .unwrap();
        assert!(!output.status.success());
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(stderr.contains(reason), "{stderr}");
        assert!(!stderr.contains("listening"), "{stderr}");
    }
}
