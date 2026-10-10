use std::process::Command;

#[test]
fn cli_verifies_and_reports_the_second_state_hash() {
    let output = Command::new(env!("CARGO_BIN_EXE_inf-sim"))
        .args([
            "--scenario",
            "m0-smoke",
            "--seed",
            "0xC0FFEE",
            "--commands",
            "100",
            "--verify-determinism",
        ])
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(stdout.contains("state_hash="), "state omitted: {stdout}");
    assert!(stdout.contains("state hash identical"), "state not verified: {stdout}");
}

#[test]
fn every_sweep_path_honors_state_verification_for_each_seed() {
    for scenario in [
        "m2-durable",
        "m2-combined",
        "m4-recovery",
        "m4-tiered",
        "m2-ns-create-window",
        "m2-ns-ddl-race",
    ] {
        let output = Command::new(env!("CARGO_BIN_EXE_inf-sim"))
            .args([
                "--scenario",
                scenario,
                "--seed",
                "0xC0FFEE",
                "--sweep",
                "2",
                "--verify-determinism",
            ])
            .output()
            .unwrap();
        assert!(output.status.success(), "{scenario}: {output:?}");
        let stdout = String::from_utf8_lossy(&output.stdout);
        assert_eq!(stdout.matches("state hash identical").count(), 2, "{scenario}: {stdout}");
    }
}
