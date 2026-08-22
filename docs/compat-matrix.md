# InfinityDB Redis Compatibility Matrix

> **GENERATED — do not edit.** Rendered by `tests/compat/src/matrixgen.rs`
> from the `inf-wire` command registry and the oracle-diff corpus.
> Regenerate: `INF_REGEN_MATRIX=1 cargo test -p compat --test matrix_artifact`
> (CI fails when this file is stale — the release pipeline inherits that refusal).

Oracles: **Redis 8.0.5** for the core surface; RedisJSON uses
**redis/redis-stack-server:7.4.0-v8@sha256:798ab84d9f266936b034ab11c4d04a2b8e4b441884c5aa7d17ac951eefdf742a** with ReJSON/20809.
Every covered behavior is byte-diffed under its declared protocol; any new or
stale deviation fails CI (L8 — honesty is total).

**Corpus:** 546 byte-compared executions · 58 documented deviations · 0 tolerated failures.
**Surface:** 90 commands — 54 full · 32 partial · 0 stub · 2 extension · 2 internal.

Status vocabulary: `full` = behavior-contract equivalent (recorded deviations
are representational: ordering, identity payloads, opaque cursors/art);
`partial` = a documented semantic difference exists; `stub` = accepted but
inert; `extension` = `INF.*` surface unknown to Redis; `internal` = fabric
program primitives, not a client surface.

## Commands

| Command | Status | Since | Flags | Arity | Cases | Notes |
|---|---|---|---|---|---|---|
| `PING` | full | M0 | fast | -1 | 7 |  |
| `ECHO` | full | M0 | fast | 2 | 3 |  |
| `HELLO` | full | M0 | fast | -1 | 1 | identity fields (server/version) are InfinityDB's own, as for any non-Redis server |
| `QUIT` | partial | M1 | fast | 1 | 0 | replies +OK and closes the connection (Redis-equivalent); not in the byte-diff corpus because closing tears down the shared oracle connection — covered by a unit test and the client-smoke suite |
| `GET` | full | M0 | readonly fast | 2 | 27 |  |
| `SET` | full | M0 | write denyoom | -3 | 73 |  |
| `SETNX` | full | M0 | write denyoom fast | 3 | 2 |  |
| `SETEX` | full | M0 | write denyoom | 4 | 4 |  |
| `PSETEX` | full | M0 | write denyoom | 4 | 2 |  |
| `GETSET` | full | M0 | write denyoom fast | 3 | 2 |  |
| `GETDEL` | full | M0 | write fast | 2 | 2 |  |
| `DEL` | full | M0 | write | -2 | 4 |  |
| `EXISTS` | full | M0 | readonly fast | -2 | 10 |  |
| `TYPE` | full | M0 | readonly fast | 2 | 4 | only the string type exists until M3 |
| `INCR` | full | M0 | write denyoom fast | 2 | 8 |  |
| `DECR` | full | M0 | write denyoom fast | 2 | 2 |  |
| `INCRBY` | full | M0 | write denyoom fast | 3 | 3 |  |
| `DECRBY` | full | M0 | write denyoom fast | 3 | 2 |  |
| `APPEND` | full | M0 | write denyoom fast | 3 | 4 |  |
| `STRLEN` | full | M0 | readonly fast | 2 | 5 |  |
| `EXPIRE` | full | M0 | write fast | -3 | 17 | TTLs ≥ ~34.8 years clamp to the u40 record bound |
| `PEXPIRE` | full | M0 | write fast | -3 | 1 | same u40 clamp |
| `TTL` | full | M0 | readonly fast | 2 | 18 |  |
| `PTTL` | full | M0 | readonly fast | 2 | 3 |  |
| `PERSIST` | full | M0 | write fast | 2 | 3 |  |
| `INFO` | partial | M0 | admin | -1 | 0 | sections + field vocabulary present; gauges are this cell's slice until the control plane aggregates (client-smoke CI is the open M1-S14 AC) |
| `COMMAND` | partial | M0 | admin | -1 | 3 | COMMAND DOCS is an honest empty map; the registry covers the implemented surface only |
| `MGET` | full | M1 | readonly fast | -2 | 4 |  |
| `MSET` | full | M1 | write denyoom | -3 | 3 |  |
| `MSETNX` | partial | M1 | write denyoom | -3 | 3 | cross-cell keys are check-then-set until M4 transactions; single-cell exact |
| `GETRANGE` | full | M1 | readonly | 4 | 8 |  |
| `SETRANGE` | full | M1 | write denyoom | 4 | 4 | values bound at 16 MiB − 1 (record format v0) |
| `GETEX` | full | M1 | write fast | -2 | 8 |  |
| `INCRBYFLOAT` | partial | M1 | write denyoom fast | 3 | 6 | computes in f64 (Redis: long double); formatting matches on the pinned corpus, precision tails may differ |
| `SUBSTR` | full | M1 | readonly | 4 | 1 |  |
| `RENAME` | partial | M1 | write | 3 | 2 | cross-owner pairs run as a two-cell fabric program — atomic per cell, not across cells until M4; same-owner pairs exact |
| `RENAMENX` | partial | M1 | write fast | 3 | 3 | same cross-owner window as RENAME |
| `COPY` | partial | M1 | write denyoom | -3 | 12 | same cross-owner window as RENAME; TTL transfers as relative ms across cells |
| `TOUCH` | full | M1 | readonly fast | -2 | 1 |  |
| `UNLINK` | full | M1 | write fast | -2 | 1 |  |
| `DBSIZE` | full | M1 | readonly fast | 1 | 5 |  |
| `KEYS` | full | M1 | readonly | 2 | 4 | result ordering is engine-defined (set equality holds) |
| `RANDOMKEY` | full | M1 | readonly | 1 | 1 | two-level random: cell, then key |
| `SCAN` | full | M1 | readonly | -2 | 2 | cursor values are engine-internal; the every-resident-key-≥-once guarantee is proptested |
| `FLUSHDB` | full | M1 | write | -1 | 4 |  |
| `FLUSHALL` | partial | M1 | write | -1 | 2 | atomic per cell, eventually complete across cells within one scatter round (no global pause) |
| `OBJECT` | partial | M1 | readonly | -2 | 11 | IDLETIME is an honest 0 (CLOCK recency, no LRU clock); FREQ is the CMS Morris estimate |
| `DEBUG` | partial | M1 | admin | -2 | 3 | subset: SLEEP / JMAP / OBJECT / SET-ACTIVE-EXPIRE; SLEEP stalls one cell, never the node |
| `EXPIREAT` | full | M1 | write fast | -3 | 6 |  |
| `PEXPIREAT` | full | M1 | write fast | -3 | 1 |  |
| `EXPIRETIME` | full | M1 | readonly fast | 2 | 5 |  |
| `PEXPIRETIME` | full | M1 | readonly fast | 2 | 4 |  |
| `SELECT` | full | M1 | fast | 2 | 7 |  |
| `CONFIG` | partial | M1 | admin | -2 | 27 | typed M1 key subset with frozen hot-reload classes |
| `CLIENT` | partial | M1 | admin | -2 | 5 | KILL supports the ID filter form; LIST addr/fd are placeholders until peername capture |
| `LOLWUT` | partial | M1 | readonly | -1 | 0 | the whole reply is version art (nothing byte-comparable by design) |
| `SUBSCRIBE` | full | M1 | fast | -2 | 5 |  |
| `UNSUBSCRIBE` | full | M1 | fast | -1 | 5 | bare-form confirmations emit in subscription order (Redis: dict order) |
| `PSUBSCRIBE` | full | M1 | fast | -2 | 2 |  |
| `PUNSUBSCRIBE` | full | M1 | fast | -1 | 3 | same bare-form ordering note as UNSUBSCRIBE |
| `PUBLISH` | full | M1 | fast | 3 | 4 | a publisher subscribed to its own channel via a remote owner cell may receive its frame before the publish reply (local owners match Redis order) |
| `PUBSUB` | partial | M1 | readonly | -2 | 8 | SHARDCHANNELS / SHARDNUMSUB arrive with sharded pub/sub (M3 cut line) |
| `INF.NS` | extension | M1 | admin | -2 | 0 | namespace registry (M2 durability seam; M4-S19 adds SET + the ADR-0062 tiering keys; M4-S26 lifts the D8 `USE` refusal — the string family, `SCAN`, and `DBSIZE` serve tiered namespaces; two extension error classes are live on their writes: `DISKFULL …` (ADR-0063 — disk budget or device full; new-tier-byte placements only) and `STALLED tiered write timed out waiting for flush progress (TAIL-STALL-TIMEOUT)` (ADR-0053 D4 — retryable). Deviations on tiered namespaces: no expiry (the `EXPIRE` family + `SET` expiry options refuse typed; `TTL` = -1 for live keys), non-string families refuse typed, multi-key ops resolve sequentially. M4-S27 (ADR-0068): `MAXMEMORY`/`EVICTION` on named *memory* namespaces are enforced and Hot via `SET` (`inherit`/`0` reset them); a namespace with its own budget answers the Redis-exact OOM error scoped to that namespace and reclaims only its own keys; durable and tiered namespaces refuse both keys typed (tiered budgets belong to `MEM-BUDGET`) |
| `INF.CKPT` | extension | M2 | admin | -1 | 0 | checkpoint operator surface (M2-S20): [CELL k] [WAIT]; WAIT returns after the new MANIFEST is durable — no fork, per-cell timing (ADR-0021) |
| `BGSAVE` | partial | M2 | admin | -1 | 0 | maps onto INF.CKPT (fuzzy checkpoint, no fork, no RDB file); SCHEDULE accepted and moot; reply byte-identical; memory-only nodes answer a documented error |
| `LASTSAVE` | partial | M2 | readonly fast | 1 | 0 | unix seconds of the newest durable MANIFEST publication; 0 before the first (Redis reports process-start time); loading flag docs-derived, not capture-verified |
| `INF.TAKE` | internal | M1 | write fast | 2 | 0 | cross-cell RENAME/COPY program primitive |
| `INF.PEEK` | internal | M1 | readonly fast | 2 | 0 | cross-cell COPY program primitive |
| `JSON.SET` | partial | M3 | write denyoom | -4 | 32 | S21 corpus exact except parser-specific malformed-input text; root sets preserve TTL (as RedisJSON — S22 probe); durable writes use M3-S17 document records |
| `JSON.GET` | partial | M3 | readonly | -2 | 36 | S21 corpus exact except documented large-exponent f64 text and module-specific WRONGTYPE wording; INDENT/NEWLINE/SPACE covered; path match sets capped by doc-max-path-matches |
| `JSON.MGET` | partial | M3 | readonly | -3 | 2 | S21 corpus exact; per-key atomicity only — no cross-cell snapshot (each cell serves its key at its own serve time) |
| `JSON.DEL` | partial | M3 | write | -2 | 6 | recursive-overlap result count differs from RedisJSON while post-state is identical |
| `JSON.FORGET` | partial | M3 | write | -2 | 2 | alias of JSON.DEL — inherits its recursive-overlap count difference (S22 probe: the oracle reports 2 where InfinityDB reports 3 raw matches); own S21 corpus case exact |
| `JSON.TYPE` | full | M3 | readonly fast | -2 | 4 | RESP2/RESP3 type-name vocabulary and frames are exact in the S21 corpus |
| `JSON.NUMINCRBY` | partial | M3 | write denyoom | 4 | 4 | i64 preserved exactly; i64 overflow errors atomically where the pinned RedisJSON wraps to i64::MIN (S22 probe); non-finite results error on both; value echoes share JSON.GET's large-exponent f64 deviation |
| `JSON.NUMMULTBY` | partial | M3 | write denyoom | 4 | 2 | same numeric model and deviation classes as JSON.NUMINCRBY; S21 RESP2/RESP3 corpus exact |
| `JSON.STRAPPEND` | full | M3 | write denyoom | -3 | 4 | lengths reported in bytes and the implicit legacy root path match the pinned oracle (S21 corpus + S22 probes) |
| `JSON.STRLEN` | full | M3 | readonly fast | -2 | 6 | lengths reported in bytes, matching the pinned oracle (S21 corpus + S22 multibyte probe) |
| `JSON.TOGGLE` | full | M3 | write fast | -2 | 4 | S21 RESP2/RESP3 corpus exact; non-boolean skip (modern) / error (legacy) split matches the pinned oracle (S22 probe) |
| `JSON.CLEAR` | full | M3 | write | -2 | 2 | already-empty containers and zero numbers skip (uncounted), matching the pinned oracle (S21 corpus + S22 probe) |
| `JSON.ARRAPPEND` | partial | M3 | write denyoom | -3 | 4 | S21 corpus exact; three-argument form appends one value at the legacy root, a form the pinned RedisJSON rejects with an arity error (S22 probe) |
| `JSON.ARRINSERT` | partial | M3 | write denyoom | -5 | 6 | resolved index outside 0..=len aborts the whole command atomically (§3.4 R4); RedisJSON can mutate an earlier match before a later index error |
| `JSON.ARRINDEX` | partial | M3 | readonly | -4 | 6 | scalar needles only (container needles rejected — ADR-0042 D3); mixed-width numbers compare numerically; S21 corpus exact |
| `JSON.ARRLEN` | partial | M3 | readonly fast | -2 | 8 | S21 corpus exact except module-specific WRONGTYPE error text |
| `JSON.ARRPOP` | partial | M3 | write | -2 | 6 | out-of-range clamps and empty-array null match the pinned oracle (S22 probes); the popped-value text shares JSON.GET's large-exponent f64 deviation (the oracle echoes a 3e72 literal as 2.9999999999999996e72); S21 corpus exact |
| `JSON.ARRTRIM` | partial | M3 | write | 5 | 6 | inclusive window and out-of-range clamps; overlapping mixed-type reply/error shape differs from RedisJSON with the same post-state |
| `JSON.OBJKEYS` | full | M3 | readonly | -2 | 4 | keys in insertion order, as the pinned RedisJSON returns them (the only order the format has — ADR-0036); S21 corpus exact |
| `JSON.OBJLEN` | full | M3 | readonly fast | -2 | 6 | S21 RESP2/RESP3 corpus exact |
| `JSON.MERGE` | partial | M3 | write denyoom | 4 | 10 | RFC 7386 at the selected value; null members inside object patches delete keys, while a path-targeted null is literal (ADR-0042 D6); retaining overlaps use one immutable snapshot rather than RedisJSON cascade semantics; missing keys create at the root only |
| `JSON.DEBUG` | partial | M3 | readonly fast | 3 | 4 | MEMORY reports InfinityDB-attributed record + external document bytes; missing-key and allocator-specific RedisJSON parity are intentionally not claimed |

## Documented deviations (the allowlist, verbatim)

Entries come verbatim from the core `SkipDiff` corpus or the protocol-keyed
RedisJSON allowlist. The candidate still produces well-formed RESP, but the
bytes or post-state differ by an understood, reviewed design decision.

### `HELLO`

- identity fields differ by design (L8: server/version)
- identity fields differ; proto switch verified locally
- NOPROTO error text verified in unit tests

### `INFO`

- section payloads differ (InfinityDB identity/tripwires); shape client-parseable

### `COMMAND`

- registry is the M0+M1 surface, not the full Redis set
- registry size differs by design
- flags/acl detail differs; arity+keyspec verified in inf-wire
- docs payload not implemented (honest empty map)

### `KEYS`

- result ordering differs (home-group vs dict order); set equality via DBSIZE

### `RANDOMKEY`

- two-level random (cell, then key) — documented deviation

### `SCAN`

- cursor values are engine-internal; guarantee proptested in inf-store
- cursor values engine-internal

### `OBJECT`

- no LRU clock until the eviction engine (M1-E3); honest 0
- popularity scale differs: CMS Morris estimate vs Redis log counter

### `DEBUG`

- removed in Redis 8; InfinityDB accepts it as a no-op (M1-S03 surface)
- value-address/serialized-length fields are engine-internal

### `CONFIG`

- InfinityDB returns the typed M1 key subset
- error detail text differs; both reject
- error text shape differs slightly

### `CLIENT`

- connection ids are engine-internal counters
- addr/fd/timing fields differ; field vocabulary matches

### `LOLWUT`

- version art differs by design

### `PUBSUB`

- sharded pub/sub (SSUBSCRIBE family) is the recorded M3 cut line

### `INF.NS`

- InfinityDB extension
- InfinityDB extension; durable live since M2-S08 (node tier — the planeless compat candidate answers its documented no-runtime error)
- InfinityDB extension (M2-S08): named-namespace selection, SELECT-class conn state; durable-mode deviations documented in ADR-0015
- InfinityDB extension (M4-S19, ADR-0062): MEM-BUDGET declares a durable-tiered namespace; the planeless compat candidate answers its documented no-runtime error
- InfinityDB extension (M4-S19, ADR-0062 D3): per-namespace hot-reload; CreateOnly keys (TIER-IO-MODE, COLD-READ-QD) refuse typed

### `INF.CKPT`

- InfinityDB extension (M2-S20): checkpoint trigger; the planeless compat candidate answers its documented no-durable-plane error

### `BGSAVE`

- maps onto INF.CKPT (M2-S20): fuzzy checkpoint, no fork, no RDB file; reply byte-identical on durable nodes; the planeless candidate answers its documented error (node_e2e pins the live reply)

### `LASTSAVE`

- M2-S20: newest durable MANIFEST publication time; 0 before the first save vs Redis's process-start time; the planeless candidate answers its documented error

### `JSON.SET`

- RedisJSON RESP2 `edge-invalid-json`: both reject the malformed input at the first member; parser-specific error text differs
- RedisJSON RESP3 `edge-invalid-json`: both reject the malformed input at the first member; parser-specific error text differs

### `JSON.GET`

- RedisJSON RESP2 `edge-wrongtype-get`: InfinityDB uses the core Redis WRONGTYPE envelope; RedisJSON uses module-specific error text
- RedisJSON RESP2 `fuzz-exp-get`: large-exponent f64 parsing and canonical text differ from RedisJSON rounding
- RedisJSON RESP3 `edge-wrongtype-get`: InfinityDB uses the core Redis WRONGTYPE envelope; RedisJSON uses module-specific error text
- RedisJSON RESP3 `fuzz-exp-get`: large-exponent f64 parsing and canonical text differ from RedisJSON rounding

### `JSON.DEL`

- RedisJSON RESP2 `edge-del-overlap`: recursive overlap: InfinityDB reports three raw matches; RedisJSON reports two removals; post-state is identical
- RedisJSON RESP3 `edge-del-overlap`: recursive overlap: InfinityDB reports three raw matches; RedisJSON reports two removals; post-state is identical

### `JSON.ARRINSERT`

- RedisJSON RESP2 `edge-get-after-abort`: InfinityDB validates the full match set before commit; RedisJSON mutates an earlier match before a later index error
- RedisJSON RESP3 `edge-get-after-abort`: InfinityDB validates the full match set before commit; RedisJSON mutates an earlier match before a later index error

### `JSON.ARRLEN`

- RedisJSON RESP2 `edge-wrongtype-arrlen`: InfinityDB uses the core Redis WRONGTYPE envelope; RedisJSON uses module-specific error text
- RedisJSON RESP3 `edge-wrongtype-arrlen`: InfinityDB uses the core Redis WRONGTYPE envelope; RedisJSON uses module-specific error text

### `JSON.ARRTRIM`

- RedisJSON RESP2 `edge-trim-overlap`: overlapping mixed-type matches reach the same post-state; RedisJSON returns a path error while InfinityDB reports per-match length/null results
- RedisJSON RESP3 `edge-trim-overlap`: overlapping mixed-type matches reach the same post-state; RedisJSON returns a path error while InfinityDB reports per-match length/null results

### `JSON.MERGE`

- RedisJSON RESP2 `edge-merge-overlap-get`: InfinityDB computes retaining overlaps from one snapshot and lets a changed ancestor supersede descendants; RedisJSON cascades descendant results
- RedisJSON RESP3 `edge-merge-overlap-get`: InfinityDB computes retaining overlaps from one snapshot and lets a changed ancestor supersede descendants; RedisJSON cascades descendant results

### `JSON.DEBUG`

- RedisJSON RESP2 `s15-debug-missing`: missing document: InfinityDB returns null; RedisJSON returns integer 0
- RedisJSON RESP2 `edge-debug-memory`: InfinityDB reports canonical document attribution; RedisJSON reports module allocator bytes
- RedisJSON RESP3 `s15-debug-missing`: missing document: InfinityDB returns null; RedisJSON returns integer 0
- RedisJSON RESP3 `edge-debug-memory`: InfinityDB reports canonical document attribution; RedisJSON reports module allocator bytes

## Durable write backpressure (extension surface, L8 note — M4.5-S27, ADR-0083)

Durable namespaces under log-staging pressure **pace** (the reply is
delayed while the command stays suspended) instead of erroring — every
path, local and fabric-routed (ADR-0083 D1). Redis has no equivalent
surface (no durable log). Mainstream Redis clients do not auto-retry
`-BUSY`, which is why refusal is not the design response to pressure;
the remaining typed `-BUSY` emitters are the document exact late
admission and the tiered cold-read queue cap, both counted
(`log_admission_busy`) and expected ≈ 0 — a climbing rate is a finding,
not designed behaviour. A durable write whose record can never fit the
staging domain refuses up front with typed
`ERR write exceeds durable log staging capacity` — non-retryable by
design (ADR-0083 D2; retrying it is a livelock). That bound is the
staging buffer minus the frame framing: **4 MiB − 56 B per record at the
default `--log-staging-mib 4`** (2 MiB − 56 B under the measured
`--frames-in-flight 3 --log-staging-mib 2` arm, ADR-0087 D1) — below the
16 MiB − 1 record-format cap memory namespaces honour in full; tiered
namespaces route values at or above `BLOB-THRESHOLD` out of line and are
not bound by it. The barrier class (`--barrier-class`, ADR-0086) and the
frame pipeline depth do not move the bound; only the staging buffer size does.

`FSYNC everysec` namespaces ack on apply and fsync on the 1 s tick — the
`appendfsync everysec` loss window (≤ 1 s on power loss). With the frame-fill
policy on (`--fill-window-us N`, M4.5-S39a; off by default until its A/B), a
barrier-less frame on an aligned segment may hold un-sealed for up to `N` µs
(design point 1 000) before it reaches the device: the **process-crash**
exposure of `everysec` records is then ≤ `N` µs of writes per cell, where
Redis's AOF buffer reaches the page cache every event loop. The power-loss
window is unchanged; `always` acks are never held (their frames carry the
barrier the ack waits on).

## Absent (owner milestone)

| Family | Arrives |
|---|---|
| Persistence admin (SAVE, …) | M9 — RDB import/export |
| Hashes, lists, sets, zsets, bitmaps, bitfield, HyperLogLog | M5 — data types |
| Keyspace notifications, SLOWLOG, MONITOR, sharded pub/sub (SSUBSCRIBE/SPUBLISH) | M5 |
| Connection control (RESET) | M6 (RESET pairs with transaction state) |
| MULTI / EXEC / WATCH / DISCARD, EVAL / Lua, FUNCTION, WAIT | M6 — transactions |
| Streams (X*), AUTH / TLS / ACL, CLIENT TRACKING | M7 |
| JSONPath filter expressions `?(@…)`, secondary indexes, query engine | M4.5 — ADR-0024 |
| `JSON.RESP` | Never — deprecated upstream; declared absent per the M3 plan anti-goals |
| Vector sets | M8 |
| Replication / cluster admin | M9+ |

---

Master plan §14 owns the staging policy; milestone plans own acceptance
criteria. Performance claims live in the claim ledger, never here (L10).
