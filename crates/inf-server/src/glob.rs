//! Redis-style glob matcher (M1-S02): a faithful port of `stringmatchlen`
//! semantics — `*`, `?`, `[abc]`, `[^abc]`, `[a-z]`, `\x` escapes — written
//! iteratively (explicit star backtracking, no recursion) so hostile
//! patterns cannot exhaust the stack. KEYS/SCAN MATCH use case-sensitive
//! matching; CONFIG GET uses `nocase`.
//!
//! Classic CVE surface in Redis — the M1 test plan fuzzes it from day one;
//! the unit oracle below pins the Redis edge behaviors (unterminated
//! classes, reversed ranges, trailing backslash).

/// Does `pattern` match all of `string`?
pub fn glob_match(pattern: &[u8], string: &[u8], nocase: bool) -> bool {
    let (mut p, mut s) = (0usize, 0usize);
    // Backtrack point: position after the last `*` and the string position
    // it has consumed up to.
    let mut star: Option<(usize, usize)> = None;

    while s < string.len() {
        match advance(pattern, &mut p, string[s], nocase) {
            Advance::Star => {
                star = Some((p, s));
                continue;
            }
            Advance::Matched => {
                s += 1;
                continue;
            }
            Advance::Mismatch => {}
        }
        // Mismatch: resume at the last star, consuming one more byte.
        let Some((star_p, star_s)) = star else { return false };
        p = star_p;
        s = star_s + 1;
        star = Some((star_p, star_s + 1));
    }
    // String exhausted: only trailing stars may remain.
    while p < pattern.len() && pattern[p] == b'*' {
        p += 1;
    }
    p == pattern.len()
}

enum Advance {
    /// Pattern had `*` at `p` (now consumed): record backtrack point.
    Star,
    /// One pattern element matched one string byte (both advanced).
    Matched,
    /// Element did not match (pattern position unspecified — backtrack).
    Mismatch,
}

/// Consumes one pattern element at `*p`, matching it against byte `c`.
fn advance(pattern: &[u8], p: &mut usize, c: u8, nocase: bool) -> Advance {
    if *p >= pattern.len() {
        return Advance::Mismatch;
    }
    match pattern[*p] {
        b'*' => {
            // Collapse runs of stars.
            while *p < pattern.len() && pattern[*p] == b'*' {
                *p += 1;
            }
            Advance::Star
        }
        b'?' => {
            *p += 1;
            Advance::Matched
        }
        b'[' => {
            let (matched, next) = class_match(pattern, *p, c, nocase);
            if matched {
                *p = next;
                Advance::Matched
            } else {
                Advance::Mismatch
            }
        }
        b'\\' if *p + 1 < pattern.len() => {
            if eq(pattern[*p + 1], c, nocase) {
                *p += 2;
                Advance::Matched
            } else {
                Advance::Mismatch
            }
        }
        lit => {
            if eq(lit, c, nocase) {
                *p += 1;
                Advance::Matched
            } else {
                Advance::Mismatch
            }
        }
    }
}

/// `[...]` class at `pattern[start] == b'['`. Returns (matched, position
/// after the class). Redis edge semantics preserved: `^` negates, `a-b`
/// ranges normalize when reversed, `\x` escapes inside, an unterminated
/// class consumes to the end of the pattern.
fn class_match(pattern: &[u8], start: usize, c: u8, nocase: bool) -> (bool, usize) {
    let mut i = start + 1;
    let negate = pattern.get(i) == Some(&b'^');
    if negate {
        i += 1;
    }
    let mut matched = false;
    loop {
        match pattern.get(i) {
            None => break, // unterminated: class ends with the pattern
            Some(b']') => {
                i += 1;
                break;
            }
            Some(b'\\') if i + 1 < pattern.len() => {
                if eq(pattern[i + 1], c, nocase) {
                    matched = true;
                }
                i += 2;
            }
            // NB: Redis's range branch does NOT exclude `]` as the range
            // end — `[a-]` is the (reversed) range `]`..`a`, and the class
            // then runs unterminated. Pinned by the unit tests below.
            Some(&lo) if pattern.get(i + 1) == Some(&b'-') && i + 2 < pattern.len() => {
                let hi = pattern[i + 2];
                let (a, b) = if lo <= hi { (lo, hi) } else { (hi, lo) };
                let probe = if nocase { c.to_ascii_lowercase() } else { c };
                let (a, b) =
                    if nocase { (a.to_ascii_lowercase(), b.to_ascii_lowercase()) } else { (a, b) };
                if (a..=b).contains(&probe) {
                    matched = true;
                }
                i += 3;
            }
            Some(&lit) => {
                if eq(lit, c, nocase) {
                    matched = true;
                }
                i += 1;
            }
        }
    }
    (matched != negate, i)
}

#[inline]
fn eq(a: u8, b: u8, nocase: bool) -> bool {
    if nocase { a.eq_ignore_ascii_case(&b) } else { a == b }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn m(pat: &str, s: &str) -> bool {
        glob_match(pat.as_bytes(), s.as_bytes(), false)
    }

    #[test]
    fn literal_star_question() {
        assert!(m("", ""));
        assert!(!m("", "x"));
        assert!(m("*", ""));
        assert!(m("*", "anything"));
        assert!(m("hello", "hello"));
        assert!(!m("hello", "hellx"));
        assert!(m("h?llo", "hello"));
        assert!(!m("h?llo", "hllo"));
        assert!(m("h*llo", "heeeello"));
        assert!(m("h*llo", "hllo"));
        assert!(m("a*b*c", "aXbYc"));
        assert!(!m("a*b*c", "aXbYd"));
        assert!(m("**a**", "bba"));
        assert!(m("user:*:cart", "user:42:cart"));
    }

    #[test]
    fn classes_and_ranges() {
        assert!(m("h[ae]llo", "hello"));
        assert!(m("h[ae]llo", "hallo"));
        assert!(!m("h[ae]llo", "hillo"));
        assert!(m("h[^e]llo", "hallo"));
        assert!(!m("h[^e]llo", "hello"));
        assert!(m("h[a-c]llo", "hbllo"));
        assert!(!m("h[a-c]llo", "hdllo"));
        // Reversed range normalizes (Redis behavior).
        assert!(m("h[c-a]llo", "hbllo"));
        // `[a-]` is the reversed range `]`..`a` and the `]` is consumed BY
        // the range, leaving the class unterminated (Redis stringmatchlen,
        // not fnmatch): `llo` join the class, nothing follows it.
        assert!(!m("h[a-]llo", "h-llo"));
        assert!(m("h[a-]llo", "ha"));
        assert!(m("h[a-]llo", "hl"));
        assert!(!m("h[a-]llo", "hallo"));
    }

    #[test]
    fn escapes() {
        assert!(m("h\\*llo", "h*llo"));
        assert!(!m("h\\*llo", "hxllo"));
        assert!(m("h\\?llo", "h?llo"));
        assert!(m("[\\]]", "]"));
        // Trailing backslash matches a literal backslash (Redis fallthrough).
        assert!(m("a\\", "a\\"));
    }

    #[test]
    fn unterminated_class_consumes_rest() {
        // `[abc` with no `]`: the class extends to pattern end.
        assert!(m("h[abc", "ha"));
        assert!(!m("h[abc", "hd"));
        // And nothing may follow the matched byte.
        assert!(!m("h[abc", "hab"));
    }

    #[test]
    fn nocase_mode() {
        assert!(glob_match(b"MaxMemory*", b"maxmemory-policy", true));
        assert!(glob_match(b"h[A-C]llo", b"hbllo", true));
        assert!(!glob_match(b"abc", b"ABC", false));
    }

    /// Oracle storm: random tiny patterns/strings vs a recursive reference
    /// implementation of the same semantics.
    #[test]
    fn storm_matches_reference() {
        fn reference(p: &[u8], s: &[u8]) -> bool {
            if p.is_empty() {
                return s.is_empty();
            }
            match p[0] {
                b'*' => reference(&p[1..], s) || (!s.is_empty() && reference(p, &s[1..])),
                b'?' if !s.is_empty() => reference(&p[1..], &s[1..]),
                b'[' if !s.is_empty() => {
                    let (hit, next) = class_match(p, 0, s[0], false);
                    hit && reference(&p[next..], &s[1..])
                }
                b'\\' if p.len() >= 2 && !s.is_empty() => {
                    p[1] == s[0] && reference(&p[2..], &s[1..])
                }
                c if !s.is_empty() => c == s[0] && reference(&p[1..], &s[1..]),
                _ => false,
            }
        }
        let mut x: u64 = 0xBADC_0FFE;
        let mut rand = move || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        let alphabet: &[u8] = b"ab*?[]^-\\c";
        for case in 0..50_000u32 {
            let plen = (rand() % 8) as usize;
            let slen = (rand() % 8) as usize;
            let pat: Vec<u8> =
                (0..plen).map(|_| alphabet[(rand() % alphabet.len() as u64) as usize]).collect();
            let s: Vec<u8> = (0..slen).map(|_| b"abc"[(rand() % 3) as usize]).collect();
            assert_eq!(
                glob_match(&pat, &s, false),
                reference(&pat, &s),
                "case {case}: pattern {pat:?} vs {s:?}"
            );
        }
    }
}
