# Test-only module stripper for the `check-*.sh` greps (ADR-0106 D3).
#
# The old panic-policy grep cut each file at its FIRST `#[cfg(test)]`,
# which is only right when that attribute opens a trailing `mod tests`;
# `ckpt.rs` gates a one-line accessor that way, so 78 % of the checkpoint
# writer was never scanned (review 2026-08-30, P1c). This script strips
# exactly the regions that are test-only and nothing else:
#
#   #[cfg(test)]                      or  #[cfg(all(test, …))]
#   mod name {                        (any visibility, rustfmt-shaped)
#       …                             ← blanked, line numbers preserved
#   }                                 ← the closing brace at the SAME indent
#
# Everything else is emitted verbatim, including an inline `#[cfg(test)]`
# on a fn/impl item: that item is scanned as production. Over-approximation
# errs safe — a test-only helper that trips a rule moves into the test
# module or carries the script's `…-allow: <reason>` marker.
# `#[cfg(any(test, …))]` is NOT test-only (it also compiles under the other
# predicate) and is never stripped.
#
# Modes (`-v mode=…`):
#   (default)  print the file with test-only module bodies blanked
#   report     print `stripped <lines>`, `inline <count>` (test-only
#              attributes NOT followed by a module block — scanned as
#              production) and one `modfile <name>` per `mod name;`
#              declaration under a test-only attribute (the file it names
#              is test-only in its entirety; the caller excludes it).
#
# POSIX awk only (mawk, gawk, BSD awk): the CI matrix includes macOS.

function is_test_only_attr(line) {
    return line ~ /^[[:space:]]*#\[cfg\(test\)\][[:space:]]*$/ ||
           line ~ /^[[:space:]]*#\[cfg\(all\(test,.*\)\)\][[:space:]]*$/
}

BEGIN { skipping = 0; pending = 0; stripped = 0; inline = 0 }

skipping == 1 {
    stripped++
    if ($0 == closer) { skipping = 0 }
    if (mode != "report") { print "" }
    next
}

pending == 1 {
    pending = 0
    if ($0 ~ /^[[:space:]]*#\[/) {
        # Stacked attributes (`#[cfg(test)]` then `#[allow(…)]`): keep
        # waiting for the item; the test-only verdict carries forward.
        if (mode != "report") { print attr }
        attr = $0
        pending = 1
        next
    }
    if ($0 ~ /^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?mod[[:space:]]+[A-Za-z0-9_]+[[:space:]]*\{[[:space:]]*$/) {
        match($0, /^[[:space:]]*/)
        closer = substr($0, 1, RLENGTH) "}"
        skipping = 1
        stripped += 2
        if (mode != "report") { print ""; print "" }
        next
    }
    if ($0 ~ /^[[:space:]]*(pub(\([a-z]+\))?[[:space:]]+)?mod[[:space:]]+[A-Za-z0-9_]+[[:space:]]*;[[:space:]]*$/) {
        name = $0
        sub(/^.*mod[[:space:]]+/, "", name)
        sub(/[[:space:]]*;.*$/, "", name)
        if (mode == "report") { print "modfile " name }
        if (mode != "report") { print attr; print $0 }
        next
    }
    inline++
    if (mode != "report") { print attr }
}

is_test_only_attr($0) {
    pending = 1
    attr = $0
    next
}

{ if (mode != "report") { print } }

END {
    if (mode == "report") { print "stripped " stripped; print "inline " inline }
    if (skipping == 1 && mode == "report") { print "unterminated 1" }
}
