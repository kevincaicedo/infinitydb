//! Oracle absence must fail even when libtest captures the skip message.

use std::process::Command;

#[test]
fn missing_oracle_cannot_pass() {
    let output =
        child_command().env("PATH", "/nonexistent-inf-compat-oracle").output().expect("run child");
    assert!(!output.status.success(), "the missing Redis oracle silently passed");
    assert!(String::from_utf8_lossy(&output.stderr).contains("redis-server"));
}

fn child_command() -> Command {
    let mut child = Command::new(std::env::current_exe().expect("test executable"));
    child
        .args(["--exact", "oracle_requirement_child", "--nocapture"])
        .env("INF_COMPAT_ORACLE_CHILD", "1")
        .env_remove("INF_COMPAT_ORACLE_ADDR");
    child
}

#[cfg(unix)]
#[test]
fn invalid_local_oracles_cannot_pass() {
    use std::os::unix::fs::PermissionsExt;
    let directory = std::env::temp_dir().join(format!("inf-oracle-fixture-{}", std::process::id()));
    std::fs::create_dir(&directory).expect("fixture directory");
    let executable = std::env::current_exe().expect("test executable");
    let executable = executable.to_str().expect("UTF-8 path").replace('\'', "'\"'\"'");
    let stub = directory.join("redis-server");
    std::fs::write(
        &stub,
        format!(
            "#!/bin/sh\nexport INF_COMPAT_STUB_PORT=\"$2\"\n\
             exec '{executable}' --exact oracle_stub_child --nocapture\n"
        ),
    )
    .expect("stub launcher");
    std::fs::set_permissions(&stub, std::fs::Permissions::from_mode(0o755)).expect("executable");
    for version in ["8.6.2", "", compat::harness::ORACLE_VERSION] {
        let output = child_command()
            .env("PATH", &directory)
            .env("INF_COMPAT_STUB_VERSION", version)
            .output()
            .expect("run child");
        assert_eq!(
            output.status.success(),
            version == compat::harness::ORACLE_VERSION,
            "oracle version {version:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if version != compat::harness::ORACLE_VERSION {
            assert!(String::from_utf8_lossy(&output.stderr).contains("compat requires Redis"));
        }
    }
    std::fs::write(&stub, "#!/bin/sh\nexit 17\n").expect("broken oracle");
    let output = child_command().env("PATH", &directory).output().expect("run broken oracle");
    assert!(!output.status.success(), "a broken oracle silently passed");
    assert!(String::from_utf8_lossy(&output.stderr).contains("redis-server exited"));
    std::fs::remove_dir_all(directory).expect("remove fixture");
}

#[test]
fn oracle_stub_child() {
    use std::io::{Read, Write};
    let Ok(port) = std::env::var("INF_COMPAT_STUB_PORT") else { return };
    let port = port.parse::<u16>().expect("port");
    let listener = std::net::TcpListener::bind(("127.0.0.1", port)).expect("stub listener");
    let (mut stream, _) = listener.accept().expect("oracle client");
    stream.set_read_timeout(Some(std::time::Duration::from_secs(5))).expect("timeout");
    let command = b"*2\r\n$4\r\nINFO\r\n$6\r\nserver\r\n";
    let mut request = vec![0; command.len()];
    stream.read_exact(&mut request).expect("INFO");
    assert_eq!(request, command);
    let version = std::env::var("INF_COMPAT_STUB_VERSION").expect("version");
    let reply = format!("# Server\r\nredis_version:{version}\r\n");
    write!(stream, "${}\r\n{reply}\r\n", reply.len()).expect("reply");
}

#[test]
fn oracle_requirement_child() {
    if std::env::var_os("INF_COMPAT_ORACLE_CHILD").is_some() {
        let _ = compat::harness::oracle();
    }
}
