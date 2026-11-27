//! ADR-0209 D3: every std::env function that reads or writes the process
//! environment draws its lint with its exact path at one plant here. libc's
//! four entries have no plant: the probe names no third-party crate, so
//! they are resolved-only, and the deny-list grep reads their spellings
//! (ADR-0209 A1). Compile only.
#![allow(deprecated)]

use std::ffi::OsString;

pub fn var() {
    let _ = std::env::var("X"); // PLANT clippy::disallowed_methods std::env::var
}

pub fn var_os() {
    let _ = std::env::var_os("X"); // PLANT clippy::disallowed_methods std::env::var_os
}

pub fn vars() {
    let _ = std::env::vars(); // PLANT clippy::disallowed_methods std::env::vars
}

pub fn vars_os() {
    let _ = std::env::vars_os(); // PLANT clippy::disallowed_methods std::env::vars_os
}

pub fn set_var() {
    let _ = std::env::set_var::<&str, &str>; // PLANT clippy::disallowed_methods std::env::set_var
}

pub fn remove_var() {
    let _ = std::env::remove_var::<&str>; // PLANT clippy::disallowed_methods std::env::remove_var
}

pub fn temp_dir() {
    let _ = std::env::temp_dir(); // PLANT clippy::disallowed_methods std::env::temp_dir
}

pub fn home_dir() {
    let _ = std::env::home_dir(); // PLANT clippy::disallowed_methods std::env::home_dir
}

pub fn renamed_import() {
    use std::env::var_os as lookup;
    let _ = lookup("X"); // PLANT clippy::disallowed_methods std::env::var_os
}

pub fn decoder_argument() {
    // The shape a decoder takes: the function itself, handed to a caller.
    let lookup: fn(&'static str) -> Option<OsString> = std::env::var_os; // PLANT clippy::disallowed_methods std::env::var_os
    let _ = lookup;
}

pub fn compile_time() {
    let _ = env!("CARGO_PKG_NAME"); // CONTROL
}

pub fn argument_vector() {
    let _ = std::env::args(); // CONTROL
}

pub fn target_constant() {
    let _ = std::env::consts::OS; // CONTROL
}
