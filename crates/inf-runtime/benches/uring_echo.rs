//! M0-S04 AC: loopback echo microbench on `UringDriver`.
//!
//! Gate: ≥ 1M echo frames/s (64 B frames) on ONE pinned server core with
//! `sqes_per_submit ≥ 16` under pipelined load — the anti-Vortex check
//! (batch=1.0, 75% syscall CPU) measured on the real backend.
//!
//! Custom harness rather than criterion (recorded deviation): the metric is
//! saturated steady-state throughput plus syscall-amortization ratios across
//! load-generator threads, not a nanosecond-scale closure latency.
//!
//! Run: `cargo bench -p inf-runtime --features uring --bench uring_echo`
//! Env: `INF_ECHO_SECS` (measure window, default 10) · `INF_ECHO_CONNS`
//! (default 16) · `INF_ECHO_RELAX=1` (report without asserting).

#[cfg(not(all(target_os = "linux", feature = "uring")))]
fn main() {
    eprintln!("uring_echo requires Linux and --features uring");
}

#[cfg(all(target_os = "linux", feature = "uring"))]
fn main() {
    imp::run();
}

#[cfg(all(target_os = "linux", feature = "uring"))]
mod imp {
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::os::fd::IntoRawFd;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    use inf_alloc::BufferPool;
    use inf_runtime::{
        BackendDriver, CompletionResult, CompletionToken, IoOp, TokenClass, UringDriver, Wait,
    };

    const FRAME: u64 = 64;
    const BUF_SIZE: usize = 4096;
    /// 64 KiB pipelined window per connection: enough in-flight bytes that
    /// the server's CQ always has a batch waiting (the AC's "pipelined load").
    const WINDOW: usize = 64 * 1024;
    const SERVER_CORE: usize = 4;

    fn pin_to(core: usize) {
        // SAFETY: plain sched_setaffinity on self; failure just unpins.
        unsafe {
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(core, &mut set);
            libc::sched_setaffinity(0, size_of::<libc::cpu_set_t>(), &raw const set);
        }
    }

    fn env_u64(name: &str, default: u64) -> u64 {
        std::env::var(name).ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    }

    /// Echo replies are sub-MSS segments; without NODELAY the server-side
    /// Nagle timer + client delayed ACK serialize each window at ~40 ms.
    fn set_nodelay(fd: i32) {
        let one: libc::c_int = 1;
        // SAFETY: setsockopt with a valid int pointer on a live socket.
        unsafe {
            libc::setsockopt(
                fd,
                libc::IPPROTO_TCP,
                libc::TCP_NODELAY,
                (&raw const one).cast(),
                size_of::<libc::c_int>() as libc::socklen_t,
            );
        }
    }

    pub fn run() {
        let secs = env_u64("INF_ECHO_SECS", 10);
        let conns = env_u64("INF_ECHO_CONNS", 16) as usize;
        let relax = std::env::var_os("INF_ECHO_RELAX").is_some();

        let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let stop = Arc::new(AtomicBool::new(false));

        // Load generators: write a full window, read it back, repeat —
        // pinned away from the server core (E-core side of the box is fine;
        // the gate measures the SERVER core).
        for i in 0..conns {
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                pin_to(8 + (i % 14));
                let mut s = TcpStream::connect(("127.0.0.1", port)).expect("connect");
                s.set_nodelay(true).expect("nodelay");
                let tx = vec![0xC5u8; WINDOW];
                let mut rx = vec![0u8; WINDOW];
                while !stop.load(Ordering::Relaxed) {
                    if s.write_all(&tx).is_err() || s.read_exact(&mut rx).is_err() {
                        break;
                    }
                }
            });
        }

        pin_to(SERVER_CORE);
        let mut driver = UringDriver::new(1024).expect("uring");
        let mut pool = BufferPool::new(512, BUF_SIZE);
        driver.register_pool(&mut pool).expect("register");
        println!("capabilities: {:?}", driver.capabilities());

        let lfd = listener.into_raw_fd();
        driver.push(IoOp::AcceptArm {
            listener: lfd,
            token: CompletionToken::new(TokenClass::Accept, 1, 0),
        });

        let warmup = Duration::from_secs(1);
        let measure = Duration::from_secs(secs);
        let started = Instant::now();
        let mut measuring = false;
        let mut window_start = started;
        let (mut bytes, mut syscalls, mut sqes, mut cqes, mut iters) =
            (0u64, 0u64, 0u64, 0u64, 0u64);
        let (mut drops, mut errs) = (0u64, 0u64);
        let mut out = Vec::with_capacity(1024);
        let mut wait = Wait::Poll;

        loop {
            let now = Instant::now();
            if !measuring && now.duration_since(started) >= warmup {
                measuring = true;
                window_start = now;
                (bytes, syscalls, sqes, cqes, iters, drops, errs) = (0, 0, 0, 0, 0, 0, 0);
            }
            if measuring && now.duration_since(window_start) >= measure {
                break;
            }

            out.clear();
            let produced = driver.submit_and_reap(&mut pool, wait, &mut out).expect("reap");
            let st = driver.submit_stats();
            syscalls += st.syscalls;
            sqes += st.sqes;
            cqes += produced as u64;
            iters += 1;

            for c in &out {
                match c.result {
                    CompletionResult::Accepted { fd } => {
                        set_nodelay(fd);
                        // Token slot carries the fd so Recv completions can
                        // route the echo without a side table.
                        let slot = u32::try_from(fd).expect("fd fits token slot");
                        driver.push(IoOp::RecvArm {
                            fd,
                            token: CompletionToken::new(TokenClass::Recv, slot, 0),
                        });
                    }
                    CompletionResult::Recv { buf, len } => {
                        let fd = c.token.slot() as i32;
                        if len == 0 {
                            pool.release(buf);
                            driver.push(IoOp::Close {
                                fd,
                                token: CompletionToken::new(TokenClass::Close, c.token.slot(), 0),
                            });
                        } else {
                            bytes += u64::from(len);
                            // Echo from the received buffer itself — it is
                            // consumer-owned now; `Sent` returns it.
                            driver.push(IoOp::Send { fd, buf, len, token: c.token });
                        }
                    }
                    CompletionResult::Sent { buf } => pool.release(buf),
                    CompletionResult::RecvDropped => drops += 1,
                    CompletionResult::Closed => {}
                    CompletionResult::Error { buf, .. } => {
                        errs += 1;
                        if let Some(buf) = buf {
                            pool.release(buf);
                        }
                    }
                }
            }
            // Reactor idle discipline: spin while work flows, park briefly
            // when a reap comes back empty.
            wait = if produced == 0 {
                Wait::Park { timeout: Some(Duration::from_micros(200)) }
            } else {
                Wait::Poll
            };
        }

        stop.store(true, Ordering::Relaxed);
        let elapsed = window_start.elapsed().as_secs_f64();
        let frames = bytes / FRAME;
        let frames_per_sec = frames as f64 / elapsed;
        let sqes_per_submit = sqes as f64 / syscalls.max(1) as f64;
        let cqes_per_reap = cqes as f64 / iters.max(1) as f64;

        println!("--- uring_echo ({conns} conns, {WINDOW} B windows, {secs}s window) ---");
        println!("bytes           {bytes}");
        println!("frames(64B)     {frames}");
        println!("frames/s/core   {frames_per_sec:.0}");
        println!("throughput      {:.2} GiB/s", bytes as f64 / elapsed / (1 << 30) as f64);
        println!("iterations      {iters}");
        println!("syscalls        {syscalls}");
        println!("sqes            {sqes}");
        println!("sqes_per_submit {sqes_per_submit:.1}");
        println!("cqes_per_reap   {cqes_per_reap:.1}");
        println!("recv_dropped    {drops}");
        println!("errors          {errs}");

        if relax {
            println!("RELAXED RUN: gate asserts skipped (INF_ECHO_RELAX)");
            std::process::exit(0);
        }
        assert!(
            sqes_per_submit >= 16.0,
            "GATE FAIL: sqes_per_submit {sqes_per_submit:.1} < 16 — submission batching broken"
        );
        assert!(frames_per_sec >= 1_000_000.0, "GATE FAIL: {frames_per_sec:.0} frames/s/core < 1M");
        println!("GATES PASS: ≥1M frames/s/core, sqes_per_submit ≥ 16");
        std::process::exit(0);
    }
}
