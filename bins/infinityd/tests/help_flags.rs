//! `infinityd --help` names every flag the parser accepts: an operator
//! reads the usage line, not the parser. The flags are the string
//! literals that open a match arm in `parse_args`; a flag the usage line
//! omits on purpose is a row of `NOT_IN_HELP` with its reason.

/// Parsed flags the usage line leaves out, each with why.
const NOT_IN_HELP: &[(&str, &str)] = &[
    ("--sync-pipeline", "parsed only to be refused with the flag that replaced it"),
    ("--blind-overwrite-ceiling", "built only with the bench-diagnostics feature"),
    ("--help", "the flag that prints the usage line"),
];

/// The flags `parse_args` matches: each `"--name"` literal that is
/// followed on its line by `=>` or `|` (an arm's pattern).
fn parsed_flags() -> Vec<String> {
    let source = include_str!("../src/main.rs");
    let begin = source.find("fn parse_args").expect("parse_args in main.rs");
    let end = begin + source[begin..].find("\n}\n").expect("parse_args ends");
    let mut flags = Vec::new();
    for line in source[begin..end].lines() {
        let mut rest = line.trim_start();
        while let Some(stripped) = rest.strip_prefix("\"--") {
            let Some(close) = stripped.find('"') else { break };
            let after = stripped[close + 1..].trim_start();
            if !(after.starts_with("=>") || after.starts_with('|')) {
                break;
            }
            flags.push(format!("--{}", &stripped[..close]));
            rest = after.trim_start_matches('|').trim_start();
        }
    }
    flags
}

#[test]
fn help_names_every_parsed_flag() {
    let flags = parsed_flags();
    assert!(flags.len() > 30, "the scan found the parser's arms: {flags:?}");
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
        assert!(flags.iter().any(|parsed| parsed == flag), "{flag} is not parsed ({reason})");
    }
}
