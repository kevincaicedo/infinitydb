# M2.5-S14 — DST fleet deepening: planted-bug demonstrations

date: 2026-07-07 · box: HomeLab (i7-13700KF) · kernel 7.0.0-27-generic ·
tree: feat/m2-durability-log at the S14 working commit (SUT files
`plane.rs`/`keyspace.rs` clean before and after each demo — mutations were
applied, built, run, then reverted with `git checkout`) · sim = deterministic
(virtual time + seeded entropy), so every catch below replays byte-identically
via `--seed`.

The Execution-Discipline rule this satisfies: *"an oracle without a
demonstrated planted-bug catch is decoration."* One catch per S14 addition.

---

## Addition 1 — Device-stall model · oracle: everysec ack-deferral

**Claim the oracle makes:** an `everysec` write acks on execution (never gated
on the durability watermark — `plane.rs` gates only `FsyncClass::Always`), so
its client-visible latency is bounded by scheduling, **independent of the
device**. On the instant-fsync device this is invisible; under the S14 stall
model (50–90 ms episodes) a bug that gates everysec acks balloons their
latency to the stall length.

**Planted bug (one line, both apply sites):** in `crates/inf-server/src/plane.rs`
```
-            && class == FsyncClass::Always
+            && matches!(class, FsyncClass::Always | FsyncClass::Everysec)
```
(a plausible "simplify/unify the ack gate" refactor error — now everysec acks
gate on the watermark too).

**Command:** `inf-sim --scenario m2-durable --seed 0x5EED0000 --sweep 300`
(m2-durable arms the stall device by default).

**Result — CAUGHT (new oracle red):**
```
seed 0x5eed0000: EVERYSEC DEFERRAL VIOLATION ... key "k:3:5": ack latency 32 ms exceeds 30 ms — everysec acked behind the device
seed 0x5eed0002: EVERYSEC DEFERRAL VIOLATION ... key "k:3:3": ack latency 874 ms exceeds 30 ms — everysec acked behind the device
seed 0x5eed0004: EVERYSEC DEFERRAL VIOLATION ... key "k:3:0": ack latency 921 ms exceeds 30 ms — everysec acked behind the device
```
**Why the old fleet misses it:** on the instant-fsync device the watermark
advances the same scheduler step the frame is written (0 virtual-time fsync),
so a gated everysec ack still fires with ~µs latency ≪ the 30 ms
`EVERYSEC_ACK_BOUND` — the durability audit stays green (everysec writes are
merely *more* durable). The stall model is what gives the oracle signal.

**Revert → green:** `git checkout plane.rs`; same 300 seeds →
`300 seeds, 0 violations, 2 legal taxonomy refusals`.

---

## Addition 3 — Combined scenario · oracle: L2 memory-volatility / log-scan

**Claim the oracle makes:** memory namespaces never touch the log (L2), so a
memory-namespace key written before a power cut must read **absent** after
recovery, and **no** memory-namespace record may appear in the recovered log.

**Planted bug (one line):** in `crates/inf-store/src/keyspace.rs`,
`ns_fsync_class`
```
-        if spec.mode == NsMode::Durable { spec.fsync } else { None }
+        if spec.mode == NsMode::Durable { spec.fsync } else { Some(FsyncClass::Everysec) }
```
(a "unify the write path" refactor error — memory namespaces incorrectly
acquire a durable fsync class, so their writes stage log records).

**Command:** `inf-sim --scenario m2-combined --seed 0xC0FFEE00 --sweep 200`.

**Result — CAUGHT (new oracle red):**
```
seed 0xc0ffee00: MEMORY VOLATILITY VIOLATION ... key "k:6:2": memory-namespace key survived the power cut (recovered "$10\r\nv:6:138:30\r\n") — memory state leaked into the durable path (L2)
seed 0xc0ffee01: MEMORY VOLATILITY VIOLATION ... key "k:6:1": memory-namespace key survived the power cut (recovered "$10\r\nv:6:139:13\r\n") ...
seed 0xc0ffee02: MEMORY VOLATILITY VIOLATION ... key "k:6:0": ...
```
(The independent L2 log-scan oracle also fires on the same runs; the sweep
prints the first violation per seed, which is the volatility check.)

**Why the old fleet misses it:** the pure durable scenario has memory writers
but never audits them post-recovery; the pure cache scenario has no log at
all. Neither asserts memory volatility across a cut nor scans the log for
memory-ns records.

**Revert → green:** `git checkout keyspace.rs`; same 200 seeds →
`200 seeds, 0 violations, 0 legal taxonomy refusals`.

---

## Addition 2 — Boot-storm · oracle: fsync-free ready path (pre-existing, S01)

The boot-storm oracle and its planted-bug catch shipped with M2.5-S01 and are
kept as a permanent in-suite test:
`bins/inf-sim/tests/bootstorm.rs::oracle_catches_the_synchronous_boot_path`
— it drives the pre-S01 synchronous boot path and asserts the ready-path
`sync_dir_calls` delta goes red (`≥ 4` blocking dir-fsyncs, the wedge
mechanism), then confirms the fixed path's delta is 0. **Passes** in
`cargo test -p inf-sim` (tests/bootstorm.rs, 3 tests green). S14 promotes the
scenario into the nightly fleet and threads the stall device through it; the
demonstrated catch is the existing test.

---

## Summary

| Addition | Oracle | Planted bug | Caught | Reverted-green |
|---|---|---|---|---|
| Device-stall | everysec ack-deferral | everysec gated on watermark | ✓ (32–921 ms vs 30 ms) | ✓ |
| Combined | L2 volatility / log-scan | memory ns gets durable class | ✓ (memory key survives cut) | ✓ |
| Boot-storm | fsync-free ready path | synchronous boot dir-fsync | ✓ (in-suite test, S01) | ✓ |
