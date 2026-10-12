//! Keeping the Windows user name out of the logs.
//!
//! People paste these logs into public bug reports, and nearly every path in
//! them that is not under the game folder is under the user's profile:
//! `C:\Users\<name>\AppData\...`. The prefix is replaced with `%USERPROFILE%`,
//! which says as much to whoever reads the report and names nobody.

/// What the profile path is replaced with.
pub const PLACEHOLDER: &str = "%USERPROFILE%";

/// Replace every occurrence of the profile directory in `line`.
///
/// Case-insensitive, and each backslash of the profile path may appear as a
/// backslash, a forward slash, or doubled the way a debug-formatted or JSON
/// string writes it, because all three occur in what Streamline and NGX log.
/// A match must end at the end of a path component, so the profile of `ann`
/// is not found inside `C:\Users\anna`.
///
/// Profiles shorter than four characters (`C:`, an unset variable) are not
/// redacted: such a needle would match things that are not paths. A user
/// name appearing outside the profile path, or an 8.3 short path, is not
/// recognised.
pub fn redact(line: &mut String, profile: &str) {
    let profile = profile.trim_end_matches(['\\', '/']);
    if profile.len() < 4 || line.len() < profile.len() {
        return;
    }
    let needle: Vec<char> = profile.chars().collect();
    let Some(first) = needle.first().map(char::to_ascii_lowercase) else {
        return;
    };
    let mut out: Option<String> = None;
    let mut copied = 0usize;
    let mut position = 0usize;
    while let Some(character) = line[position..].chars().next() {
        if character.to_ascii_lowercase() == first {
            if let Some(length) = match_at(&line[position..], &needle) {
                let text = out.get_or_insert_with(|| String::with_capacity(line.len()));
                text.push_str(&line[copied..position]);
                text.push_str(PLACEHOLDER);
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
                r"LoadLibraryW C:\Users\Jane Doe\AppData\Local\x\y.dll",
                PROFILE
            ),
            r"LoadLibraryW %USERPROFILE%\AppData\Local\x\y.dll"
        );
        assert_eq!(
            redacted(r"log: C:\Users\Jane Doe", PROFILE),
            "log: %USERPROFILE%"
        );
    }

    #[test]
    fn case_and_slash_direction_do_not_matter() {
        assert_eq!(
            redacted("c:/users/JANE DOE/AppData/x", PROFILE),
            "%USERPROFILE%/AppData/x"
        );
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
    fn lines_without_the_profile_or_with_a_useless_profile_are_untouched() {
        let line = r"adapter 0: NVIDIA GeForce RTX, C:\Program Files (x86)\Steam";
        assert_eq!(redacted(line, PROFILE), line);
        assert_eq!(redacted(line, ""), line);
        assert_eq!(redacted(line, "C:"), line);
        assert_eq!(redacted("C:", PROFILE), "C:");
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
