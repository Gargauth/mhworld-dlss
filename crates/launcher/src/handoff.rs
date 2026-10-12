//! What the launcher and the mod share across the process boundary, and how
//! an injection is judged.
//!
//! The names are the interface with `mhw-dlss`: the mod opens
//! [`BOOTED_EVENT`] and signals it when its boot has wired everything the
//! game's start-up reaches (the launcher resumes the game only then, or
//! after [`BOOT_TIMEOUT`]); it opens [`RENDERER_READY_EVENT`] and signals it
//! from its `LoadLibrary` hook the moment `d3d12.dll` arrives; and it loads
//! the libraries named in [`AT_RENDERER_VARIABLE`] itself, on the game's
//! thread, at that moment.

use std::path::{Path, PathBuf};
use std::time::Duration;

/// Named event, created by the launcher before the game starts, that the
/// mod signals when its boot is done. The launcher waits for it before it
/// resumes the game's main thread.
pub const BOOTED_EVENT: &str = r"Local\mhw-dlss-booted";

/// The longest the game is kept waiting for the mod's boot: by the
/// launcher before it resumes the game, and by the mod's import hooks
/// before they let the game's renderer start-up through without the mod's
/// wiring. Boot creates one Direct3D 12 device; a cold start with a virus
/// scanner takes a few seconds.
pub const BOOT_TIMEOUT: Duration = Duration::from_secs(10);

/// Named event, created by the launcher before the game starts, that the
/// mod signals when the game has loaded `d3d12.dll`. `after` entries wait
/// for it.
pub const RENDERER_READY_EVENT: &str = r"Local\mhw-dlss-renderer-ready";

/// Environment variable through which the launcher tells the mod which
/// libraries to load when the game loads its renderer: absolute paths
/// separated by [`RENDERER_LIST_SEPARATOR`]. Absent when the chain file has
/// no `renderer` entry. The game inherits it because it is created with the
/// launcher's environment.
pub const AT_RENDERER_VARIABLE: &str = "MHW_DLSS_AT_RENDERER";

/// Separates paths in [`AT_RENDERER_VARIABLE`]; `|` cannot occur in a
/// Windows path.
pub const RENDERER_LIST_SEPARATOR: char = '|';

/// The value of [`AT_RENDERER_VARIABLE`] for `paths`, `None` for none.
pub fn renderer_list<'a>(paths: impl IntoIterator<Item = &'a Path>) -> Option<String> {
    let joined: Vec<String> = paths
        .into_iter()
        .map(|path| path.to_string_lossy().into_owned())
        .collect();
    (!joined.is_empty()).then(|| joined.join(&RENDERER_LIST_SEPARATOR.to_string()))
}

/// The paths in a value of [`AT_RENDERER_VARIABLE`], empty items skipped.
pub fn parse_renderer_list(value: &str) -> Vec<PathBuf> {
    value
        .split(RENDERER_LIST_SEPARATOR)
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(PathBuf::from)
        .collect()
}

/// Whether a remote `LoadLibraryW` loaded its library.
///
/// The remote thread's exit code is the module handle cut to 32 bits, and a
/// library can be loaded at a base whose low half is zero (`0x7ff9_0000_0000`
/// is a valid base). Zero alone was once read as failure and reported a
/// working injection as failed; it now only raises the question, and the
/// game's module list answers it.
///
/// `exit_code` is `None` when it could not be read; `in_module_list` is
/// `None` when the list could not be read (or was not consulted).
pub fn was_loaded(exit_code: Option<u32>, in_module_list: Option<bool>) -> bool {
    match (exit_code, in_module_list) {
        (Some(code), _) if code != 0 => true,
        (_, Some(listed)) => listed,
        // Nothing could be read. Nothing says it failed.
        (None, None) => true,
        // A zero exit code and no module list to check it against.
        (Some(_), None) => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_zero_exit_code_alone_is_not_a_failed_injection() {
        assert!(was_loaded(Some(0x1234_0000), None));
        assert!(was_loaded(Some(0x1234_0000), Some(false)));
        assert!(was_loaded(Some(0), Some(true)));
        assert!(!was_loaded(Some(0), Some(false)));
        assert!(!was_loaded(Some(0), None));
        assert!(was_loaded(None, Some(true)));
        assert!(!was_loaded(None, Some(false)));
        assert!(was_loaded(None, None));
    }

    #[test]
    fn the_renderer_list_round_trips() {
        let paths = [
            PathBuf::from("C:\\Program Files (x86)\\Game\\mhw-dlss\\ReShade64.dll"),
            PathBuf::from("D:\\x\\OptiScaler.dll"),
        ];
        let value = renderer_list(paths.iter().map(PathBuf::as_path)).expect("a value");
        assert_eq!(value.matches(RENDERER_LIST_SEPARATOR).count(), 1);
        assert_eq!(parse_renderer_list(&value), paths);
        assert_eq!(renderer_list(std::iter::empty()), None);
        assert_eq!(
            parse_renderer_list("| a.dll ||"),
            vec![PathBuf::from("a.dll")]
        );
    }
}
