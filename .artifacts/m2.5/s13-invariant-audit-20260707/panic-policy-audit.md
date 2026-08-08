# M2.5-S13 — Panic-policy audit (durable / cell-resident paths)

Policy audited against INFINITY_STYLE §Safety → **Panics and errors**:
*"Panics are for violated internal invariants only. Input validation,
protocol errors, I/O failures, allocation pressure, disk-full, and every
other operating condition returns a typed error. ... `unwrap()`/`expect()`
on an operational `Result` is a review reject; `expect()` with an invariant
justification is an assertion and is judged as one."* Plus §8.4: fsync /
durability-metadata failure is **fail-stop** — a sanctioned, deliberate
crash, not a policy violation.

Scope: non-test code in the durable path and the cell-resident gate:
`inf-server/src/{durable,ckpt,recover,control}.rs`,
`inf-log/src/{commit,staging}.rs`, `inf-runtime/src/gate.rs`.

Classification: **VII** = violated-internal-invariant (policy-conformant) ·
**FS** = deliberate durability fail-stop (§8.4, conformant) ·
**IO/INPUT/ALLOC** = operating-condition panic (policy **violation** — a
finding).

---

## Findings first

**Zero policy violations.** No `unwrap()`/`expect()`/`panic!`/indexing on
the durable or cell path panics on an input-, I/O-, or allocation-controlled
value that should be a typed error. Naked `.unwrap()` count in these files
outside `#[cfg(test)]`: **0** (verified by grep — every fallible call uses
`expect("<invariant justification>")` or a typed `Result`). The path is
already policy-clean; the work here is *locking that in* with a CI grep,
because **no panic-policy grep exists today** (see the proposal below).

Two hits deserve an explicit note because a naive grep would flag them:

1. **`control.rs:525` `panic!("catalog META swap failed (fail-stop): {err}")`**
   — this panics on an **I/O error** (`write_meta` failed). It is *not* a
   violation: the catalog META file is durability metadata, a DDL was acked
   against the swap, and losing it after `+OK` is a §8.2 violation. The code
   comment frames it as the §8.4 fsync-failure rule class. **Conformant
   (FS).** Consistency observation: the cell durable path fail-stops via
   `process::exit(EXIT_DURABLE_FAILSTOP)` (durable.rs:392) while the control
   thread uses `panic!`. Both terminate; the control thread's panic unwinds
   only its own thread, but the cell's in-order join then fail-stops the
   process. Consider routing control-thread durability failures through the
   same `EXIT_DURABLE_FAILSTOP` for a uniform exit code (minor; out of S13
   scope, note for the persistence workflow).

2. **`control.rs:326` `.expect("control thread alive (fail-stop)")`** — send
   on the control channel; a dead control thread is fatal (§8.4). Justified
   `expect` = assertion. **Conformant (VII/FS).**

---

## Full hit classification

### `inf-server/src/durable.rs`
| Line | Site | Class | Note |
|---|---|---|---|
| 227 | `assert!(!self.failed, "staging into a failed durable cell")` | VII | release assert (per-op, single bool) — keep |
| 228 | `.stage(...).expect("admission pre-checked by would_fit")` | VII | `would_fit` is the precondition |
| 292, 308, 322, 365 | `.expect("std segment tier has fds")` | VII | reactor tier guarantees real fds; MemFs branches are `if let Some(fd)`-guarded |
| 331 | `.expect("LogWritten with no in-flight lease")` | VII | driver exactly-once |
| 430 | `.stage(...).expect("admission pre-checked")` | VII | precondition checked line 429 |
| 458, 478 | `unreachable!("just opened" / "matched above")` | VII | control-flow invariant |
| 459, 620 | `.expect("header staged by open_stream" / "CkptWrite with no in-flight lease")` | VII | phase invariant |
| 618, 628 | `panic!("CkptWrite/CkptSync completion with no checkpoint streaming")` | VII | driver completion for a non-streaming phase = internal bug |

### `inf-server/src/ckpt.rs`
| Line | Site | Class | Note |
|---|---|---|---|
| 220, 445 | `debug_assert!` (publish / one-transition) | VII | → **promote** (see promotions P1/P5) |
| 472, 493 | `panic!("ManifestSync completion/error with no barrier in flight")` | VII | driver exactly-once vs phase |
| 537, 586, 621 | `.expect("std tier has fds")` | VII | reactor tier invariant |

### `inf-server/src/recover.rs`
| Line | Site | Class | Note |
|---|---|---|---|
| 328, 356, 444, 471, 658, 662, 715, 716 | `.expect(...)` (fs/scan/manifest present, non-empty scan) | VII | machine-lifecycle invariants |
| 592 | `unreachable!("Io read errors fail-stop above")` | VII | Io matched + returned above |
| 396–437, 668–675 (branches) | typed `io::Error` fail-stops | *(not a panic)* | correct: on-disk corruption is an operating condition → typed error (§Panics) |
| 516, 577, 599, 622–623, 653–681 | slice indexing `[idx]`/`[last_data..]` | VII | indices are phase-driven (by-construction); a desync panics loud, never corrupts |

### `inf-server/src/control.rs`
| Line | Site | Class | Note |
|---|---|---|---|
| 326 | `.expect("control thread alive (fail-stop)")` | FS | §8.4 |
| 525 | `panic!("catalog META swap failed (fail-stop)")` | FS | §8.4 durability metadata; see finding 1 |
| 614 | `.expect("spawn control thread")` | VII/boot | boot-time thread spawn (allocation-adjacent, but a node that cannot spawn its control thread cannot serve — fail-stop at boot is correct) |
| 154, 245 | `cells[usize::from(cell)]` indexing | VII | OOB cell id = assembly bug, documented `# Panics` |

### `inf-log/src/commit.rs` (non-test)
| Line | Site | Class | Note |
|---|---|---|---|
| 340, 372, 373, 539, 571 | release `assert!`/`assert_eq!` | VII | keep (ledger invariants) |
| 408, 432, 477, 498 | `debug_assert!` | VII | 408/432/498 → **promote** |
| 429, 452, 476, 538, 557, 570 | `.expect(...)` | VII | call-site predicates (`*_fsync_due`) / driver exactly-once |

### `inf-log/src/staging.rs`
| Line | Site | Class | Note |
|---|---|---|---|
| 138, 204, 206, 328, 329, 349, 357 | release `assert!`/`assert_eq!` | VII | generation-token custody + boot config — keep |
| 293, 335 | `.expect("frame fits u32")` | VII | frame ≤ `DEFAULT_MAX_FRAME_LEN` (asserted at construction) |
| 348, 356 | `.expect("no frame in flight" / "release with no frame in flight")` | VII | lease-lifecycle invariant |

### `inf-runtime/src/gate.rs`
| Line | Site | Class | Note |
|---|---|---|---|
| 100, 154, 325 | `panic!("gate key completed twice" / "polled after completion/cancellation")` | VII | token uniqueness / future-lifecycle — routing bugs, not load |
| 145 | `unreachable!("matched Delivered above")` | VII | control-flow |
| 468 | `unreachable!("watermark waiters are never cancelled")` | VII | `WatermarkWait` has no cancel path (inventory 2.4) |

---

## CI grep proposal (the extension the story asks for)

Today there is **no** panic-policy grep. `check-cell-denylist.sh` bans
locks/sleep/async in cell crates; `clippy.toml` disallows `Mutex`/`RwLock`/
`thread::sleep`; neither touches `unwrap`/`panic`. The durable-path files
(`durable.rs`, `ckpt.rs`, `recover.rs`, `control.rs`) live in
`inf-server/src/` but **not** under `inf-server/src/cell/`, so no existing
check covers them.

Proposed new script `scripts/check-panic-policy.sh`, modeled on the audited-
allowlist idiom of `check-fsync-fail-stop.sh`. The enforceable, shell-cheap
rule (§Tools "five-line CI greps"): **naked `.unwrap()`, `todo!`, and
`unimplemented!` are banned outside tests on the durable + cell paths;
`.expect("…")`, `assert!`, `panic!`, `unreachable!` are allowed** (they are
assertions/fail-stops, reviewed per this audit). This passes clean today (0
naked unwrap) and prevents regression — the honest scope for a grep, since
shell cannot tell an invariant `panic!` from an operating-condition one
(that stays a reviewer's job, backed by this inventory).

```bash
#!/usr/bin/env bash
# Panic-policy grep (M2.5-S13, INFINITY_STYLE §Panics): on the durable and
# cell-resident paths, operating conditions return typed errors — a naked
# `.unwrap()` (or `todo!`/`unimplemented!`) is a review reject. `expect()`
# with an invariant justification is an assertion and is allowed; `panic!`/
# `unreachable!` are for violated internal invariants (audited in
# reviews/ + the interfaces-m2 invariant inventory). Backstops review.
set -euo pipefail
cd "$(dirname "$0")/.."

# Durable-path + cell-resident non-test sources (the §8 durability surface
# and the L1/L6 cell path). Extend as the durable surface grows.
FILES=(
    crates/inf-log/src/commit.rs
    crates/inf-log/src/staging.rs
    crates/inf-log/src/segment.rs
    crates/inf-runtime/src/gate.rs
    crates/inf-server/src/durable.rs
    crates/inf-server/src/ckpt.rs
    crates/inf-server/src/recover.rs
    crates/inf-server/src/control.rs
    crates/inf-server/src/plane.rs
)

# Banned on these paths: naked unwrap (unwrap_or* is fine), and the
# unfinished-code macros. `expect(`, `assert`, `panic!`, `unreachable!`
# are intentionally NOT banned (assertions / audited fail-stops).
PATTERN='(\.unwrap\(\)|todo!|unimplemented!)'

fail=0
for f in "${FILES[@]}"; do
    [ -f "$f" ] || continue
    # Strip the test module: everything from the first `#[cfg(test)]`.
    body=$(awk '/#\[cfg\(test\)\]/{exit} {print}' "$f")
    if hits=$(printf '%s\n' "$body" | grep -nE "$PATTERN" | grep -vE 'unwrap_or|denylist-allow'); then
        echo "PANIC-POLICY violation in $f (naked unwrap / todo / unimplemented):"
        echo "$hits"
        fail=1
    fi
done

if [ "$fail" -ne 0 ]; then
    echo "Operating conditions return typed errors (§Panics); use expect(\"<invariant>\")"
    echo "for a justified assertion, or a Result for an operating error."
    exit 1
fi
echo "panic-policy grep OK (${#FILES[@]} durable/cell sources, 0 naked unwrap)"
```

Wire it into `just check` next to the existing greps
(`check-fsync-fail-stop.sh`, `check-cell-denylist.sh`). Caveat on the
`#[cfg(test)]` strip: the awk cut stops at the *first* `#[cfg(test)]`, which
is correct for these files (tests are a trailing `mod tests`); a file with
an inline `#[cfg(test)]` helper above production code would need the
per-item scoping the other scripts use — none of the listed files have that
today.
