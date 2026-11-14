//! An arm only a fuzz build compiles names a crate this package does not
//! depend on. The gate sets `cfg(fuzzing)` as cargo-fuzz does, so it must
//! see E0433 naming `absent_cfg_crate`.

fn main() {
    #[cfg(fuzzing)]
    absent_cfg_crate::f();
}
