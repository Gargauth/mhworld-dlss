//! The chain file: what is loaded into the game, and when.
//!
//! `mhw-dlss-loader.ini` in the mod's folder lists one library per line. The
//! launcher writes [`DEFAULT_CHAIN`] the first time; after that the file is
//! the user's and is only read.

use std::path::{Path, PathBuf};

use crate::layout::{self, folder_above};

/// The chain file, in the mod's folder.
pub const CHAIN_FILE: &str = "mhw-dlss-loader.ini";

/// What the chain file says when it does not exist yet.
pub const DEFAULT_CHAIN: &str = "\
# Libraries loaded into the game, one per line, in this order. A name or a
# relative path is looked for from the folder this file is in; an absolute
# path is taken as given, and a path may contain spaces or be written in
# double quotes. Lines starting with # are ignored, and a library that cannot
# be found is reported in mhw-dlss-launcher.log and does not stop the others.
#
# mhw-dlss.dll is the mod itself and stays first. It is in this folder,
# beside the NVIDIA libraries that fetch-nvidia.cmd puts here.
#
# A line may end with one of two words.
#
#   renderer   loaded by the mod itself, on the game's own thread, at the
#              moment the game loads d3d12.dll and before it creates its DXGI
#              factory or its device. This is the word for ReShade.
#   after      injected by the launcher once the game has loaded d3d12.dll,
#              optionally with a fallback timeout in milliseconds:
#              `after 1000`. For tools that hook nothing at creation, such as
#              RenderDoc's capture library.
#
# ReShade cannot load itself in this game: the game loads none of the usual
# proxy names from its folder in time. It has to be in before the game
# creates its DXGI factory, and `after` is too late for that, so use
# `renderer`: put ReShade64.dll in this folder and remove the # below.

mhw-dlss.dll
# ReShade64.dll  renderer
";

/// Fallback timeout for an `after` entry, used only when the mod never
/// signals that the renderer's library has loaded.
pub const DEFAULT_TIMEOUT_MS: u64 = 15_000;

/// When a chain line asks to be loaded, as written.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum When {
    /// By the launcher, before the game's first instruction.
    Suspended,
    /// By the mod, when the game loads its renderer.
    Renderer,
    /// By the launcher once the renderer is loaded, with the fallback
    /// timeout in milliseconds if one was written.
    After(Option<u64>),
}

/// One line of the chain file, taken apart.
#[derive(Debug, PartialEq, Eq)]
pub struct ChainLine {
    /// The path as written, without quotes and keyword.
    pub name: String,
    /// When it is loaded.
    pub when: When,
    /// Something on the line that was not understood, for the log.
    pub complaint: Option<String>,
}

/// Take a chain line apart: a path, then optionally `renderer`, or `after`
/// with an optional number of milliseconds. `None` for a blank line or a
/// comment.
///
/// A path may contain spaces, as the game's default install path does, so
/// the keyword is taken off the *end* of the line and everything before it
/// is the path. A path may also be written in double quotes, and then
/// whatever follows the closing quote is the keyword and is checked.
pub fn parse_line(line: &str) -> Option<ChainLine> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    if let Some(rest) = line.strip_prefix('"') {
        let (name, tail) = rest.split_once('"').unwrap_or((rest, ""));
        let (when, complaint) = keyword_after_quote(tail);
        return Some(ChainLine {
            name: name.trim().to_owned(),
            when,
            complaint,
        });
    }

    let words: Vec<&str> = line.split_whitespace().collect();
    let last = words.last().copied().unwrap_or_default();
    let before_last = words.len().checked_sub(2).and_then(|i| words.get(i));
    let (name, when) = if words.len() >= 2 && last.eq_ignore_ascii_case("renderer") {
        (without_last_words(line, 1), When::Renderer)
    } else if words.len() >= 2 && last.eq_ignore_ascii_case("after") {
        (without_last_words(line, 1), When::After(None))
    } else if let (true, Some(word), Ok(ms)) = (words.len() >= 3, before_last, last.parse::<u64>())
    {
        if word.eq_ignore_ascii_case("after") {
            (without_last_words(line, 2), When::After(Some(ms)))
        } else {
            (line.to_owned(), When::Suspended)
        }
    } else {
        (line.to_owned(), When::Suspended)
    };
    Some(ChainLine {
        name,
        when,
        complaint: None,
    })
}

/// The keyword after a quoted path, and a complaint about anything else.
fn keyword_after_quote(tail: &str) -> (When, Option<String>) {
    let mut words = tail.split_whitespace();
    match (words.next(), words.next()) {
        (None, _) => (When::Suspended, None),
        (Some(word), None) if word.eq_ignore_ascii_case("renderer") => (When::Renderer, None),
        (Some(word), number) if word.eq_ignore_ascii_case("after") => match number {
            None => (When::After(None), None),
            Some(number) => match number.parse() {
                Ok(ms) => (When::After(Some(ms)), None),
                Err(_) => (
                    When::After(None),
                    Some(format!("`{number}` is not a number of milliseconds")),
                ),
            },
        },
        (Some(word), _) => (
            When::Suspended,
            Some(format!("ignoring unrecognised `{word}`")),
        ),
    }
}

/// `line` without its last `count` whitespace-separated words.
fn without_last_words(line: &str, count: usize) -> String {
    let mut rest = line;
    for _ in 0..count {
        let cut = rest.trim_end();
        let end = cut.rfind(char::is_whitespace).map_or(0, |index| index + 1);
        rest = cut.get(..end).unwrap_or_default();
    }
    rest.trim_end().to_owned()
}

/// When an entry is loaded, with defaults applied.
#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum Load {
    /// Injected by the launcher while the game is suspended.
    Suspended,
    /// Handed to the mod through [`crate::handoff::AT_RENDERER_VARIABLE`].
    Renderer,
    /// Injected by the launcher once the renderer is loaded, or after
    /// `timeout_ms` without that signal.
    After {
        /// Fallback timeout in milliseconds.
        timeout_ms: u64,
    },
}

impl Load {
    /// What the log says about it.
    pub fn describe(self) -> String {
        match self {
            Load::Suspended => "while suspended".to_owned(),
            Load::Renderer => "loaded by the mod when the game loads its renderer".to_owned(),
            Load::After { timeout_ms } => format!("when the renderer loads, or {timeout_ms} ms"),
        }
    }
}

/// One library to load.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Entry {
    /// Where it is, resolved.
    pub path: PathBuf,
    /// When.
    pub load: Load,
}

/// Which chain file is read, and whether it is the one in the folder above.
///
/// The one in `home`. When there is none and the folder above (the game
/// folder) has one, an earlier install's, that file is used where it is: it
/// is the user's and is not moved. When neither exists the default is
/// written in `home`.
pub fn chain_file(home: &Path, exists: impl Fn(&Path) -> bool) -> (PathBuf, bool) {
    let here = home.join(CHAIN_FILE);
    if !exists(&here) {
        if let Some(above) = folder_above(home).map(|folder| folder.join(CHAIN_FILE)) {
            if exists(&above) {
                return (above, true);
            }
        }
    }
    (here, false)
}

/// The chain file's text, taken apart.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct Chain {
    /// Every entry, in file order.
    pub entries: Vec<Entry>,
    /// Lines for the log: complaints about the file, redirections.
    pub notes: Vec<String>,
}

/// Read a chain file's `text`. Relative names are resolved from `base`, the
/// folder the file is in; an earlier spelling of the mod is pointed at the
/// library in `home` ([`layout::mod_entry`]).
pub fn read(text: &str, base: &Path, home: &Path, exists: impl Fn(&Path) -> bool) -> Chain {
    let mut chain = Chain::default();
    for parsed in text.lines().filter_map(parse_line) {
        if let Some(complaint) = &parsed.complaint {
            chain.notes.push(format!("{}: {complaint}", parsed.name));
        }
        let load = match parsed.when {
            When::Suspended => Load::Suspended,
            When::Renderer => Load::Renderer,
            When::After(timeout) => Load::After {
                timeout_ms: timeout.unwrap_or(DEFAULT_TIMEOUT_MS),
            },
        };
        let candidate = Path::new(&parsed.name);
        let resolved = if candidate.is_absolute() {
            candidate.to_path_buf()
        } else {
            base.join(candidate)
        };
        let (path, redirected) = layout::mod_entry(home, resolved.clone(), &exists);
        if redirected {
            chain.notes.push(format!(
                "{CHAIN_FILE} names the mod as {}, as an earlier layout had it; injecting {} \
                 instead. The line can be changed to say {}",
                resolved.display(),
                path.display(),
                layout::PAYLOAD
            ));
        }
        chain.entries.push(Entry { path, load });
    }
    chain
}

/// What will be done with the entries, decided before the game starts so
/// that a typo is reported before the game runs without the mod.
#[derive(Debug, PartialEq, Eq, Default)]
pub struct Plan {
    /// Entries whose file exists, in file order.
    pub present: Vec<Entry>,
    /// Listed files that do not exist.
    pub missing: Vec<PathBuf>,
    /// The mod itself is listed and its library is missing.
    pub mod_missing: bool,
}

impl Plan {
    /// Split `entries` into what exists and what does not.
    pub fn new(entries: Vec<Entry>, home: &Path, exists: impl Fn(&Path) -> bool) -> Plan {
        let mut plan = Plan::default();
        for entry in entries {
            if exists(&entry.path) {
                plan.present.push(entry);
            } else {
                plan.mod_missing |= layout::names_the_mod(home, &entry.path);
                plan.missing.push(entry.path);
            }
        }
        plan
    }

    /// Entries injected while the game is suspended.
    pub fn suspended(&self) -> impl Iterator<Item = &Entry> {
        self.with(Load::Suspended)
    }

    /// Entries the mod loads when the renderer loads.
    pub fn renderer(&self) -> impl Iterator<Item = &Entry> {
        self.with(Load::Renderer)
    }

    /// Entries injected once the renderer is loaded.
    pub fn late(&self) -> impl Iterator<Item = &Entry> {
        self.present
            .iter()
            .filter(|entry| matches!(entry.load, Load::After { .. }))
    }

    /// How long to wait for the renderer signal: the longest timeout of the
    /// late entries, `None` when there is none.
    pub fn late_timeout_ms(&self) -> Option<u64> {
        self.late()
            .filter_map(|entry| match entry.load {
                Load::After { timeout_ms } => Some(timeout_ms),
                _ => None,
            })
            .max()
    }

    /// Missing files other than the mod's own library (which has a message
    /// of its own).
    pub fn missing_others<'a>(&'a self, home: &'a Path) -> impl Iterator<Item = &'a PathBuf> {
        self.missing
            .iter()
            .filter(move |path| !layout::names_the_mod(home, path))
    }

    fn with(&self, load: Load) -> impl Iterator<Item = &Entry> {
        self.present.iter().filter(move |entry| entry.load == load)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{HOME_FOLDER, PAYLOAD};

    fn line(text: &str) -> ChainLine {
        parse_line(text).expect("a line")
    }

    fn plain(name: &str, when: When) -> ChainLine {
        ChainLine {
            name: name.to_owned(),
            when,
            complaint: None,
        }
    }

    const SPACED: &str =
        "C:\\Program Files (x86)\\Steam\\steamapps\\common\\Monster Hunter World\\OptiScaler.dll";

    #[test]
    fn blank_lines_and_comments_are_nothing() {
        assert_eq!(parse_line(""), None);
        assert_eq!(parse_line("   \t"), None);
        assert_eq!(parse_line("   # ReShade64.dll  renderer"), None);
    }

    #[test]
    fn a_line_is_a_path_and_an_optional_keyword_at_its_end() {
        assert_eq!(line("mhw-dlss.dll"), plain("mhw-dlss.dll", When::Suspended));
        assert_eq!(
            line("ReShade64.dll  renderer"),
            plain("ReShade64.dll", When::Renderer)
        );
        assert_eq!(
            line("renderdoc.dll after 1000"),
            plain("renderdoc.dll", When::After(Some(1000)))
        );
        assert_eq!(
            line("renderdoc.dll AFTER"),
            plain("renderdoc.dll", When::After(None))
        );
        assert_eq!(
            line("..\\x.dll renderer"),
            plain("..\\x.dll", When::Renderer)
        );
    }

    #[test]
    fn a_path_may_hold_spaces() {
        assert_eq!(line(SPACED), plain(SPACED, When::Suspended));
        assert_eq!(
            line(&format!("{SPACED}  renderer")),
            plain(SPACED, When::Renderer)
        );
        assert_eq!(
            line(&format!("  {SPACED}\tafter   250  ")),
            plain(SPACED, When::After(Some(250)))
        );
    }

    #[test]
    fn a_quoted_path_is_followed_by_a_checked_keyword() {
        assert_eq!(
            line(&format!("\"{SPACED}\"")),
            plain(SPACED, When::Suspended)
        );
        assert_eq!(
            line(&format!("\"{SPACED}\" renderer")),
            plain(SPACED, When::Renderer)
        );
        assert_eq!(
            line("\"a b.dll\" after 30"),
            plain("a b.dll", When::After(Some(30)))
        );
        let odd = line("\"a b.dll\" sometimes");
        assert_eq!((odd.name.as_str(), odd.when), ("a b.dll", When::Suspended));
        assert!(odd.complaint.is_some_and(|text| text.contains("sometimes")));
        let odd = line("\"a b.dll\" after soon");
        assert_eq!(odd.when, When::After(None));
        assert!(odd.complaint.is_some_and(|text| text.contains("soon")));
        // An unclosed quote is the rest of the line.
        assert_eq!(line("\"a b.dll"), plain("a b.dll", When::Suspended));
    }

    #[test]
    fn keywords_alone_or_out_of_place_are_part_of_the_name() {
        assert_eq!(line("renderer"), plain("renderer", When::Suspended));
        assert_eq!(line("after"), plain("after", When::Suspended));
        assert_eq!(
            line("my after party.dll"),
            plain("my after party.dll", When::Suspended)
        );
        assert_eq!(
            line("x.dll before 10"),
            plain("x.dll before 10", When::Suspended)
        );
    }

    #[test]
    fn the_default_chain_loads_the_mod_and_nothing_else() {
        let chain = read(DEFAULT_CHAIN, Path::new("home"), Path::new("home"), |_| {
            true
        });
        assert_eq!(
            chain.entries,
            vec![Entry {
                path: Path::new("home").join(PAYLOAD),
                load: Load::Suspended
            }]
        );
        assert!(chain.notes.is_empty());
    }

    #[test]
    fn the_chain_file_is_home_unless_only_the_game_folder_has_one() {
        let game = PathBuf::from("C:\\Game");
        let folder = game.join(HOME_FOLDER);
        let here = folder.join(CHAIN_FILE);
        let above = game.join(CHAIN_FILE);
        assert_eq!(chain_file(&folder, |_| false), (here.clone(), false));
        assert_eq!(chain_file(&folder, |_| true), (here.clone(), false));
        assert_eq!(chain_file(&folder, |p| p == above), (above.clone(), true));
        // Not the mod's own folder: no folder above to look in.
        assert_eq!(chain_file(&game, |p| p == above), (above.clone(), false));
    }

    #[test]
    fn names_resolve_from_the_files_folder_and_absolute_paths_stay() {
        let home = PathBuf::from("C:\\Game").join(HOME_FOLDER);
        let absolute = std::env::temp_dir().join("x.dll");
        let text = format!(
            "ReShade64.dll renderer\n{}\nsub\\y.dll after\n",
            absolute.display()
        );
        let chain = read(&text, &home, &home, |_| true);
        assert_eq!(
            chain.entries,
            vec![
                Entry {
                    path: home.join("ReShade64.dll"),
                    load: Load::Renderer
                },
                Entry {
                    path: absolute,
                    load: Load::Suspended
                },
                Entry {
                    path: home.join("sub\\y.dll"),
                    load: Load::After {
                        timeout_ms: DEFAULT_TIMEOUT_MS
                    }
                },
            ]
        );
    }

    #[test]
    fn an_earlier_chain_line_finds_the_mod_and_says_so() {
        let game = PathBuf::from("C:\\Game");
        let home = game.join(HOME_FOLDER);
        let payload = home.join(PAYLOAD);
        // The file in the game folder, as an earlier install had it.
        let chain = read("mhw-dlss.dll\n", &game, &home, |p| p == payload);
        assert_eq!(chain.entries[0].path, payload);
        assert!(chain.notes[0].contains("earlier layout"));
    }

    #[test]
    fn complaints_reach_the_notes() {
        let chain = read(
            "\"a.dll\" whenever\n",
            Path::new("h"),
            Path::new("h"),
            |_| true,
        );
        assert_eq!(chain.notes.len(), 1);
        assert!(chain.notes[0].starts_with("a.dll: "));
    }

    fn entry(path: &Path, load: Load) -> Entry {
        Entry {
            path: path.to_path_buf(),
            load,
        }
    }

    #[test]
    fn the_plan_separates_missing_files_and_notices_the_mod() {
        let home = PathBuf::from("C:\\Game").join(HOME_FOLDER);
        let payload = home.join(PAYLOAD);
        let reshade = home.join("ReShade64.dll");
        let doc = home.join("renderdoc.dll");
        let gone = home.join("gone.dll");
        let entries = vec![
            entry(&payload, Load::Suspended),
            entry(&reshade, Load::Renderer),
            entry(&doc, Load::After { timeout_ms: 10 }),
            entry(&gone, Load::After { timeout_ms: 99 }),
        ];

        let all_but_gone = |p: &Path| p != gone;
        let plan = Plan::new(entries.clone(), &home, all_but_gone);
        assert!(!plan.mod_missing);
        assert_eq!(plan.missing, vec![gone.clone()]);
        assert_eq!(plan.suspended().count(), 1);
        assert_eq!(
            plan.renderer().map(|e| &e.path).collect::<Vec<_>>(),
            vec![&reshade]
        );
        // The missing late entry's timeout does not count.
        assert_eq!(plan.late_timeout_ms(), Some(10));

        let only_reshade = |p: &Path| p == reshade;
        let plan = Plan::new(entries, &home, only_reshade);
        assert!(plan.mod_missing);
        assert_eq!(plan.late_timeout_ms(), None);
        assert_eq!(
            plan.missing_others(&home).collect::<Vec<_>>(),
            vec![&doc, &gone]
        );
    }
}
