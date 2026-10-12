use std::ffi::CStr;

use super::*;

fn size(w: u32, h: u32) -> Size {
    Size::new(w, h)
}

fn desc() -> FeatureDesc {
    FeatureDesc {
        render: size(2560, 1440),
        display: size(3840, 2160),
        quality: Quality::MaxQuality,
        preset: RenderPreset::Default,
        flags: CreateFlags {
            is_hdr: true,
            mv_low_res: true,
            mv_jittered: false,
            depth_inverted: true,
            auto_exposure: true,
        },
        output_subrects: false,
    }
}

fn find(writes: &[Write], name: &CStr) -> Option<Value> {
    let mut found = writes.iter().filter(|w| w.name == name);
    let first = found.next().map(|w| w.value);
    assert!(found.next().is_none(), "{name:?} written twice");
    first
}

#[test]
fn quality_values_are_the_sdk_enumeration() {
    assert_eq!(Quality::MaxPerf.value(), 0);
    assert_eq!(Quality::Balanced.value(), 1);
    assert_eq!(Quality::MaxQuality.value(), 2);
    assert_eq!(Quality::UltraPerformance.value(), 3);
    assert_eq!(Quality::UltraQuality.value(), 4);
    assert_eq!(Quality::Dlaa.value(), 5);
}

#[test]
fn quality_follows_the_rendered_ratio() {
    let display = size(3840, 2160);
    let at = |w: u32| Quality::for_sizes(size(w, 1), display);
    assert_eq!(at(3840), Quality::Dlaa);
    assert_eq!(at(2561), Quality::MaxQuality); // 0.667
    assert_eq!(at(2227), Quality::Balanced); // 0.58
    assert_eq!(at(1920), Quality::MaxPerf); // 0.5
    assert_eq!(at(1280), Quality::UltraPerformance); // 0.333
                                                     // Custom scales go to the nearest level, never UltraQuality.
    assert_eq!(at(3072), Quality::MaxQuality); // 0.8
    assert_eq!(at(3456), Quality::Dlaa); // 0.9
    assert_eq!(Quality::for_sizes(size(1, 1), size(0, 0)), Quality::Dlaa);
}

#[test]
fn presets_map_to_the_header_values() {
    assert_eq!(RenderPreset::from(Preset::Default).value(), 0);
    assert_eq!(RenderPreset::from(Preset::J).value(), 10);
    assert_eq!(RenderPreset::from(Preset::K).value(), 11);
    assert_eq!(RenderPreset::from(Preset::L).value(), 12);
    assert_eq!(RenderPreset::from(Preset::M).value(), 13);
}

#[test]
fn flags_are_the_sdk_bits() {
    let none = CreateFlags::default();
    assert_eq!(none.bits(), 0);
    let bit = |flags: CreateFlags| flags.bits();
    assert_eq!(
        bit(CreateFlags {
            is_hdr: true,
            ..none
        }),
        0x1
    );
    assert_eq!(
        bit(CreateFlags {
            mv_low_res: true,
            ..none
        }),
        0x2
    );
    assert_eq!(
        bit(CreateFlags {
            mv_jittered: true,
            ..none
        }),
        0x4
    );
    assert_eq!(
        bit(CreateFlags {
            depth_inverted: true,
            ..none
        }),
        0x8
    );
    assert_eq!(
        bit(CreateFlags {
            auto_exposure: true,
            ..none
        }),
        0x40
    );
    // The 0.2 shipped set: HDR, inverted depth, auto exposure (+ MVLowRes
    // when upscaling).
    assert_eq!(
        bit(CreateFlags {
            mv_low_res: false,
            ..desc().flags
        }),
        0x49
    );
    assert_eq!(desc().flags.bits(), 0x4b);
}

#[test]
fn mv_low_res_is_set_exactly_when_upscaling() {
    assert!(CreateFlags::mv_low_res_for(
        size(1920, 1080),
        size(3840, 2160)
    ));
    assert!(CreateFlags::mv_low_res_for(
        size(3840, 2000),
        size(3840, 2160)
    ));
    assert!(!CreateFlags::mv_low_res_for(
        size(3840, 2160),
        size(3840, 2160)
    ));
}

#[test]
fn descriptions_are_checked() {
    assert_eq!(desc().validate(), Ok(()));
    let mut empty = desc();
    empty.render = size(0, 1440);
    assert_eq!(empty.validate(), Err(ParamError::EmptySize));
    let mut larger = desc();
    larger.render = size(3840, 2400);
    assert_eq!(larger.validate(), Err(ParamError::RenderLargerThanOutput));
    let mut dlaa = desc();
    dlaa.render = dlaa.display;
    assert_eq!(dlaa.validate(), Ok(()));
}

#[test]
fn creation_writes_sizes_quality_and_flags_with_their_types() {
    let writes = desc().create_writes();
    assert_eq!(
        find(&writes, names::CREATION_NODE_MASK),
        Some(Value::U32(1))
    );
    assert_eq!(
        find(&writes, names::VISIBILITY_NODE_MASK),
        Some(Value::U32(1))
    );
    assert_eq!(find(&writes, names::WIDTH), Some(Value::U32(2560)));
    assert_eq!(find(&writes, names::HEIGHT), Some(Value::U32(1440)));
    assert_eq!(find(&writes, names::OUT_WIDTH), Some(Value::U32(3840)));
    assert_eq!(find(&writes, names::OUT_HEIGHT), Some(Value::U32(2160)));
    assert_eq!(
        find(&writes, names::PERF_QUALITY_VALUE),
        Some(Value::I32(2))
    );
    assert_eq!(find(&writes, names::RTX_VALUE), Some(Value::I32(0)));
    assert_eq!(find(&writes, names::CREATE_FLAGS), Some(Value::I32(0x4b)));
    assert_eq!(
        find(&writes, names::ENABLE_OUTPUT_SUBRECTS),
        Some(Value::I32(0))
    );
    // The default preset is left to the library: no hint written.
    for hint in names::PRESET_HINTS {
        assert_eq!(find(&writes, hint), None);
    }
}

#[test]
fn a_chosen_preset_is_hinted_for_every_quality_level() {
    let mut with_k = desc();
    with_k.preset = RenderPreset::K;
    with_k.output_subrects = true;
    let writes = with_k.create_writes();
    for hint in names::PRESET_HINTS {
        assert_eq!(find(&writes, hint), Some(Value::U32(11)), "{hint:?}");
    }
    assert_eq!(
        find(&writes, names::ENABLE_OUTPUT_SUBRECTS),
        Some(Value::I32(1))
    );
}

fn inputs() -> RawInputs {
    RawInputs {
        colour: 0x1000,
        motion: 0x2000,
        depth: 0x3000,
        output: 0x4000,
        exposure: 0,
    }
}

fn params() -> EvalParams {
    EvalParams {
        jitter: Vec2 { x: 0.25, y: -0.125 },
        mv_scale: Vec2 {
            x: 2560.0,
            y: 1440.0,
        },
        subrects: Subrects::full(size(2560, 1440)),
        reset: true,
    }
}

#[test]
fn evaluation_writes_every_input_with_its_type() {
    let mut p = params();
    p.subrects.colour = Offset { x: 1, y: 2 };
    p.subrects.depth = Offset { x: 3, y: 4 };
    p.subrects.motion = Offset { x: 5, y: 6 };
    p.subrects.output = Offset { x: 7, y: 8 };
    p.subrects.render = size(2000, 1000);
    let writes = eval_writes(
        &RawInputs {
            exposure: 0x5000,
            ..inputs()
        },
        &p,
    );
    let expect = [
        (names::COLOR, Value::D3d12Resource(0x1000)),
        (names::MOTION_VECTORS, Value::D3d12Resource(0x2000)),
        (names::DEPTH, Value::D3d12Resource(0x3000)),
        (names::OUTPUT, Value::D3d12Resource(0x4000)),
        (names::EXPOSURE_TEXTURE, Value::D3d12Resource(0x5000)),
        (names::JITTER_OFFSET_X, Value::F32(0.25)),
        (names::JITTER_OFFSET_Y, Value::F32(-0.125)),
        (names::MV_SCALE_X, Value::F32(2560.0)),
        (names::MV_SCALE_Y, Value::F32(1440.0)),
        (names::SHARPNESS, Value::F32(0.0)),
        (names::RESET, Value::I32(1)),
        (names::COLOR_SUBRECT_X, Value::U32(1)),
        (names::COLOR_SUBRECT_Y, Value::U32(2)),
        (names::DEPTH_SUBRECT_X, Value::U32(3)),
        (names::DEPTH_SUBRECT_Y, Value::U32(4)),
        (names::MV_SUBRECT_X, Value::U32(5)),
        (names::MV_SUBRECT_Y, Value::U32(6)),
        (names::OUTPUT_SUBRECT_X, Value::U32(7)),
        (names::OUTPUT_SUBRECT_Y, Value::U32(8)),
        (names::RENDER_SUBRECT_WIDTH, Value::U32(2000)),
        (names::RENDER_SUBRECT_HEIGHT, Value::U32(1000)),
    ];
    assert_eq!(expect.len(), EVAL_WRITES);
    for (name, value) in expect {
        assert_eq!(find(&writes, name), Some(value), "{name:?}");
    }
}

#[test]
fn a_missing_exposure_texture_is_cleared_and_reset_off_is_zero() {
    let mut p = params();
    p.reset = false;
    let writes = eval_writes(&inputs(), &p);
    assert_eq!(
        find(&writes, names::EXPOSURE_TEXTURE),
        Some(Value::D3d12Resource(0))
    );
    assert_eq!(find(&writes, names::RESET), Some(Value::I32(0)));
}

#[test]
fn evaluations_are_checked_against_the_feature() {
    let d = desc();
    assert_eq!(params().validate(&d, &inputs()), Ok(()));
    for (broken, which) in [
        (
            RawInputs {
                colour: 0,
                ..inputs()
            },
            "Color",
        ),
        (
            RawInputs {
                motion: 0,
                ..inputs()
            },
            "MotionVectors",
        ),
        (
            RawInputs {
                depth: 0,
                ..inputs()
            },
            "Depth",
        ),
        (
            RawInputs {
                output: 0,
                ..inputs()
            },
            "Output",
        ),
    ] {
        assert_eq!(
            params().validate(&d, &broken),
            Err(ParamError::MissingInput(which))
        );
    }
    let mut too_big = params();
    too_big.subrects.render = size(2561, 1440);
    assert_eq!(
        too_big.validate(&d, &inputs()),
        Err(ParamError::SubrectOutOfRange)
    );
    let mut empty = params();
    empty.subrects.render = size(2560, 0);
    assert_eq!(
        empty.validate(&d, &inputs()),
        Err(ParamError::SubrectOutOfRange)
    );
    let mut nan = params();
    nan.jitter.x = f32::NAN;
    assert_eq!(nan.validate(&d, &inputs()), Err(ParamError::NotFinite));
    let mut inf = params();
    inf.mv_scale.y = f32::INFINITY;
    assert_eq!(inf.validate(&d, &inputs()), Err(ParamError::NotFinite));
}

#[test]
fn only_the_two_stand_ins_are_recognised() {
    assert!(is_stand_in(STAND_IN_SUPER_SAMPLING));
    assert!(is_stand_in(STAND_IN_IMAGE_SIGNAL));
    assert!(!is_stand_in(0));
    // Neighbours and pointer-like values are real handles.
    assert!(!is_stand_in(STAND_IN_SUPER_SAMPLING + 1));
    assert!(!is_stand_in(0x0000_01d1_5500_0001));
    // Not mistakable for user-space pointers NGX would hand out.
    assert_eq!(STAND_IN_SUPER_SAMPLING & 0xfff, 1);
    assert_eq!(STAND_IN_IMAGE_SIGNAL & 0xfff, 9);
}
