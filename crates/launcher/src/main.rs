//! `mhw-dlss-launcher.exe`: see the library documentation
//! (`src/lib.rs`) for why it exists and how it is used.
//!
//! A GUI-subsystem program: it never opens a window of its own accord.
//! Everything it has to say goes to `mhw-dlss-launcher.log`, a failure the
//! user has to know about gets a message box, and `--verbose` adds a
//! console.

#![cfg_attr(windows, windows_subsystem = "windows")]

#[cfg(windows)]
mod run;
#[cfg(windows)]
mod sys;

#[cfg(windows)]
fn main() {
    std::process::exit(run::main());
}

/// Built on other hosts only so that the workspace builds and its tests run
/// there; there is no game to start.
#[cfg(not(windows))]
fn main() {
    eprintln!("mhw-dlss-launcher starts Monster Hunter: World and runs on Windows only.");
    std::process::exit(2);
}
