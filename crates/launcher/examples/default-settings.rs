//! Print the settings file the mod writes on first start.
//!
//! The release packaging runs this on the build host to ship
//! `mhw-dlss.default.ini`, from which `install.ps1` starts a settings file
//! when it has to set a key before the mod has ever run:
//!
//! ```text
//! cargo run -p mhw-dlss-launcher --example default-settings > mhw-dlss.default.ini
//! cargo run -p mhw-dlss-launcher --example default-settings --features dev   # with [Dev]/[Experimental]
//! ```

use mhw_core::config::{Build, Settings};

fn main() {
    print!("{}", Settings::write_default(Build::CURRENT));
}
