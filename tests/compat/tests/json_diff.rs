//! M3-S21: byte-exact InfinityDB vs pinned RedisJSON under RESP2 and RESP3.

use compat::candidate::Candidate;
use compat::json_oracle::{
    Comparison, DEVIATIONS, JSON_CASES, Protocol, REDIS_STACK_DIGEST, REDIS_STACK_IMAGE,
    REDISJSON_MODULE_VERSION, compare,
};
use compat::resp::{RespConn, encode_command};

fn strings(argv: &[&str]) -> Vec<String> {
    argv.iter().map(|arg| (*arg).to_string()).collect()
}

fn candidate_roundtrip(candidate: &mut Candidate, argv: &[&str]) -> Vec<u8> {
    candidate.execute_wire(&encode_command(&strings(argv)))
}

fn contains(bytes: &[u8], needle: &[u8]) -> bool {
    bytes.windows(needle.len()).any(|window| window == needle)
}

#[test]
fn json_matrix_matches_pinned_redisjson() {
    let Ok(addr) = std::env::var("INF_COMPAT_JSON_ORACLE_ADDR") else {
        eprintln!(
            "SKIPPED: set INF_COMPAT_JSON_ORACLE_ADDR to {REDIS_STACK_IMAGE}@{REDIS_STACK_DIGEST}"
        );
        return;
    };

    let mut probe = RespConn::connect(&addr).expect("connect pinned RedisJSON oracle");
    let modules = probe.roundtrip(&strings(&["INFO", "MODULES"])).expect("module probe");
    assert!(contains(&modules, b"module:name=ReJSON"), "oracle has no ReJSON module");
    let version = format!("ver={REDISJSON_MODULE_VERSION}");
    assert!(
        contains(&modules, version.as_bytes()),
        "RedisJSON module version drifted: {}",
        String::from_utf8_lossy(&modules)
    );

    let mut used = vec![false; DEVIATIONS.len()];
    let mut exact = 0usize;
    let mut allowed = 0usize;
    let mut semantic_equal = 0usize;
    let mut failures = Vec::new();
    for protocol in Protocol::ALL {
        let mut oracle = RespConn::connect(&addr).expect("oracle protocol connection");
        let mut candidate = Candidate::new();
        if protocol == Protocol::Resp3 {
            oracle.roundtrip(&strings(&["HELLO", "3"])).expect("oracle HELLO 3");
            candidate_roundtrip(&mut candidate, &["HELLO", "3"]);
        }
        oracle.roundtrip(&strings(&["FLUSHALL"])).expect("oracle reset");
        candidate_roundtrip(&mut candidate, &["FLUSHALL"]);

        for case in JSON_CASES {
            let argv = strings(case.argv);
            let oracle_reply = oracle.roundtrip(&argv).expect("oracle command");
            let candidate_reply = candidate_roundtrip(&mut candidate, case.argv);
            match compare(case, protocol, &oracle_reply, &candidate_reply, DEVIATIONS) {
                Ok(Comparison::Exact) => exact += 1,
                Ok(Comparison::Allowed { deviation_index, semantic_equal: same }) => {
                    used[deviation_index] = true;
                    allowed += 1;
                    semantic_equal += usize::from(same);
                    println!(
                        "allowed {} {} semantic_equal={} — {}",
                        protocol.name(),
                        case.id,
                        same,
                        DEVIATIONS[deviation_index].justification
                    );
                }
                Err(failure) => failures.push(failure),
            }
        }
    }

    for (index, deviation) in DEVIATIONS.iter().enumerate() {
        if !used[index] {
            failures.push(format!(
                "stale allowlist entry {} {} ({})",
                deviation.protocol.name(),
                deviation.case_id,
                deviation.justification
            ));
        }
    }
    println!(
        "redisjson-diff: image={REDIS_STACK_IMAGE}@{REDIS_STACK_DIGEST} module=ReJSON/{REDISJSON_MODULE_VERSION} cases={} protocols=2 exact={exact} allowed={allowed} semantic_equal_allowed={semantic_equal} failures={}",
        JSON_CASES.len(),
        failures.len()
    );
    assert!(
        failures.is_empty(),
        "{} RedisJSON mismatches:\n{}",
        failures.len(),
        failures.join("\n")
    );
}
