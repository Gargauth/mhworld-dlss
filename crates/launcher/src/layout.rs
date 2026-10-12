//! Where the mod's files are.
//!
//! Everything the mod installs or writes lives in one folder inside the game
//! folder, [`HOME_FOLDER`]: the launcher, the mod's library, NVIDIA's
//! libraries, the settings, the chain file and the logs. Removing the mod is
//! removing that folder.
//!
//! The mod's library has to sit beside NVIDIA's: NGX looks for the DLSS
//! library in the folder of the module that initialises it, not in the
//! application data path it is given.
//!
//! Every function here takes an `exists` predicate instead of touching the
//! file system, so the decisions are tested without files.

use std::path::{Path, PathBuf};

/// The folder inside the game folder that holds everything of the mod's.
pub const HOME_FOLDER: &str = "mhw-dlss";
/// The mod's library, in [`HOME_FOLDER`].
pub const PAYLOAD: &str = "mhw-dlss.dll";
/// This program, in [`HOME_FOLDER`].
pub const LAUNCHER_EXE: &str = "mhw-dlss-launcher.exe";

/// The folder the mod's files are in, given the folder this launcher is in,
/// and whether that is a different folder.
///
/// They are beside the launcher. A launcher started from the game folder,
/// as an earlier install had it, finds them one folder down.
pub fn home_of(directory: &Path, exists: impl Fn(&Path) -> bool) -> (PathBuf, bool) {
    let below = directory.join(HOME_FOLDER);
    if !exists(&directory.join(PAYLOAD)) && exists(&below.join(PAYLOAD)) {
        (below, true)
    } else {
        (directory.to_path_buf(), false)
    }
}

/// The game folder, when `home` is the [`HOME_FOLDER`] inside one.
pub fn folder_above(home: &Path) -> Option<PathBuf> {
    let named = home
        .file_name()
        .is_some_and(|name| name.to_string_lossy().eq_ignore_ascii_case(HOME_FOLDER));
    if named {
        home.parent().map(Path::to_path_buf)
    } else {
        None
    }
}

/// Whether two paths name the same file as far as text can tell: the same
/// characters regardless of case and of which slash was used.
pub fn same_file_name(a: &str, b: &str) -> bool {
    let fold = |text: &str| text.replace('/', "\\").to_lowercase();
    fold(a) == fold(b)
}

/// Whether `resolved` is one of the ways an earlier chain file spelled the
/// mod: `mhw-dlss\mhw-dlss.dll` read from inside the folder, or
/// `mhw-dlss.dll` read from the game folder.
fn is_earlier_spelling_of_the_mod(home: &Path, resolved: &Path) -> bool {
    let resolved = resolved.to_string_lossy();
    let nested = home.join(HOME_FOLDER).join(PAYLOAD);
    same_file_name(&resolved, &nested.to_string_lossy())
        || folder_above(home)
            .is_some_and(|above| same_file_name(&resolved, &above.join(PAYLOAD).to_string_lossy()))
}

/// Whether a resolved chain entry names the mod's library, however spelled.
pub fn names_the_mod(home: &Path, resolved: &Path) -> bool {
    same_file_name(
        &resolved.to_string_lossy(),
        &home.join(PAYLOAD).to_string_lossy(),
    ) || is_earlier_spelling_of_the_mod(home, resolved)
}

/// A chain entry for the mod, pointed at the mod's library in `home`, and
/// whether it was redirected.
///
/// The chain file is the user's and is never rewritten, so a line written
/// for an earlier layout keeps working. Any other entry, and an earlier
/// spelling when the library is not in `home` either (it is then reported
/// missing), is returned as given.
pub fn mod_entry(
    home: &Path,
    resolved: PathBuf,
    exists: impl Fn(&Path) -> bool,
) -> (PathBuf, bool) {
    let payload = home.join(PAYLOAD);
    if is_earlier_spelling_of_the_mod(home, &resolved) && exists(&payload) {
        (payload, true)
    } else {
        (resolved, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `C:\Game`: on a non-Windows host the backslash is part of the first
    /// component's name, and every join after it works the same.
    fn game() -> PathBuf {
        PathBuf::from("C:\\Game")
    }

    #[test]
    fn the_mods_folder_is_beside_the_launcher_or_one_folder_down() {
        let game = game();
        let folder = game.join(HOME_FOLDER);
        let payload = folder.join(PAYLOAD);
        assert_eq!(home_of(&folder, |p| p == payload), (folder.clone(), false));
        assert_eq!(home_of(&game, |p| p == payload), (folder.clone(), true));
        // Nothing found: beside the launcher, where it is reported missing.
        assert_eq!(home_of(&game, |_| false), (game.clone(), false));
    }

    #[test]
    fn only_the_mods_own_folder_has_a_game_folder_above_it() {
        let game = game();
        assert_eq!(folder_above(&game.join(HOME_FOLDER)), Some(game.clone()));
        assert_eq!(folder_above(&game.join("MHW-DLSS")), Some(game.clone()));
        assert_eq!(folder_above(&game), None);
    }

    #[test]
    fn paths_compare_without_case_or_slash_direction() {
        assert!(same_file_name(
            "C:\\Game\\mhw-dlss.dll",
            "c:/game/MHW-DLSS.dll"
        ));
        assert!(!same_file_name(
            "C:\\Game\\mhw-dlss.dll",
            "C:\\Other\\mhw-dlss.dll"
        ));
    }

    #[test]
    fn every_spelling_of_the_mod_is_the_mod_and_nothing_else_is() {
        let game = game();
        let folder = game.join(HOME_FOLDER);
        assert!(names_the_mod(&folder, &folder.join(PAYLOAD)));
        assert!(names_the_mod(
            &folder,
            Path::new("c:/game/mhw-dlss/MHW-DLSS.DLL")
        ));
        assert!(names_the_mod(
            &folder,
            &folder.join(HOME_FOLDER).join(PAYLOAD)
        ));
        assert!(names_the_mod(&folder, &game.join(PAYLOAD)));
        assert!(!names_the_mod(&folder, &game.join("ReShade64.dll")));
        assert!(!names_the_mod(&folder, &folder.join("nvngx_dlss.dll")));
        assert!(!names_the_mod(
            &folder,
            &PathBuf::from("C:\\Other").join(PAYLOAD)
        ));
    }

    #[test]
    fn an_earlier_spelling_is_redirected_only_when_the_library_is_home() {
        let game = game();
        let folder = game.join(HOME_FOLDER);
        let payload = folder.join(PAYLOAD);
        let nested = folder.join(HOME_FOLDER).join(PAYLOAD);
        let stray = game.join(PAYLOAD);
        let other = folder.join("ReShade64.dll");

        // Not home: left to be reported missing.
        assert_eq!(
            mod_entry(&folder, nested.clone(), |_| false),
            (nested.clone(), false)
        );
        let home = |p: &Path| p == payload;
        assert_eq!(
            mod_entry(&folder, payload.clone(), home),
            (payload.clone(), false)
        );
        assert_eq!(mod_entry(&folder, nested, home), (payload.clone(), true));
        // A stray copy in the game folder is not what is injected.
        assert_eq!(mod_entry(&folder, stray, home), (payload.clone(), true));
        assert_eq!(mod_entry(&folder, other.clone(), home), (other, false));
    }
}
