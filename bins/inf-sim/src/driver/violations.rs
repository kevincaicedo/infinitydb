//! A run's violations, each with the check that raised it.
//!
//! A violation is its text and its *check*: the source location of the
//! `push` that recorded it, which `#[track_caller]` hands the one
//! constructor. A function that records a violation for its caller carries
//! the attribute too, so the check is the caller's line. The compiler, not
//! a string, names the check: no entry exists without one, and an entry
//! moved between lists keeps it (`extend` and `append` take a `Violations`,
//! and no conversion from strings exists).
//!
//! The list keeps every entry: in-process readers see them all. Only what a
//! single run prints is bounded ([`Violations::printed_lines`]): every entry
//! among the run's first [`VIOLATIONS_PRINTED_FIRST`] or the first of its
//! check, in raise order, then one crossing line that counts the rest by
//! check. A witness that is a fixed part of its check's message is printed
//! whatever the run's order and count, and the first entries are printed as
//! a capped path printed them before.

use std::collections::BTreeMap;
use std::fmt;
use std::ops::Index;
use std::panic::Location;

use super::line::write_text;
use crate::limits::VIOLATIONS_PRINTED_FIRST;

/// One run's violations, in the order the run raised them.
#[derive(Clone, Default)]
pub struct Violations {
    entries: Vec<Entry>,
}

/// One violation: its text and the check that raised it.
#[derive(Clone)]
struct Entry {
    text: String,
    check: &'static Location<'static>,
}

impl Violations {
    /// The empty list.
    #[must_use]
    pub const fn new() -> Violations {
        Violations { entries: Vec::new() }
    }

    /// Records one violation; its check is the caller's source location.
    #[track_caller]
    pub fn push(&mut self, text: String) {
        self.entries.push(Entry { text, check: Location::caller() });
    }

    /// Moves every entry of `other` here, each with its check.
    pub fn extend(&mut self, other: Violations) {
        self.entries.extend(other.entries);
    }

    /// Moves every entry of `other` here, each with its check, and leaves
    /// `other` empty.
    pub fn append(&mut self, other: &mut Violations) {
        self.entries.append(&mut other.entries);
    }

    /// The texts, in raise order.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &str> + ExactSizeIterator {
        self.entries.iter().map(|entry| entry.text.as_str())
    }

    /// The first text the run raised.
    #[must_use]
    pub fn first(&self) -> Option<&str> {
        self.entries.first().map(|entry| entry.text.as_str())
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every entry, printed or not: a report that folds the count into its
    /// trace counts them all.
    #[must_use]
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// The texts, joined by `separator`.
    #[must_use]
    pub fn join(&self, separator: &str) -> String {
        self.iter().collect::<Vec<_>>().join(separator)
    }

    /// The lines a single run prints: every entry that is among the first
    /// [`VIOLATIONS_PRINTED_FIRST`] or is the first of its check, in raise
    /// order, `inf-sim: ORACLE VIOLATION: <text>` each, the text through the
    /// line writer. If an entry was left out, one crossing line follows:
    /// `inf-sim: <n> more violations not printed: <m> from the check of line
    /// <j>, …`, one clause for each check with entries left out, in the
    /// order of its first printed line `j` (counted among the printed
    /// lines). With nothing left out there is no crossing line.
    #[must_use]
    pub fn printed_lines(&self) -> Vec<String> {
        let mut lines = Vec::new();
        // A check's place in `left_out`, and per check its first printed
        // line and the entries left out, in the order of that line.
        let mut checks: BTreeMap<(&str, u32, u32), usize> = BTreeMap::new();
        let mut left_out: Vec<(usize, usize)> = Vec::new();
        for (index, entry) in self.entries.iter().enumerate() {
            let check = (entry.check.file(), entry.check.line(), entry.check.column());
            let seen = checks.get(&check).copied();
            if index < VIOLATIONS_PRINTED_FIRST || seen.is_none() {
                let mut line = String::from("inf-sim: ORACLE VIOLATION: ");
                write_text(&mut line, &entry.text);
                lines.push(line);
                if seen.is_none() {
                    checks.insert(check, left_out.len());
                    left_out.push((lines.len(), 0));
                }
            } else if let Some(clause) = seen.and_then(|place| left_out.get_mut(place)) {
                clause.1 += 1;
            }
        }
        let clauses: Vec<String> = left_out
            .iter()
            .filter(|(_, count)| *count > 0)
            .map(|(line, count)| format!("{count} from the check of line {line}"))
            .collect();
        let total: usize = left_out.iter().map(|(_, count)| count).sum();
        if total > 0 {
            lines.push(format!(
                "inf-sim: {total} more violations not printed: {}",
                clauses.join(", ")
            ));
        }
        lines
    }
}

impl Index<usize> for Violations {
    type Output = str;

    /// The text of the entry at `index`, which must be below `len()`.
    fn index(&self, index: usize) -> &str {
        &self.entries[index].text
    }
}

/// Every entry's text, as a `Vec<String>` prints: an assertion message that
/// prints a list with `{:?}` reads the same entries it read before the list
/// carried checks. The checks are not printed.
impl fmt::Debug for Violations {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_list().entries(self.iter()).finish()
    }
}

#[cfg(test)]
mod tests {
    use super::Violations;

    /// Six entries from one check, one from a second and a 3,000-byte text
    /// with a control byte from a third: the first five print, then each
    /// later check's first, the long one escaped and cut, and the crossing
    /// line counts the sixth by its check's first printed line.
    #[test]
    fn a_run_prints_its_first_five_and_each_checks_first_then_counts_the_rest() {
        let mut violations = Violations::new();
        for i in 0..6 {
            violations.push(format!("check A entry {i}"));
        }
        violations.push("check B entry".to_owned());
        violations.push(format!("check C \u{1}{}", "c".repeat(3_000)));
        let lines = violations.printed_lines();
        assert_eq!(lines.len(), 8, "{lines:#?}");
        for (i, line) in lines.iter().take(5).enumerate() {
            assert_eq!(*line, format!("inf-sim: ORACLE VIOLATION: check A entry {i}"));
        }
        assert_eq!(lines[5], "inf-sim: ORACLE VIOLATION: check B entry");
        let long = format!("inf-sim: ORACLE VIOLATION: check C \\x01{}", "c".repeat(2_048 - 12));
        assert!(lines[6].starts_with(&long), "{}", &lines[6][..80]);
        assert!(lines[6].ends_with(" … (964 bytes not printed)"), "{}", lines[6]);
        assert_eq!(lines[7], "inf-sim: 1 more violations not printed: 1 from the check of line 1");
        assert_eq!(violations.len(), 8, "the list keeps every entry");
    }

    /// The control: five entries print whole, with no crossing line, since
    /// nothing is cut.
    #[test]
    fn five_entries_print_whole_with_no_crossing_line() {
        let mut violations = Violations::new();
        for i in 0..5 {
            violations.push(format!("entry {i}"));
        }
        let lines = violations.printed_lines();
        assert_eq!(lines.len(), 5, "{lines:#?}");
        assert!(lines.iter().all(|line| line.starts_with("inf-sim: ORACLE VIOLATION: entry ")));
        assert!(Violations::new().printed_lines().is_empty());
    }

    /// An entry moved keeps its check: two lists pushed at two sites and
    /// merged print one line per check past the first five, and the crossing
    /// line names each check with entries left out.
    #[test]
    fn a_moved_entry_keeps_its_check() {
        let mut first = Violations::new();
        let mut second = Violations::new();
        for i in 0..4 {
            first.push(format!("first {i}"));
            second.push(format!("second {i}"));
        }
        let mut merged = Violations::new();
        merged.extend(first);
        merged.append(&mut second);
        assert!(second.is_empty(), "append leaves its source empty");
        let lines = merged.printed_lines();
        assert_eq!(lines.len(), 6, "{lines:#?}");
        assert_eq!(lines[4], "inf-sim: ORACLE VIOLATION: second 0", "{lines:#?}");
        assert_eq!(lines[5], "inf-sim: 3 more violations not printed: 3 from the check of line 5");
    }

    /// `Debug` prints the texts as a `Vec<String>` does, and the readers
    /// outside the crate read the texts.
    #[test]
    fn debug_and_the_readers_see_the_texts() {
        let mut violations = Violations::new();
        violations.push("a \"quoted\" one".to_owned());
        violations.push(String::from("two"));
        let strings = vec!["a \"quoted\" one".to_owned(), "two".to_owned()];
        assert_eq!(format!("{violations:?}"), format!("{strings:?}"));
        assert_eq!(violations.first(), Some("a \"quoted\" one"));
        assert_eq!(&violations[1], "two");
        assert_eq!(violations.join("|"), "a \"quoted\" one|two");
        assert_eq!(violations.iter().next_back(), Some("two"));
        let copy = violations.clone();
        assert_eq!(copy.printed_lines(), violations.printed_lines());
    }
}
