# boot-storm (M2.5-S01)

- date: 1783320649 (unix)
- kernel: 7.0.0-27-generic
- infinityd: target/release/infinityd
- cycles: 200 · cells: 4 · pressure: 2048 MiB/cycle · ready bound: 60s · pin-start: 4
- data-root: /home/kcaicedo/.cache/inf-bootstorm (must not be tmpfs)

| metric | value |
|---|---|
| wedges (gate: 0) | 15 |
| retries consumed (by design) | 0 |
| time-to-all-ready p50 | 21 ms |
| time-to-all-ready p99 | 42 ms |
| time-to-all-ready max | 120 ms |

## wedges

cycle 0: spawn/listen wedge: /home/kcaicedo/Documents/Projects/databases/infinitydb/.artifacts/m2.5/boot-storm-diag-stderr/infinityd-40395.stderr: No such file or directory (os error 2)
cycle 1: spawn/listen wedge: /home/kcaicedo/Documents/Projects/databases/infinitydb/.artifacts/m2.5/boot-storm-diag-stderr/infinityd-41051.stderr: No such file or directory (os error 2)
cycle 2: spawn/listen wedge: /home/kcaicedo/Documents/Projects/databases/infinitydb/.artifacts/m2.5/boot-storm-diag-stderr/infinityd-41865.stderr: No such file or directory (os error 2)
cycle 3: spawn/listen wedge: /home/kcaicedo/Documents/Projects/databases/infinitydb/.artifacts/m2.5/boot-storm-diag-stderr/infinityd-42667.stderr: No such file or directory (os error 2)
cycle 4: spawn/listen wedge: /home/kcaicedo/Documents/Projects/databases/infinitydb/.artifacts/m2.5/boot-storm-diag-stderr/infinityd-42343.stderr: No such file or directory (os error 2)
cycle 5: spawn/listen wedge: /home/kcaicedo/Documents/Projects/databases/infinitydb/.artifacts/m2.5/boot-storm-diag-stderr/infinityd-37489.stderr: No such file or directory (os error 2)
cycle 6: spawn/listen wedge: /home/kcaicedo/Documents/Projects/databases/infinitydb/.artifacts/m2.5/boot-storm-diag-stderr/infinityd-44285.stderr: No such file or directory (os error 2)
cycle 7: spawn/listen wedge: /home/kcaicedo/Documents/Projects/databases/infinitydb/.artifacts/m2.5/boot-storm-diag-stderr/infinityd-36683.stderr: No such file or directory (os error 2)
cycle 8: spawn/listen wedge: /home/kcaicedo/Documents/Projects/databases/infinitydb/.artifacts/m2.5/boot-storm-diag-stderr/infinityd-35835.stderr: No such file or directory (os error 2)
cycle 29: node stayed -LOADING past 60s (stderr: infinityd-39595.stderr)
cycle 31: node stayed -LOADING past 60s (stderr: infinityd-37819.stderr)
cycle 100: node stayed -LOADING past 60s (stderr: infinityd-39125.stderr)
cycle 106: node stayed -LOADING past 60s (stderr: infinityd-41413.stderr)
cycle 150: node stayed -LOADING past 60s (stderr: infinityd-43453.stderr)
cycle 191: node stayed -LOADING past 60s (stderr: infinityd-44393.stderr)
