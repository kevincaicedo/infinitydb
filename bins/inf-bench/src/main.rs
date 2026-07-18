//! `inf-bench` — InfinityDB benchmark harness (M0).
//!
//! Subcommands:
//! - `env-check` — benchmark environment validation (M0-S03): dirty tree,
//!   cpufreq governor/EPP, thermal throttling, macOS power state.
//! - `load` — native pipelined RESP load generator (M0-S18 harness core).
//! - `gate-run m0` — replicate runner + gate-report generator against
//!   `docs/milestones/m0-gates.toml` (M0-S18/S19 scaffold).
//! - `zipfian` — zipfian LFU hit-rate parity vs Redis (M1 `hit_rate_parity`).
//! - `doc-corpus` — seeded M3 JSON reference-corpus generator (M3-S20).
//!
//! Tooling tier: `std::thread` and blocking sockets are fine here; this
//! binary never runs on the data plane. It deliberately does not depend on
//! `inf-wire` — the measurement tool shares no code with the system under
//! test (client-side RESP lives in [`resp`]).
#![forbid(unsafe_code)]

mod bootstorm;
mod cli;
mod doc_corpus;
mod envcheck;
mod gaterun;
mod gates;
mod load;
mod m1rows;
mod m2rows;
mod m4rows;
mod resp;
mod zipfian;

use std::process::ExitCode;

const USAGE: &str = "\
inf-bench — InfinityDB benchmark harness (M0)

USAGE:
    inf-bench env-check [--allow-dirty]
    inf-bench load --host H --port P [--threads N] [--conns-per-thread N] [--pipeline P]
                   [--duration SECS] [--mix SET:GET] [--keys N] [--key-prefix S]
                   [--value-size BYTES] [--seed N] [--out FILE.toml]
    inf-bench gate-run m0|m1|m2|m4 [--replicates N] [--gates FILE] [--artifacts-root DIR]
                   [--allow-dirty] [--unsafe-env] [--reference-box] [--skip-fill]
                   [--cells N] [--duration SECS] [--fill-keys N]
                   [--infinityd-bin PATH] [--redis-bin PATH]
                   m1 rows: [--storm-keys N] [--flushall-keys N] [--maxmemory-mb N]
                            [--subs N] [--sub-channels N]
                   m2 rows: [--baseline-bin PATH]  (pre-M2 infinityd for the
                            zero-cost A/B; delta rows PENDING without it)
                   m4 rows: [--baseline-bin PATH]  (M3-tip infinityd for the
                            degenerate-case A/B — M4-S03 hard sub-gate;
                            tiering-counter tripwire binds on every box)
    inf-bench boot-storm --infinityd-bin PATH [--cycles 500] [--cells 4]
                   [--pressure-mb 2048] [--data-root DIR] [--ready-timeout-s 10]
                   [--pin-start N] [--artifacts-root DIR]
                   (M2.5-S01 wedge regression; data-root must not be tmpfs)
    inf-bench zipfian [--keyspace N] [--ops N] [--warmup N] [--theta F] [--seed N]
                   [--maxmemory-mb N] [--value-size BYTES] [--cells N] [--window N]
                   [--threshold-pp F] [--infinityd-bin PATH] [--redis-bin PATH]
                   [--artifacts-root DIR] [--reference-box]
    inf-bench doc-corpus --seed N [--out DIR]
                   [--pipe FILE --counts shape=N[,shape=N...]]
                   (--pipe emits a RESP JSON.SET load file with per-index
                    unique documents — corpus v2, ADR-0046 D3; the RSS
                    gate binds on this form)

See bins/inf-bench/README.md for what runs on macOS vs what is Linux-pending.";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Some(cmd) = args.first() else {
        eprintln!("{USAGE}");
        return ExitCode::from(2);
    };
    let rest = &args[1..];
    let outcome = match cmd.as_str() {
        "env-check" => envcheck::cmd_env_check(rest),
        "load" => load::cmd_load(rest),
        "gate-run" => gaterun::cmd_gate_run(rest),
        "boot-storm" => bootstorm::cmd_boot_storm(rest),
        "zipfian" => zipfian::cmd_zipfian(rest),
        "doc-corpus" => doc_corpus::cmd_doc_corpus(rest),
        "help" | "--help" | "-h" => {
            println!("{USAGE}");
            Ok(())
        }
        other => Err(format!("unknown subcommand `{other}`\n\n{USAGE}")),
    };
    match outcome {
        Ok(()) => ExitCode::SUCCESS,
        Err(msg) => {
            eprintln!("inf-bench: {msg}");
            ExitCode::FAILURE
        }
    }
}
