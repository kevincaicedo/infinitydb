# InfinityDB Redis Compatibility Matrix

> **GENERATED — do not edit.** Rendered by `tests/compat/src/matrixgen.rs`
> from the `inf-wire` command registry and the oracle-diff corpus.
> Regenerate: `INF_REGEN_MATRIX=1 cargo test -p compat --test matrix_artifact`
> (CI fails when this file is stale — the release pipeline inherits that refusal).

Oracle: **Redis 8.0.5** (local oracle on the dev box; the dockerized CI oracle
pin lands with the M1-S14 release pipeline). Every declared-`full` behavior is
byte-diffed against the oracle on every test run; any new deviation fails CI
until it is allowlisted with a justification (L8 — honesty is total).

**Corpus:** 378 byte-compared cases · 32 documented deviations · 0 tolerated failures.
**Surface:** 65 commands — 47 full · 15 partial · 0 stub · 1 extension · 2 internal.

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
| `SET` | full | M0 | write denyoom | -3 | 71 |  |
| `SETNX` | full | M0 | write denyoom fast | 3 | 2 |  |
| `SETEX` | full | M0 | write denyoom | 4 | 4 |  |
| `PSETEX` | full | M0 | write denyoom | 4 | 2 |  |
| `GETSET` | full | M0 | write denyoom fast | 3 | 2 |  |
| `GETDEL` | full | M0 | write fast | 2 | 2 |  |
| `DEL` | full | M0 | write | -2 | 4 |  |
| `EXISTS` | full | M0 | readonly fast | -2 | 10 |  |
| `TYPE` | full | M0 | readonly fast | 2 | 2 | only the string type exists until M3 |
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
| `INF.NS` | extension | M1 | admin | -2 | 0 | namespace registry v1 — the M2 durability seam |
| `INF.TAKE` | internal | M1 | write fast | 2 | 0 | cross-cell RENAME/COPY program primitive |
| `INF.PEEK` | internal | M1 | readonly fast | 2 | 0 | cross-cell COPY program primitive |

## Documented deviations (the allowlist, verbatim)

Each entry is a `SkipDiff` justification from the corpus: the candidate must
still produce well-formed RESP for these cases, but the bytes differ from the
oracle by design.

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
- InfinityDB extension; durable mode honestly rejected until M2

## Absent (owner milestone)

| Family | Arrives |
|---|---|
| Persistence admin (SAVE, BGSAVE, INF.CKPT, …) | M2 — durability |
| Hashes, lists, sets, zsets, bitmaps, bitfield, HyperLogLog | M3 — data types |
| Keyspace notifications, SLOWLOG, MONITOR, sharded pub/sub (SSUBSCRIBE/SPUBLISH) | M3 |
| Connection control (QUIT, RESET) | M3 (RESET pairs with transaction state) |
| MULTI / EXEC / WATCH / DISCARD, EVAL / Lua, FUNCTION, WAIT | M4 — transactions |
| Streams (X*), AUTH / TLS / ACL, CLIENT TRACKING | M5 |
| JSON.* documents | M6 |
| Vector sets | M8 |
| Replication / cluster admin | M9+ |

---

Master plan §14 owns the staging policy; milestone plans own acceptance
criteria. Performance claims live in the claim ledger, never here (L10).
