/// Named event the launcher waits on before injecting anything that must not
/// be present while the game's protection stub unpacks the executable.
///
/// Included by both the library and the launcher rather than shared through a
/// crate dependency: making the library an rlib as well as a cdylib stopped
/// link-time optimisation applying and doubled the DLL, which is a poor trade
/// for one string.
pub const RENDERER_READY_EVENT: &str = r"Local\mhw-dlss-renderer-ready";

/// Environment variable through which the launcher tells the library which
/// companion libraries to load when the game loads its renderer: absolute
/// paths separated by `|`. Absent unless the chain file has a `renderer`
/// entry, and then nothing of this runs.
pub const AT_RENDERER_VARIABLE: &str = "MHW_DLSS_AT_RENDERER";

/// Whether a file is a build of this mod, old or new.
///
/// Every build carries the name of its settings file as plain text, and
/// System32's `dinput8.dll` and other loaders' do not. Used for exactly one
/// question: is the `dinput8.dll` in the game folder a leftover of ours.
// The launcher includes this file for the two names above and has no use for
// this function.
#[allow(dead_code)]
pub fn carries_our_marker(path: &std::path::Path) -> bool {
    const MARKER: &[u8] = b"mhw-dlss.ini";
    match std::fs::read(path) {
        Ok(bytes) => bytes.windows(MARKER.len()).any(|window| window == MARKER),
        Err(_) => false,
    }
}
