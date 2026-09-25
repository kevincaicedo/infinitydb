//! Planted violations for `scripts/check-lint-scopes.sh` (ADR-0144). Each
//! `// PLANT <lint>` line must draw exactly that clippy lint on that line;
//! each `// CONTROL` line must draw none. One plant per function, so a
//! plant can only fail for its own reason.
#![forbid(unsafe_code)]
#![cfg_attr(
    not(test),
    deny(clippy::wildcard_enum_match_arm, clippy::match_wildcard_for_single_variants)
)]

pub mod filesystem;

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

/// ADR-0144 D2/D3: the August review's two defects, re-planted under the
/// decoder deny set. Each must draw its lint; a clean compile is a red gate.
pub mod decoder {
    #![cfg_attr(
        not(test),
        deny(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_possible_wrap,
            clippy::arithmetic_side_effects
        )
    )]

    // C10: a client `i64` index narrowed with `as`.
    pub fn c10(client_index: i64) -> u32 {
        client_index as u32 // PLANT clippy::cast_possible_truncation,clippy::cast_sign_loss
    }

    // C11: a slice cursor advanced with `+` by a decoded length.
    pub fn c11(cursor: usize, decoded_len: usize) -> usize {
        cursor + decoded_len // PLANT clippy::arithmetic_side_effects
    }

    pub fn wrap(len: u64) -> i64 {
        len as i64 // PLANT clippy::cast_possible_wrap
    }

    pub fn checked(cursor: usize, decoded_len: usize) -> Option<usize> {
        cursor.checked_add(decoded_len) // CONTROL
    }

    pub fn narrowed(client_index: i64) -> Option<u32> {
        u32::try_from(client_index).ok() // CONTROL
    }
}

/// The mixed tier: casts denied where arithmetic still ratchets. The
/// narrowing must draw its lint and the `+` must draw nothing — a cast-only
/// attribute that also fired on arithmetic would make the tier unusable.
pub mod casts_only {
    #![cfg_attr(
        not(test),
        deny(clippy::cast_possible_truncation, clippy::cast_sign_loss, clippy::cast_possible_wrap)
    )]

    pub fn narrowing(decoded_len: u64) -> u32 {
        decoded_len as u32 // PLANT clippy::cast_possible_truncation
    }

    pub fn ratcheted_sum(cursor: usize, decoded_len: usize) -> usize {
        cursor + decoded_len // CONTROL
    }
}

/// The item form: a decoder that is one function inside a larger file.
#[cfg_attr(not(test), deny(clippy::arithmetic_side_effects))]
pub fn item_scope(cursor: usize, decoded_len: usize) -> usize {
    cursor + decoded_len // PLANT clippy::arithmetic_side_effects
}
