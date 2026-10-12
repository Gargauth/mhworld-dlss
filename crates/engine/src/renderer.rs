//! Which renderer the game is going to start: the same decision it makes
//! from `graphics_option.ini` and `rendering_api_fallback.txt`
//! (`docs/re/ReLoadInitDx11.md` §2.5).
//!
//! Pure; the files are read by the caller.
//!
//! The game asks `GetPrivateProfileStringW("GraphicsOption",
//! "DirectX12Enable", "", buf, 0x80, "graphics_option.ini")` and compares the
//! answer with `_stricmp(value, "On")`. So:
//!
//! * no file, no `[GraphicsOption]` section, no key, or any value but `On`
//!   (any case) is DirectX 11;
//! * a UTF-8 byte-order mark is read as part of the first line, so a file
//!   that starts with one has no `[GraphicsOption]` header there; the
//!   profile API reads UTF-16 only with its own byte-order mark;
//! * a `rendering_api_fallback.txt` whose first 16 bytes `atoi` to 1 (the
//!   game writes it after a GPU crash in DirectX 12) forces DirectX 11 for
//!   the next start, whatever the ini says.

use std::fmt;

/// The section and key the game reads.
pub const SECTION: &str = "GraphicsOption";
/// See [`SECTION`].
pub const KEY: &str = "DirectX12Enable";
/// The profile API's buffer: 0x80 UTF-16 units including the terminator.
const VALUE_MAX: usize = 0x7f;

/// The renderer the game will pick.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Renderer {
    /// DirectX 12: the only renderer the mod supports.
    Dx12,
    /// DirectX 11, and why.
    Dx11(Dx11Reason),
}

/// Why the game starts on DirectX 11.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Dx11Reason {
    /// `graphics_option.ini` does not exist (or cannot be read).
    NoFile,
    /// The file starts with a UTF-8 byte-order mark and the only
    /// `[GraphicsOption]` header is on its first line.
    Utf8Bom,
    /// No `[GraphicsOption]` section.
    NoSection,
    /// No `DirectX12Enable` key in it.
    NoKey,
    /// The key's value is not `On`.
    Value(String),
    /// `rendering_api_fallback.txt` says 1: the game crashed in DirectX 12
    /// last time and forces DirectX 11 once.
    FallbackMarker,
}

impl fmt::Display for Dx11Reason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Dx11Reason::NoFile => f.write_str("graphics_option.ini is missing"),
            Dx11Reason::Utf8Bom => f.write_str(
                "graphics_option.ini starts with a UTF-8 byte-order mark, which hides its \
                 [GraphicsOption] header from the game",
            ),
            Dx11Reason::NoSection => f.write_str("graphics_option.ini has no [GraphicsOption]"),
            Dx11Reason::NoKey => f.write_str("graphics_option.ini has no DirectX12Enable"),
            Dx11Reason::Value(value) => {
                write!(f, "graphics_option.ini has DirectX12Enable={value}")
            }
            Dx11Reason::FallbackMarker => f.write_str(
                "rendering_api_fallback.txt is set: the game falls back to DirectX 11 after a \
                 DirectX 12 crash",
            ),
        }
    }
}

/// The renderer for these file contents (`None` = file absent).
pub fn renderer(graphics_option: Option<&[u8]>, fallback_marker: Option<&[u8]>) -> Renderer {
    if fallback_marker.is_some_and(marker_set) {
        return Renderer::Dx11(Dx11Reason::FallbackMarker);
    }
    let Some(bytes) = graphics_option else {
        return Renderer::Dx11(Dx11Reason::NoFile);
    };
    match profile_value(&decode(bytes), SECTION, KEY) {
        Lookup::Found(value) if value.eq_ignore_ascii_case("On") => Renderer::Dx12,
        Lookup::Found(value) => Renderer::Dx11(Dx11Reason::Value(value)),
        Lookup::NoKey => Renderer::Dx11(Dx11Reason::NoKey),
        Lookup::NoSection if bytes.starts_with(UTF8_BOM) => Renderer::Dx11(Dx11Reason::Utf8Bom),
        Lookup::NoSection => Renderer::Dx11(Dx11Reason::NoSection),
    }
}

const UTF8_BOM: &[u8] = &[0xef, 0xbb, 0xbf];

/// Whether `rendering_api_fallback.txt` says 1: the game reads 16 bytes and
/// `atoi`s them.
pub fn marker_set(bytes: &[u8]) -> bool {
    atoi(bytes.get(..16).unwrap_or(bytes)) == 1
}

/// C `atoi`: leading whitespace, optional sign, decimal digits; 0 for none.
fn atoi(bytes: &[u8]) -> i64 {
    let mut rest = bytes
        .iter()
        .copied()
        .skip_while(|b| matches!(b, b' ' | b'\t' | b'\n' | b'\r' | 0x0b | 0x0c))
        .peekable();
    let negative = match rest.peek() {
        Some(b'-') => {
            rest.next();
            true
        }
        Some(b'+') => {
            rest.next();
            false
        }
        _ => false,
    };
    let mut value: i64 = 0;
    for digit in rest.take_while(u8::is_ascii_digit) {
        value = value
            .saturating_mul(10)
            .saturating_add(i64::from(digit - b'0'));
    }
    if negative {
        -value
    } else {
        value
    }
}

/// The text as the profile API sees it: UTF-16 LE with its byte-order mark,
/// otherwise single bytes (the ANSI code page; only ASCII matters here, and
/// a UTF-8 byte-order mark stays in the text).
fn decode(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xff, 0xfe]) {
        let units: Vec<u16> = rest
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| u16::from_le_bytes(*pair))
            .collect();
        return String::from_utf16_lossy(&units);
    }
    bytes.iter().map(|&b| char::from(b)).collect()
}

enum Lookup {
    Found(String),
    NoSection,
    NoKey,
}

/// `GetPrivateProfileString` semantics: the first section of that name
/// (case-insensitive, the line trimmed), the first key in it
/// (case-insensitive, trimmed), the value trimmed and one pair of enclosing
/// quotes removed, truncated to the buffer.
fn profile_value(text: &str, section: &str, key: &str) -> Lookup {
    let mut in_section = false;
    let mut seen_section = false;
    for line in text.split(['\n', '\r']) {
        let line = line.trim_matches([' ', '\t']);
        if let Some(name) = line.strip_prefix('[') {
            if seen_section && in_section {
                // Only the first section of that name is read.
                return Lookup::NoKey;
            }
            let name = name
                .split(']')
                .next()
                .unwrap_or("")
                .trim_matches([' ', '\t']);
            in_section = name.eq_ignore_ascii_case(section);
            seen_section |= in_section;
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((name, value)) = line.split_once('=') else {
            continue;
        };
        if !name.trim_matches([' ', '\t']).eq_ignore_ascii_case(key) {
            continue;
        }
        let mut value = value.trim_matches([' ', '\t']);
        for quote in ['"', '\''] {
            if value.len() >= 2 && value.starts_with(quote) && value.ends_with(quote) {
                value = &value[1..value.len() - 1];
                break;
            }
        }
        return Lookup::Found(value.chars().take(VALUE_MAX).collect());
    }
    if seen_section {
        Lookup::NoKey
    } else {
        Lookup::NoSection
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ON: &str = "[GraphicsOption]\r\nScreenMode=Borderless\r\nDirectX12Enable=On\r\n";

    #[test]
    fn on_in_any_case_is_dx12() {
        assert_eq!(renderer(Some(ON.as_bytes()), None), Renderer::Dx12);
        let lower = ON.replace("=On", "= on ");
        assert_eq!(renderer(Some(lower.as_bytes()), None), Renderer::Dx12);
        let quoted = ON.replace("=On", "=\"ON\"");
        assert_eq!(renderer(Some(quoted.as_bytes()), None), Renderer::Dx12);
        let key_case = ON.replace("DirectX12Enable", " directx12enable ");
        assert_eq!(renderer(Some(key_case.as_bytes()), None), Renderer::Dx12);
    }

    #[test]
    fn anything_but_on_is_dx11_with_the_value() {
        let off = ON.replace("=On", "=Off");
        assert_eq!(
            renderer(Some(off.as_bytes()), None),
            Renderer::Dx11(Dx11Reason::Value("Off".into()))
        );
        // `_stricmp` is equality, not a prefix.
        let longer = ON.replace("=On", "=Only");
        assert!(matches!(
            renderer(Some(longer.as_bytes()), None),
            Renderer::Dx11(Dx11Reason::Value(_))
        ));
    }

    #[test]
    fn a_missing_file_section_or_key_is_dx11() {
        assert_eq!(renderer(None, None), Renderer::Dx11(Dx11Reason::NoFile));
        assert_eq!(
            renderer(Some(b"[Other]\nDirectX12Enable=On\n"), None),
            Renderer::Dx11(Dx11Reason::NoSection)
        );
        assert_eq!(
            renderer(
                Some(b"[GraphicsOption]\nScreenMode=x\n[Other]\nDirectX12Enable=On\n"),
                None
            ),
            Renderer::Dx11(Dx11Reason::NoKey)
        );
        assert_eq!(
            renderer(Some(b""), None),
            Renderer::Dx11(Dx11Reason::NoSection)
        );
    }

    #[test]
    fn a_utf8_bom_hides_the_first_header() {
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(ON.as_bytes());
        assert_eq!(
            renderer(Some(&bytes), None),
            Renderer::Dx11(Dx11Reason::Utf8Bom)
        );
        // With the BOM on a line of its own the header is intact.
        let mut bytes = UTF8_BOM.to_vec();
        bytes.extend_from_slice(b"\r\n");
        bytes.extend_from_slice(ON.as_bytes());
        assert_eq!(renderer(Some(&bytes), None), Renderer::Dx12);
    }

    #[test]
    fn utf16_with_its_bom_is_read() {
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(ON.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(renderer(Some(&bytes), None), Renderer::Dx12);
    }

    #[test]
    fn only_the_first_section_and_key_count() {
        let twice = "[GraphicsOption]\nDirectX12Enable=Off\nDirectX12Enable=On\n";
        assert_eq!(
            renderer(Some(twice.as_bytes()), None),
            Renderer::Dx11(Dx11Reason::Value("Off".into()))
        );
        let split = "[GraphicsOption]\nA=1\n[GraphicsOption]\nDirectX12Enable=On\n";
        assert_eq!(
            renderer(Some(split.as_bytes()), None),
            Renderer::Dx11(Dx11Reason::NoKey)
        );
    }

    #[test]
    fn the_fallback_marker_overrides_the_ini() {
        assert_eq!(
            renderer(Some(ON.as_bytes()), Some(b"1")),
            Renderer::Dx11(Dx11Reason::FallbackMarker)
        );
        assert_eq!(
            renderer(Some(ON.as_bytes()), Some(b" 1\r\n")),
            Renderer::Dx11(Dx11Reason::FallbackMarker)
        );
        assert_eq!(renderer(Some(ON.as_bytes()), Some(b"0")), Renderer::Dx12);
        assert_eq!(renderer(Some(ON.as_bytes()), Some(b"")), Renderer::Dx12);
        assert_eq!(renderer(Some(ON.as_bytes()), Some(b"10")), Renderer::Dx12);
        // Only 16 bytes are read.
        assert!(!marker_set(b"                1"));
        assert!(marker_set(b"1junk"));
    }
}
