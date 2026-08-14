# S04 zero-index instruction-path audit (the M4-S02 degenerate-case discipline)

Method (the `.artifacts/m4/s02` precedent): `objdump -d --demangle` of the
`store` bench binary at the **baseline** (inner commit `9d03b09`, the tree
the S04 diff applies to) vs **S04**, symbols filtered to
`inf_store::store::CellStore` + `inf_store::index::Index`, normalized
(addresses stripped, call/jump targets → `+OFF`, rip displacements
zeroed), per-symbol diff. Full listings: `base-hotpath.asm` /
`s04-hotpath.asm`; per-symbol diffs beside them.

## Verdict

| symbol | verdict | explanation |
|---|---|---|
| `Index::insert` / `::position_of` / `::remove`, `CellStore::probe_groups`, `::get_with_hash`, `::write_record_carrying` | **identical shape** (equal instruction counts) | byte-diffs are LLVM anonymous-symbol renames and `CellStore` field-offset constants only (the struct grew the attach block, shifting offsets — see `get_with_hash.diff`) |
| `CellStore::set` | **identical shape** (461 = 461 insns) | the only semantic byte-diff is `cmp $0x9` → `cmp $0xb`: the `OpError` discriminant count grew with the `IndexMaintenance` variant. No new branch, no new call (`set.diff`) |
| `CellStore::resolve_hashed` | 308 → **298** insns | the lazy-expiry reap arm now calls the outlined `free_record` instead of an inlined free — a **cold-arm** change (the reap path); the hot find/touch path is shape-identical (`resolve_hashed.diff`) |
| `CellStore::free_record` | newly standalone (was inlined) | opens with `cmpb $0x1, <idx.active>; jne <free>` — **exactly one predictable branch** on the cached flag before the untouched free path (the ADR-0072 D2 allowance); the death-hook body is behind it |
| `CellStore::new`, `drop_in_place`, `::report` | +21 / +3 / newly standalone | constructor, destructor, and attribution — control-plane/diagnostic, off the hot path (the M4-S02 "informational" class) |

**Conclusion:** zero-index namespaces keep the M4 baseline instruction
path on every hot read/write symbol; the added cost is one cached-flag
branch at the record-death choke point, per ADR-0072 D2. Slim (no-`doc`)
builds compile the hook out entirely (the stub's `false` folds every call
site away — verified by the L11 slim-build lane).
