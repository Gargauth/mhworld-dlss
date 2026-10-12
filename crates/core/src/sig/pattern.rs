//! Byte patterns with wildcards, scanning, and reading x64 displacements.

use std::fmt;

/// A byte pattern: fixed bytes and wildcards, from IDA-style text
/// (`"48 8B 05 ?? ?? ?? ?? C7 80"`).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Pattern {
    bytes: Vec<Option<u8>>,
    /// Index of the fixed byte the scan searches for first.
    anchor: usize,
}

/// Why a pattern text is not a pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PatternError {
    /// No tokens at all.
    Empty,
    /// Only wildcards: it would match everywhere.
    OnlyWildcards,
    /// A token that is neither two hex digits nor `?`/`??`.
    BadToken {
        /// 0-based token position.
        index: usize,
        /// The token as written.
        token: String,
    },
}

impl fmt::Display for PatternError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PatternError::Empty => f.write_str("empty pattern"),
            PatternError::OnlyWildcards => f.write_str("pattern has no fixed byte"),
            PatternError::BadToken { index, token } => {
                write!(f, "token {index} {token:?} is not a hex byte or ??")
            }
        }
    }
}

impl std::error::Error for PatternError {}

/// Bytes too common in x64 code to be a good first filter.
const COMMON: [u8; 8] = [0x00, 0xFF, 0xCC, 0x48, 0x8B, 0x89, 0x0F, 0x4C];

impl Pattern {
    /// Parse whitespace-separated tokens: two hex digits (any case) for a
    /// fixed byte, `?` or `??` for any byte.
    pub fn parse(text: &str) -> Result<Pattern, PatternError> {
        let mut bytes = Vec::new();
        for (index, token) in text.split_whitespace().enumerate() {
            let byte = match token {
                "?" | "??" => None,
                _ if token.len() == 2 => {
                    Some(
                        u8::from_str_radix(token, 16).map_err(|_| PatternError::BadToken {
                            index,
                            token: token.to_owned(),
                        })?,
                    )
                }
                _ => {
                    return Err(PatternError::BadToken {
                        index,
                        token: token.to_owned(),
                    })
                }
            };
            bytes.push(byte);
        }
        if bytes.is_empty() {
            return Err(PatternError::Empty);
        }
        let fixed = || {
            bytes
                .iter()
                .enumerate()
                .filter_map(|(i, b)| b.map(|b| (i, b)))
        };
        let anchor = fixed()
            .find(|(_, byte)| !COMMON.contains(byte))
            .or_else(|| fixed().next())
            .map(|(index, _)| index)
            .ok_or(PatternError::OnlyWildcards)?;
        Ok(Pattern { bytes, anchor })
    }

    /// Length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Never true for a parsed pattern; for API completeness.
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// The pattern's bytes, `None` for a wildcard.
    pub fn bytes(&self) -> &[Option<u8>] {
        &self.bytes
    }

    /// Whether `bytes` starts with this pattern.
    pub fn matches(&self, bytes: &[u8]) -> bool {
        bytes.len() >= self.bytes.len()
            && self
                .bytes
                .iter()
                .zip(bytes)
                .all(|(want, have)| want.is_none_or(|want| want == *have))
    }
}

impl fmt::Display for Pattern {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        for (index, byte) in self.bytes.iter().enumerate() {
            if index > 0 {
                f.write_str(" ")?;
            }
            match byte {
                Some(byte) => write!(f, "{byte:02X}")?,
                None => f.write_str("??")?,
            }
        }
        Ok(())
    }
}

/// Every offset in `haystack` where `pattern` matches, ascending
/// (overlapping matches included).
pub fn scan(haystack: &[u8], pattern: &Pattern) -> Vec<usize> {
    scan_up_to(haystack, pattern, usize::MAX)
}

/// Like [`scan`], stopping after `limit` matches.
pub fn scan_up_to(haystack: &[u8], pattern: &Pattern, limit: usize) -> Vec<usize> {
    let mut hits = Vec::new();
    let length = pattern.len();
    if length == 0 || haystack.len() < length || limit == 0 {
        return hits;
    }
    let anchor = pattern.anchor;
    let Some(Some(anchor_byte)) = pattern.bytes.get(anchor).copied() else {
        return hits;
    };
    // Candidate starts are 0..=last; the anchor byte sits `anchor` later.
    let last = haystack.len() - length;
    let window = &haystack[anchor..=last + anchor];
    let mut from = 0;
    while let Some(found) = window[from..].iter().position(|&b| b == anchor_byte) {
        let start = from + found;
        if pattern.matches(&haystack[start..]) {
            hits.push(start);
            if hits.len() >= limit {
                break;
            }
        }
        from = start + 1;
    }
    hits
}

/// The target of a RIP-relative operand: `insn_addr + insn_len + disp32`,
/// with the signed 32-bit displacement read little-endian from
/// `bytes[disp_offset..]`, where `bytes` starts at the instruction.
/// `None` if `bytes` is too short or the address would overflow.
///
/// `mov rax, [rip+0x2f34053]` at `0x142290426` (7 bytes, disp at 3) →
/// `0x1451c4480`.
pub fn rip_target(
    insn_addr: usize,
    disp_offset: usize,
    insn_len: usize,
    bytes: &[u8],
) -> Option<usize> {
    let disp = displacement(bytes, disp_offset, 4)?;
    insn_addr
        .checked_add(insn_len)?
        .checked_add_signed(isize::try_from(disp).ok()?)
}

/// A signed displacement of `size` bytes (1, 2 or 4) at `bytes[at..]`,
/// sign-extended as the CPU does: `[rax+0x38]` encodes disp8 `0x38`, and
/// struct offsets of 0x80 and more use disp32. `None` for another size or
/// too few bytes.
pub fn displacement(bytes: &[u8], at: usize, size: usize) -> Option<i32> {
    let raw = bytes.get(at..at.checked_add(size)?)?;
    match *raw {
        [a] => Some(i32::from(a as i8)),
        [a, b] => Some(i32::from(i16::from_le_bytes([a, b]))),
        [a, b, c, d] => Some(i32::from_le_bytes([a, b, c, d])),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(text: &str) -> Pattern {
        Pattern::parse(text).unwrap()
    }

    #[test]
    fn patterns_parse_hex_and_wildcards() {
        let pattern = p("48 8b 05 ?? ? ?? ?? C7 80");
        assert_eq!(pattern.len(), 9);
        assert_eq!(pattern.bytes()[1], Some(0x8B));
        assert_eq!(pattern.bytes()[4], None);
        assert_eq!(pattern.to_string(), "48 8B 05 ?? ?? ?? ?? C7 80");
        // The scan anchors on a rarer byte than 0x48.
        assert_eq!(pattern.anchor, 2);
        assert_eq!(p("48 8B").anchor, 0);
    }

    #[test]
    fn malformed_patterns_are_refused() {
        assert_eq!(Pattern::parse("  "), Err(PatternError::Empty));
        assert_eq!(Pattern::parse("?? ??"), Err(PatternError::OnlyWildcards));
        for (text, index, token) in [
            ("48 4G", 1, "4G"),
            ("488B", 0, "488B"),
            ("48 8", 1, "8"),
            ("*", 0, "*"),
        ] {
            assert_eq!(
                Pattern::parse(text),
                Err(PatternError::BadToken {
                    index,
                    token: token.into()
                }),
                "{text}"
            );
        }
    }

    #[test]
    fn scan_finds_every_match_including_overlaps_and_edges() {
        let hay = [0xAA, 0xAA, 0xAA, 0x01, 0xAA, 0xAA];
        assert_eq!(scan(&hay, &p("AA AA")), vec![0, 1, 4]);
        assert_eq!(scan_up_to(&hay, &p("AA AA"), 2), vec![0, 1]);
        assert_eq!(scan(&hay, &p("01 AA AA")), vec![3]);
        assert_eq!(scan(&hay, &p("?? 01")), vec![2]);
        assert_eq!(scan(&hay, &p("AA ?? ??")), vec![0, 1, 2]);
        assert_eq!(scan(&hay, &p("01 AA AA AA")), Vec::<usize>::new());
        assert_eq!(scan(&hay[..1], &p("AA AA")), Vec::<usize>::new());
        assert_eq!(scan(&[], &p("AA")), Vec::<usize>::new());
    }

    #[test]
    fn scan_agrees_with_a_naive_search() {
        // Pseudo-random haystack with planted copies.
        let mut state = 0x1234_5678u32;
        let mut hay: Vec<u8> = (0..4096)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (state >> 24) as u8 & 0x0F
            })
            .collect();
        hay[100..104].copy_from_slice(&[0x48, 0x8B, 0x05, 0x07]);
        hay[4092..4096].copy_from_slice(&[0x48, 0x8B, 0x05, 0x09]);
        for text in ["48 8B 05 ??", "01 ?? 03", "?? 0A 0B", "05 05"] {
            let pattern = p(text);
            let naive: Vec<usize> = (0..=hay.len() - pattern.len())
                .filter(|&i| pattern.matches(&hay[i..]))
                .collect();
            assert_eq!(scan(&hay, &pattern), naive, "{text}");
        }
    }

    #[test]
    fn rip_targets_resolve_like_the_re_notes() {
        // CONFIG_LOAD: 0x142290426 mov rax,[rip+0x2f34053] -> 0x1451c4480.
        let insn = [0x48, 0x8B, 0x05, 0x53, 0x40, 0xF3, 0x02];
        assert_eq!(rip_target(0x1_4229_0426, 3, 7, &insn), Some(0x1_451c_4480));
        // A negative displacement points backwards.
        let back = [0xF3, 0x0F, 0x10, 0x05, 0xF0, 0xFF, 0xFF, 0xFF];
        assert_eq!(rip_target(0x1000, 4, 8, &back), Some(0x1000 + 8 - 16));
        assert_eq!(rip_target(0x1000, 4, 8, &back[..7]), None);
        assert_eq!(rip_target(usize::MAX - 2, 3, 7, &insn), None);
    }

    #[test]
    fn displacements_are_sign_extended() {
        // READ_X: movss xmm1,[rax+0x38] -> disp8 0x38 at 4.
        let read_x = [0xF3, 0x0F, 0x10, 0x48, 0x38];
        assert_eq!(displacement(&read_x, 4, 1), Some(0x38));
        assert_eq!(displacement(&[0xF0], 0, 1), Some(-16));
        // CONFIG_LOAD: mov rdx,[rax+0x198] -> disp32 at 13.
        let config = [0x48, 0x8B, 0x90, 0x98, 0x01, 0x00, 0x00];
        assert_eq!(displacement(&config, 3, 4), Some(0x198));
        assert_eq!(displacement(&[0x34, 0x12], 0, 2), Some(0x1234));
        assert_eq!(displacement(&config, 5, 4), None);
        assert_eq!(displacement(&config, 0, 3), None);
        assert_eq!(displacement(&config, usize::MAX, 4), None);
    }
}
