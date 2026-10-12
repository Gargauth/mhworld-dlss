//! The launcher's own command line, and the game's.
//!
//! Everything after the launcher's path is the command line Steam
//! substituted for `%command%`. The launcher's options come first and all
//! begin with `-`; the first argument that is not one of them is the game,
//! and everything after it is the game's and is never looked at, so an
//! option of the game's cannot be taken for one of ours.

use std::path::PathBuf;

/// Shown when there is no game to start.
pub const USAGE: &str = "\
Usage: mhw-dlss-launcher.exe [--verbose] [--wait] <game executable> [arguments]

As a Steam launch option:
\"<path to mhw-dlss-launcher.exe>\" %command%

  --verbose  (or --debug, -v) print to a console as well as to the log
  --wait     stay until the game exits";

/// The launcher's own options.
#[derive(Debug, Default, PartialEq, Eq, Clone, Copy)]
pub struct Options {
    /// Attach to the parent console, or open one, and print there as well
    /// as to the log.
    pub verbose: bool,
    /// Stay until the game exits, still without a window. Nothing here
    /// needs it (Steam follows processes started from the one it launched);
    /// it is for scripts that wait for the game.
    pub wait: bool,
}

/// The game to start.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Game {
    /// Its executable, as given.
    pub executable: PathBuf,
    /// The full command line to start it with ([`command_line`]).
    pub command_line: String,
}

/// What the launcher was asked to do.
#[derive(Debug, PartialEq, Eq, Clone)]
pub struct Invocation {
    /// The launcher's options.
    pub options: Options,
    /// `None` when no game was given.
    pub game: Option<Game>,
}

/// Parse the arguments after the launcher's own path.
pub fn parse<I>(arguments: I) -> Invocation
where
    I: IntoIterator<Item = String>,
{
    let mut options = Options::default();
    let mut arguments = arguments.into_iter().peekable();
    while let Some(argument) = arguments.peek() {
        match argument.as_str() {
            "--verbose" | "--debug" | "-v" => options.verbose = true,
            "--wait" => options.wait = true,
            _ => break,
        }
        arguments.next();
    }
    let game = arguments.next().map(|executable| {
        let rest: Vec<String> = arguments.collect();
        Game {
            command_line: command_line(&executable, &rest),
            executable: PathBuf::from(executable),
        }
    });
    Invocation { options, game }
}

/// The command line for the game: its path, then its arguments, each quoted
/// so that the game reads back exactly the arguments the launcher was given.
///
/// They reach the launcher already split and unquoted; appended as they
/// are, one holding a space would arrive as two.
pub fn command_line(executable: &str, arguments: &[String]) -> String {
    // The program name is not parsed like the rest: it ends at the closing
    // quote and backslashes in it mean nothing, so it is only wrapped.
    let mut line = format!("\"{executable}\"");
    for argument in arguments {
        line.push(' ');
        line.push_str(&quote(argument));
    }
    line
}

/// One argument, as `CommandLineToArgvW` and the C runtime read it back.
///
/// Their rule: backslashes are literal unless they run up to a double
/// quote, where 2n of them and a quote mean n backslashes and a delimiter,
/// and 2n+1 mean n backslashes and a literal quote. So a quote inside the
/// argument is written as a backslash and a quote with every backslash
/// before it doubled, and the backslashes at the very end are doubled
/// because the closing quote follows them.
pub fn quote(argument: &str) -> String {
    let plain = !argument.is_empty()
        && !argument
            .chars()
            .any(|c| matches!(c, ' ' | '\t' | '\n' | '\u{b}' | '"'));
    if plain {
        return argument.to_owned();
    }
    let mut out = String::with_capacity(argument.len() + 2);
    out.push('"');
    let mut backslashes = 0usize;
    for character in argument.chars() {
        match character {
            '\\' => backslashes += 1,
            '"' => {
                out.extend(std::iter::repeat_n('\\', backslashes * 2 + 1));
                out.push('"');
                backslashes = 0;
            }
            other => {
                out.extend(std::iter::repeat_n('\\', backslashes));
                out.push(other);
                backslashes = 0;
            }
        }
    }
    out.extend(std::iter::repeat_n('\\', backslashes * 2));
    out.push('"');
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The documented `CommandLineToArgvW` rules (program name: up to the
    /// closing quote; arguments: the 2n/2n+1 backslash rule), so that the
    /// round trip is checked on any host.
    fn split_like_windows(line: &str) -> Vec<String> {
        let mut chars = line.chars().peekable();
        let mut out = Vec::new();
        let mut name = String::new();
        if chars.peek() == Some(&'"') {
            chars.next();
            for c in chars.by_ref() {
                if c == '"' {
                    break;
                }
                name.push(c);
            }
        } else {
            while let Some(&c) = chars.peek() {
                if c == ' ' || c == '\t' {
                    break;
                }
                name.push(c);
                chars.next();
            }
        }
        out.push(name);
        loop {
            while matches!(chars.peek(), Some(' ' | '\t')) {
                chars.next();
            }
            if chars.peek().is_none() {
                return out;
            }
            let mut argument = String::new();
            let mut quoted = false;
            while let Some(&c) = chars.peek() {
                match c {
                    '\\' => {
                        let mut count = 0;
                        while chars.peek() == Some(&'\\') {
                            chars.next();
                            count += 1;
                        }
                        if chars.peek() == Some(&'"') {
                            argument.extend(std::iter::repeat_n('\\', count / 2));
                            if count % 2 == 1 {
                                argument.push('"');
                                chars.next();
                            }
                        } else {
                            argument.extend(std::iter::repeat_n('\\', count));
                        }
                    }
                    '"' => {
                        chars.next();
                        if quoted && chars.peek() == Some(&'"') {
                            argument.push('"');
                            chars.next();
                        } else {
                            quoted = !quoted;
                        }
                    }
                    ' ' | '\t' if !quoted => break,
                    other => {
                        argument.push(other);
                        chars.next();
                    }
                }
            }
            out.push(argument);
        }
    }

    fn strings(items: &[&str]) -> Vec<String> {
        items.iter().map(|item| (*item).to_owned()).collect()
    }

    #[test]
    fn every_argument_survives_the_round_trip() {
        let executable =
            "C:\\Program Files (x86)\\Steam\\common\\Monster Hunter World\\MonsterHunterWorld.exe";
        let arguments = strings(&[
            "-windowed",
            "two words",
            "",
            "a \"quoted\" word",
            "ends with a backslash\\",
            "C:\\path with space\\",
            "backslashes before a quote \\\\\" here",
            "plain\\path\\no\\space",
            "tab\there",
            "\"",
            "\\\\",
        ]);
        let back = split_like_windows(&command_line(executable, &arguments));
        assert_eq!(back[0], executable);
        assert_eq!(&back[1..], &arguments[..]);
    }

    #[test]
    fn plain_arguments_are_not_quoted() {
        assert_eq!(quote("-dx12"), "-dx12");
        assert_eq!(quote("C:\\a\\b"), "C:\\a\\b");
        assert_eq!(quote(""), "\"\"");
    }

    #[test]
    fn options_come_before_the_game_and_only_there() {
        let invocation = parse(strings(&[
            "--verbose",
            "--wait",
            "C:\\g.exe",
            "--wait",
            "x y",
        ]));
        assert_eq!(
            invocation.options,
            Options {
                verbose: true,
                wait: true
            }
        );
        let game = invocation.game.expect("a game");
        assert_eq!(game.executable, PathBuf::from("C:\\g.exe"));
        assert_eq!(game.command_line, "\"C:\\g.exe\" --wait \"x y\"");

        let invocation = parse(strings(&["-v", "g.exe"]));
        assert!(invocation.options.verbose && !invocation.options.wait);
        let invocation = parse(strings(&["--debug", "g.exe"]));
        assert!(invocation.options.verbose);
    }

    #[test]
    fn no_game_is_reported_as_none() {
        assert_eq!(parse(Vec::new()).game, None);
        let invocation = parse(strings(&["--wait"]));
        assert!(invocation.options.wait);
        assert_eq!(invocation.game, None);
    }

    #[test]
    fn an_unknown_option_is_the_game() {
        let game = parse(strings(&["--frobnicate", "g.exe"]))
            .game
            .expect("a game");
        assert_eq!(game.executable, PathBuf::from("--frobnicate"));
    }
}
