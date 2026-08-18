//! The S09 table-driven suite (plan AC 1): every case compiles one
//! statement against the fixture catalog and pins either the golden
//! EXPLAIN rendering or the exact documented rejection string —
//! `docs/partiql-subset.md` §7 verbatim. There is no external oracle
//! for `INF.QL`: this file and its golden are the compat contract
//! (L8), so a diff here is a compat decision, not a test fixup.
//!
//! Golden: `tests/golden/partiql_suite.txt`. Bless with
//! `PARTIQL_BLESS=1 cargo test -p inf-query --test partiql_suite`.
//! Every accepted case additionally proves: recompilation determinism
//! (byte-identical programs — L7), serialized round-trip through the
//! `from_bytes` trust boundary, and rendering determinism.

use std::fmt::Write as _;

use inf_doc::path;
use inf_query::access::AccessProgram;
use inf_query::partiql::{CatalogView, compile};
use inf_store::{IndexId, IndexKeyType, IndexSpec, IndexState, NsId};

// ---------------------------------------------------------------------
// Fixture catalog
// ---------------------------------------------------------------------

struct FixtureCatalog {
    namespaces: Vec<(&'static str, NsId)>,
    specs: Vec<IndexSpec>,
}

impl CatalogView for FixtureCatalog {
    fn resolve_ns(&self, name: &[u8]) -> Option<NsId> {
        self.namespaces.iter().find(|(n, _)| n.as_bytes() == name).map(|&(_, id)| id)
    }

    fn index_by_name(&self, ns: NsId, name: &[u8]) -> Option<&IndexSpec> {
        self.specs.iter().find(|s| s.ns == ns && s.name == name)
    }

    fn indexes(&self, ns: NsId) -> impl Iterator<Item = &IndexSpec> {
        self.specs.iter().filter(move |s| s.ns == ns)
    }

    fn catalog_epoch(&self) -> u64 {
        42
    }
}

fn spec(
    id: u32,
    ns: u32,
    name: &str,
    text: &str,
    key_type: IndexKeyType,
    state: IndexState,
) -> IndexSpec {
    IndexSpec {
        id: IndexId(id),
        generation: u64::from(id % 3 + 1),
        ns: NsId(ns),
        name: name.as_bytes().to_vec(),
        program: path::compile(text.as_bytes()).expect("fixture path").as_bytes().to_vec(),
        key_type,
        state,
    }
}

fn fixture() -> FixtureCatalog {
    use IndexKeyType::{Bool, F64, I64, Utf8};
    use IndexState::{Backfilling, Ready};
    FixtureCatalog {
        namespaces: vec![
            ("orders", NsId(1)),
            ("dup", NsId(2)),
            ("empty", NsId(3)),
            ("my.ns", NsId(4)),
        ],
        specs: vec![
            spec(1, 1, "price_idx", "$.price", I64, Ready),
            spec(2, 1, "score_idx", "$.score", F64, Ready),
            spec(3, 1, "name_idx", "$.name", Utf8, Ready),
            spec(4, 1, "active_idx", "$.active", Bool, Ready),
            spec(5, 1, "tags_idx", "$.tags[*]", Utf8, Ready),
            spec(6, 1, "pending_idx", "$.pending", I64, Backfilling),
            spec(7, 1, "region_idx", "$.region", Utf8, Ready),
            spec(8, 1, "nested_idx", "$.meta.depth", I64, Ready),
            spec(9, 1, "item0_idx", "$.items[0].sku", Utf8, Ready),
            spec(10, 1, "nums_idx", "$.nums[*]", I64, Ready),
            spec(20, 2, "p1", "$.price", I64, Ready),
            spec(21, 2, "p2", "$.price", I64, Ready),
            spec(30, 4, "m_idx", "$.v", I64, Ready),
            spec(31, 4, "dot.idx", "$.w", I64, Ready),
        ],
    }
}

// ---------------------------------------------------------------------
// Cases
// ---------------------------------------------------------------------

#[rustfmt::skip]
const CASES: &[(&str, &str)] = &[
    // --- A. statement shapes over the primary key ---
    ("pk-point", "SELECT * FROM orders WHERE $key = 'user:1'"),
    ("pk-count", "SELECT COUNT(*) FROM orders WHERE $key = 'user:1'"),
    ("pk-lowercase-keywords", "select * from orders where $key = 'a'"),
    ("pk-mixed-case-keywords", "SeLeCt * FrOm orders WhErE $key = 'a'"),
    ("pk-semicolon", "SELECT * FROM orders WHERE $key = 'a';"),
    ("pk-tabs", "SELECT\t*\tFROM\torders\tWHERE\t$key = 'a'"),
    ("pk-residual-one", "SELECT * FROM orders WHERE $key = 'u' AND price > 10"),
    ("pk-residual-two", "SELECT * FROM orders WHERE $key = 'u' AND price > 10 AND name = 'x'"),
    ("pk-residual-not-first", "SELECT * FROM orders WHERE price > 10 AND $key = 'u'"),
    ("pk-limit", "SELECT * FROM orders WHERE $key = 'u' LIMIT 100"),
    ("pk-limit-max", "SELECT * FROM orders WHERE $key = 'u' LIMIT 4294967295"),
    ("pk-limit-one", "SELECT * FROM orders WHERE $key = 'u' LIMIT 1"),
    ("pk-parenthesized", "SELECT * FROM orders WHERE ($key = 'a')"),
    ("pk-case-insensitive-pseudo", "SELECT * FROM orders WHERE $KEY = 'a'"),
    ("pk-quoted-escape", "SELECT * FROM orders WHERE $key = 'it''s'"),

    // --- B. i64 index: same-type bounds ---
    ("i64-eq", "SELECT * FROM orders WHERE price = 10"),
    ("i64-lt", "SELECT * FROM orders WHERE price < 10"),
    ("i64-le", "SELECT * FROM orders WHERE price <= 10"),
    ("i64-gt", "SELECT * FROM orders WHERE price > 10"),
    ("i64-ge", "SELECT * FROM orders WHERE price >= 10"),
    ("i64-between", "SELECT * FROM orders WHERE price BETWEEN 5 AND 10"),
    ("i64-between-point", "SELECT * FROM orders WHERE price BETWEEN 7 AND 7"),
    ("i64-between-reversed", "SELECT * FROM orders WHERE price BETWEEN 10 AND 5"),
    ("i64-intersect-range", "SELECT * FROM orders WHERE price >= 5 AND price < 10"),
    ("i64-intersect-tighter", "SELECT * FROM orders WHERE price > 10 AND price > 20"),
    ("i64-intersect-le-lt-same", "SELECT * FROM orders WHERE price <= 10 AND price < 10"),
    ("i64-intersect-eq-range", "SELECT * FROM orders WHERE price = 5 AND price < 10"),
    ("i64-intersect-contradiction", "SELECT * FROM orders WHERE price > 10 AND price < 5"),
    ("i64-intersect-two-eq", "SELECT * FROM orders WHERE price = 5 AND price = 7"),
    ("i64-negative", "SELECT * FROM orders WHERE price >= -3"),
    ("i64-min", "SELECT * FROM orders WHERE price = -9223372036854775808"),
    ("i64-max", "SELECT * FROM orders WHERE price = 9223372036854775807"),
    ("i64-triple-fold", "SELECT * FROM orders WHERE price >= 1 AND price <= 9 AND price < 8"),

    // --- B2. i64 index: coerced f64 literals (ADR-0080 D3) ---
    ("i64-f64-eq-integral", "SELECT * FROM orders WHERE price = 10.0"),
    ("i64-f64-eq-fractional", "SELECT * FROM orders WHERE price = 10.5"),
    ("i64-f64-gt-fractional", "SELECT * FROM orders WHERE price > 10.5"),
    ("i64-f64-gt-integral", "SELECT * FROM orders WHERE price > 10.0"),
    ("i64-f64-ge-fractional", "SELECT * FROM orders WHERE price >= 10.5"),
    ("i64-f64-lt-fractional", "SELECT * FROM orders WHERE price < 10.5"),
    ("i64-f64-le-fractional", "SELECT * FROM orders WHERE price <= 10.5"),
    ("i64-f64-negative-fraction-gt", "SELECT * FROM orders WHERE price > -2.5"),
    ("i64-f64-negative-fraction-lt", "SELECT * FROM orders WHERE price < -2.5"),
    ("i64-f64-huge-lt", "SELECT * FROM orders WHERE price < 1e20"),
    ("i64-f64-huge-gt", "SELECT * FROM orders WHERE price > 1e20"),
    ("i64-f64-huge-eq", "SELECT * FROM orders WHERE price = 1e20"),
    ("i64-f64-negative-huge-gt", "SELECT * FROM orders WHERE price > -1e20"),
    ("i64-f64-negative-huge-lt", "SELECT * FROM orders WHERE price < -1e20"),
    ("i64-f64-two-pow-63-eq", "SELECT * FROM orders WHERE price = 9.223372036854776e18"),
    ("i64-f64-below-two-pow-63-gt", "SELECT * FROM orders WHERE price > 9.223372036854775e18"),
    ("i64-f64-neg-two-pow-63-eq", "SELECT * FROM orders WHERE price = -9.223372036854776e18"),
    ("i64-f64-neg-zero", "SELECT * FROM orders WHERE price = -0.0"),
    ("i64-f64-between-fractional", "SELECT * FROM orders WHERE price BETWEEN 1.5 AND 3.5"),

    // --- C. f64 index: same-type and coerced i64 literals ---
    ("f64-eq", "SELECT * FROM orders WHERE score = 10.5"),
    ("f64-lt", "SELECT * FROM orders WHERE score < 10.5"),
    ("f64-le", "SELECT * FROM orders WHERE score <= 10.5"),
    ("f64-gt", "SELECT * FROM orders WHERE score > 10.5"),
    ("f64-ge", "SELECT * FROM orders WHERE score >= 10.5"),
    ("f64-between", "SELECT * FROM orders WHERE score BETWEEN 0.5 AND 2.0"),
    ("f64-between-reversed", "SELECT * FROM orders WHERE score BETWEEN 5.0 AND 1.0"),
    ("f64-exponent", "SELECT * FROM orders WHERE score > 1e3"),
    ("f64-max-magnitude", "SELECT * FROM orders WHERE score <= 1e308"),
    ("f64-denormal", "SELECT * FROM orders WHERE score > 5e-324"),
    ("f64-neg-zero", "SELECT * FROM orders WHERE score = -0.0"),
    ("f64-i64-eq-lossless", "SELECT * FROM orders WHERE score = 10"),
    ("f64-i64-gt-lossless", "SELECT * FROM orders WHERE score > 10"),
    ("f64-i64-two-pow-53", "SELECT * FROM orders WHERE score = 9007199254740992"),
    ("f64-i64-eq-lossy", "SELECT * FROM orders WHERE score = 9007199254740993"),
    ("f64-i64-gt-lossy", "SELECT * FROM orders WHERE score > 9007199254740993"),
    ("f64-i64-ge-lossy", "SELECT * FROM orders WHERE score >= 9007199254740993"),
    ("f64-i64-lt-lossy", "SELECT * FROM orders WHERE score < 9007199254740993"),
    ("f64-i64-le-lossy", "SELECT * FROM orders WHERE score <= 9007199254740993"),
    ("f64-i64-neg-lossy-gt", "SELECT * FROM orders WHERE score > -9007199254740993"),
    ("f64-i64-max-lossy", "SELECT * FROM orders WHERE score < 9223372036854775807"),
    ("f64-i64-between-mixed", "SELECT * FROM orders WHERE score BETWEEN 1 AND 2.5"),

    // --- D. utf8 index ---
    ("utf8-eq", "SELECT * FROM orders WHERE name = 'alice'"),
    ("utf8-eq-empty", "SELECT * FROM orders WHERE name = ''"),
    ("utf8-lt", "SELECT * FROM orders WHERE name < 'bob'"),
    ("utf8-le", "SELECT * FROM orders WHERE name <= 'bob'"),
    ("utf8-gt", "SELECT * FROM orders WHERE name > 'alice'"),
    ("utf8-ge", "SELECT * FROM orders WHERE name >= 'alice'"),
    ("utf8-between", "SELECT * FROM orders WHERE name BETWEEN 'a' AND 'b'"),
    ("utf8-between-reversed", "SELECT * FROM orders WHERE name BETWEEN 'b' AND 'a'"),
    ("utf8-unicode", "SELECT * FROM orders WHERE name = 'café'"),
    ("utf8-quote-escape", "SELECT * FROM orders WHERE name = 'it''s'"),
    ("utf8-begins-with", "SELECT * FROM orders WHERE begins_with(name, 'al')"),
    ("utf8-begins-with-empty", "SELECT * FROM orders WHERE begins_with(name, '')"),
    ("utf8-begins-with-unicode", "SELECT * FROM orders WHERE begins_with(name, 'café')"),
    ("utf8-begins-with-and-range", "SELECT * FROM orders WHERE begins_with(name, 'al') AND name < 'alz'"),
    ("utf8-begins-with-case-fn", "SELECT * FROM orders WHERE BEGINS_WITH(name, 'al')"),

    // --- E. bool index ---
    ("bool-eq-true", "SELECT * FROM orders WHERE active = TRUE"),
    ("bool-eq-false", "SELECT * FROM orders WHERE active = FALSE"),
    ("bool-eq-lowercase", "SELECT * FROM orders WHERE active = true"),
    ("bool-gt-false", "SELECT * FROM orders WHERE active > FALSE"),
    ("bool-between", "SELECT * FROM orders WHERE active BETWEEN FALSE AND TRUE"),
    ("bool-ne-not-servable", "SELECT * FROM orders WHERE active != TRUE"),

    // --- F. multi-valued index paths (ADR-0080 D1). The statement
    // spells the declared path — `tags[*]`, the per-element view; a
    // bare `tags` names the array node itself and matches no index. ---
    ("multi-eq", "SELECT * FROM orders WHERE tags[*] = 'x'"),
    ("multi-eq-second-residual", "SELECT * FROM orders WHERE tags[*] = 'x' AND tags[*] = 'y'"),
    ("multi-bare-array-node", "SELECT * FROM orders WHERE tags = 'x'"),
    ("multi-range-not-candidate", "SELECT * FROM orders WHERE tags[*] > 'a'"),
    ("multi-range-explicit", "SELECT * FROM orders.tags_idx WHERE tags[*] > 'a'"),
    ("multi-begins-with-explicit", "SELECT * FROM orders.tags_idx WHERE begins_with(tags[*], 'a')"),
    ("multi-eq-plus-range-explicit", "SELECT * FROM orders.tags_idx WHERE tags[*] = 'x' AND tags[*] > 'a'"),
    ("multi-eq-explicit", "SELECT * FROM orders.tags_idx WHERE tags[*] = 'x'"),
    ("multi-numeric-eq", "SELECT * FROM orders WHERE nums[*] = 5"),
    ("multi-numeric-range-explicit", "SELECT * FROM orders.nums_idx WHERE nums[*] BETWEEN 1 AND 5"),
    ("multi-anchor-plus-single", "SELECT * FROM orders WHERE tags[*] = 'x' AND price > 10"),

    // --- G. resolution: path matching, naming, ambiguity ---
    ("resolve-two-indexes-ambiguous", "SELECT * FROM orders WHERE price > 10 AND region = 'eu'"),
    ("resolve-explicit-disambiguation", "SELECT * FROM orders.price_idx WHERE price > 10 AND region = 'eu'"),
    ("resolve-explicit-other-side", "SELECT * FROM orders.region_idx WHERE price > 10 AND region = 'eu'"),
    ("resolve-explicit-quoted", "SELECT * FROM orders.\"region_idx\" WHERE region = 'eu'"),
    ("resolve-not-ready", "SELECT * FROM orders WHERE pending = 3"),
    ("resolve-not-ready-explicit", "SELECT * FROM orders.pending_idx WHERE pending = 3"),
    ("resolve-no-candidates", "SELECT * FROM orders WHERE qty = 3"),
    ("resolve-no-where", "SELECT * FROM orders"),
    ("resolve-nested-path", "SELECT * FROM orders WHERE meta.depth > 4"),
    ("resolve-array-index-path", "SELECT * FROM orders WHERE items[0].sku = 'a'"),
    ("resolve-bracket-spelling-double", "SELECT * FROM orders WHERE [\"price\"] > 10"),
    ("resolve-bracket-spelling-single", "SELECT * FROM orders WHERE ['price'] > 10"),
    ("resolve-bracket-mixed", "SELECT * FROM orders WHERE ['meta'].depth = 2"),
    ("resolve-dup-path-ambiguous", "SELECT * FROM dup WHERE price = 1"),
    ("resolve-dup-explicit", "SELECT * FROM dup.p1 WHERE price = 1"),
    ("resolve-unknown-ns", "SELECT * FROM nowhere WHERE price = 1"),
    ("resolve-unknown-index", "SELECT * FROM orders.missing_idx WHERE price = 1"),
    ("resolve-unconstrained-index", "SELECT * FROM orders.price_idx WHERE region = 'eu'"),
    ("resolve-dotted-ns-quoted", "SELECT * FROM \"my.ns\" WHERE v = 1"),
    ("resolve-dotted-ns-unquoted", "SELECT * FROM my.ns WHERE v = 1"),
    ("resolve-dotted-index-quoted", "SELECT * FROM \"my.ns\".\"dot.idx\" WHERE w = 1"),
    ("resolve-three-part-target", "SELECT * FROM a.b.c WHERE v = 1"),
    ("resolve-or-not-servable", "SELECT * FROM orders WHERE (price > 10 OR price < 5)"),
    ("resolve-ne-not-servable", "SELECT * FROM orders WHERE price != 10"),
    ("resolve-in-not-servable", "SELECT * FROM orders WHERE price IN (1, 2)"),
    ("resolve-exists-not-servable", "SELECT * FROM orders WHERE exists(price)"),
    ("resolve-not-servable", "SELECT * FROM orders WHERE NOT price = 5"),

    // --- H. residuals riding a servable anchor ---
    ("residual-or", "SELECT * FROM orders WHERE price = 5 AND (qty = 1 OR qty = 2)"),
    ("residual-not-exists", "SELECT * FROM orders WHERE price = 5 AND NOT exists(deleted)"),
    ("residual-exists", "SELECT * FROM orders WHERE price = 5 AND exists(meta.flag)"),
    ("residual-in-i64", "SELECT * FROM orders WHERE price = 5 AND qty IN (1, 2, 3)"),
    ("residual-in-mixed-numeric", "SELECT * FROM orders WHERE price = 5 AND qty IN (1, 2.5)"),
    ("residual-in-utf8", "SELECT * FROM orders WHERE price = 5 AND city IN ('a', 'b')"),
    ("residual-in-bool", "SELECT * FROM orders WHERE price = 5 AND flag IN (TRUE, FALSE)"),
    ("residual-not-in", "SELECT * FROM orders WHERE price = 5 AND qty NOT IN (1, 2)"),
    ("residual-not-between", "SELECT * FROM orders WHERE price = 5 AND qty NOT BETWEEN 1 AND 9"),
    ("residual-ne-on-indexed-path", "SELECT * FROM orders WHERE price = 5 AND name != 'x'"),
    ("residual-in-on-indexed-path", "SELECT * FROM orders WHERE price = 5 AND price IN (1, 2)"),
    ("residual-exists-on-indexed-path", "SELECT * FROM orders WHERE price = 5 AND exists(name)"),
    ("residual-begins-with", "SELECT * FROM orders WHERE price = 5 AND begins_with(city, 'ab')"),
    ("residual-cross-family-cmp", "SELECT * FROM orders WHERE price = 5 AND city = 3"),
    ("residual-nested-parens", "SELECT * FROM orders WHERE price = 5 AND (a = 1 OR (b = 2 AND c = 3))"),
    ("residual-double-not", "SELECT * FROM orders WHERE price = 5 AND NOT NOT qty = 2"),
    ("residual-not-parens", "SELECT * FROM orders WHERE price = 5 AND NOT (a = 1 OR b = 2)"),
    ("residual-wildcard-path", "SELECT * FROM orders WHERE price = 5 AND items[*].qty > 2"),
    ("residual-deep-path", "SELECT * FROM orders WHERE price = 5 AND a.b.c.d = 1"),
    ("residual-or-of-ands", "SELECT * FROM orders WHERE price = 5 AND (a = 1 AND b = 2 OR c = 3)"),
    ("residual-between-utf8", "SELECT * FROM orders WHERE price = 5 AND city BETWEEN 'a' AND 'b'"),
    ("residual-keyword-attr-bracket", "SELECT * FROM orders WHERE price = 5 AND ['select'] = 1"),
    ("residual-attr-named-exists", "SELECT * FROM orders WHERE price = 5 AND exists = 1"),
    ("residual-attr-named-count", "SELECT * FROM orders WHERE price = 5 AND count = 1"),

    // --- I. $key rules (spec §4) ---
    ("pk-range-rejected", "SELECT * FROM orders WHERE $key > 'a'"),
    ("pk-between-rejected", "SELECT * FROM orders WHERE $key BETWEEN 'a' AND 'b'"),
    ("pk-in-rejected", "SELECT * FROM orders WHERE $key IN ('a')"),
    ("pk-numeric-literal", "SELECT * FROM orders WHERE $key = 3"),
    ("pk-bool-literal", "SELECT * FROM orders WHERE $key = TRUE"),
    ("pk-duplicate", "SELECT * FROM orders WHERE $key = 'a' AND $key = 'b'"),
    ("pk-under-or", "SELECT * FROM orders WHERE $key = 'a' OR price > 3"),
    ("pk-under-not", "SELECT * FROM orders WHERE NOT $key = 'a'"),
    ("pk-empty-key", "SELECT * FROM orders WHERE $key = ''"),
    ("pk-with-named-index", "SELECT * FROM orders.price_idx WHERE $key = 'a'"),
    ("pk-with-scan", "SELECT * FROM orders.SCAN WHERE $key = 'a'"),
    ("pk-unknown-pseudo", "SELECT * FROM orders WHERE $id = 'a'"),

    // --- J. scan consent (grammar lands here; execution is S14) ---
    ("scan-bare", "SELECT * FROM orders.SCAN"),
    ("scan-filter", "SELECT * FROM orders.SCAN WHERE price > 10"),
    ("scan-count", "SELECT COUNT(*) FROM orders.SCAN WHERE exists(deleted)"),
    ("scan-limit", "SELECT * FROM orders.SCAN LIMIT 50"),
    ("scan-lowercase", "SELECT * FROM orders.scan WHERE a = 1"),
    ("scan-quoted-is-index", "SELECT * FROM orders.\"SCAN\""),
    ("scan-empty-ns", "SELECT * FROM empty.SCAN"),
    ("scan-or-filter", "SELECT * FROM orders.SCAN WHERE a = 1 OR b = 2"),

    // --- K. COUNT(*) paging rules (ADR-0080 D4) ---
    ("count-index-range", "SELECT COUNT(*) FROM orders WHERE price > 10"),
    ("count-utf8", "SELECT COUNT(*) FROM orders WHERE begins_with(name, 'a')"),
    ("count-with-limit", "SELECT COUNT(*) FROM orders WHERE price > 10 LIMIT 5"),
    ("count-spaced", "SELECT COUNT (*) FROM orders WHERE price = 1"),
    ("count-lower", "select count(*) from orders where price = 1"),

    // --- L. key-condition family strictness ---
    ("family-utf8-index-numeric", "SELECT * FROM orders WHERE name = 3"),
    ("family-utf8-index-bool", "SELECT * FROM orders WHERE name = TRUE"),
    ("family-bool-index-numeric", "SELECT * FROM orders WHERE active = 1"),
    ("family-i64-index-string", "SELECT * FROM orders WHERE price = 'ten'"),
    ("family-i64-index-bool", "SELECT * FROM orders WHERE price = TRUE"),
    ("family-begins-with-i64-index", "SELECT * FROM orders WHERE begins_with(price, 'a')"),
    ("family-between-mixed-on-key", "SELECT * FROM orders WHERE price BETWEEN 1 AND 'a'"),
    ("family-between-mixed-utf8-key", "SELECT * FROM orders WHERE name BETWEEN 'a' AND 2"),

    // --- M. unsupported productions (documented rejections) ---
    ("reject-order-by", "SELECT * FROM orders WHERE price = 1 ORDER BY price"),
    ("reject-order-by-no-where", "SELECT * FROM orders ORDER BY price"),
    ("reject-group-by", "SELECT * FROM orders WHERE price = 1 GROUP BY name"),
    ("reject-having", "SELECT * FROM orders WHERE price = 1 HAVING price > 2"),
    ("reject-offset", "SELECT * FROM orders WHERE price = 1 LIMIT 5 OFFSET 10"),
    ("reject-offset-no-limit", "SELECT * FROM orders WHERE price = 1 OFFSET 10"),
    ("reject-join-keyword", "SELECT * FROM orders JOIN other WHERE price = 1"),
    ("reject-comma-join", "SELECT * FROM orders, other WHERE price = 1"),
    ("reject-column-projection", "SELECT price FROM orders WHERE price = 1"),
    ("reject-column-list", "SELECT price, name FROM orders"),
    ("reject-star-plus-column", "SELECT *, name FROM orders"),
    ("reject-distinct", "SELECT DISTINCT * FROM orders"),
    ("reject-count-column", "SELECT COUNT(price) FROM orders"),
    ("reject-count-bare", "SELECT count FROM orders"),
    ("reject-insert", "INSERT INTO orders VALUE {'a': 1}"),
    ("reject-update", "UPDATE orders SET price = 1"),
    ("reject-delete", "DELETE FROM orders WHERE price = 1"),
    ("reject-is-null", "SELECT * FROM orders WHERE price = 1 AND name IS NULL"),
    ("reject-is-not-null", "SELECT * FROM orders WHERE name IS NOT NULL"),
    ("reject-is-missing", "SELECT * FROM orders WHERE name IS MISSING"),
    ("reject-like", "SELECT * FROM orders WHERE name LIKE 'a%'"),
    ("reject-not-like", "SELECT * FROM orders WHERE name NOT LIKE 'a%'"),
    ("reject-eq-null", "SELECT * FROM orders WHERE name = NULL"),
    ("reject-ne-null", "SELECT * FROM orders WHERE name != NULL"),
    ("reject-null-in-list", "SELECT * FROM orders WHERE price = 1 AND a IN (1, NULL)"),
    ("reject-null-between", "SELECT * FROM orders WHERE price = 1 AND a BETWEEN NULL AND 2"),
    ("reject-mixed-in", "SELECT * FROM orders WHERE price = 1 AND a IN (1, 'x')"),
    ("reject-mixed-in-bool", "SELECT * FROM orders WHERE price = 1 AND a IN (TRUE, 1)"),
    ("reject-mixed-between-residual", "SELECT * FROM orders WHERE price = 1 AND a BETWEEN 1 AND 'z'"),
    ("reject-unknown-function", "SELECT * FROM orders WHERE contains(name, 'a')"),
    ("reject-attr-call", "SELECT * FROM orders WHERE price(3)"),

    // --- N. lexical and syntax rejections ---
    ("lex-unterminated-string", "SELECT * FROM orders WHERE name = 'alice"),
    ("lex-unterminated-quoted", "SELECT * FROM \"orders WHERE name = 'a'"),
    ("lex-bad-number-trailing-dot", "SELECT * FROM orders WHERE price = 1."),
    ("lex-bad-number-exponent", "SELECT * FROM orders WHERE price = 1e"),
    ("lex-int-overflow", "SELECT * FROM orders WHERE price = 9223372036854775808"),
    ("lex-int-underflow", "SELECT * FROM orders WHERE price = -9223372036854775809"),
    ("lex-float-overflow", "SELECT * FROM orders WHERE score = 1e999"),
    ("lex-unexpected-hash", "SELECT * FROM orders WHERE price = 1 # comment"),
    ("lex-unexpected-question", "SELECT * FROM orders WHERE price = ?"),
    ("lex-bare-bang", "SELECT * FROM orders WHERE price ! 1"),
    ("lex-bare-dollar", "SELECT * FROM orders WHERE $ = 1"),
    ("lex-bare-minus", "SELECT * FROM orders WHERE price = -"),
    ("syntax-empty", ""),
    ("syntax-just-select", "SELECT"),
    ("syntax-missing-from", "SELECT * WHERE price = 1"),
    ("syntax-missing-ns", "SELECT * FROM"),
    ("syntax-missing-index", "SELECT * FROM orders."),
    ("syntax-where-empty", "SELECT * FROM orders WHERE"),
    ("syntax-where-and-only", "SELECT * FROM orders WHERE AND"),
    ("syntax-bare-path", "SELECT * FROM orders WHERE price"),
    ("syntax-missing-literal", "SELECT * FROM orders WHERE price >"),
    ("syntax-double-op", "SELECT * FROM orders WHERE price > < 1"),
    ("syntax-literal-first", "SELECT * FROM orders WHERE 1 = price"),
    ("syntax-between-missing-and", "SELECT * FROM orders WHERE price BETWEEN 1 2"),
    ("syntax-in-missing-paren", "SELECT * FROM orders WHERE price IN 1, 2"),
    ("syntax-in-empty", "SELECT * FROM orders WHERE price IN ()"),
    ("syntax-in-unclosed", "SELECT * FROM orders WHERE price IN (1, 2"),
    ("syntax-in-semicolon", "SELECT * FROM orders WHERE price IN (1; 2)"),
    ("syntax-not-eq", "SELECT * FROM orders WHERE price NOT = 1"),
    ("syntax-unclosed-paren", "SELECT * FROM orders WHERE (price = 1"),
    ("syntax-unmatched-close", "SELECT * FROM orders WHERE price = 1)"),
    ("syntax-trailing-tokens", "SELECT * FROM orders WHERE price = 1 extra"),
    ("syntax-double-limit", "SELECT * FROM orders WHERE price = 1 LIMIT 5 LIMIT 6"),
    ("syntax-where-after-limit", "SELECT * FROM orders LIMIT 5 WHERE price = 1"),
    ("syntax-after-semicolon", "SELECT * FROM orders WHERE price = 1; SELECT"),
    ("syntax-limit-zero", "SELECT * FROM orders WHERE price = 1 LIMIT 0"),
    ("syntax-limit-negative", "SELECT * FROM orders WHERE price = 1 LIMIT -1"),
    ("syntax-limit-too-big", "SELECT * FROM orders WHERE price = 1 LIMIT 4294967296"),
    ("syntax-limit-float", "SELECT * FROM orders WHERE price = 1 LIMIT 1.5"),
    ("syntax-limit-string", "SELECT * FROM orders WHERE price = 1 LIMIT 'a'"),
    ("path-descend", "SELECT * FROM orders WHERE items..price > 3"),
    ("path-slice", "SELECT * FROM orders WHERE items[1:2] = 3"),
    ("path-slice-open", "SELECT * FROM orders WHERE items[:2] = 3"),
    ("path-union", "SELECT * FROM orders WHERE items[1,2] = 3"),
    ("path-union-names", "SELECT * FROM orders WHERE a['x','y'] = 3"),
    ("path-empty-bracket", "SELECT * FROM orders WHERE a[] = 3"),
    ("path-bare-ident-bracket", "SELECT * FROM orders WHERE a[b] = 3"),
    ("path-float-index", "SELECT * FROM orders WHERE a[1.5] = 3"),
    ("path-trailing-dot", "SELECT * FROM orders WHERE a. = 3"),
    ("path-dot-star", "SELECT * FROM orders WHERE a.* = 3"),
    ("path-dash-attr", "SELECT * FROM orders WHERE a-b = 3"),
    ("fn-begins-with-one-arg", "SELECT * FROM orders WHERE begins_with(name)"),
    ("fn-begins-with-numeric", "SELECT * FROM orders WHERE begins_with(name, 3)"),
    ("fn-begins-with-unclosed", "SELECT * FROM orders WHERE begins_with(name, 'a'"),
    ("fn-exists-two-args", "SELECT * FROM orders WHERE exists(name, price)"),
    ("fn-exists-empty", "SELECT * FROM orders WHERE exists()"),
    ("fn-exists-literal", "SELECT * FROM orders WHERE exists('a')"),

    // --- O. operator spellings and shapes ---
    ("op-ne-bang", "SELECT * FROM orders WHERE price = 1 AND a != 2"),
    ("op-ne-angle", "SELECT * FROM orders WHERE price = 1 AND a <> 2"),
    ("op-chain-precedence", "SELECT * FROM orders.SCAN WHERE a = 1 OR b = 2 AND c = 3"),
    ("op-not-precedence", "SELECT * FROM orders WHERE NOT a = 1 AND price = 5"),
    ("op-flat-and-chain", "SELECT * FROM orders WHERE price = 1 AND a = 2 AND b = 3 AND c = 4"),
    ("op-flat-or-chain", "SELECT * FROM orders.SCAN WHERE a = 1 OR b = 2 OR c = 3"),
    ("op-paren-nesting-kept", "SELECT * FROM orders WHERE price = 1 AND (a = 2 AND (b = 3 AND c = 4))"),
    ("op-or-group-with-anchor", "SELECT * FROM orders WHERE (a = 1 OR b = 2) AND price = 5"),
    ("count-multi-valued", "SELECT COUNT(*) FROM orders WHERE tags[*] = 'x'"),
    ("pk-limit-with-residual", "SELECT * FROM orders WHERE $key = 'u' AND price > 1 LIMIT 10"),
    ("scan-not-filter", "SELECT * FROM orders.SCAN WHERE NOT exists(a)"),
    ("i64-f64-between-vacuous", "SELECT * FROM orders WHERE price BETWEEN -1e20 AND 1e20"),
];

// ---------------------------------------------------------------------
// Generated cases (sizes and boundaries a literal table can't hold)
// ---------------------------------------------------------------------

fn generated_cases() -> Vec<(String, String)> {
    let mut cases = Vec::new();
    // 101-member IN list — one over the ADR-0079 D7 cap.
    let over_in: Vec<String> = (0..101).map(|i| i.to_string()).collect();
    cases.push((
        "gen-in-over-cap".to_string(),
        format!("SELECT * FROM orders WHERE price = 1 AND a IN ({})", over_in.join(", ")),
    ));
    // Exactly 100 members — the cap itself is legal.
    let at_in: Vec<String> = (0..100).map(|i| i.to_string()).collect();
    cases.push((
        "gen-in-at-cap".to_string(),
        format!("SELECT * FROM orders WHERE price = 1 AND a IN ({})", at_in.join(", ")),
    ));
    // A 70-conjunct AND chain over ONE path (distinct constants, so no
    // pool cap interferes): flattens to the 64-arity cap and nests
    // (ADR-0079 D2 — the golden shows the nesting shape).
    let chain: Vec<String> = (0..70).map(|i| format!("a = {i}")).collect();
    cases.push((
        "gen-and-chain-nests".to_string(),
        format!("SELECT * FROM orders WHERE price = 1 AND {}", chain.join(" AND ")),
    ));
    // 33 nested parens — one past the depth bound.
    cases.push((
        "gen-depth-over".to_string(),
        format!("SELECT * FROM orders WHERE {}price = 1{}", "(".repeat(33), ")".repeat(33)),
    ));
    // 31 nested parens — inside the bound.
    cases.push((
        "gen-depth-at".to_string(),
        format!("SELECT * FROM orders WHERE {}price = 1{}", "(".repeat(31), ")".repeat(31)),
    ));
    // 65 distinct residual paths — one over PATHS_MAX.
    let paths: Vec<String> = (0..65).map(|i| format!("p{i} = 1")).collect();
    cases.push((
        "gen-paths-over-cap".to_string(),
        format!("SELECT * FROM orders WHERE price = 1 AND {}", paths.join(" AND ")),
    ));
    // A 256-byte primary key — one over MAX_KEY_LEN.
    cases.push((
        "gen-pk-over-cap".to_string(),
        format!("SELECT * FROM orders WHERE $key = '{}'", "k".repeat(256)),
    ));
    // A 255-byte primary key — the cap itself is legal.
    cases.push((
        "gen-pk-at-cap".to_string(),
        format!("SELECT * FROM orders WHERE $key = '{}'", "k".repeat(255)),
    ));
    // Over-cap utf8 equality literal: no entry can exist (ADR-0074 D3)
    // — compiles to the empty range.
    cases.push((
        "gen-utf8-eq-over-cap".to_string(),
        format!("SELECT * FROM orders WHERE name = '{}'", "x".repeat(1100)),
    ));
    // Over-cap comparison literal: binds at the truncated image with
    // the inclusivity flip (ADR-0080 D3).
    cases.push((
        "gen-utf8-lt-over-cap".to_string(),
        format!("SELECT * FROM orders WHERE name < '{}'", "x".repeat(1100)),
    ));
    cases.push((
        "gen-utf8-gt-over-cap".to_string(),
        format!("SELECT * FROM orders WHERE name > '{}'", "x".repeat(1100)),
    ));
    // Over-cap begins_with prefix: no match can be stored — empty.
    cases.push((
        "gen-begins-with-over-cap".to_string(),
        format!("SELECT * FROM orders WHERE begins_with(name, '{}')", "x".repeat(1100)),
    ));
    // The statement-size ceiling itself.
    cases.push((
        "gen-statement-too-long".to_string(),
        format!("SELECT * FROM orders WHERE $key = '{}'", "s".repeat(9000)),
    ));
    cases
}

// ---------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------

fn render_case(stmt: &str, catalog: &FixtureCatalog) -> String {
    match compile(stmt.as_bytes(), catalog) {
        Err(e) => format!("error: {e}\n"),
        Ok(compiled) => {
            // Determinism (L7): recompilation is byte-identical.
            let again = compile(stmt.as_bytes(), catalog).expect("deterministic acceptance");
            assert_eq!(
                compiled.program.as_bytes(),
                again.program.as_bytes(),
                "recompilation must be byte-identical: {stmt}"
            );
            // The serialized form survives its own trust boundary.
            let revalidated = AccessProgram::from_bytes(compiled.program.as_bytes())
                .expect("encoder output revalidates");
            assert_eq!(revalidated.as_bytes(), compiled.program.as_bytes());
            assert_eq!(revalidated.decode(), compiled.access, "decode round-trip");
            let explained = compiled.program.explain();
            assert_eq!(explained, revalidated.explain(), "rendering is deterministic");
            explained
        }
    }
}

#[test]
fn partiql_suite_matches_golden() {
    let catalog = fixture();
    let mut names = std::collections::HashSet::new();
    let mut actual = String::new();
    let generated = generated_cases();
    let all: Vec<(&str, &str)> = CASES
        .iter()
        .copied()
        .chain(generated.iter().map(|(n, s)| (n.as_str(), s.as_str())))
        .collect();
    assert!(all.len() >= 300, "the plan AC names a 300-case suite; have {}", all.len());
    for (name, stmt) in &all {
        assert!(names.insert(*name), "duplicate case name {name}");
        assert!(!stmt.contains('\n'), "suite statements are single-line: {name}");
        let _ = write!(actual, "### {name}\n{stmt}\n---\n{}\n", render_case(stmt, &catalog));
    }
    let golden_path = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/golden/partiql_suite.txt");
    if std::env::var_os("PARTIQL_BLESS").is_some() {
        std::fs::write(golden_path, &actual).expect("write golden");
        return;
    }
    let golden = std::fs::read_to_string(golden_path).unwrap_or_default();
    if golden != actual {
        let mismatch = golden
            .lines()
            .zip(actual.lines())
            .enumerate()
            .find(|(_, (g, a))| g != a)
            .map(|(i, (g, a))| {
                format!("first diff at line {}:\n  golden: {g}\n  actual: {a}", i + 1)
            })
            .unwrap_or_else(|| "outputs differ in length".to_string());
        panic!(
            "golden mismatch — the suite output IS the compat contract; review the diff and \
             bless deliberately with PARTIQL_BLESS=1.\n{mismatch}"
        );
    }
}
