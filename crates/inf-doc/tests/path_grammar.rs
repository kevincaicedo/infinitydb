//! M3-S08 grammar suite: the table-driven accept/reject corpus (cases
//! re-derived from the RedisJSON documentation and RFC 9535 — RedisJSON's
//! own test files are RSALv2/SSPL-licensed and are not copied) plus the
//! `parse(print(ast)) == ast` property. Grammar authority:
//! `infinitydb/docs/jsonpath-subset.md`; encoding authority: ADR-0040.

use inf_doc::path::{
    self, Member, PathAst, PathErrorKind as K, Segment, SliceSpec, compile, parse_ast,
};
use proptest::prelude::*;

/// Valid inputs: `(text, canonical print)`. Parsing must succeed and the
/// canonical print must itself reparse to the identical AST (checked for
/// every row), so each row pins acceptance and printer shape at once.
#[rustfmt::skip]
const VALID: &[(&str, &str)] = &[
    // Roots and modes.
    ("$", "$"), (".", "."), ("", "."),
    // Dot children, shorthand shapes.
    ("$.a", "$.a"), ("$.abc", "$.abc"), ("$._", "$._"), ("$._x9", "$._x9"),
    ("$.A9", "$.A9"), ("$.a.b.c", "$.a.b.c"), ("$.snake_case", "$.snake_case"),
    // Unicode shorthand (RFC 9535 name-shorthand).
    ("$.имя", "$.имя"), ("$.名前", "$.名前"), ("$.café", "$.café"), ("$.κλειδί.β", "$.κλειδί.β"),
    // Bracket children, both quotes, escapes.
    ("$['a']", "$.a"), ("$[\"a\"]", "$.a"), ("$['a b']", "$['a b']"),
    ("$['']", "$['']"), ("$['9lives']", "$['9lives']"),
    ("$['it\\'s']", "$['it\\'s']"), ("$[\"say \\\"hi\\\"\"]", "$['say \"hi\"']"),
    ("$['back\\\\slash']", "$['back\\\\slash']"), ("$['sla\\/sh']", "$['sla/sh']"),
    ("$['tab\\there']", "$['tab\\u0009here']"), ("$['nl\\n']", "$['nl\\u000a']"),
    ("$['\\b\\f\\r']", "$['\\u0008\\u000c\\u000d']"),
    ("$['\\u0041']", "$.A"), ("$['\\u00e9']", "$.é"),
    ("$['\\ud83d\\ude00']", "$.😀"), ("$['dollar$sign']", "$['dollar$sign']"),
    ("$['dot.ted']", "$['dot.ted']"), ("$['bra[ck]et']", "$['bra[ck]et']"),
    ("$['*']", "$['*']"), ("$['question?mark']", "$['question?mark']"),
    // Wildcards.
    ("$.*", "$.*"), ("$[*]", "$.*"), ("$.a.*", "$.a.*"), ("$[ * ]", "$.*"),
    // Indices.
    ("$[0]", "$[0]"), ("$[7]", "$[7]"), ("$[-1]", "$[-1]"), ("$[-12]", "$[-12]"),
    ("$[9223372036854775807]", "$[9223372036854775807]"),
    ("$[-9223372036854775808]", "$[-9223372036854775808]"),
    ("$[ 3 ]", "$[3]"), ("$.a[0].b", "$.a[0].b"),
    // Slices — every presence combination.
    ("$[:]", "$[:]"), ("$[1:]", "$[1:]"), ("$[:2]", "$[:2]"), ("$[1:2]", "$[1:2]"),
    ("$[::]", "$[:]"), ("$[::2]", "$[::2]"), ("$[1::2]", "$[1::2]"),
    ("$[:2:2]", "$[:2:2]"), ("$[1:2:2]", "$[1:2:2]"), ("$[::-1]", "$[::-1]"),
    ("$[-3:-1]", "$[-3:-1]"), ("$[10:0:-2]", "$[10:0:-2]"), ("$[ 1 : 2 : 3 ]", "$[1:2:3]"),
    ("$[1:2:]", "$[1:2]"),
    // Unions.
    ("$[0,1]", "$[0,1]"), ("$[0, 1, 2]", "$[0,1,2]"), ("$['a','b']", "$['a','b']"),
    ("$[\"a\", 'b']", "$['a','b']"), ("$['a',0]", "$['a',0]"),
    ("$[0,-1]", "$[0,-1]"), ("$[0,1:3]", "$[0,1:3]"), ("$[1:2,3:4]", "$[1:2,3:4]"),
    ("$['x', 1, 2:3]", "$['x',1,2:3]"),
    ("$[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15]", "$[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15]"),
    // Recursive descent.
    ("$..a", "$..a"), ("$..*", "$..*"), ("$..[0]", "$..[0]"), ("$..['a b']", "$..['a b']"),
    ("$..[0,1]", "$..[0,1]"), ("$..[1:2]", "$..[1:2]"), ("$.a..b.c", "$.a..b.c"),
    ("$..a..b", "$..a..b"), ("$..имя", "$..имя"),
    // Legacy mode.
    ("a", ".a"), (".a", ".a"), ("a.b", ".a.b"), (".a.b", ".a.b"),
    ("foo[2]", ".foo[2]"), ("[0]", "[0]"), ("['a']", ".a"), ("[*]", ".*"),
    ("..a", "..a"), ("a..b", ".a..b"), (".a[*]", ".a.*"), ("имя", ".имя"),
    ("a[-1].b", ".a[-1].b"), ("[0,1]", "[0,1]"), ("[1:2]", "[1:2]"),
    // Whitespace tolerance (brackets only).
    ("$[ 'a' ]", "$.a"), ("$[ 'a' , 'b' ]", "$['a','b']"), ("$[\t0\t]", "$[0]"),
    // Chained shapes (realistic command paths).
    ("$.store.book[0].title", "$.store.book[0].title"),
    ("$.store.book[*].author", "$.store.book.*.author"),
    ("$.a[0][1]", "$.a[0][1]"), ("$.a[-1][-2]", "$.a[-1][-2]"),
    ("$['a']['b']['c']", "$.a.b.c"), ("$.a['b'].c", "$.a.b.c"),
    ("$..book[2]", "$..book[2]"), ("$..a.b..c", "$..a.b..c"),
    ("$[0].a", "$[0].a"), ("$[0][*]", "$[0].*"), ("$..*.a", "$..*.a"),
    ("$['x'][0]['y']", "$.x[0].y"), ("$.a[1:2].b", "$.a[1:2].b"),
    ("$['long key with spaces']", "$['long key with spaces']"),
    ("$['ключ пробел']", "$['ключ пробел']"), ("$['emoji 😀 key']", "$['emoji 😀 key']"),
    ("$['\\u0451\\u0436']", "$.ёж"), ("$.a1.b2.c3", "$.a1.b2.c3"),
    ("$._private._x", "$._private._x"), ("$[16]", "$[16]"), ("$[100000]", "$[100000]"),
    ("$[-100000]", "$[-100000]"), ("$[0:0]", "$[0:0]"), ("$[-1:]", "$[-1:]"),
    ("$[:-1]", "$[:-1]"), ("$[5:5:5]", "$[5:5:5]"), ("$[-5:-1:2]", "$[-5:-1:2]"),
    ("$[0:10:-3]", "$[0:10:-3]"), ("$['a','b','c','d']", "$['a','b','c','d']"),
    ("$[-1,-2]", "$[-1,-2]"), ("$[:2,4:]", "$[:2,4:]"), ("$['k',1:2,'m']", "$['k',1:2,'m']"),
    ("$..[ 'a' , 'b' ]", "$..['a','b']"), ("$..[-1]", "$..[-1]"), ("$..[::2]", "$..[::2]"),
    ("x", ".x"), ("x1_y", ".x1_y"), ("[\"q\"]", ".q"), ("['a b']", "['a b']"),
    ("a[0].b[1].c", ".a[0].b[1].c"), ("..*", "..*"), ("..[0,1]", "..[0,1]"),
];

/// Invalid inputs: `(text, expected kind)`. Offsets are asserted for a
/// focused subset below (exact-offset discipline, the S05 pattern).
#[rustfmt::skip]
const INVALID: &[(&str, K)] = &[
    // Filters: the documented M4.5 cut line.
    ("$[?(@.a > 1)]", K::FilterUnsupported), ("$.a[?(@.b)]", K::FilterUnsupported),
    ("$..?(@.a)", K::FilterUnsupported),
    // Structure.
    ("$.", K::UnexpectedChar), ("$.a.", K::UnexpectedChar), ("$..", K::TrailingDescend),
    ("$...a", K::UnexpectedChar), ("$.a..", K::TrailingDescend),
    ("$[", K::Unterminated), ("$[0", K::Unterminated), ("$['a'", K::Unterminated),
    ("$['a", K::Unterminated), ("$[\"a", K::Unterminated),
    ("$]", K::UnexpectedChar), ("$.a]", K::UnexpectedChar),
    ("$$", K::UnexpectedChar), ("$.$", K::UnexpectedChar), ("$.a$", K::UnexpectedChar),
    ("$ .a", K::UnexpectedChar), ("$. a", K::UnexpectedChar), ("$.a .b", K::UnexpectedChar),
    ("$.[0]", K::UnexpectedChar), ("$.['a']", K::UnexpectedChar),
    ("$()", K::UnexpectedChar), ("$.a()", K::UnexpectedChar),
    ("$[]", K::UnexpectedChar), ("$[,]", K::UnexpectedChar), ("$[0,]", K::UnexpectedChar),
    ("$[,0]", K::UnexpectedChar), ("$[0 1]", K::UnexpectedChar), ("$[0;1]", K::UnexpectedChar),
    ("$[0][", K::Unterminated), ("$.a[}", K::UnexpectedChar),
    ("$[(1)]", K::UnexpectedChar), ("$[@]", K::UnexpectedChar),
    // Wildcard misuse.
    ("$[*,0]", K::BadUnionMember), ("$[0,*]", K::BadUnionMember), ("$[**]", K::UnexpectedChar),
    ("$.**", K::UnexpectedChar),
    // Numbers.
    ("$[01]", K::BadNumber), ("$[-0]", K::BadNumber), ("$[00]", K::BadNumber),
    ("$[1.5]", K::UnexpectedChar), ("$[-]", K::BadNumber), ("$[+1]", K::UnexpectedChar),
    ("$[9223372036854775808]", K::BadNumber), ("$[-9223372036854775809]", K::BadNumber),
    ("$[1e2]", K::UnexpectedChar),
    // Slices.
    ("$[::0]", K::BadSlice), ("$[1:2:0]", K::BadSlice),
    ("$[:::]", K::UnexpectedChar), ("$[1:2:3:4]", K::UnexpectedChar),
    // Escapes.
    ("$['\\x41']", K::BadEscape), ("$['\\u12']", K::BadEscape), ("$['\\u12zz']", K::BadEscape),
    ("$['\\ud800']", K::BadEscape), ("$['\\udc00']", K::BadEscape),
    ("$['\\ud83d\\u0041']", K::BadEscape), ("$['\\ud83dx']", K::BadEscape),
    ("$['\\", K::BadEscape), ("$['a\\'", K::Unterminated),
    // Raw controls inside quotes.
    ("$['a\u{0009}b']", K::UnexpectedChar),
    // Union cardinality (17 members).
    ("$[0,1,2,3,4,5,6,7,8,9,10,11,12,13,14,15,16]", K::BadUnionMember),
    // Legacy-mode structural errors surface identically.
    ("a.", K::UnexpectedChar), ("a[", K::Unterminated), ("a..", K::TrailingDescend),
    (".a.", K::UnexpectedChar), ("..", K::TrailingDescend), ("...a", K::UnexpectedChar),
    ("a b", K::UnexpectedChar), ("-", K::UnexpectedChar), ("!", K::UnexpectedChar),
    // More structure and quoting edges.
    ("$['a']x", K::UnexpectedChar), ("$[0]b", K::UnexpectedChar), ("$.a,b", K::UnexpectedChar),
    ("$[''", K::Unterminated), ("$[\"", K::Unterminated), ("$['a'x]", K::UnexpectedChar),
    ("$['a' 'b']", K::UnexpectedChar), ("$[0 ,, 1]", K::UnexpectedChar),
    ("$[--1]", K::BadNumber), ("$[1-]", K::UnexpectedChar), ("$[0x1]", K::UnexpectedChar),
    ("$[ ]", K::UnexpectedChar), ("$.a\n", K::UnexpectedChar), ("$\t.a", K::UnexpectedChar),
    ("$['\\u dead']", K::BadEscape), ("$['\\uD83D\\uD83D']", K::BadEscape),
    ("$..''", K::UnexpectedChar), ("$.'a'", K::UnexpectedChar), ("$.\"a\"", K::UnexpectedChar),
    ("$.-a", K::UnexpectedChar), ("$.9a", K::UnexpectedChar), ("$[a]", K::UnexpectedChar),
    ("$[*][", K::Unterminated), ("$[0]..", K::TrailingDescend),
];

#[test]
fn table_valid_cases_parse_and_print_canonically() {
    for (text, want_print) in VALID {
        let ast =
            parse_ast(text.as_bytes()).unwrap_or_else(|e| panic!("{text:?} must parse, got {e:?}"));
        let printed = path::ast::print(&ast);
        assert_eq!(&printed, want_print, "canonical print of {text:?}");
        let reparsed = parse_ast(printed.as_bytes())
            .unwrap_or_else(|e| panic!("print of {text:?} = {printed:?} must reparse: {e:?}"));
        assert_eq!(reparsed, ast, "print(parse({text:?})) reparses identically");
        // Compile + decode: the bytecode leg of the same round trip.
        let program = compile(text.as_bytes()).expect("valid text compiles");
        assert_eq!(program.decode(), ast, "decode(encode(ast)) for {text:?}");
        let revalidated = inf_doc::PathProgram::from_bytes(program.as_bytes())
            .expect("compiled bytes revalidate");
        assert_eq!(revalidated, program);
    }
    assert!(VALID.len() + INVALID.len() >= 200, "the S08 AC demands ≥ 200 table cases");
}

#[test]
fn table_invalid_cases_reject_with_the_expected_kind() {
    for (text, want) in INVALID {
        match parse_ast(text.as_bytes()) {
            Ok(ast) => panic!("{text:?} must reject, parsed {ast:?}"),
            Err(e) => assert_eq!(&e.kind, want, "kind for {text:?} (offset {})", e.offset),
        }
    }
}

/// Exact-offset subset (the S05 error-offset discipline).
#[test]
fn error_offsets_are_exact() {
    let cases: &[(&str, usize)] = &[
        ("$[?(@.a)]", 2),  // the `?`
        ("$.a$", 3),       // the stray `$`
        ("$[01]", 2),      // the number start
        ("$[::0]", 4),     // the zero step
        ("$['\\x41']", 3), // the escape backslash
        ("$['a", 2),       // the unterminated quote opens at 2
        ("$..", 1),        // descend starts at the first dot
    ];
    for (text, offset) in cases {
        let e = parse_ast(text.as_bytes()).expect_err("rejects");
        assert_eq!(e.offset, *offset, "offset for {text:?} ({:?})", e.kind);
    }
}

#[test]
fn mode_is_recorded_on_the_program() {
    assert!(!compile(b"$.a").expect("compiles").is_legacy());
    assert!(compile(b".a").expect("compiles").is_legacy());
    assert!(compile(b"a.b").expect("compiles").is_legacy());
    assert!(compile(b"").expect("compiles").is_legacy());
}

#[test]
fn limits_bind() {
    // Text cap: config lowers, never raises.
    let long = format!("$.{}", "a".repeat(100));
    assert_eq!(
        path::compile_with_max_bytes(long.as_bytes(), 16).expect_err("caps").kind,
        K::PathTooLong
    );
    assert!(path::compile_with_max_bytes(long.as_bytes(), 4096).is_ok());
    // Segment cap at 128: 128 segments pass, 129 reject.
    let deep_ok = format!("${}", ".a".repeat(128));
    let deep_bad = format!("${}", ".a".repeat(129));
    assert!(parse_ast(deep_ok.as_bytes()).is_ok());
    assert_eq!(parse_ast(deep_bad.as_bytes()).expect_err("caps").kind, K::PathTooDeep);
    // Invalid UTF-8 is typed with an offset.
    assert_eq!(parse_ast(b"$.a\xFF").expect_err("rejects").kind, K::InvalidUtf8);
}

/// Golden program bytes: pins the wire encoding itself (ADR-0040 D2) —
/// drift in any byte is a format event, exactly like the idoc goldens.
#[test]
fn bytecode_golden_vectors() {
    let cases: &[(&str, &[u8])] = &[
        ("$", &[1, 0, 0x01]),
        (".", &[1, 1, 0x01]),
        ("$.a", &[1, 0, 0x01, 0x02, 1, b'a']),
        ("$[0]", &[1, 0, 0x01, 0x04, 0]),
        ("$[-1]", &[1, 0, 0x01, 0x04, 1]), // zigzag(−1) = 1
        ("$[2]", &[1, 0, 0x01, 0x04, 4]),  // zigzag(2) = 4
        ("$.*", &[1, 0, 0x01, 0x03]),
        ("$[1:2]", &[1, 0, 0x01, 0x05, 0b011, 2, 4]),
        ("$[::-1]", &[1, 0, 0x01, 0x05, 0b100, 1]),
        ("$..a", &[1, 0, 0x01, 0x07, 0x02, 1, b'a']),
        ("$['k',0]", &[1, 0, 0x01, 0x06, 2, 0x02, 1, b'k', 0x04, 0]),
    ];
    for (text, want) in cases {
        let program = compile(text.as_bytes()).expect("compiles");
        assert_eq!(program.as_bytes(), *want, "program bytes for {text:?}");
    }
}

/// Foreign-byte rejection matrix (the replay trust boundary).
#[test]
fn program_byte_rejections_are_typed() {
    use inf_doc::PathProgram;
    let kind = |bytes: &[u8]| PathProgram::from_bytes(bytes).expect_err("rejects").kind;
    assert_eq!(kind(&[]), K::Truncated);
    assert_eq!(kind(&[1, 0]), K::Truncated);
    assert_eq!(kind(&[2, 0, 0x01]), K::BadVersion);
    assert_eq!(kind(&[1, 0b10, 0x01]), K::BadFlags);
    assert_eq!(kind(&[1, 0, 0x02]), K::MissingRoot);
    assert_eq!(kind(&[1, 0, 0x01, 0x00]), K::BadOpcode);
    assert_eq!(kind(&[1, 0, 0x01, 0xFF]), K::BadOpcode);
    assert_eq!(kind(&[1, 0, 0x01, 0x01]), K::BadOpcode); // Root only leads
    assert_eq!(kind(&[1, 0, 0x01, 0x02, 5, b'a']), K::Truncated); // short key
    assert_eq!(kind(&[1, 0, 0x01, 0x02, 1, 0xFF]), K::InvalidUtf8);
    assert_eq!(kind(&[1, 0, 0x01, 0x07]), K::TrailingDescend);
    assert_eq!(kind(&[1, 0, 0x01, 0x07, 0x07, 0x03]), K::BadOpcode); // Descend Descend
    assert_eq!(kind(&[1, 0, 0x01, 0x05, 0b1000]), K::BadFlags); // slice presence bits
    assert_eq!(kind(&[1, 0, 0x01, 0x05, 0b100, 0]), K::BadSlice); // step 0
    assert_eq!(kind(&[1, 0, 0x01, 0x06, 1, 0x04, 0]), K::BadUnionMember); // n = 1
    assert_eq!(kind(&[1, 0, 0x01, 0x06, 2, 0x03, 0x03]), K::BadUnionMember); // wildcard member
}

fn arb_name() -> impl Strategy<Value = Vec<u8>> {
    prop_oneof![
        "[a-z_][a-z0-9_]{0,6}".prop_map(|s| s.into_bytes()),
        // Non-shorthand shapes: spaces, quotes, escapes, unicode, empty.
        "[ -~κλ😀]{0,5}".prop_map(|s| s.into_bytes()),
    ]
}

/// The `i64` extremes and `u32` aliases (review C10/C11): the text form
/// must print and reparse every one of them exactly.
static EXTREME_INTS: [i64; 8] = [
    u32::MAX as i64,
    1 << 32,
    (1 << 32) + 1,
    i64::MAX,
    -(1 << 32),
    -(1 << 32) - 1,
    i64::MIN + 1,
    i64::MIN,
];

fn arb_int() -> BoxedStrategy<i64> {
    prop_oneof![8 => -9i64..9, 1 => proptest::sample::select(&EXTREME_INTS[..])].boxed()
}

fn arb_slice() -> impl Strategy<Value = SliceSpec> {
    let field = proptest::option::of(arb_int());
    let step = proptest::option::of(
        prop_oneof![
            8 => prop_oneof![(-4i64..0), (1i64..4)],
            1 => proptest::sample::select(&EXTREME_INTS[..]),
        ]
        .boxed(),
    );
    (field.clone(), field, step).prop_map(|(start, end, step)| SliceSpec { start, end, step })
}

fn arb_member() -> impl Strategy<Value = Member> {
    prop_oneof![
        arb_name().prop_map(Member::Name),
        arb_int().prop_map(Member::Index),
        arb_slice().prop_map(Member::Slice),
    ]
}

fn arb_selector() -> impl Strategy<Value = Segment> {
    prop_oneof![
        arb_name().prop_map(Segment::Child),
        Just(Segment::ChildAny),
        arb_int().prop_map(Segment::Index),
        arb_slice().prop_map(Segment::Slice),
        proptest::collection::vec(arb_member(), 2..5).prop_map(Segment::Union),
    ]
}

fn arb_path() -> impl Strategy<Value = PathAst> {
    let segment = prop_oneof![
        4 => arb_selector(),
        1 => arb_selector().prop_map(|s| Segment::Descend(Box::new(s))),
    ];
    (any::<bool>(), proptest::collection::vec(segment, 0..6))
        .prop_map(|(legacy, segments)| PathAst { legacy, segments })
}

/// M4.5 §3.1 indexable-path fence (ADR-0075 D2.4): child steps, `[*]`,
/// and array indices are inside; recursive descent, slices, and unions
/// are outside — each case pinned on the compiled program, the exact
/// bytes the index catalog stores.
#[test]
fn index_fence_splits_the_grammar() {
    for text in ["$", "$.a", "$.a.b", "$[0]", "$.items[2].price", "$.tags[*]", "$.a[*].b"] {
        let program = compile(text.as_bytes()).expect("valid path");
        assert!(program.within_index_fence(), "{text} is inside the fence");
    }
    for text in ["$..a", "$.a..b", "$[1:3]", "$[:2]", "$['a','b']", "$[0,1]", "$..[*]"] {
        let program = compile(text.as_bytes()).expect("valid path");
        assert!(!program.within_index_fence(), "{text} is outside the fence");
    }
}

proptest! {
    /// The S08 property AC: `parse(print(ast)) == ast` over generated
    /// paths — both modes, every selector kind, escapes included.
    #[test]
    fn print_parse_round_trip(ast in arb_path()) {
        let printed = path::ast::print(&ast);
        let reparsed = parse_ast(printed.as_bytes())
            .unwrap_or_else(|e| panic!("print {printed:?} must reparse: {e:?}"));
        prop_assert_eq!(&reparsed, &ast, "text: {}", printed);
    }

    /// The S09 bytecode AC: encode → decode is the identity, and the
    /// encoded bytes revalidate through the foreign-byte boundary.
    #[test]
    fn bytecode_round_trip(ast in arb_path()) {
        let program = path::encode_ast(&ast);
        prop_assert_eq!(&program.decode(), &ast);
        let revalidated = inf_doc::PathProgram::from_bytes(program.as_bytes()).expect("validates");
        prop_assert_eq!(&revalidated, &program);
    }
}
