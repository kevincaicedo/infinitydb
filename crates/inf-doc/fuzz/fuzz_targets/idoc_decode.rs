//! idoc decoder fuzz (M3-S01/S02 + S04, L9): arbitrary bytes never panic,
//! and every *accepted* document is canonically stable — re-encoding the
//! decoded model reproduces the canonical plain form byte-for-byte, and
//! the arena projection freezes back to it. With the `doc-intern-keys`
//! feature (always on for this target — the fuzz surface must cover the
//! interned arm) the oracle extends per ADR-0038: `unintern ∘ intern` is
//! the identity, interned decode equals plain decode, and freezing an
//! interned document yields its canonical plain bytes.

#![no_main]

use libfuzzer_sys::fuzz_target;

use inf_alloc::arena::{Arena, ArenaConfig};
use inf_doc::model;
use inf_doc::{ArenaDoc, FLAG_INTERNED, TapeDoc, intern};

fuzz_target!(|data: &[u8]| {
    let Ok(doc) = TapeDoc::from_bytes(data) else {
        return; // rejection is a fine outcome; panics are the bug
    };
    let value = model::from_tape(&doc);
    let canonical_plain = model::encode(&value).expect("accepted documents re-encode");
    let interned_input = data[3] & FLAG_INTERNED != 0;
    if interned_input {
        // The plain projection of an accepted interned doc is canonical
        // and value-identical (ADR-0038 D3).
        let plain = intern::unintern(data);
        assert_eq!(plain, canonical_plain, "unintern yields the canonical plain form");
        let plain_doc = TapeDoc::from_bytes(&plain).expect("unintern output validates");
        assert_eq!(model::from_tape(&plain_doc), value, "unintern preserves the value");
    } else {
        assert_eq!(
            canonical_plain, data,
            "canonical stability: accepted plain bytes re-encode identically"
        );
        // Interning round-trips and strictly shrinks or declines.
        if let Some(interned) = intern::intern(data) {
            assert!(interned.len() < data.len(), "interned form is strictly smaller");
            assert_eq!(intern::unintern(&interned), data, "unintern of intern is the identity");
            let idoc = TapeDoc::from_bytes(&interned).expect("intern output validates");
            assert_eq!(model::from_tape(&idoc), value, "interned decode equals plain decode");
        }
    }
    let mut arena = Arena::new(ArenaConfig::default());
    let adoc = ArenaDoc::from_tape(&doc, &mut arena).expect("morph of a valid doc");
    let frozen = adoc.freeze(&arena).expect("freeze of a valid doc");
    assert_eq!(frozen, canonical_plain, "freeze(morph(t)) == canonical plain t");
    adoc.free(&mut arena);
    assert_eq!(arena.report().live_bytes, 0, "no leaks across morph/free");
});
