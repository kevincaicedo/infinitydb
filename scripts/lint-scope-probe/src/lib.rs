//! Planted violations for `scripts/check-lint-scopes.sh` (ADR-0144). Each
//! `// PLANT <lint>` line must draw exactly that clippy lint on that line;
//! each `// CONTROL` line must draw none. One plant per function, so a
//! plant can only fail for its own reason.
#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)
)]

pub enum Three {
    A,
    B,
    C,
}

pub fn wildcard(e: Three) -> u8 {
    match e {
        Three::A => 1,
        _ => 0, // PLANT clippy::wildcard_enum_match_arm
    }
}

pub fn binding(e: Three) -> u8 {
    match e {
        Three::A => 1,
        other => other as u8, // PLANT clippy::wildcard_enum_match_arm
    }
}

// `wildcard_enum_match_arm` is silent when exactly one variant is left.
pub fn one_left(e: Three) -> u8 {
    match e {
        Three::A => 1,
        Three::B => 2,
        _ => 0, // PLANT clippy::match_wildcard_for_single_variants
    }
}

// A foreign `#[non_exhaustive]` enum: the lint fires; the wildcard is
// unavoidable and lives under a `foreign:` allow.
pub fn foreign(e: std::io::ErrorKind) -> u8 {
    match e {
        std::io::ErrorKind::NotFound => 1,
        _ => 0, // PLANT clippy::wildcard_enum_match_arm
    }
}

pub fn named(e: Three) -> u8 {
    match e {
        Three::A => 1,
        Three::B | Three::C => 0, // CONTROL
    }
}

pub fn integer(n: u8) -> u8 {
    match n {
        0 => 1,
        _ => 0, // CONTROL
    }
}
