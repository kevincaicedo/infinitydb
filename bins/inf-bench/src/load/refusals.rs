use super::*;
use std::net::TcpListener;

fn serve_replies(replies: &[&[u8]]) -> LoadReport {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    std::thread::scope(|scope| {
        scope.spawn(|| {
            let (mut stream, _) = listener.accept().unwrap();
            stream.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
            let mut pending = Vec::new();
            let mut chunk = [0; 4096];
            for reply in replies {
                loop {
                    if let Some(end) = reply_len(&pending) {
                        pending.drain(..end);
                        break;
                    }
                    let count = stream.read(&mut chunk).unwrap();
                    assert_ne!(count, 0);
                    pending.extend_from_slice(&chunk[..count]);
                }
                stream.write_all(reply).unwrap();
            }
        });
        run(&LoadSpec {
            port,
            conns: 1,
            pipeline: 2,
            fill: Some(replies.len() as u64),
            ..Default::default()
        })
        .unwrap()
    })
}

#[test]
fn all_refusals_have_zero_successful_throughput() {
    for error in [b"-ERR nope\r\n".as_slice(), b"-OOM full\r\n", b"-BUSY retry\r\n"] {
        let report = serve_replies(&[error; 8]);
        assert_eq!(report.errors, 8);
        assert_eq!(report.ops, 0, "refusals are not served operations");
        assert_eq!(report.ops_per_sec, 0.0);
        assert_eq!(report.p999_us, 0, "no successful latency samples");
        assert!(report.require_no_errors().is_err());
    }
}

#[test]
fn mixed_replies_count_only_successes() {
    let report = serve_replies(&[b"+OK\r\n", b"-ERR nope\r\n", b"$-1\r\n", b"-BUSY retry\r\n"]);
    assert_eq!(report.ops, 2);
    assert_eq!(report.errors, 2);
    assert_eq!(report.busy_retryable, 1);
    assert_eq!(report.nils, 1);
    assert_eq!(report.ops_per_sec, 2.0 / report.elapsed_s);
}

#[test]
fn healthy_replies_retain_their_throughput() {
    let report = serve_replies(&[b"+OK\r\n".as_slice(); 8]);
    assert_eq!(report.ops, 8);
    assert_eq!(report.errors, 0);
    assert_eq!(report.ops_per_sec, 8.0 / report.elapsed_s);
    assert!(report.require_no_errors().is_ok());
}

fn empty_result() -> ConnResult {
    ConnResult {
        ops: 0,
        errors: 0,
        warmup_errors: 0,
        busy: 0,
        nils: 0,
        error_samples: Vec::new(),
        hist_us: FineHistogram::new(),
        error_hist_us: FineHistogram::new(),
        max_us: 0,
        max_intended_at_s: 0.0,
        max_sent_at_s: 0.0,
        max_done_at_s: 0.0,
        max_per_second: vec![0; 2],
        sent: 0,
        skipped_pipeline_full: 0,
    }
}

#[test]
fn successful_and_error_latencies_have_distinct_populations() {
    let mut result = empty_result();
    let start = Instant::now();
    let at = SentAt { intended: start, sent: start };
    result.record_reply(b"+OK\r\n", at, start, start + Duration::from_micros(10));
    result.record_reply(b"-BUSY retry\r\n", at, start, start + Duration::from_micros(1000));
    assert_eq!(result.hist_us.percentile(99.9), 10);
    assert_eq!(result.error_hist_us.percentile(99.9), 1000);
    assert_eq!(result.max_us, 10);
    assert_eq!((result.ops, result.errors, result.busy), (1, 1, 1));
}

#[test]
fn warmup_errors_cannot_certify_a_gate_or_pollute_measured_statistics() {
    let mut result = empty_result();
    let start = Instant::now();
    let window = start + Duration::from_secs(1);
    let at = SentAt { intended: start, sent: start };
    result.record_reply(b"-ERR warmup\r\n", at, window, window);
    assert_eq!((result.ops, result.errors, result.warmup_errors), (0, 0, 1));
    assert_eq!(result.hist_us.max(), 0);
    assert_eq!(result.error_hist_us.max(), 0);
    let report = LoadReport { warmup_errors: 1, ..Default::default() };
    assert!(report.require_no_errors().unwrap_err().contains("1 warmup errors"));
}
