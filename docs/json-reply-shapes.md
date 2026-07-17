# InfinityDB `JSON.*` Reply Shapes

> **GENERATED — do not edit.** Rendered by `tests/compat/src/replyshapes.rs`
> from `inf_server::JSON_REPLY_SHAPES` (the table beside the handlers).
> Regenerate: `INF_REGEN_REPLY_SHAPES=1 cargo test -p compat --test reply_shapes_artifact`
> (CI fails when this file is stale — the release pipeline inherits that refusal).

The RedisJSON reply contract differs by **path mode**: `$` paths answer
match *sets*, legacy (non-`$`) paths answer single values — the first match
for reads, the last applied match for mutations (ADR-0041 D7). Most RESP3
shapes differ only by protocol-level nulls (`_` for `$-1`/`*-1`); the
RedisJSON-native TYPE and number frames are declared explicitly below. M3-S21
byte-diffs both protocols against the pinned container, and every accepted
divergence is generated into `docs/compat-matrix.md` (L8).

Errors shared across the family: missing keys on mutations answer
`ERR could not perform this operation on a key that doesn't exist`; legacy
paths with zero matches answer `ERR Path '<path>' does not exist`; legacy
paths whose matches are all type-inapplicable answer
`ERR Path '<path>' does not contain a <type>`; size/depth limits answer the
ADR-0039 D5 pinned lines. Durable namespaces accept `JSON.*` writes through
M3-S17's `DocDelta`/`DocFull` path (ADR-0043).

| Command | Kind | `$` path | Legacy path | RESP3 delta | Notes |
|---|---|---|---|---|---|
| `JSON.SET` | write | `+OK`; null when NX/XX skips | same as `$` mode | nulls are `_` instead of `$-1` | parent-creation rules per ADR-0041 D6; root sets preserve TTL |
| `JSON.GET` | read | bulk JSON text: array of matches; multi-path wraps an object keyed by the path strings as given | bulk JSON text: first match, unwrapped; zero matches error | nulls are `_` instead of `$-1` | `INDENT`/`NEWLINE`/`SPACE` honored; missing key is null in both modes |
| `JSON.MGET` | read | array: per key, bulk JSON match-array or null | array: per key, bulk first match or null | nulls are `_` instead of `$-1` | per-key atomicity only — no cross-cell snapshot (ADR-0041 D9) |
| `JSON.DEL` | write | integer: matches removed | integer: matches removed | identical | root path deletes the key (kernel-owned lifecycle) |
| `JSON.FORGET` | write | integer: matches removed | integer: matches removed | identical | alias of JSON.DEL |
| `JSON.TYPE` | read | array of type-name bulk strings | bulk string: first match's type; null when the path misses | `$` mode: array of one-element bulk-string arrays; legacy: one-element array containing the bulk string or null | `integer` and `number` are distinct names (RedisJSON parity) |
| `JSON.NUMINCRBY` | write | bulk JSON text array: new value per match, null for non-numbers | bulk JSON text: last applied match's new value | native integer/double/null array in both modes; legacy has one element | i64 overflow / non-finite results abort the whole command (§3.4 R4) |
| `JSON.NUMMULTBY` | write | bulk JSON text array: new value per match, null for non-numbers | bulk JSON text: last applied match's new value | native integer/double/null array in both modes; legacy has one element | same numeric model as NUMINCRBY |
| `JSON.STRAPPEND` | write | array: new byte length per match, null for non-strings | integer: last applied match's new length | nulls are `_` instead of `$-1` | operand must be a JSON string; the no-path form appends at the legacy root |
| `JSON.STRLEN` | read | array: byte length per match, null for non-strings | integer: first match's length | nulls are `_` instead of `$-1` | missing key is null |
| `JSON.TOGGLE` | write | array: 0/1 per match, null for non-booleans | bulk `true`/`false`: last applied match's new value | nulls are `_` instead of `$-1` | booleans only; others skip |
| `JSON.CLEAR` | write | integer: values cleared | integer: values cleared | identical | already-empty containers and zero numbers skip, uncounted (ADR-0041 D8) |
| `JSON.ARRAPPEND` | write | array: new length per match, null for non-arrays | integer: last applied match's new length | nulls are `_` instead of `$-1` | three-argument form appends one value at the legacy root (ADR-0042 D7) |
| `JSON.ARRINSERT` | write | array: new length per match, null for non-arrays | integer: last applied match's new length | nulls are `_` instead of `$-1` | resolved index outside `0..=len` aborts the whole command (ADR-0042 D3) |
| `JSON.ARRINDEX` | read | array: found index or -1 per match, null for non-arrays | integer: first match's found index or -1 | nulls are `_` instead of `$-1` | scalar needles only; `[start, stop)` with `stop == 0` meaning end |
| `JSON.ARRLEN` | read | array: length per match, null for non-arrays | integer: first match's length | nulls are `_` instead of `$-1` | missing key is null |
| `JSON.ARRPOP` | write | array: popped element as bulk JSON text per match, null for non-arrays and empty arrays | bulk JSON text: last array match's popped element; null when it was empty | nulls are `_` instead of `$-1` | index defaults to -1; out-of-range clamps to the nearest end (ADR-0042 D3) |
| `JSON.ARRTRIM` | write | array: new length per match, null for non-arrays | integer: last applied match's new length | nulls are `_` instead of `$-1` | inclusive window; out-of-range clamps, never errors (ADR-0042 D3) |
| `JSON.OBJKEYS` | read | array: per match, array of key bulk strings or null for non-objects | array of key bulk strings: first match | nulls are `_` instead of `$-1` | keys in insertion order (ADR-0036) |
| `JSON.OBJLEN` | read | array: entry count per match, null for non-objects | integer: first match's entry count | nulls are `_` instead of `$-1` | missing key is null |
| `JSON.MERGE` | write | `+OK` | `+OK` | identical | RFC 7386 at the selected value; null members inside object patches delete keys (ADR-0042 D6); creates missing keys at the root |
| `JSON.DEBUG` | read | integer: exact attributed bytes for `MEMORY key`; missing key is null | same (the command has no path mode) | nulls are `_` instead of `$-1` | partial: shared pools and allocator slack remain in INFO memory, not per key |

---

Compatibility status per command lives in `docs/compat-matrix.md`; this
artifact pins the *shapes* the corpus executes under both protocols
(`inf-server/tests/json_commands.rs`). Performance claims live in the
claim ledger, never here (L10).
