//! Exercise the shipping snapshot producer through the benchmark's real scraper.
#![cfg(any(target_os = "linux", target_os = "macos"))]
#![allow(dead_code)]
#![allow(clippy::disallowed_methods, reason = "native process harness uses wall-clock deadlines")]

#[path = "../../inf-bench/src/loop_histogram.rs"]
mod loop_histogram;
#[path = "../../inf-bench/src/gaterun/loop_scrape.rs"]
mod loop_scrape;
#[path = "../../inf-bench/src/resp.rs"]
mod resp;

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

struct Server {
    child: Child,
    root: PathBuf,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn spawn() -> (Server, u16) {
    let port = std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port();
    let root =
        std::env::temp_dir().join(format!("inf-loop-snapshot-{}-{port}", std::process::id()));
    std::fs::create_dir(&root).unwrap();
    let stderr = root.join("stderr");
    let child = Command::new(env!("CARGO_BIN_EXE_infinityd"))
        .args(["--cells", "4", "--port", &port.to_string(), "--device-probe", "off"])
        .arg("--data-dir")
        .arg(&root)
        .stdout(Stdio::null())
        .stderr(std::fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let mut server = Server { child, root };
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        assert!(
            server.child.try_wait().unwrap().is_none(),
            "{}",
            std::fs::read_to_string(&stderr).unwrap()
        );
        if let Ok(mut stream) = resp::connect("127.0.0.1", port) {
            let info =
                resp::parse_info(&resp::request(&mut stream, &[b"INFO", b"server"]).unwrap());
            assert_eq!(info["process_id"], server.child.id().to_string());
            return (server, port);
        }
        assert!(Instant::now() < deadline, "server never became ready");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn native_four_cell_windows_have_fresh_coherent_buckets_and_counters() {
    let (_server, port) = spawn();
    let mut client = resp::connect("127.0.0.1", port).unwrap();
    let info = resp::request(&mut client, &[b"INFO"]).unwrap();
    assert!(!String::from_utf8_lossy(&info).contains("loop_histogram_"));
    let mut scraper = loop_scrape::LoopScraper::connect(port, 4).unwrap();
    for _ in 0..2 {
        let before = scraper.fresh().unwrap();
        std::thread::scope(|scope| {
            for _ in 0..8 {
                scope.spawn(move || {
                    let mut client = resp::connect("127.0.0.1", port).unwrap();
                    for _ in 0..128 {
                        assert_eq!(resp::request(&mut client, &[b"PING"]).unwrap(), b"+PONG\r\n");
                    }
                });
            }
        });
        let after = scraper.fresh().unwrap();
        let window = loop_histogram::LoadWindow::between(&before, &after).unwrap();
        assert_eq!(window.cells.len(), 4);
        assert!(window.cells.iter().all(|cell| cell.samples > 0));
        assert!(window.sqes_per_submit.is_finite());
        assert!(window.sqes_per_submit > 0.0);
    }
}
