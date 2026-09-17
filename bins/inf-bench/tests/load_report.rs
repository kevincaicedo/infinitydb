use std::net::TcpListener;
use std::process::Command;

#[test]
fn load_cli_labels_stdout_and_saved_report() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port().to_string();
    let directory =
        std::env::temp_dir().join(format!("inf-bench-mode-{}-{port}", std::process::id()));
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join("report.txt");
    // A zero-key fill connects without sending requests or waiting through warmup.
    let output = Command::new(env!("CARGO_BIN_EXE_inf-bench"))
        .args(["load", "--port", &port, "--conns", "1", "--fill", "0", "--out"])
        .arg(&path)
        .output()
        .unwrap();
    assert!(output.status.success(), "{}", String::from_utf8_lossy(&output.stderr));
    let saved = std::fs::read_to_string(&path).unwrap();
    std::fs::remove_dir_all(&directory).unwrap();
    assert_eq!(output.stdout, saved.as_bytes());
    assert!(saved.lines().any(|line| line == "mode = closed-loop"), "{saved}");
}
