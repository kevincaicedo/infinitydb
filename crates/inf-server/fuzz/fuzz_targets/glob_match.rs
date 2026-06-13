//! Glob matcher totality (M1 §5): arbitrary pattern × string in both case
//! modes must never panic, never recurse (the matcher is iterative by
//! construction), and agree with basic identities. `stringmatchlen` is a
//! classic Redis CVE surface — fuzzed from day one.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    if data.is_empty() {
        return;
    }
    // First byte picks the split point between pattern and string.
    let split = usize::from(data[0]) % data.len();
    let (pattern, string) = data[1..].split_at(split.min(data.len() - 1));

    let sensitive = inf_server::glob_match(pattern, string, false);
    let _ = inf_server::glob_match(pattern, string, true);

    // Identities: `*` matches everything; an exact, metacharacter-free
    // pattern matches itself.
    assert!(inf_server::glob_match(b"*", string, false));
    if sensitive && !pattern.iter().any(|b| matches!(b, b'*' | b'?' | b'[' | b'\\')) {
        assert_eq!(pattern, string, "literal pattern matched a different string");
    }
});
