# boot-storm (M2.5-S01)

- date: 1783360302 (unix)
- kernel: 7.0.0-27-generic
- infinityd: target/release/infinityd
- cycles: 500 · cells: 4 · pressure: 2048 MiB/cycle · ready bound: 10s · pin-start: 4
- data-root: /home/kcaicedo/.cache/inf-bootstorm (must not be tmpfs)

| metric | value |
|---|---|
| wedges (gate: 0) | 0 |
| named fail-stop exits (ADR-0026 D3 Phase-H item; informational under pressure) | 35 |
| retries consumed (by design) | 0 |
| time-to-all-ready p50 | 21 ms |
| time-to-all-ready p99 | 35 ms |
| time-to-all-ready max | 290 ms |

## named fail-stop exits

cycle 0: server fail-stopped during setup (exit status: 1; stderr: infinityd-46349.stderr)
cycle 3: server fail-stopped during setup (exit status: 1; stderr: infinityd-38077.stderr)
cycle 13: server fail-stopped during setup (exit status: 1; stderr: infinityd-36419.stderr)
cycle 20: server fail-stopped during setup (exit status: 1; stderr: infinityd-38707.stderr)
cycle 23: server fail-stopped during setup (exit status: 1; stderr: infinityd-44515.stderr)
cycle 24: server fail-stopped during setup (exit status: 1; stderr: infinityd-33997.stderr)
cycle 34: server fail-stopped during setup (exit status: 1; stderr: infinityd-39751.stderr)
cycle 78: server fail-stopped during setup (exit status: 1; stderr: infinityd-46851.stderr)
cycle 85: server fail-stopped during setup (exit status: 1; stderr: infinityd-46567.stderr)
cycle 90: server fail-stopped during setup (exit status: 1; stderr: infinityd-46679.stderr)
cycle 172: server fail-stopped during setup (exit status: 1; stderr: infinityd-35121.stderr)
cycle 174: server fail-stopped during setup (exit status: 1; stderr: infinityd-34241.stderr)
cycle 207: server fail-stopped during setup (exit status: 1; stderr: infinityd-38429.stderr)
cycle 231: server fail-stopped during setup (exit status: 1; stderr: infinityd-42693.stderr)
cycle 241: server fail-stopped during setup (exit status: 1; stderr: infinityd-33051.stderr)
cycle 265: server fail-stopped during setup (exit status: 1; stderr: infinityd-41779.stderr)
cycle 298: server fail-stopped during setup (exit status: 1; stderr: infinityd-34463.stderr)
cycle 316: server fail-stopped during setup (exit status: 1; stderr: infinityd-39951.stderr)
cycle 318: server fail-stopped during setup (exit status: 1; stderr: infinityd-43323.stderr)
cycle 320: server fail-stopped during setup (exit status: 1; stderr: infinityd-37225.stderr)
cycle 327: server fail-stopped during setup (exit status: 1; stderr: infinityd-36903.stderr)
cycle 331: server fail-stopped during setup (exit status: 1; stderr: infinityd-43897.stderr)
cycle 341: server fail-stopped during setup (exit status: 1; stderr: infinityd-46545.stderr)
cycle 351: server fail-stopped during setup (exit status: 1; stderr: infinityd-44357.stderr)
cycle 355: server fail-stopped during setup (exit status: 1; stderr: infinityd-43499.stderr)
cycle 356: server fail-stopped during setup (exit status: 1; stderr: infinityd-46683.stderr)
cycle 359: server fail-stopped during setup (exit status: 1; stderr: infinityd-32807.stderr)
cycle 377: server fail-stopped during setup (exit status: 1; stderr: infinityd-33541.stderr)
cycle 400: server fail-stopped during setup (exit status: 1; stderr: infinityd-44033.stderr)
cycle 404: server fail-stopped during setup (exit status: 1; stderr: infinityd-42723.stderr)
cycle 431: server fail-stopped during setup (exit status: 1; stderr: infinityd-38633.stderr)
cycle 485: server fail-stopped during setup (exit status: 1; stderr: infinityd-46515.stderr)
cycle 486: server fail-stopped during setup (exit status: 1; stderr: infinityd-39247.stderr)
cycle 492: server fail-stopped during setup (exit status: 1; stderr: infinityd-39071.stderr)
cycle 498: server fail-stopped during setup (exit status: 1; stderr: infinityd-36479.stderr)
