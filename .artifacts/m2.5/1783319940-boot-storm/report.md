# boot-storm (M2.5-S01)

- date: 1783319940 (unix)
- kernel: 7.0.0-27-generic
- infinityd: target/release/infinityd
- cycles: 500 · cells: 4 · pressure: 2048 MiB/cycle · ready bound: 10s · pin-start: 4
- data-root: /home/kcaicedo/.cache/inf-bootstorm (must not be tmpfs)

| metric | value |
|---|---|
| wedges (gate: 0) | 14 |
| retries consumed (by design) | 0 |
| time-to-all-ready p50 | 21 ms |
| time-to-all-ready p99 | 40 ms |
| time-to-all-ready max | 61 ms |

## wedges

cycle 9: node stayed -LOADING past 10s (stderr: infinityd-38385.stderr)
cycle 26: node stayed -LOADING past 10s (stderr: infinityd-46707.stderr)
cycle 56: node stayed -LOADING past 10s (stderr: infinityd-45953.stderr)
cycle 134: node stayed -LOADING past 10s (stderr: infinityd-37339.stderr)
cycle 199: node stayed -LOADING past 10s (stderr: infinityd-37459.stderr)
cycle 218: node stayed -LOADING past 10s (stderr: infinityd-43825.stderr)
cycle 232: node stayed -LOADING past 10s (stderr: infinityd-41019.stderr)
cycle 260: node stayed -LOADING past 10s (stderr: infinityd-33291.stderr)
cycle 319: node stayed -LOADING past 10s (stderr: infinityd-33361.stderr)
cycle 355: node stayed -LOADING past 10s (stderr: infinityd-41763.stderr)
cycle 400: node stayed -LOADING past 10s (stderr: infinityd-34633.stderr)
cycle 426: node stayed -LOADING past 10s (stderr: infinityd-46759.stderr)
cycle 468: node stayed -LOADING past 10s (stderr: infinityd-36835.stderr)
cycle 479: node stayed -LOADING past 10s (stderr: infinityd-46499.stderr)
