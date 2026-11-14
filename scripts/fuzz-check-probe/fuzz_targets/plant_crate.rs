//! A file included by `#[path]` names a crate this package does not depend
//! on: the shape of a production edit that breaks a fuzz target which
//! compiles that file. The gate must see E0433 naming `absent_crate` in
//! `shared/leaf.rs`.

#[path = "../shared/leaf.rs"]
mod leaf;

fn main() {
    let _ = leaf::bound();
}
