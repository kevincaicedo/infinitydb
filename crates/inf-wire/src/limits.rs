//! Representation limits owned by the RESP decoder and the reply writer.

/// Bytes addressed by one argv frame's u32 offsets. Larger declarations
/// return `WireError::FrameTooLong` before any payload is buffered.
pub const ARGV_FRAME_BYTES_MAX: usize = u32::MAX as usize;

/// Entries represented by one argv's u32 count. Larger declarations
/// return `WireError::TooManyArgs` before any argv storage grows.
pub const ARGV_ENTRIES_MAX: usize = u32::MAX as usize;

/// Bytes a patched bulk's reserved length header can shrink by when it is
/// patched: the reserve is `PATCHED_DIGITS` digits and a length needs at
/// least one. A reply account that bounds a patched bulk while its payload
/// is still being written lets the payload run this far past the account,
/// then checks the patched frame; a frame over the account is rolled back
/// and refused (ADR-0099 A1).
pub const PATCHED_HEADER_SLACK_BYTES: usize = crate::writer::PATCHED_DIGITS - 1;

/// The longest frame one `f64` reply takes: the RESP2 bulk
/// `$336\r\n<336 bytes>\r\n` (344 B; the RESP3 `,<336 bytes>\r\n` is
/// 339 B), with 336 the longest `Display` of an `f64`. A reply account that
/// must admit a double before its value exists reserves this much; no
/// double reply writes more (ADR-0099 A1).
pub const DOUBLE_REPLY_BYTES_MAX: usize = {
    let display = crate::writer::F64_DISPLAY_MAX;
    let resp2 = 1 + decimal_digits(display) + 2 + display + 2;
    let resp3 = 1 + display + 2;
    if resp2 > resp3 { resp2 } else { resp3 }
};

const _: () = assert!(DOUBLE_REPLY_BYTES_MAX == 344, "the widest double frame is 344 bytes");

/// Decimal digits of `value`: the width of a RESP length header.
const fn decimal_digits(mut value: usize) -> usize {
    let mut digits = 1;
    while value >= 10 {
        value /= 10;
        digits += 1;
    }
    digits
}
