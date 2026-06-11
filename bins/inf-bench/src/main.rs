//! `inf-bench` — InfinityDB benchmark harness (M0).
//!
//! Subcommands:
//! - `env-check` — benchmark environment validation (M0-S03): dirty tree,
//!   cpufreq governor/EPP, thermal throttling, macOS power state.
//! - `load` — native pipelined RESP load generator (M0-S18 harness core).
//! - `gate-run m0` — replicate runner + gate-report generator against
//!   `docs/milestones/m0-gates.toml` (M0-S18/S19 scaffold).
//!
//! Tooling tier: `std::thread` and blocking sockets are fine here; this
//! binary never runs on the data plane. It deliberately does not depend on
//! `inf-wire` — the measurement tool shares no code with the system under
//! test (client-side RESP lives in [`resp`]).
#![forbid(unsafe_code)]

mod cli;
mod envcheck;

use std::process::ExitCode;

const USAGE: &str = "\
inf-bench — InfinityDB benchmark harness (M0)

USAGE:
    inf-bench env-check [--allow-dirty]
    inf-bench load --host H --port P [--threads N] [--conns-per-thread N] [--pipeline P]
                   [--duration SECS] [--mix SET:GET] [--keys N] [--key-prefix S]
                   [--value-size BYTES] [--seed N] [--out FILE.toml]
    inf-bench gate-run m0 (--target HOST:PORT | --ab A_HOST:PORT,B_HOST:PORT)
                   [--replicates N] [--gates FILE] [--artifacts-root DIR] [--allow-dirty]
                   [load flags: --threads --conns-per-thread --pipeline --duration
                    --mix --keys --key-prefix --value-size --seed]

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
        // M0-S18 (harness core + gate runner) is in progress; the subcommands
        // are reserved so scripts written against the final surface fail with
        // a clear message instead of "unknown subcommand".
        "load" | "gate-run" => {
            Err(format!("`{cmd}` is not built yet (M0-S18 in progress); only env-check ships"))
        }
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
