//! Bounded Linux process-stat parsing; independent of clocks and filesystem I/O.
#![cfg_attr(
    not(test),
    deny(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_possible_wrap,
        clippy::arithmetic_side_effects
    )
)]

/// (system, user) CPU microseconds. Preserve INFO's Linux USER_HZ=100
/// convention; scheduler tick frequency is not this exported unit.
pub(super) fn cpu_microseconds(text: &str) -> Option<(u64, u64)> {
    if text.len() as u64 > crate::limits::PROCESS_STAT_BYTES_MAX {
        return None;
    }
    // comm may contain spaces and ')'; the final ')' terminates it.
    let (_, after) = text.rsplit_once(')')?;
    let mut fields = after.split_whitespace();
    let user = fields.nth(11)?.parse::<u64>().ok()?;
    let sys = fields.next()?.parse::<u64>().ok()?;
    Some((sys.saturating_mul(10_000), user.saturating_mul(10_000)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cpu_fields_follow_the_parenthesized_name() {
        let prefix = "1 (a name ) with parens) R 0 0 0 0 0 0 0 0 0 0";
        assert_eq!(cpu_microseconds(&format!("{prefix} 23 17 0")), Some((170_000, 230_000)));
        for suffix in ["", " 23", " -1 17", " 23 invalid"] {
            assert_eq!(cpu_microseconds(&format!("{prefix}{suffix}")), None);
        }
        assert_eq!(cpu_microseconds("no delimiter"), None);
        assert_eq!(
            cpu_microseconds(&format!("{prefix} {} {}", u64::MAX, u64::MAX)),
            Some((u64::MAX, u64::MAX))
        );
        let mut oversized = format!("{prefix} 23 17");
        oversized.extend(std::iter::repeat_n(' ', crate::limits::PROCESS_STAT_BYTES_MAX as usize));
        assert_eq!(cpu_microseconds(&oversized), None);
    }
}
