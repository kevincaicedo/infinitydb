//! The one writer of a printed violation text: escape first, then cut.
//!
//! A violation can quote raw key bytes, and a panic message can span
//! several lines, so every control character (U+0000 to U+001F and U+007F)
//! is written as `\xNN`, four bytes, and the text stays on its one line.
//! The escaped text is then cut at [`VIOLATION_TEXT_BYTES_MAX`] bytes,
//! between two characters and outside an escape, and a suffix counts the
//! escaped bytes left out. So a printed text is at most that many bytes and
//! its suffix, whatever its source length.
//!
//! The writer cannot panic, since the panic hook calls it and a panic
//! inside the hook aborts the process: it is one pass over the text's
//! characters that writes a whole escape or a whole character, or stops,
//! and it slices no `str`, indexes nothing and unwraps nothing.

use crate::limits::VIOLATION_TEXT_BYTES_MAX;

/// `text`, escaped and cut, appended to `out`.
pub fn write_text(out: &mut String, text: &str) {
    let mut written = 0usize;
    let mut left_out = 0usize;
    for c in text.chars() {
        let width = escaped_width(c);
        if left_out == 0 && written.saturating_add(width) <= VIOLATION_TEXT_BYTES_MAX {
            push_escaped(out, c);
            written += width;
        } else {
            left_out = left_out.saturating_add(width);
        }
    }
    if left_out > 0 {
        out.push_str(" … (");
        out.push_str(&left_out.to_string());
        out.push_str(" bytes not printed)");
    }
}

/// `text`, escaped and cut, as a new string.
#[must_use]
pub fn one_line(text: &str) -> String {
    let mut out = String::with_capacity(text.len().min(VIOLATION_TEXT_BYTES_MAX + 32));
    write_text(&mut out, text);
    out
}

/// A control character the writer escapes.
fn is_escaped(c: char) -> bool {
    matches!(c, '\u{0}'..='\u{1f}' | '\u{7f}')
}

/// The bytes `c` takes once escaped.
fn escaped_width(c: char) -> usize {
    if is_escaped(c) { 4 } else { c.len_utf8() }
}

fn push_escaped(out: &mut String, c: char) {
    if !is_escaped(c) {
        out.push(c);
        return;
    }
    let byte = u32::from(c);
    out.push('\\');
    out.push('x');
    out.push(char::from_digit(byte >> 4, 16).unwrap_or('0'));
    out.push(char::from_digit(byte & 0xf, 16).unwrap_or('0'));
}

#[cfg(test)]
mod tests {
    use super::{VIOLATION_TEXT_BYTES_MAX, one_line};

    /// The texts at the bound: a text that fits is whole with no suffix; a
    /// character astride the cut, a control byte whose escape crosses it,
    /// and a run of escapes are each cut between characters and outside an
    /// escape, and the suffix counts the escaped bytes left out.
    #[test]
    fn a_text_is_escaped_first_and_cut_second() {
        assert_eq!(VIOLATION_TEXT_BYTES_MAX, 2_048);
        let whole = "a".repeat(2_048);
        assert_eq!(one_line(&whole), whole, "2,048 ASCII bytes print whole");

        let astride = format!("{}é", "a".repeat(2_047));
        assert_eq!(one_line(&astride), format!("{} … (2 bytes not printed)", "a".repeat(2_047)));

        let control = format!("{}\u{1}", "a".repeat(2_046));
        assert_eq!(one_line(&control), format!("{} … (4 bytes not printed)", "a".repeat(2_046)));

        let escapes = format!("{}{}", "\u{2}".repeat(600), "b".repeat(100));
        let printed = one_line(&escapes);
        assert_eq!(printed, format!("{} … (452 bytes not printed)", "\\x02".repeat(512)));
    }

    /// Every control character is one four-byte escape, and every other
    /// character, a C1 control or a multi-byte one included, is kept.
    #[test]
    fn a_control_character_becomes_one_escape_and_the_line_stays_one() {
        assert_eq!(one_line("a\nb\tc\u{1}\u{7f}"), "a\\x0ab\\x09c\\x01\\x7f");
        assert_eq!(one_line("plain — text \u{85}"), "plain — text \u{85}");
        assert!(!one_line("multi\nline\r\npanic").contains(['\n', '\r']));
    }
}
