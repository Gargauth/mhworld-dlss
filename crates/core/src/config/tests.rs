use super::*;

fn kinds(diagnostics: &[Diagnostic]) -> Vec<(usize, &str, &DiagnosticKind)> {
    diagnostics
        .iter()
        .map(|d| (d.line, d.key.as_str(), &d.kind))
        .collect()
}

#[test]
fn the_default_file_parses_to_the_defaults_without_diagnostics() {
    let text = Settings::write_default(Build::Release);
    assert_eq!(parse(&text, Build::Release), (Settings::default(), vec![]));
    assert!(!text.contains("[Dev]") && !text.contains("[Experimental]"));
    assert!(text.contains("LogLevel = Info\n"));
    assert!(text.contains("Mode = Quality\n"));
}

#[test]
fn a_typical_file_is_read_case_insensitively_with_comments_and_quotes() {
    let text = "\
; my settings
[general]
enabled = ON            ; inline comment
LOGLEVEL = debug

[ Upscaling ]
Mode = performance  # another comment
Preset = \"k\"
Sharpness = 0.25
DownsampleVolume = off

[FrameGeneration]
Enabled = 1
Multiplier = 3
ReflexBoost = true
";
    let (settings, diagnostics) = parse(text, Build::Release);
    assert_eq!(diagnostics, vec![]);
    assert!(settings.general.enabled);
    assert_eq!(settings.general.log_level, LevelFilter::Debug);
    assert_eq!(settings.upscaling.mode, UpscalingMode::Performance);
    assert_eq!(settings.upscaling.scale(), 0.5);
    assert_eq!(settings.upscaling.preset, Preset::K);
    assert_eq!(settings.upscaling.sharpness, 0.25);
    assert!(!settings.upscaling.downsample_volume);
    assert!(settings.frame_generation.enabled && settings.frame_generation.reflex_boost);
    assert_eq!(settings.frame_generation.multiplier, 3);
}

#[test]
fn modes_map_to_scales() {
    let scales: Vec<Option<f32>> = UpscalingMode::NAMES
        .iter()
        .map(|(_, m)| m.scale())
        .collect();
    assert_eq!(
        scales,
        [Some(1.0), Some(0.667), Some(0.58), Some(0.5), None]
    );
    let (settings, _) = parse(
        "[Upscaling]\nMode = Custom\nRenderScale = 0.8",
        Build::Release,
    );
    assert_eq!(settings.upscaling.scale(), 0.8);
}

#[test]
fn byte_order_marks_and_line_endings_are_handled() {
    let text = "\u{feff}[General]\r\nEnabled = off\r\n";
    let (settings, diagnostics) = parse(text, Build::Release);
    assert!(!settings.general.enabled);
    assert_eq!(diagnostics, vec![]);

    let mut utf8 = vec![0xEF, 0xBB, 0xBF];
    utf8.extend_from_slice(b"[General]\nEnabled = off\n");
    let mut utf16 = vec![0xFF, 0xFE];
    utf16.extend(
        "[General]\r\nEnabled = off\r\n"
            .encode_utf16()
            .flat_map(u16::to_le_bytes),
    );
    for bytes in [utf8, utf16] {
        let (settings, diagnostics) = parse_bytes(&bytes, Build::Release);
        assert!(!settings.general.enabled);
        assert_eq!(diagnostics, vec![]);
    }
}

#[test]
fn garbage_is_reported_and_never_fatal() {
    let bytes =
        b"\x00\x01[General\nEnabled = \xFF\xFEyes\n=\n[\n]]]\nhello world\n[Upscaling]\nMode";
    let (settings, diagnostics) = parse_bytes(bytes, Build::Release);
    assert_eq!(settings, Settings::default());
    assert_eq!(diagnostics[0].kind, DiagnosticKind::Encoding);
    assert!(diagnostics
        .iter()
        .any(|d| matches!(d.kind, DiagnosticKind::Syntax { .. })));
    assert!(diagnostics
        .iter()
        .any(|d| d.kind == DiagnosticKind::OutsideSection));
    assert!(diagnostics.iter().all(Diagnostic::is_rejection));

    // Pseudo-random bytes and lines never panic.
    let mut state = 0x2545_f491u32;
    for round in 0..200 {
        let bytes: Vec<u8> = (0..(round * 7) % 400)
            .map(|_| {
                state ^= state << 13;
                state ^= state >> 17;
                state ^= state << 5;
                b"[]=;#\" \nAaZz019.-\r\xFF\xEF"[(state % 20) as usize]
            })
            .collect();
        let _ = parse_bytes(&bytes, Build::CURRENT);
    }
}

#[test]
fn out_of_range_numbers_are_clamped_and_reported() {
    let text = "\
[Upscaling]
Mode = Custom
RenderScale = 0.3
Sharpness = -1
[FrameGeneration]
Multiplier = 5
";
    let (settings, diagnostics) = parse(text, Build::Release);
    assert_eq!(settings.upscaling.render_scale, 0.5);
    assert_eq!(settings.upscaling.sharpness, 0.0);
    assert_eq!(settings.frame_generation.multiplier, 4);
    let clamped = |value: &str, used: &str| DiagnosticKind::Clamped {
        value: value.into(),
        used: used.into(),
    };
    assert_eq!(
        kinds(&diagnostics),
        [
            (3, "RenderScale", &clamped("0.3", "0.5")),
            (4, "Sharpness", &clamped("-1", "0.0")),
            (6, "Multiplier", &clamped("5", "4")),
        ]
    );
    assert!(diagnostics.iter().all(|d| !d.is_rejection()));
}

#[test]
fn invalid_values_keep_the_default() {
    let text = "\
[General]
Enabled = yes
LogLevel = Off
[Upscaling]
Mode = Ultra
RenderScale = NaN
Preset = Z
[FrameGeneration]
Multiplier = two
";
    let (settings, diagnostics) = parse(text, Build::Release);
    assert_eq!(settings, Settings::default());
    let keys: Vec<&str> = diagnostics.iter().map(|d| d.key.as_str()).collect();
    assert_eq!(
        keys,
        [
            "Enabled",
            "LogLevel",
            "Mode",
            "RenderScale",
            "Preset",
            "Multiplier"
        ]
    );
    assert!(diagnostics
        .iter()
        .all(|d| matches!(d.kind, DiagnosticKind::Invalid { .. })));
    assert_eq!(
        diagnostics[2].to_string(),
        "mhw-dlss.ini:5 [Upscaling] Mode: \"Ultra\" is not valid (expected DLAA | Quality | Balanced | Performance | Custom), ignored"
    );
}

#[test]
fn unknown_sections_keys_and_keys_outside_sections_are_reported() {
    let text = "\
render_scale = 0.667
[General]
Colour = blue
[Telemetry]
Upload = true
Endpoint = x
";
    let (settings, diagnostics) = parse(text, Build::Release);
    assert_eq!(settings, Settings::default());
    assert_eq!(
        kinds(&diagnostics),
        [
            (1, "render_scale", &DiagnosticKind::OutsideSection),
            (3, "Colour", &DiagnosticKind::UnknownKey),
            (4, "", &DiagnosticKind::UnknownSection),
        ]
    );
}

#[test]
fn a_repeated_key_reports_and_the_last_wins() {
    let text = "[General]\nLogLevel = Warn\n\n[general]\nloglevel = Trace\n";
    let (settings, diagnostics) = parse(text, Build::Release);
    assert_eq!(settings.general.log_level, LevelFilter::Trace);
    assert_eq!(
        kinds(&diagnostics),
        [(5, "loglevel", &DiagnosticKind::Duplicate { first_line: 2 })]
    );
}

#[test]
fn render_scale_without_custom_mode_is_reported_as_without_effect() {
    let no_effect = DiagnosticKind::NoEffect {
        reason: "only used with Mode = Custom",
    };
    let (_, diagnostics) = parse(
        "[Upscaling]\nRenderScale = 0.8\nMode = DLAA",
        Build::Release,
    );
    assert_eq!(kinds(&diagnostics), [(2, "RenderScale", &no_effect)]);
    let (_, diagnostics) = parse(
        "[Upscaling]\nRenderScale = 0.8\nMode = Custom",
        Build::Release,
    );
    assert_eq!(diagnostics, vec![]);
    let (_, diagnostics) = parse("[Upscaling]\nRenderScale = 0.667", Build::Release);
    assert_eq!(diagnostics, vec![]);
}

const DEV_FILE: &str = "\
[Dev]
Listen = 0.0.0.0:47801
LogTargets = \"render=trace, fg=debug\"
DebugLayer = true
[Experimental]
JitterScale = render
JitterPhases = 64
MipBias = -1.5
DlssHdr = off
Reset = gameonly
";

#[test]
fn dev_keys_are_ignored_by_a_release_build() {
    let (settings, diagnostics) = parse(DEV_FILE, Build::Release);
    assert_eq!(settings, Settings::default());
    assert_eq!(*settings.experimental(), Experimental::DEFAULT);
    assert_eq!(diagnostics.len(), 8);
    assert!(diagnostics
        .iter()
        .all(|d| d.kind == DiagnosticKind::DevOnly));
    assert_eq!(
        diagnostics[0].to_string(),
        "mhw-dlss.ini:2 [Dev] Listen: ignored, dev build only"
    );
    let mut settings = Settings::default();
    let error = settings
        .set("Experimental", "JitterScale", "Render", Build::Release)
        .unwrap_err();
    assert_eq!(error.kind, DiagnosticKind::DevOnly);
}

#[test]
fn the_release_behaviour_is_the_experimental_default() {
    let exp = Experimental::DEFAULT;
    assert_eq!(exp.input_path, InputPath::Supersampling);
    assert_eq!(exp.jitter_scale, JitterScale::Display);
    assert_eq!(exp.jitter_phases, 32);
    assert!(!exp.hud_less && exp.hold_anti_aliasing);
    assert_eq!(exp.taa_sharpen, TaaSharpen::Off);
    assert_eq!(exp.wait_latch, WaitLatch::Auto);
    assert_eq!(exp.reset, ResetPolicy::Auto);
    assert_eq!(exp.mip_bias, MipBias::Auto);
    assert_eq!(exp.dlss_hdr, AutoBool::Auto);
    assert_eq!(*Settings::default().experimental(), exp);
}

#[test]
fn set_applies_like_the_file() {
    let mut settings = Settings::default();
    assert_eq!(
        settings.set("Upscaling", "Mode", "dlaa", Build::Release),
        Ok(None)
    );
    assert_eq!(settings.upscaling.mode, UpscalingMode::Dlaa);
    let note = settings
        .set("upscaling", "RenderScale", "0.2", Build::Release)
        .unwrap()
        .unwrap();
    assert_eq!(settings.upscaling.render_scale, 0.5);
    assert!(matches!(note.kind, DiagnosticKind::Clamped { .. }));
    assert_eq!(note.line, 0);
    let error = settings
        .set("Upscaling", "Mode", "x", Build::Release)
        .unwrap_err();
    assert!(error.is_rejection());
    assert_eq!(settings.upscaling.mode, UpscalingMode::Dlaa);
    assert_eq!(
        settings
            .set("Nope", "Mode", "x", Build::Release)
            .unwrap_err()
            .kind,
        DiagnosticKind::UnknownSection
    );
    let note = settings
        .set("Upscaling", "RenderScale", "0.75", Build::Release)
        .unwrap()
        .unwrap();
    assert!(matches!(note.kind, DiagnosticKind::NoEffect { .. }));
}

#[test]
fn diff_names_changed_groups_and_restarts() {
    let old = Settings::default();
    assert!(Settings::diff(&old, &old).is_empty());

    let mut new = old.clone();
    new.general.log_level = LevelFilter::Trace;
    new.upscaling.preset = Preset::M;
    let changes = Settings::diff(&old, &new);
    assert_eq!(
        changes.iter().collect::<Vec<_>>(),
        [Group::LogLevel, Group::Preset]
    );
    assert!(!changes.needs_restart());

    new.frame_generation.enabled = true;
    new.frame_generation.multiplier = 4;
    let changes = Settings::diff(&old, &new);
    assert!(
        changes.contains(Group::FrameGeneration) && changes.contains(Group::FrameGenerationOptions)
    );
    assert!(changes.needs_restart());
    assert_eq!(Group::FrameGeneration.reload(), Reload::Restart);
    assert_eq!(Group::Sharpness.reload(), Reload::Live);

    // The fog upsample is chosen when the game builds it.
    let mut fog = old.clone();
    fog.upscaling.downsample_volume = false;
    let changes = Settings::diff(&old, &fog);
    assert_eq!(
        changes.iter().collect::<Vec<_>>(),
        [Group::DownsampleVolume]
    );
    assert!(changes.needs_restart());

    // The same effective scale is no change.
    let mut custom = old.clone();
    custom.upscaling.mode = UpscalingMode::Custom;
    assert!(Settings::diff(&old, &custom).is_empty());
    custom.upscaling.render_scale = 0.9;
    assert_eq!(
        Settings::diff(&old, &custom).iter().collect::<Vec<_>>(),
        [Group::Scale]
    );
}

#[test]
fn every_group_has_its_own_bit() {
    let mut all = Changes::NONE;
    for (index, group) in Group::ALL.iter().enumerate() {
        assert!(!all.contains(*group));
        all.insert(*group);
        assert_eq!(all.iter().count(), index + 1);
    }
}

#[cfg(feature = "dev")]
mod dev {
    use super::*;

    #[test]
    fn the_dev_default_file_parses_to_the_defaults_and_a_release_build_skips_its_dev_part() {
        let text = Settings::write_default(Build::Dev);
        assert!(text.contains("[Dev]") && text.contains("[Experimental]"));
        assert_eq!(parse(&text, Build::Dev), (Settings::default(), vec![]));
        let (settings, diagnostics) = parse(&text, Build::Release);
        assert_eq!(settings, Settings::default());
        assert_eq!(diagnostics.len(), 17);
        assert!(diagnostics
            .iter()
            .all(|d| d.kind == DiagnosticKind::DevOnly));
    }

    #[test]
    fn dev_keys_are_read_by_a_dev_build() {
        let (settings, diagnostics) = parse(DEV_FILE, Build::Dev);
        assert_eq!(diagnostics, vec![]);
        assert_eq!(settings.dev.listen.to_string(), "0.0.0.0:47801");
        assert_eq!(
            settings.dev.log_targets,
            crate::log::parse_targets("render=trace,fg=debug").unwrap()
        );
        assert!(settings.dev.debug_layer && !settings.dev.dred);
        let exp = settings.experimental();
        assert_eq!(exp.jitter_scale, JitterScale::Render);
        assert_eq!(exp.jitter_phases, 64);
        assert_eq!(exp.mip_bias, MipBias::Value(-1.5));
        assert_eq!(exp.dlss_hdr, AutoBool::Forced(false));
        assert_eq!(exp.reset, ResetPolicy::GameOnly);
    }

    #[test]
    fn invalid_dev_values_are_reported() {
        let text = "\
[Dev]
Listen = localhost
LogTargets = rendr=trace
TokenFile =
[Experimental]
JitterPhases = 0
MipBias = -9
Turbo = on
";
        let (settings, diagnostics) = parse(text, Build::Dev);
        assert_eq!(settings.dev, Dev::default());
        assert_eq!(settings.experimental.jitter_phases, 1);
        assert_eq!(settings.experimental.mip_bias, MipBias::Value(-4.0));
        let summary: Vec<(&str, bool)> = diagnostics
            .iter()
            .map(|d| (d.key.as_str(), d.is_rejection()))
            .collect();
        assert_eq!(
            summary,
            [
                ("Listen", true),
                ("LogTargets", true),
                ("TokenFile", true),
                ("JitterPhases", false),
                ("MipBias", false),
                ("Turbo", true),
            ]
        );
    }

    #[test]
    fn dev_groups_diff() {
        let old = Settings::default();
        let mut new = old.clone();
        new.dev.debug_layer = true;
        new.experimental.jitter_scale = JitterScale::Off;
        new.experimental.mip_bias = MipBias::Off;
        let changes = Settings::diff(&old, &new);
        assert_eq!(
            changes.iter().collect::<Vec<_>>(),
            [Group::GpuDebug, Group::Jitter, Group::Tune]
        );
        assert!(changes.needs_restart());
    }
}
