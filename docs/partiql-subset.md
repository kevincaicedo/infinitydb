# The InfinityDB PartiQL subset (v1 — M4.5-S09, ADR-0080)

This document is the **compat contract** for `INF.QL` statement text
(L8): the accepted productions, their semantics, and the rejected
productions **with their exact error strings**. There is no external
oracle for `INF.QL` — the S09 table-driven suite
(`crates/inf-query/tests/partiql_suite.rs` + its golden file) pins this
document verbatim; changing a string here is a compat break and takes the
same discipline as a wire-format change. Library strings below are the
stable substrate; the RESP error prefix is pinned at S10/S11 against the
compat matrix.

Authority: ADR-0080 (compilation + access-program form), ADR-0079
(predicate semantics), ADR-0074 (typed keys + the numeric truth table),
ADR-0024 Decision 2 (the planner fence).

## 1. Shape

```
SELECT ( * | COUNT(*) )
FROM   target
[WHERE condition]
[LIMIT integer]
[;]
```

- Keywords and function names are **case-insensitive**; document
  attribute names, namespace names, and index names are
  **case-sensitive**.
- One optional trailing `;` is accepted.
- Statements are UTF-8, at most **8192 bytes** (config-lowerable).
- The only statement form is `SELECT`. Every statement compiles to
  exactly **one access step** (primary-key get, one index range, or a
  consented scan) plus an optional residual predicate — or is rejected
  with a string from §7. There is no planner: nothing below ever chooses
  between two plans.

## 2. FROM targets

```
target := ns | ns . index | ns . SCAN
part   := bare-name | "quoted-name"
```

- A **bare** name uses the namespace charset (`A–Z a–z 0–9 _ . -`)
  **without dots**; in an unquoted dotted target the first dot splits
  namespace from suffix. Names that themselves contain dots must be
  double-quoted: `FROM "my.ns"`, `FROM "my.ns"."idx"`.
- `FROM ns` — access by primary key (`$key`, §4) or by **path
  matching** against the namespace's `ready` indexes (§5).
- `FROM ns.index` — explicit index naming (the DynamoDB register). The
  index must exist and be `ready`, and the WHERE clause must constrain
  its path (§5).
- `FROM ns.SCAN` — the **explicit-consent full-namespace scan**. `SCAN`
  is recognized case-insensitively and is a reserved index name.
  Consent is syntax: an unindexed WHERE clause is never "helpfully"
  compiled into a scan (§7 `no access path`).

## 3. WHERE grammar

```
condition := or
or        := and *( OR and )
and       := unary *( AND unary )
unary     := [NOT] primary
primary   := '(' or ')' | leaf
leaf      := path cmp literal
           | path [NOT] BETWEEN literal AND literal
           | path [NOT] IN '(' literal *( ',' literal ) ')'
           | begins_with '(' path ',' string ')'
           | exists '(' path ')'
           | '$key' '=' string                    # FROM ns only, §4
cmp       := = | != | <> | < | <= | > | >=
```

- Nesting depth (every level, leaf included) ≤ **32**; AND/OR chains
  flatten to 64-ary ops and nest beyond that; a compiled predicate holds
  ≤ 256 operations, ≤ 64 distinct paths, ≤ 512 constants (ADR-0079 D7).
- `IN` lists hold 1..=100 members, one type family ({i64, f64} count as
  one family). `BETWEEN` bounds share a family likewise.

**Paths** are spelled without the `$.` root and are fence-shaped:

```
path    := first *( '.' name | '[' selector ']' )
first   := name | '[' selector ']'
selector:= integer | 'quoted' | "quoted" | *
```

`a.b`, `items[0].id`, `tags[*]`, `["order by"].total` are paths.
Recursive descent (`..`), slices, and unions are rejected (§7).
Attribute names that collide with keywords are bracket-quoted:
`["select"] = 1`.

**Literals:** single-quoted strings with `''` escaping (`'it''s'`);
integers (i64 — no leading zeros rule is *not* imposed, `-0` is `0`);
decimals/exponent forms are f64 (`10.5`, `1e3`); `TRUE`/`FALSE`; `NULL`
is a keyword that only ever produces the §7 rejection. Numeric literals
keep their lexical type: `10` is i64, `10.0` is f64 — cross-numeric
truth is owned by the shared ADR-0074 compare functions at evaluation.

## 4. The primary key: `$key`

`$key` is a **pseudo-path** (dollar-prefixed spellings can never be
document paths). Its only legal use is a top-level AND conjunct
`$key = '<string>'` under `FROM ns`, which compiles to the primary-key
get; every other conjunct becomes the residual. Primary keys are
hash-distributed — no order exists — so ranges, IN, duplicates, `OR`/
`NOT` positions, non-string literals, and `.SCAN` combinations are all
typed rejections (§7). Key literals are 1..=255 bytes of UTF-8.

## 5. Key-condition resolution (total, planner-free)

The WHERE clause's **top-level AND conjuncts** are examined. Servable
key-condition operators are `=`, `<`, `<=`, `>`, `>=`, `BETWEEN`,
`begins_with`; `!=`, `IN`, `exists`, and anything under `OR`/`NOT` are
residual-only.

- **Path matching (`FROM ns`):** conjuncts whose compiled path equals a
  `ready` index's declared path are candidates. Exactly one candidate
  index ⇒ it serves the statement. Two or more distinct ready indexes ⇒
  rejection (§7 `ambiguous`) — the compiler refuses rather than
  chooses. None ⇒ rejection (§7 `no access path`).
- **Explicit naming (`FROM ns.index`):** all ambiguity is gone;
  conjuncts on the named index's path form the key condition, everything
  else (including conjuncts matching *other* indexes) is residual. At
  least one servable conjunct must constrain the index (§7
  `unconstrained index`).
- **Single-valued index paths** (no `[*]` step): every servable conjunct
  on the path joins the key condition; bounds intersect (`price >= 10
  AND price < 20` is one range). Folded conjuncts are dropped from the
  residual — the range is the check.
- **Multi-valued index paths** (a `[*]` step): comparisons are
  existential (§6), so only **equality** is servable (one document
  appears at most once under one key); the first equality conjunct in
  statement order is the key condition, later path conjuncts are
  residual. Ranges/`begins_with` on a multi-valued path are rejected
  under explicit naming (§7 `multi-valued`) and are simply not
  candidates under path matching.
- **Key-condition literals are family-strict:** the literal must match
  the index's declared type ({i64, f64} interchange exactly per the
  ADR-0074 truth table — `price = 10` serves an f64 index; `price =
  10.5` on an i64 index compiles to the empty range since no integer
  equals it). A utf8/bool family mismatch is a rejection (§7 `key type`).
  Residual comparisons keep full document semantics (§6) — the
  strictness applies to the index probe only.
- Reversed `BETWEEN` (and any statically empty intersection) compiles to
  a valid, empty range: **zero rows, never an error**.

**Declared-type partiality (disclosed):** a typed index sees only values
that admit into its declared type (ADR-0074 D4) — an f64 index does not
contain integers beyond exact f64 representation (counted as
`idx_skipped_inexact`), no index contains nulls or type-mismatched
values. Queries **through an index** answer over the index's declared
contents. The equivalence oracle (S15) checks implementation against
these declared semantics.

## 6. Evaluation semantics (the residual, and what a range means)

- **Existential multi-match:** a comparison is true iff **any** value the
  path resolves to satisfies it (`tags[*] = 'x'` with tags
  `["a","x"]` is true). Consequently on multi-match paths
  **`!=` is not `NOT(=)`**: `[1, 2] != 1` is true (2 ≠ 1) while
  `NOT([1, 2] = 1)` is false. Both are expressible; they mean different
  things.
- **Two-valued verdicts + flags:** there is no SQL `UNKNOWN`. A missing
  path or a type-mismatched comparison is **false** and sets the
  `MISSING` / `TYPE_MISMATCH` flag; `NOT` flips verdicts only, so
  `NOT(missing = 5)` is true-with-`MISSING`. Flags are **leaf-atomic**:
  an evaluated comparison tests every resolved value (flags are a pure
  function of the match set), but operands skipped by AND/OR
  short-circuit contribute no flags. `exists(path)` is true iff the path
  resolves at all (explicit `null` and containers exist) and never sets
  `MISSING`.
- **Numbers:** one truth table (ADR-0074): `3 = 3.0` is true; `10 IN
  (10.0)` is true; comparisons across i64/f64 are exact, never lossy.
- **Fuel:** every statement evaluates under a fuel budget (1 per opcode
  decoded, 1 per document node visited, 1 per IN member tested; an IN
  list over an incomparable family classifies once). Exhaustion is a
  **typed error** — never a truncated or silently-false result.

## 7. Rejected productions — the exact strings

Offsets are byte offsets into the statement. `{…}` marks a value
interpolated verbatim.

| Class | String |
|---|---|
| size | `statement exceeds the size limit` |
| encoding | `statement is not valid UTF-8 at offset {o}` |
| lexical | `unexpected character at offset {o}` |
| lexical | `unterminated string literal at offset {o}` |
| lexical | `invalid number literal at offset {o}` |
| lexical | `integer literal out of i64 range at offset {o}` |
| lexical | `number literal overflows f64 at offset {o}` |
| syntax | `expected {what} at offset {o}` |
| syntax | `unexpected input after statement end at offset {o}` |
| depth | `WHERE nesting exceeds depth 32 at offset {o}` |
| path | `recursive descent ('..') is not allowed in statement paths at offset {o}` |
| path | `slices and unions are not allowed in statement paths at offset {o}` |
| path | `invalid document path at offset {o}` |
| arity | `IN list must hold 1 to 100 members at offset {o}` |
| arity | `LIMIT must be between 1 and 4294967295 at offset {o}` |
| function | `unknown function at offset {o} (supported: begins_with, exists)` |
| function | `begins_with takes (path, 'prefix') at offset {o}` |
| function | `exists takes (path) at offset {o}` |
| pseudo-path | `unknown pseudo-path at offset {o} ($key is the only pseudo-path)` |
| projection | `unsupported projection: only * and COUNT(*) are in the subset` |
| production | `unsupported: ORDER BY (results follow index-key order)` |
| production | `unsupported: GROUP BY` |
| production | `unsupported: HAVING` |
| production | `unsupported: JOIN (statements read one namespace)` |
| production | `unsupported: OFFSET (pages resume from cursors)` |
| production | `unsupported: INSERT/UPDATE/DELETE (mutations use the native command set)` |
| production | `unsupported: IS NULL / IS MISSING (null is never indexed; exists(path) tests presence)` |
| production | `unsupported: LIKE (begins_with(path, 'prefix') is the indexed prefix test)` |
| production | `unsupported: comparison to NULL (null is never indexed; exists(path) tests presence)` |
| families | `IN members must share one type family` |
| families | `BETWEEN bounds must share one type family` |
| resolution | `unknown namespace '{ns}'` |
| resolution | `unknown index '{name}'` |
| resolution | `index '{name}' is {state}; only ready indexes serve queries` |
| resolution | `the WHERE clause matches more than one ready index ('{a}', '{b}'); name one: FROM ns."index"` |
| resolution | `no key condition names the primary key or a ready index; declare one (INF.IDX CREATE) or scan with explicit consent (FROM ns.SCAN)` |
| resolution | `index '{name}' is over a multi-valued path; only equality is servable (ranges would page duplicates)` |
| resolution | `key condition value does not match index '{name}' declared type {type}` |
| resolution | `no key condition constrains index '{name}'; walking a whole index needs FROM ns.SCAN consent` |
| primary key | `the primary key supports equality only ($key = 'k'); ranges need a declared index` |
| primary key | `$key is only valid as a top-level AND conjunct '$key = <string>'` |
| primary key | `$key compares to a string literal` |
| primary key | `more than one $key condition` |
| primary key | `$key does not combine with FROM ns.SCAN (point lookups use FROM ns)` |
| primary key | `primary key literals must be 1 to 255 bytes` |
| paging | `LIMIT does not apply to COUNT(*) (each page is bounded by the page budget and returns a partial count)` |
| bounds | `the WHERE clause exceeds 256 operations` |
| bounds | `the WHERE clause exceeds 64 distinct paths` |
| bounds | `the WHERE clause exceeds 512 constants` |
| bounds | `the compiled statement exceeds the program size ceiling` |

`expected {what}` uses these `{what}` spellings: `SELECT`, `* or
COUNT(*)`, `FROM`, `a namespace`, `an index name or SCAN`, `a
condition`, `a document path`, `a literal`, `an integer`, `'('`, `')'`,
`']'`, `',' or ')'`, `AND`, `BETWEEN or IN`.

## 8. Paging, `LIMIT`, and `COUNT(*)`

- Results page through opaque cursors (wire format: S11). Pages are
  bounded by the server's page budget in **entries scanned**, not
  entries matched — a selective filter cannot make one page unbounded.
- `LIMIT n` caps the **total** matched documents across pages (SQL
  semantics). The page budget is a server bound; `LIMIT` is a statement
  bound.
- **`COUNT(*)` is cursor-paged like `SELECT`**: each page returns a
  partial count plus a cursor, and clients sum the pages. Replies carry
  both the matched count and the scanned total — the DynamoDB
  `Count`/`ScannedCount` analogy: `Count` = documents matching the
  statement in this page, `ScannedCount` = index entries examined for
  it. `LIMIT` with `COUNT(*)` is rejected (§7 `paging`).
- Pages observe per-cell read-committed state; cross-page snapshot
  semantics are documented with the execution surface (S11).

## 9. EXPLAIN

`INF.QL EXPLAIN <stmt>` renders the compiled access program —
deterministic text, stable field order: namespace id, projection, limit,
the one access step (typed bounds where the S02 debug decoder applies;
`hex:` for boundary byte strings no canonical key produces, e.g. a
`begins_with` prefix-successor), and the residual op listing. The golden
outputs in the S09 suite are the rendering contract; S12 wires the
command surface and renders these bytes, never a re-derivation.
