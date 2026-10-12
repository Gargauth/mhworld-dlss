//! Reading the file: lines, sections, keys, values.

use std::borrow::Cow;

use super::diagnostic::{Diagnostic, DiagnosticKind};
use super::value::{parse_bool, parse_f32, parse_u32, Named};
use super::{
    Build, FrameGeneration, General, LevelFilter, Preset, Settings, Upscaling, UpscalingMode,
};

/// The outcome of applying one value: applied (perhaps with a note), or
/// rejected.
type Applied = Result<Option<DiagnosticKind>, DiagnosticKind>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Section {
    General,
    Upscaling,
    FrameGeneration,
    #[cfg(feature = "dev")]
    Dev,
    #[cfg(feature = "dev")]
    Experimental,
    /// `[Dev]`/`[Experimental]` in a build that does not accept them.
    DevOnly,
    Unknown,
}

/// The section called `name` (case-insensitive) in `build`.
fn section(name: &str, build: Build) -> Section {
    #[cfg(feature = "dev")]
    let dev = build.has_dev();
    #[cfg(not(feature = "dev"))]
    let _ = build;
    match name.to_ascii_lowercase().as_str() {
        "general" => Section::General,
        "upscaling" => Section::Upscaling,
        "framegeneration" => Section::FrameGeneration,
        #[cfg(feature = "dev")]
        "dev" if dev => Section::Dev,
        #[cfg(feature = "dev")]
        "experimental" if dev => Section::Experimental,
        "dev" | "experimental" => Section::DevOnly,
        _ => Section::Unknown,
    }
}

/// Parse the file text. Never fails; see the module documentation.
pub fn parse(text: &str, build: Build) -> (Settings, Vec<Diagnostic>) {
    let mut settings = Settings::default();
    let mut diagnostics = Vec::new();
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut current: Option<(Section, String)> = None;
    // (section, key) lower-case → line, for duplicates.
    let mut seen: Vec<(Section, String, usize)> = Vec::new();
    let mut render_scale_line = None;

    for (index, raw) in text.lines().enumerate() {
        let number = index + 1;
        let line = strip_comment(raw).trim();
        if line.is_empty() {
            continue;
        }
        let diagnostic = |section: &str, key: &str, kind| Diagnostic {
            line: number,
            section: section.to_owned(),
            key: key.to_owned(),
            kind,
        };
        if let Some(header) = line.strip_prefix('[') {
            let Some(name) = header.strip_suffix(']').map(str::trim) else {
                diagnostics.push(diagnostic(
                    "",
                    "",
                    DiagnosticKind::Syntax {
                        text: line.to_owned(),
                    },
                ));
                continue;
            };
            let kind = section(name, build);
            if kind == Section::Unknown {
                diagnostics.push(diagnostic(name, "", DiagnosticKind::UnknownSection));
            }
            current = Some((kind, name.to_owned()));
            continue;
        }
        let Some((key, value)) = line.split_once('=') else {
            let section_name = current.as_ref().map_or("", |(_, name)| name.as_str());
            diagnostics.push(diagnostic(
                section_name,
                "",
                DiagnosticKind::Syntax {
                    text: line.to_owned(),
                },
            ));
            continue;
        };
        let (key, value) = (key.trim(), unquote(value.trim()));
        let Some((kind, section_name)) = &current else {
            diagnostics.push(diagnostic("", key, DiagnosticKind::OutsideSection));
            continue;
        };
        if *kind == Section::Unknown {
            continue;
        }
        let result = apply(&mut settings, *kind, key, value);
        if result.is_ok() {
            let lower = key.to_ascii_lowercase();
            if let Some((_, _, first_line)) = seen.iter().find(|(s, k, _)| s == kind && *k == lower)
            {
                let first_line = *first_line;
                diagnostics.push(diagnostic(
                    section_name,
                    key,
                    DiagnosticKind::Duplicate { first_line },
                ));
            } else {
                seen.push((*kind, lower, number));
            }
            if *kind == Section::Upscaling && key.eq_ignore_ascii_case("RenderScale") {
                render_scale_line = Some((number, section_name.clone(), key.to_owned()));
            }
        }
        match result {
            Ok(None) => {}
            Ok(Some(note)) | Err(note) => diagnostics.push(diagnostic(section_name, key, note)),
        }
    }

    if let Some((line, section, key)) = render_scale_line {
        if let Some(kind) = render_scale_note(&settings.upscaling) {
            diagnostics.push(Diagnostic {
                line,
                section,
                key,
                kind,
            });
        }
    }
    (settings, diagnostics)
}

/// Parse the file's bytes: UTF-8 with or without BOM, or UTF-16 with BOM.
pub fn parse_bytes(bytes: &[u8], build: Build) -> (Settings, Vec<Diagnostic>) {
    let (text, lossy) = decode(bytes);
    let (settings, mut diagnostics) = parse(&text, build);
    if lossy {
        diagnostics.insert(
            0,
            Diagnostic {
                line: 0,
                section: String::new(),
                key: String::new(),
                kind: DiagnosticKind::Encoding,
            },
        );
    }
    (settings, diagnostics)
}

fn decode(bytes: &[u8]) -> (Cow<'_, str>, bool) {
    let utf16 = |rest: &[u8], from: fn([u8; 2]) -> u16| {
        let (units, odd) = rest.as_chunks::<2>();
        let mut lossy = !odd.is_empty();
        let text: String = char::decode_utf16(units.iter().map(|pair| from(*pair)))
            .map(|c| {
                c.unwrap_or_else(|_| {
                    lossy = true;
                    char::REPLACEMENT_CHARACTER
                })
            })
            .collect();
        (Cow::Owned(text), lossy)
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, u16::from_le_bytes);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, u16::from_be_bytes);
    }
    let rest = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    let text = String::from_utf8_lossy(rest);
    let lossy = matches!(text, Cow::Owned(_));
    (text, lossy)
}

/// Set one key as the file would.
pub(super) fn set(
    settings: &mut Settings,
    section_name: &str,
    key: &str,
    value: &str,
    build: Build,
) -> Result<Option<Diagnostic>, Diagnostic> {
    let diagnostic = |kind| Diagnostic {
        line: 0,
        section: section_name.to_owned(),
        key: key.to_owned(),
        kind,
    };
    let kind = section(section_name.trim(), build);
    if kind == Section::Unknown {
        return Err(diagnostic(DiagnosticKind::UnknownSection));
    }
    let note = apply(settings, kind, key.trim(), unquote(value.trim())).map_err(diagnostic)?;
    let note = note.or_else(|| {
        (kind == Section::Upscaling && key.trim().eq_ignore_ascii_case("RenderScale"))
            .then(|| render_scale_note(&settings.upscaling))
            .flatten()
    });
    Ok(note.map(diagnostic))
}

/// `RenderScale` set to something other than the default while the mode
/// does not use it.
fn render_scale_note(upscaling: &Upscaling) -> Option<DiagnosticKind> {
    let default = Settings::default().upscaling.render_scale;
    (upscaling.mode != UpscalingMode::Custom && upscaling.render_scale != default).then_some(
        DiagnosticKind::NoEffect {
            reason: "only used with Mode = Custom",
        },
    )
}

/// Cut a `;`/`#` comment: the whole line if it starts with one, otherwise
/// from one preceded by whitespace and outside double quotes.
fn strip_comment(line: &str) -> &str {
    let trimmed = line.trim_start();
    if trimmed.starts_with(';') || trimmed.starts_with('#') {
        return "";
    }
    let mut quoted = false;
    let mut after_space = false;
    for (index, c) in line.char_indices() {
        match c {
            '"' => quoted = !quoted,
            ';' | '#' if after_space && !quoted => return &line[..index],
            _ => {}
        }
        after_space = c.is_whitespace();
    }
    line
}

/// Remove one pair of surrounding double quotes.
fn unquote(value: &str) -> &str {
    value
        .strip_prefix('"')
        .and_then(|inner| inner.strip_suffix('"'))
        .unwrap_or(value)
}

fn apply(settings: &mut Settings, section: Section, key: &str, value: &str) -> Applied {
    match section {
        Section::General => general(&mut settings.general, key, value),
        Section::Upscaling => upscaling(&mut settings.upscaling, key, value),
        Section::FrameGeneration => frame_generation(&mut settings.frame_generation, key, value),
        #[cfg(feature = "dev")]
        Section::Dev => dev::dev(&mut settings.dev, key, value),
        #[cfg(feature = "dev")]
        Section::Experimental => dev::experimental(&mut settings.experimental, key, value),
        Section::DevOnly => Err(DiagnosticKind::DevOnly),
        Section::Unknown => Err(DiagnosticKind::UnknownSection),
    }
}

fn invalid(value: &str, expected: impl Into<String>) -> DiagnosticKind {
    DiagnosticKind::Invalid {
        value: value.to_owned(),
        expected: expected.into(),
    }
}

fn set_bool(slot: &mut bool, value: &str) -> Applied {
    *slot = parse_bool(value).ok_or_else(|| invalid(value, "true | false"))?;
    Ok(None)
}

fn set_named<T: Named>(slot: &mut T, value: &str) -> Applied {
    *slot = T::parse(value).ok_or_else(|| invalid(value, T::choices()))?;
    Ok(None)
}

fn clamped(value: &str, used: impl std::fmt::Debug) -> Option<DiagnosticKind> {
    Some(DiagnosticKind::Clamped {
        value: value.to_owned(),
        used: format!("{used:?}"),
    })
}

fn set_f32(slot: &mut f32, value: &str, (low, high): (f32, f32)) -> Applied {
    let parsed = parse_f32(value, low, high)
        .ok_or_else(|| invalid(value, format!("a number {low:?}-{high:?}")))?;
    *slot = parsed.value;
    Ok(if parsed.clamped {
        clamped(value, parsed.value)
    } else {
        None
    })
}

fn set_u32(slot: &mut u32, value: &str, (low, high): (u32, u32)) -> Applied {
    let parsed = parse_u32(value, low, high)
        .ok_or_else(|| invalid(value, format!("a whole number {low}-{high}")))?;
    *slot = parsed.value;
    Ok(if parsed.clamped {
        clamped(value, parsed.value)
    } else {
        None
    })
}

fn general(general: &mut General, key: &str, value: &str) -> Applied {
    match key.to_ascii_lowercase().as_str() {
        "enabled" => set_bool(&mut general.enabled, value),
        "loglevel" => {
            const LEVELS: [LevelFilter; 5] = [
                LevelFilter::Error,
                LevelFilter::Warn,
                LevelFilter::Info,
                LevelFilter::Debug,
                LevelFilter::Trace,
            ];
            general.log_level = LEVELS
                .into_iter()
                .find(|level| level.as_str().eq_ignore_ascii_case(value))
                .ok_or_else(|| invalid(value, "Error | Warn | Info | Debug | Trace"))?;
            Ok(None)
        }
        _ => Err(DiagnosticKind::UnknownKey),
    }
}

fn upscaling(upscaling: &mut Upscaling, key: &str, value: &str) -> Applied {
    match key.to_ascii_lowercase().as_str() {
        "mode" => set_named::<UpscalingMode>(&mut upscaling.mode, value),
        "renderscale" => set_f32(&mut upscaling.render_scale, value, Upscaling::RENDER_SCALE),
        "preset" => set_named::<Preset>(&mut upscaling.preset, value),
        "sharpness" => set_f32(&mut upscaling.sharpness, value, Upscaling::SHARPNESS),
        "downsamplevolume" => set_bool(&mut upscaling.downsample_volume, value),
        _ => Err(DiagnosticKind::UnknownKey),
    }
}

fn frame_generation(fg: &mut FrameGeneration, key: &str, value: &str) -> Applied {
    match key.to_ascii_lowercase().as_str() {
        "enabled" => set_bool(&mut fg.enabled, value),
        "multiplier" => set_u32(&mut fg.multiplier, value, FrameGeneration::MULTIPLIER),
        "reflexboost" => set_bool(&mut fg.reflex_boost, value),
        _ => Err(DiagnosticKind::UnknownKey),
    }
}

/// The dev-only sections. Not compiled into release builds.
#[cfg(feature = "dev")]
mod dev {
    use super::super::value::{AutoBool, MipBias};
    use super::super::{
        Dev, Experimental, InputPath, JitterScale, ResetPolicy, TaaSharpen, WaitLatch,
    };
    use super::{invalid, set_bool, set_named, set_u32, Applied, DiagnosticKind};

    fn set_text(slot: &mut String, value: &str) -> Applied {
        if value.is_empty() {
            return Err(invalid(value, "a path"));
        }
        value.clone_into(slot);
        Ok(None)
    }

    pub(super) fn dev(dev: &mut Dev, key: &str, value: &str) -> Applied {
        match key.to_ascii_lowercase().as_str() {
            "listen" => {
                dev.listen = value.parse().map_err(|_| invalid(value, "address:port"))?;
                Ok(None)
            }
            "tokenfile" => set_text(&mut dev.token_file, value),
            "logtargets" => {
                dev.log_targets = crate::log::parse_targets(value)
                    .map_err(|_| invalid(value, "target=level, …"))?;
                Ok(None)
            }
            "debuglayer" => set_bool(&mut dev.debug_layer, value),
            "dred" => set_bool(&mut dev.dred, value),
            "capturedir" => set_text(&mut dev.capture_dir, value),
            "dumpshaders" => set_bool(&mut dev.dump_shaders, value),
            _ => Err(DiagnosticKind::UnknownKey),
        }
    }

    pub(super) fn experimental(exp: &mut Experimental, key: &str, value: &str) -> Applied {
        match key.to_ascii_lowercase().as_str() {
            "inputpath" => set_named::<InputPath>(&mut exp.input_path, value),
            "jitterscale" => set_named::<JitterScale>(&mut exp.jitter_scale, value),
            "jitterphases" => set_u32(&mut exp.jitter_phases, value, Experimental::JITTER_PHASES),
            "hudless" => set_bool(&mut exp.hud_less, value),
            "taasharpen" => set_named::<TaaSharpen>(&mut exp.taa_sharpen, value),
            "holdantialiasing" => set_bool(&mut exp.hold_anti_aliasing, value),
            "waitlatch" => set_named::<WaitLatch>(&mut exp.wait_latch, value),
            "reset" => set_named::<ResetPolicy>(&mut exp.reset, value),
            "mipbias" => {
                let (low, high) = MipBias::RANGE;
                let parsed = MipBias::parse(value)
                    .ok_or_else(|| invalid(value, format!("Auto | Off | {low:?}-{high:?}")))?;
                exp.mip_bias = parsed.value;
                Ok(parsed.clamped.then(|| DiagnosticKind::Clamped {
                    value: value.to_owned(),
                    used: parsed.value.to_string(),
                }))
            }
            "dlsshdr" => {
                exp.dlss_hdr =
                    AutoBool::parse(value).ok_or_else(|| invalid(value, "Auto | true | false"))?;
                Ok(None)
            }
            _ => Err(DiagnosticKind::UnknownKey),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn comments_are_cut_outside_quotes_after_whitespace() {
        assert_eq!(strip_comment("; all of it"), "");
        assert_eq!(strip_comment("   # all of it"), "");
        assert_eq!(strip_comment("Mode = DLAA ; best"), "Mode = DLAA ");
        assert_eq!(
            strip_comment("Mode = DLAA# no space"),
            "Mode = DLAA# no space"
        );
        assert_eq!(strip_comment(r#"Dir = "a ;b" ; c"#), r#"Dir = "a ;b" "#);
        assert_eq!(unquote(r#""render=trace""#), "render=trace");
        assert_eq!(unquote(r#""half"#), r#""half"#);
        assert_eq!(unquote(r#""""#), "");
    }

    #[test]
    fn utf16_and_utf8_boms_decode() {
        let text = "[General]\nEnabled = false\n";
        let mut le = vec![0xFF, 0xFE];
        le.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        let mut be = vec![0xFE, 0xFF];
        be.extend(text.encode_utf16().flat_map(u16::to_be_bytes));
        let mut utf8 = vec![0xEF, 0xBB, 0xBF];
        utf8.extend_from_slice(text.as_bytes());
        for bytes in [le, be, utf8] {
            let (decoded, lossy) = decode(&bytes);
            assert_eq!(decoded, text);
            assert!(!lossy);
        }
        let (_, lossy) = decode(&[0xFF, 0xFE, 0x41]);
        assert!(lossy);
        let (decoded, lossy) = decode(b"a\xFFb");
        assert_eq!(decoded, "a\u{FFFD}b");
        assert!(lossy);
    }
}
