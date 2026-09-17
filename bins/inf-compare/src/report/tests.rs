use super::*;

#[test]
fn durability_is_reported_per_engine_and_attached_servers_are_unverified() {
    let env = Env {
        kernel: "test".into(),
        cores: 1,
        governor: "unknown".into(),
        epp: "unknown".into(),
        git_sha: "test".into(),
        git_dirty: true,
        memtier_version: "test".into(),
        redisbench_version: "test".into(),
        envcheck: None,
        tier: "DEV-TIER".into(),
        binding: false,
        reasons: vec![],
    };
    let params = Params {
        stamp_secs: 0,
        mode: "host + attach".into(),
        generators: "memtier".into(),
        duration: 1,
        threads: 1,
        clients: 1,
        data_size: 16,
        keyspace: 10,
        pipelines: vec![1],
        fill_secs: 1,
        rb_requests: 10,
        crosscheck_pct: 25.0,
        maxmemory_mb: None,
        rate: None,
        data_root: None,
        device_stat: None,
        redis_no_auto_rewrite: false,
    };
    let engines = [
        EngineConfig {
            label: "redis",
            version: "test".into(),
            mode: "host",
            durability: Some(Durability::Everysec),
            launch_cmd: "redis-server --appendonly yes --appendfsync everysec".into(),
            peak_rss_mib: None,
        },
        EngineConfig {
            label: "dragonfly",
            version: "test".into(),
            mode: "attach",
            durability: None,
            launch_cmd: "attached".into(),
            peak_rss_mib: None,
        },
    ];
    let report = render(&env, &params, &engines, &[], &[]);
    assert!(report.contains("| Engine | Mode | Version | Durability |"), "{report}");
    assert!(report.contains("| redis | host | test | everysec |"), "{report}");
    assert!(report.contains("| dragonfly | attach | test | unverified (attached) |"), "{report}");
    assert!(!report.contains("durability=everysec"), "{report}");
    assert!(!report.contains("infinitydb ran"), "{report}");
}
