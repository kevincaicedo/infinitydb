# boot-storm (M2.5-S01)

- date: 1783319922 (unix)
- kernel: 7.0.0-27-generic
- infinityd: target/release/infinityd
- cycles: 10 · cells: 4 · pressure: 256 MiB/cycle · ready bound: 10s · pin-start: 4
- data-root: /home/kcaicedo/.cache/inf-bootstorm (must not be tmpfs)

| metric | value |
|---|---|
| wedges (gate: 0) | 0 |
| retries consumed (by design) | 0 |
| time-to-all-ready p50 | 17 ms |
| time-to-all-ready p99 | 21 ms |
| time-to-all-ready max | 21 ms |
