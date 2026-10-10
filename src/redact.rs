//! Keeping the Windows user name out of the logs.
//!
//! People paste these logs into public bug reports, and nearly every path in
//! them that is not under the game folder is under the user's profile:
//! `C:\Users\<name>\AppData\...`. The prefix is replaced with
//! `%USERPROFILE%`, which says as much to whoever reads the report and names
//! nobody.
//!
//! Shared by the mod and the launcher, which is a separate program; the
//! launcher takes this file with `#[path]`.

/// Replace every occurrence of the profile directory in `line`.
///
/// Without regard to case, and with each backslash of the profile path
/// allowed to appear as a backslash, as a forward slash, or doubled the way a
/// debug-formatted or JSON string writes it, because all three occur in what
/// Streamline and NGX log. A match must end at the end of a path component,
/// so the profile of `ann` is not found inside `C:\Users\anna`.
///
/// What this does not do: it knows nothing of a user name that appears
/// anywhere other than in the profile path, nor of an 8.3 short path.
pub fn redact(line: &mut String, profile: &str) {
    let profile = profile.trim_end_matches(['\\', '/']);
    // Not worth doing for a profile like `C:` or an unset variable, and a
    // very short needle would match things that are not paths.
    if profile.len() < 4 || line.len() < profile.len() {
        return;
    }
    let needle: Vec<char> = profile.chars().collect();
    let first = needle[0].to_ascii_lowercase();
    let mut out: Option<String> = None;
    let mut copied = 0usize;
    let mut position = 0usize;
    while position < line.len() {
        let rest = &line[position..];
        let Some(character) = rest.chars().next() else {
            break;
        };
        if character.to_ascii_lowercase() == first {
            if let Some(length) = match_at(rest, &needle) {
                let text = out.get_or_insert_with(|| String::with_capacity(line.len()));
                text.push_str(&line[copied..position]);
                text.push_str("%USERPROFILE%");
                position += length;
                copied = position;
                continue;
            }
        }
        position += character.len_utf8();
    }
    if let Some(mut text) = out {
        text.push_str(&line[copied..]);
        *line = text;
    }
}

/// How many bytes of `text` the profile path takes up at its start, if it is
/// there.
fn match_at(text: &str, needle: &[char]) -> Option<usize> {
    let mut consumed = 0usize;
    for wanted in needle {
        let rest = &text[consumed..];
        let found = rest.chars().next()?;
        if *wanted == '\\' || *wanted == '/' {
            if rest.starts_with("\\\\") {
                consumed += 2;
            } else if found == '\\' || found == '/' {
                consumed += 1;
            } else {
                return None;
            }
        } else if found.to_lowercase().eq(wanted.to_lowercase()) {
            consumed += found.len_utf8();
        } else {
            return None;
        }
    }
    // The end of a component: not in the middle of a longer name.
    match text[consumed..].chars().next() {
        Some(next) if next.is_alphanumeric() || matches!(next, '_' | '-' | '.' | '~') => None,
        _ => Some(consumed),
    }
}

#[cfg(test)]
mod tests {
    use super::redact;

    fn redacted(line: &str, profile: &str) -> String {
        let mut text = line.to_string();
        redact(&mut text, profile);
        text
    }

    const PROFILE: &str = r"C:\Users\Jane Doe";

    #[test]
    fn the_profile_prefix_is_replaced_and_the_rest_kept() {
        assert_eq!(
            redacted(
                r"[load] LoadLibraryW C:\Users\Jane Doe\AppData\Local\x\y.dll",
                PROFILE
            ),
            r"[load] LoadLibraryW %USERPROFILE%\AppData\Local\x\y.dll"
        );
        assert_eq!(
            redacted(r"log  : C:\Users\Jane Doe", PROFILE),
            "log  : %USERPROFILE%"
        );
    }

    #[test]
    fn case_and_slash_direction_do_not_matter() {
        assert_eq!(
            redacted("c:/users/JANE DOE/AppData/x", PROFILE),
            "%USERPROFILE%/AppData/x"
        );
        // As a debug-formatted string writes it.
        assert_eq!(
            redacted(r#"Some("C:\\Users\\Jane Doe\\AppData\\x")"#, PROFILE),
            r#"Some("%USERPROFILE%\\AppData\\x")"#
        );
    }

    #[test]
    fn every_occurrence_in_a_line_is_replaced() {
        assert_eq!(
            redacted(r"from C:\Users\Jane Doe\a to C:\Users\Jane Doe\b", PROFILE),
            r"from %USERPROFILE%\a to %USERPROFILE%\b"
        );
    }

    #[test]
    fn a_longer_name_that_starts_the_same_is_left_alone() {
        assert_eq!(
            redacted(r"C:\Users\Jane Doering\x", PROFILE),
            r"C:\Users\Jane Doering\x"
        );
    }

    #[test]
    fn lines_without_the_profile_are_untouched() {
        let line = r"[gpu] adapter 0: NVIDIA GeForce RTX, C:\Program Files (x86)\Steam";
        assert_eq!(redacted(line, PROFILE), line);
        assert_eq!(redacted(line, ""), line);
        assert_eq!(redacted(line, "C:"), line);
    }

    #[test]
    fn a_trailing_separator_on_the_profile_is_ignored() {
        assert_eq!(
            redacted(r"C:\Users\Jane Doe\x", r"C:\Users\Jane Doe\"),
            r"%USERPROFILE%\x"
        );
    }

    #[test]
    fn text_that_is_not_ascii_survives() {
        assert_eq!(
            redacted(
                "path C:\\Users\\Zoë Müller\\file – end",
                "C:\\Users\\Zoë Müller"
            ),
            "path %USERPROFILE%\\file – end"
        );
    }
}
