//! The control: the plants' shape with nothing planted (a clean included
//! file, nothing unused, no absent crate). It must compile.

#[path = "../shared/ok.rs"]
mod ok;

fn main() {
    let _ = ok::bound();
}
