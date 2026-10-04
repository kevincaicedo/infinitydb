//! `infinityd --help` names every flag the parser accepts: an operator
//! reads the usage line, not the parser. The flags are the string
//! literals in `parse_args` that are exactly a flag name, so the scan
//! does not depend on how rustfmt lays out an arm; a flag the usage line
//! omits on purpose is a row of `NOT_IN_HELP` with its reason.

use std::collections::BTreeSet;

/// Parsed flags the usage line leaves out, each with why.
const NOT_IN_HELP: &[(&str, &str)] = &[
    ("--sync-pipeline", "parsed only to be refused with the flag that replaced it"),
    ("--blind-overwrite-ceiling", "built only with the bench-diagnostics feature"),
    ("--help", "the flag that prints the usage line"),
];

/// How many distinct flags `parse_args` holds: the scan's scope check. A
/// change that adds or removes a flag changes this count with it.
const PARSED_FLAG_COUNT: usize = 41;

/// The body of `parse_args`, from its signature to its closing brace.
fn parse_args_source() -> &'static str {
    let source = include_str!("../src/main.rs");
    let begin = source.find("\nfn parse_args").expect("parse_args in main.rs");
    let end = begin + 1 + source[begin + 1..].find("\n}\n").expect("parse_args ends");
    &source[begin..end]
}

/// Every string literal of `code` that is exactly a flag name (`--`, then
/// lowercase letters, digits and dashes): an arm's pattern and a
/// `take("--name")` argument alike. Line comments are skipped; an escaped
/// quote, and a line continuation, stay inside their literal.
fn flag_literals(code: &str) -> BTreeSet<String> {
    let is_flag = |text: &str| {
        text.strip_prefix("--").is_some_and(|name| {
            !name.is_empty()
                && name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        })
    };
    let mut flags = BTreeSet::new();
    let mut chars = code.chars().peekable();
    let mut literal: Option<String> = None;
    let mut in_comment = false;
    while let Some(c) = chars.next() {
        if in_comment {
            in_comment = c != '\n';
            continue;
        }
        match (&mut literal, c) {
            (None, '/') if chars.peek() == Some(&'/') => in_comment = true,
            (None, '"') => literal = Some(String::new()),
            (None, _) => {}
            (Some(text), '\\') => {
                text.push(c);
                text.extend(chars.next());
            }
            (Some(text), '"') => {
                if is_flag(text) {
                    flags.insert(text.clone());
                }
                literal = None;
            }
            (Some(text), _) => text.push(c),
        }
    }
    assert!(literal.is_none(), "a string literal in parse_args never closes");
    flags
}

/// The scan reads an or-pattern rustfmt wraps, a `take` argument and a
/// literal that ends its line, and skips a comment's, a message's and a
/// continued usage string's.
#[test]
fn the_scan_reads_every_flag_literal_however_the_arm_is_laid_out() {
    let planted = r#"
            "--alpha"
            | "--beta" => {}
            "--gamma" => args.gamma = take("--delta")?,
            "--epsilon" | "--zeta"
                => {}
            // "--comment" is not a flag
            other => return Err(format!("--eta: {other} \"--theta\"")),
            let usage = "usage: [--iota N] \
                         [--kappa]";
    "#;
    let found: Vec<String> = flag_literals(planted).into_iter().collect();
    assert_eq!(found, ["--alpha", "--beta", "--delta", "--epsilon", "--gamma", "--zeta"]);
}

#[test]
fn help_names_every_parsed_flag() {
    let flags = flag_literals(parse_args_source());
    assert_eq!(flags.len(), PARSED_FLAG_COUNT, "parse_args's flags: {flags:?}");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_infinityd"))
        .arg("--help")
        .output()
        .expect("infinityd --help runs");
    assert!(output.status.success(), "--help exits 0");
    let help = String::from_utf8(output.stdout).expect("UTF-8 usage line");
    let named = |flag: &str| {
        help.split(|c: char| !(c.is_ascii_alphanumeric() || c == '-')).any(|word| word == flag)
    };
    let missing: Vec<&String> = flags
        .iter()
        .filter(|flag| !NOT_IN_HELP.iter().any(|(name, _)| name == flag) && !named(flag))
        .collect();
    assert!(missing.is_empty(), "parsed but not in --help: {missing:?}\n{help}");
    for (flag, reason) in NOT_IN_HELP {
        assert!(flags.contains(*flag), "{flag} is not parsed ({reason})");
    }
}
