//! Key-spec extraction vs a hand-written oracle (M0-S12 AC): for every
//! command in the M0 surface × arity edge cases, `extract_keys` must agree
//! with an independently-written per-command reference.

use inf_wire::{
    COMMANDS, CommandId, ConnParser, Parsed, ParserLimits, arity_ok, extract_keys, lookup,
};
use proptest::prelude::*;

/// Hand-written oracle: which argv indices are keys, per command, written
/// from the Redis docs — NOT from the KeySpec table.
fn oracle_key_indices(id: CommandId, argc: usize) -> Vec<usize> {
    match id {
        // No keys.
        CommandId::Ping
        | CommandId::Echo
        | CommandId::Hello
        | CommandId::Quit
        | CommandId::Info
        | CommandId::Command
        | CommandId::Dbsize
        | CommandId::Keys
        | CommandId::Randomkey
        | CommandId::Scan
        | CommandId::Flushdb
        | CommandId::Flushall
        | CommandId::Debug
        | CommandId::Select
        | CommandId::Config
        | CommandId::Client
        | CommandId::Lolwut
        | CommandId::InfNs
        // Pub/sub channels and patterns are not keys (M1-E5): ownership is
        // the plane's slot(channel) mapping, never the key router.
        | CommandId::Subscribe
        | CommandId::Unsubscribe
        | CommandId::Psubscribe
        | CommandId::Punsubscribe
        | CommandId::Publish
        | CommandId::Pubsub => Vec::new(),
        // All trailing args are keys.
        CommandId::Del
        | CommandId::Exists
        | CommandId::Mget
        | CommandId::Touch
        | CommandId::Unlink => (1..argc).collect(),
        // Key/value pairs: every second trailing arg starting at argv[1].
        CommandId::Mset | CommandId::Msetnx => (1..argc).step_by(2).collect(),
        // Two keys at argv[1..=2].
        CommandId::Rename | CommandId::Renamenx | CommandId::Copy => (1..argc.min(3)).collect(),
        // Subcommand shape: key at argv[2].
        CommandId::Object => {
            if argc > 2 {
                vec![2]
            } else {
                Vec::new()
            }
        }
        // Exactly one key at argv[1].
        CommandId::Get
        | CommandId::Set
        | CommandId::Setnx
        | CommandId::Setex
        | CommandId::Psetex
        | CommandId::Getset
        | CommandId::Getdel
        | CommandId::Getex
        | CommandId::Getrange
        | CommandId::Setrange
        | CommandId::Substr
        | CommandId::IncrByFloat
        | CommandId::Type
        | CommandId::Incr
        | CommandId::Decr
        | CommandId::IncrBy
        | CommandId::DecrBy
        | CommandId::Append
        | CommandId::Strlen
        | CommandId::Expire
        | CommandId::Pexpire
        | CommandId::Expireat
        | CommandId::Pexpireat
        | CommandId::Expiretime
        | CommandId::Pexpiretime
        | CommandId::Ttl
        | CommandId::Pttl
        | CommandId::Persist
        | CommandId::InfTake
        | CommandId::InfPeek => {
            if argc > 1 {
                vec![1]
            } else {
                Vec::new()
            }
        }
    }
}

/// Build a RESP frame with `argc` args (command name + filler args).
fn frame_for(name: &str, argc: usize) -> Vec<u8> {
    let mut frame = format!("*{argc}\r\n${}\r\n{name}\r\n", name.len()).into_bytes();
    for i in 1..argc {
        let arg = format!("arg{i:03}");
        frame.extend_from_slice(format!("${}\r\n{arg}\r\n", arg.len()).as_bytes());
    }
    frame
}

fn keys_via_spec(name: &str, argc: usize) -> Vec<Vec<u8>> {
    let mut parser = ConnParser::new(ParserLimits::default());
    let frame = frame_for(name, argc);
    let mut iter = parser.feed(&frame);
    let Some(Parsed::Command(argv)) = iter.next() else {
        panic!("frame for {name}/{argc} did not parse")
    };
    let meta = lookup(argv.arg(0)).expect("command resolves");
    extract_keys(meta, &argv).map(<[u8]>::to_vec).collect()
}

#[test]
fn keyspec_matches_oracle_for_every_command_and_arity() {
    for meta in &COMMANDS {
        // Arity edge cases: exact/minimum, one extra, several extra, and the
        // degenerate 1 (command name alone).
        let base = meta.arity.unsigned_abs() as usize;
        for argc in [1, base.max(1), base + 1, base + 4] {
            let expected: Vec<Vec<u8>> = oracle_key_indices(meta.id, argc)
                .into_iter()
                .map(|i| {
                    if i == 0 {
                        meta.name.as_bytes().to_vec()
                    } else {
                        format!("arg{i:03}").into_bytes()
                    }
                })
                .collect();
            let got = keys_via_spec(meta.name, argc);
            assert_eq!(got, expected, "{} argc={argc}: spec disagrees with oracle", meta.name);
        }
    }
}

#[test]
fn arity_validation_matches_redis_convention() {
    let get = lookup(b"GET").expect("GET");
    assert!(arity_ok(get, 2));
    assert!(!arity_ok(get, 1));
    assert!(!arity_ok(get, 3));

    let set = lookup(b"SET").expect("SET"); // arity -3: at least 3
    assert!(!arity_ok(set, 2));
    assert!(arity_ok(set, 3));
    assert!(arity_ok(set, 6));

    let del = lookup(b"DEL").expect("DEL"); // arity -2
    assert!(!arity_ok(del, 1));
    assert!(arity_ok(del, 2));
    assert!(arity_ok(del, 12));
}

proptest! {
    /// Random byte strings must never false-positive into a command (the
    /// verify word-compare in the perfect hash).
    #[test]
    fn random_names_do_not_false_positive(name in proptest::collection::vec(any::<u8>(), 0..12)) {
        if let Some(meta) = lookup(&name) {
            // The only way to resolve is to actually be the command name,
            // case-insensitively.
            prop_assert!(name.eq_ignore_ascii_case(meta.name.as_bytes()));
        }
    }
}
