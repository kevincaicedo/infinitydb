//! M3-S16 AC: the accepted scalar lane performs zero heap allocations on
//! both canonical tape and arena-tree representations after construction.
//! One test owns this binary because the counter is process-global.

use inf_alloc::CountingAllocator;
use inf_alloc::arena::{Arena, ArenaConfig};
use inf_doc::apply::{ApplyOp, ScalarPatch, patch_scalar_in_place};
use inf_doc::path::compile;
use inf_doc::{ArenaDoc, JsonParser, TapeDoc};

#[global_allocator]
static ALLOC: CountingAllocator = CountingAllocator::new();

#[test]
fn accepted_tape_and_arena_scalar_patches_allocate_nothing() {
    let original = JsonParser::new().parse(br#"{"n":40,"enabled":false}"#).expect("fixture parses");
    let program = compile(b"$.enabled").expect("path compiles");
    let mut tape = original.clone();
    let doc = TapeDoc::from_validated_bytes(&original);
    let mut arena = Arena::new(ArenaConfig::default());
    let mut tree = ArenaDoc::from_tape(&doc, &mut arena).expect("morph");

    // Warm both paths before observing the process-global counter.
    assert!(matches!(
        patch_scalar_in_place(&mut tape, &program, &ApplyOp::Toggle),
        Ok(ScalarPatch::Toggled(true))
    ));
    assert!(matches!(
        tree.patch_scalar(&mut arena, &program, &ApplyOp::Toggle),
        Ok(ScalarPatch::Toggled(true))
    ));

    let before = ALLOC.allocations();
    for _ in 0..10_000 {
        assert!(matches!(
            patch_scalar_in_place(&mut tape, &program, &ApplyOp::Toggle),
            Ok(ScalarPatch::Toggled(_))
        ));
        assert!(matches!(
            tree.patch_scalar(&mut arena, &program, &ApplyOp::Toggle),
            Ok(ScalarPatch::Toggled(_))
        ));
    }
    let after = ALLOC.allocations();
    assert_eq!(after - before, 0, "accepted scalar patch path allocated");
}
