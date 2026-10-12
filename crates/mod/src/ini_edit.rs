//! Changing one value in the settings file's text. Pure.
//!
//! The overlay writes the file and the hot reload applies it: the file stays
//! the one configuration. Only the value of one line changes; comments,
//! order, spacing, an inline comment after the value, the byte-order mark
//! and the line endings are kept, and [`set_key_in_file`] keeps the file's
//! encoding (UTF-8, UTF-16 LE/BE) or refuses bytes that do not decode.
//! Sections and keys match case-insensitively as the parser matches them
//! (`mhw_core::config`).

/// `text` with `[section] key = value`: the key's line in that section
/// rewritten, else a line added at the end of the section, else the
/// section appended.
pub fn set_key(text: &str, section: &str, key: &str, value: &str) -> String {
    let newline = if text.contains("\r\n") { "\r\n" } else { "\n" };
    let mut lines: Vec<String> = text.split_inclusive('\n').map(str::to_owned).collect();
    let mut in_section = false;
    let mut section_end: Option<usize> = None;
    for index in 0..lines.len() {
        let line = lines[index].trim_start_matches('\u{feff}');
        let bare = line.trim();
        if let Some(name) = bare
            .strip_prefix('[')
            .and_then(|rest| rest.split(']').next())
        {
            in_section = name.trim().eq_ignore_ascii_case(section);
            continue;
        }
        if !in_section {
            continue;
        }
        if !bare.is_empty() {
            section_end = Some(index);
        }
        let Some((name, rest)) = line.split_once('=') else {
            continue;
        };
        if bare.starts_with(';') || bare.starts_with('#') || !name.trim().eq_ignore_ascii_case(key)
        {
            continue;
        }
        let prefix_len = lines[index].len() - line.len();
        let bom = &lines[index][..prefix_len];
        let ending = if rest.ends_with("\r\n") {
            "\r\n"
        } else if rest.ends_with('\n') {
            "\n"
        } else {
            ""
        };
        let rest = &rest[..rest.len() - ending.len()];
        let spacing: String = rest.chars().take_while(|c| c.is_whitespace()).collect();
        let comment = inline_comment(rest).unwrap_or("");
        lines[index] = format!("{bom}{name}={spacing}{value}{comment}{ending}");
        return lines.concat();
    }
    let added = format!("{key} = {value}{newline}");
    match section_end {
        Some(after) => {
            if !lines[after].ends_with('\n') {
                lines[after].push_str(newline);
            }
            lines.insert(after + 1, added);
        }
        None => match section_header(&lines, section) {
            Some(header) => {
                if !lines[header].ends_with('\n') {
                    lines[header].push_str(newline);
                }
                lines.insert(header + 1, added);
            }
            None => {
                if lines.last().is_some_and(|last| !last.ends_with('\n')) {
                    lines.push(newline.to_owned());
                }
                if !lines.is_empty() {
                    lines.push(newline.to_owned());
                }
                lines.push(format!("[{section}]{newline}"));
                lines.push(added);
            }
        },
    }
    lines.concat()
}

/// Why [`set_key_in_file`] did not edit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EditRefused {
    /// The bytes are not valid in their encoding (UTF-8, or UTF-16 after
    /// its byte-order mark): rewriting them would lose the user's text.
    NotDecodable,
}

/// How the file is encoded, as the parser recognises it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Encoding {
    /// UTF-8, with or without a byte-order mark (kept in the text).
    Utf8,
    Utf16Le,
    Utf16Be,
}

/// `bytes` of the settings file with `[section] key = value`, in the
/// file's own encoding (UTF-8 with or without a BOM, UTF-16 LE/BE with
/// theirs, as `mhw_core::config::parse_bytes` reads them). `Ok(None)` when
/// the value is already that (nothing to write); refused when the bytes do
/// not decode cleanly.
pub fn set_key_in_file(
    bytes: &[u8],
    section: &str,
    key: &str,
    value: &str,
) -> Result<Option<Vec<u8>>, EditRefused> {
    let (encoding, text) = if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        (Encoding::Utf16Le, utf16(rest, u16::from_le_bytes)?)
    } else if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        (Encoding::Utf16Be, utf16(rest, u16::from_be_bytes)?)
    } else {
        let text = std::str::from_utf8(bytes).map_err(|_| EditRefused::NotDecodable)?;
        (Encoding::Utf8, text.to_owned())
    };
    let edited = set_key(&text, section, key, value);
    if edited == text {
        return Ok(None);
    }
    let encoded = match encoding {
        Encoding::Utf8 => edited.into_bytes(),
        Encoding::Utf16Le => [0xFF, 0xFE]
            .into_iter()
            .chain(edited.encode_utf16().flat_map(u16::to_le_bytes))
            .collect(),
        Encoding::Utf16Be => [0xFE, 0xFF]
            .into_iter()
            .chain(edited.encode_utf16().flat_map(u16::to_be_bytes))
            .collect(),
    };
    Ok(Some(encoded))
}

/// UTF-16 code units from byte pairs, strictly.
fn utf16(bytes: &[u8], from: fn([u8; 2]) -> u16) -> Result<String, EditRefused> {
    let (units, odd) = bytes.as_chunks::<2>();
    if !odd.is_empty() {
        return Err(EditRefused::NotDecodable);
    }
    char::decode_utf16(units.iter().map(|pair| from(*pair)))
        .collect::<Result<String, _>>()
        .map_err(|_| EditRefused::NotDecodable)
}

/// The line index of `[section]`.
fn section_header(lines: &[String], section: &str) -> Option<usize> {
    lines.iter().position(|line| {
        line.trim_start_matches('\u{feff}')
            .trim()
            .strip_prefix('[')
            .and_then(|rest| rest.split(']').next())
            .is_some_and(|name| name.trim().eq_ignore_ascii_case(section))
    })
}

/// The inline comment of a value (from the whitespace before `;`/`#`
/// outside quotes), as the parser recognises it.
fn inline_comment(value: &str) -> Option<&str> {
    let mut quoted = false;
    let mut previous_blank = false;
    for (at, c) in value.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' | '#' if !quoted && previous_blank => {
                let start = value[..at].trim_end().len();
                return Some(&value[start..]);
            }
            _ => {}
        }
        previous_blank = c.is_whitespace();
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    const FILE: &str = "\u{feff}; mhw-dlss\r\n[General]\r\nEnabled = true\r\n\r\n[Upscaling]\r\nMode = Quality   ; DLAA | Quality\r\nSharpness=0.0\r\n\r\n[FrameGeneration]\r\nEnabled = false\r\n";

    #[test]
    fn a_value_is_replaced_in_its_own_section_only() {
        let out = set_key(FILE, "FrameGeneration", "Enabled", "true");
        assert!(out.contains("[General]\r\nEnabled = true\r\n"));
        assert!(out.contains("[FrameGeneration]\r\nEnabled = true\r\n"));
        assert_eq!(out.len(), FILE.len() - 1);
    }

    #[test]
    fn spacing_comments_bom_and_endings_are_kept() {
        let out = set_key(FILE, "upscaling", "MODE", "Balanced");
        assert!(out.starts_with('\u{feff}'));
        assert!(out.contains("Mode = Balanced   ; DLAA | Quality\r\n"));
        let out = set_key(FILE, "Upscaling", "Sharpness", "0.25");
        assert!(out.contains("Sharpness=0.25\r\n"));
        assert_eq!(out.matches("\r\n").count(), FILE.matches("\r\n").count());
    }

    #[test]
    fn a_missing_key_goes_to_the_end_of_its_section() {
        let out = set_key(FILE, "Upscaling", "Preset", "K");
        assert!(out.contains("Sharpness=0.0\r\nPreset = K\r\n\r\n[FrameGeneration]"));
        let out = set_key("[Upscaling]\n", "Upscaling", "Preset", "K");
        assert_eq!(out, "[Upscaling]\nPreset = K\n");
    }

    #[test]
    fn a_missing_section_is_appended() {
        let out = set_key("[General]\nEnabled = true", "Upscaling", "Mode", "DLAA");
        assert_eq!(
            out,
            "[General]\nEnabled = true\n\n[Upscaling]\nMode = DLAA\n"
        );
        assert_eq!(
            set_key("", "Upscaling", "Mode", "DLAA"),
            "[Upscaling]\nMode = DLAA\n"
        );
    }

    #[test]
    fn the_result_parses_to_the_value() {
        use mhw_core::config::{parse, Build, UpscalingMode};
        let text = mhw_core::config::Settings::write_default(Build::Release);
        let out = set_key(&text, "Upscaling", "Mode", "Performance");
        let (settings, diagnostics) = parse(&out, Build::Release);
        assert!(diagnostics.is_empty(), "{diagnostics:?}");
        assert_eq!(settings.upscaling.mode, UpscalingMode::Performance);
    }

    #[test]
    fn commented_out_keys_are_not_values() {
        let out = set_key(
            "[Upscaling]\n; Mode = DLAA\n",
            "Upscaling",
            "Mode",
            "Quality",
        );
        assert_eq!(out, "[Upscaling]\n; Mode = DLAA\nMode = Quality\n");
    }

    #[test]
    fn a_utf16_file_is_edited_and_stays_utf16() {
        let text = "[General]\r\nEnabled = true\r\n[Upscaling]\r\nMode = Quality\r\n";
        for (bom, encode) in [
            ([0xFF, 0xFE], u16::to_le_bytes as fn(u16) -> [u8; 2]),
            ([0xFE, 0xFF], u16::to_be_bytes as fn(u16) -> [u8; 2]),
        ] {
            let bytes: Vec<u8> = bom
                .into_iter()
                .chain(text.encode_utf16().flat_map(encode))
                .collect();
            let out = set_key_in_file(&bytes, "Upscaling", "Mode", "DLAA")
                .expect("decodable")
                .expect("changed");
            assert_eq!(&out[..2], &bom);
            let (settings, diagnostics) =
                mhw_core::config::parse_bytes(&out, mhw_core::config::Build::Release);
            assert!(diagnostics.is_empty(), "{diagnostics:?}");
            assert_eq!(
                settings.upscaling.mode,
                mhw_core::config::UpscalingMode::Dlaa
            );
            assert!(settings.general.enabled, "the other values are kept");
        }
    }

    #[test]
    fn utf8_keeps_its_bom_and_an_unchanged_value_writes_nothing() {
        let bytes = FILE.as_bytes();
        let out = set_key_in_file(bytes, "Upscaling", "Sharpness", "0.5")
            .expect("decodable")
            .expect("changed");
        assert!(out.starts_with(&[0xEF, 0xBB, 0xBF]));
        assert_eq!(
            set_key_in_file(bytes, "Upscaling", "Sharpness", "0.0"),
            Ok(None)
        );
    }

    #[test]
    fn undecodable_bytes_are_not_rewritten() {
        let bad_utf8 = b"[Upscaling]\nMode = Qu\xffality\n";
        assert_eq!(
            set_key_in_file(bad_utf8, "Upscaling", "Mode", "DLAA"),
            Err(EditRefused::NotDecodable)
        );
        let odd_utf16 = [0xFF, 0xFE, b'[', 0, b'U'];
        assert_eq!(
            set_key_in_file(&odd_utf16, "Upscaling", "Mode", "DLAA"),
            Err(EditRefused::NotDecodable)
        );
        let lone_surrogate = [0xFF, 0xFE, 0x00, 0xD8, b'a', 0];
        assert_eq!(
            set_key_in_file(&lone_surrogate, "Upscaling", "Mode", "DLAA"),
            Err(EditRefused::NotDecodable)
        );
    }
}
