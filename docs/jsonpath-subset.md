# JSONPath Subset — Grammar Specification (M3-S08)

Normative grammar for the InfinityDB JSONPath subset (master plan §10.1;
milestone M3 §2/§5). The compiled form — path-program bytecode v1 — and
the evaluation contract are frozen by
[ADR-0040](../../docs/adr/0040-m3-s08-s09-jsonpath-and-path-programs.md);
this document owns what *text* is accepted and what it means. Behavior
that RedisJSON decides differently surfaces at S21 as an oracle diff and
lands in the deviation allowlist — the grammar itself changes only by
ADR.

Filter expressions are **not in this grammar** (M4.5, ADR-0024): a `?(`
token anywhere a selector may start is rejected with the documented
error (§6). This is a cut line, not an omission (L8).

## 1. Modes

| Mode | Trigger | Root | Recorded |
|---|---|---|---|
| `$` mode | first byte is `$` | explicit `$` | `flags.legacy = 0` |
| legacy | anything else (including empty) | implicit | `flags.legacy = 1` |

Legacy paths parse with an implicit root: `""` and `"."` mean the root
itself; `.foo.bar`, `foo.bar`, `foo[2]` are equivalent to
`$.foo.bar`, `$.foo.bar`, `$.foo[2]`. The selector grammar is otherwise
**identical** in both modes — mode is recorded on the compiled program
and changes reply shapes and missing-path behavior at the command layer
(S11/S15), never traversal. (RedisJSON v2 routes legacy paths through
its one parser the same way; per-command legacy quirks are pinned
against the oracle at S21.)

## 2. Grammar (ABNF-flavored; bytes, not chars, except where stated)

```abnf
path            = dollar-path / legacy-path
dollar-path     = "$" segments
legacy-path     = segments-opt-lead-dot          ; "" and "." are the root

segments        = *( child-segment / descend-segment )
child-segment   = "." shorthand-or-star / bracket
descend-segment = ".." ( shorthand-or-star / bracket )

shorthand-or-star = shorthand-name / "*"
shorthand-name  = name-first *name-char
name-first      = ALPHA / "_" / %x80-FF          ; UTF-8 continuation bytes pass through
name-char       = name-first / DIGIT

bracket         = "[" ws selector-list ws "]"
selector-list   = "*" / selector *( ws "," ws selector )   ; ≥ 2 selectors = union
selector        = quoted-name / slice / index
index           = int
slice           = [ int ] ":" [ int ] [ ":" [ int ] ]
int             = [ "-" ] 1*DIGIT                ; canonical: no leading zeros, no "-0"

quoted-name     = "'" *sq-char "'" / DQUOTE *dq-char DQUOTE
sq-char         = %x20-26 / %x28-5B / %x5D-FF / escape / "\'"   ; no raw ' or \
dq-char         = %x20-21 / %x23-5B / %x5D-FF / escape / "\""   ; no raw " or \
escape          = "\" ( "\" / "/" / "b" / "f" / "n" / "r" / "t"
                      / "u" 4HEXDIG )            ; JSON escapes; \uXXXX with
                                                 ; surrogate-pair rules as in JSON strings
ws              = *( SP / HTAB )                 ; inside brackets only
```

Notes, binding:

- **Shorthand names** follow RFC 9535's name-shorthand shape (ASCII
  alpha/underscore/digit plus non-ASCII UTF-8); anything else — spaces,
  punctuation, an empty name, a leading digit — must be bracket-quoted.
  The whole path must be valid UTF-8 (it names keys in UTF-8 documents).
- **Quoted names** use JSON string escape semantics exactly (same code
  path discipline as the S05 parser: `\uXXXX` lone surrogates rejected,
  raw control bytes < 0x20 rejected). Additionally `\'` is valid inside
  single quotes and `\"` inside double quotes; the *other* quote kind
  may appear raw.
- **Whitespace** (space/tab) is permitted only inside brackets, around
  selectors, commas, and slice colons. `$ .a`, `$. a`, and `a .b` are
  errors.
- **Slices** require at least one colon (that is what distinguishes them
  from indices). `step = 0` is a parse error (`BadSlice`). Bounds and
  step are i64; omitted fields keep Python defaults at evaluation (§4).
- **Unions** hold 2..=16 selectors of kinds {quoted-name, index, slice}
  — `*` inside a union and nested brackets are rejected
  (`BadUnionMember`). The 16-member cap is an explicit limit (bounded
  everything); raising it is an ADR event because programs replay from
  logs. A single-selector bracket is not a union — it compiles to the
  plain selector.
- **`..` requires a selector**: `$..` at end of input is `TrailingDescend`.
  `$..[...]` and `$..*` and `$..name` are all valid.
- **`$` after position 0** (outside quotes) is an error; `$` is not a
  shorthand name byte.

## 3. What each construct selects (evaluation semantics, summary)

Authoritative op-by-op semantics live in ADR-0040 §Decision 4; this table
is the reader's map.

| Text | Ops | Selects |
|---|---|---|
| `$` | `Root` | the root value |
| `.name` / `['name']` | `Child(name)` | member `name` of an object; nothing on non-objects |
| `.*` / `[*]` | `ChildAny` | every member value (objects, insertion order) / every element (arrays) |
| `[3]` / `[-1]` | `Index(3)` / `Index(-1)` | array element, negatives from the end; nothing on non-arrays or out of range. Indices are `i64`; a resolved index outside `[0, len)` selects nothing — including every value beyond the `u32` ordinal width, which never wraps onto a real element (review C10) |
| `[a:b:s]` | `Slice(a,b,s)` | Python slice semantics over arrays: negatives resolved against `len`, then clamped; `s < 0` walks backward; omitted fields default per Python (`s` omitted = 1; `a`/`b` defaults depend on sign of `s`); nothing on non-arrays |
| `[x, 1, a:b]` | `Union(n)` + members | concatenation of member selections, member order, duplicates kept (canonicalized to document order + deduplicated for mutation, ADR-0040 D5/R5) |
| `..sel` | `Descend` + sel | `sel` applied to the node itself and every descendant, pre-order (document order) |

## 4. Slice resolution (pinned)

For array length `len`, with `s` the step (default 1, never 0):

- `s > 0`: `a` defaults to 0, `b` to `len`; negatives add `len`; then
  `a` clamps to `[0, len]`, `b` clamps to `[0, len]`; yields indices
  `a, a+s, …` while `< b`.
- `s < 0`: `a` defaults to `len−1`, `b` to *before the beginning*
  (sentinel −len−1 pre-add); negatives add `len`; `a` clamps to
  `[−1, len−1]`, `b` clamps to `[−1, len−1]`; yields `a, a+s, …` while
  `> b`.

This is exactly Python's `list[a:b:s]` index set, the behavior RFC 9535
specifies and the RedisJSON implementation family inherits; the S21
oracle corpus pins the edges (`[::-1]`, `[-1:]`, `[:0:-1]`, over-range
bounds, `len 0`).

`s` is any non-zero `i64` and the walk is total: the cursor advances by
saturating addition, so a step of any magnitude yields exactly the index
set above (a step ≥ `len` from any in-range `a` yields one element) and
never wraps — `[1::9223372036854775807]` on `[10,20,30]` is `[20]`
(review C11).

## 5. Canonical printing (the `parse(print(ast)) == ast` contract)

The printer exists for tests, diagnostics, and the S15 matrix — programs
are cached and logged as bytecode, never as re-printed text.

- `$` mode prints the `$`; legacy prints with no root and dot-leading
  segments (`.a.b`); the legacy empty path prints as `.`.
- Shorthand-eligible names print as `.name`; everything else prints
  bracket-quoted with **single quotes**, escaping `\` and `'` and
  emitting control bytes as `\u00XX` (lowercase hex, the S06 discipline).
- Indices print bare; slices print only the fields the AST carries
  (`[::2]` keeps its shape); unions print comma-separated with no
  spaces; wildcard prints `.*` after a dot segment and `[*]` where it
  was bracketed — the AST records which form arrived.
- Descend prints `..` fused to its selector (`..name`, `..*`, `..[0]`).

## 6. Rejections (typed; RESP phrasing pinned at S11 against the oracle)

| Input shape | Error kind |
|---|---|
| `?(` where a selector may start | `FilterUnsupported` — message: `filter expressions are not supported (planned for M4.5)` |
| byte that fits no production | `UnexpectedChar { offset }` |
| unterminated `'…` / `"…` / missing `]` | `Unterminated { offset }` |
| bad escape / bad `\uXXXX` / lone surrogate | `BadEscape { offset }` |
| leading zeros, `-0`, > i64 magnitude | `BadNumber { offset }` |
| slice step 0 | `BadSlice { offset }` |
| empty union member, `*` in union, > 16 members | `BadUnionMember { offset }` |
| `..` with no selector | `TrailingDescend { offset }` |
| path text > `doc_max_path_bytes` (default 4 KiB, ceiling 64 KiB − 1) | `PathTooLong` |
| > 128 segments | `PathTooDeep` |
| invalid UTF-8 anywhere | `InvalidUtf8 { offset }` |

Limits are ADR-0040 D6; they exist because compiled programs are durable
(`DocDelta` payloads) and every decoder is bounded (L9).

## 7. Test contract

- ≥ 200-case table-driven suite (valid → expected AST/print; invalid →
  expected error kind and offset), **re-derived from the RedisJSON
  documentation and RFC 9535** — RedisJSON's own test files are
  RSALv2/SSPL-licensed and are not copied (the licensing arm of the S08
  AC, recorded in the ledger).
- Property: `parse(print(ast)) == ast` over generated ASTs (both modes).
- The `fuzz_path_program` target (S09) also drives this parser: text →
  compile → decode ≡ intent, arbitrary bytes never panic.
