//! What `CSTemporalAA2` binds where, and the rectangle it resolves.
//!
//! Inputs are located by name through the shader's reflection and the bound
//! root signature's layout. When the root signature was created before the
//! hooks (no layout), the measured table positions are used instead:
//! `root0[u0..8@+0 t0..31@+9]`, so `rwOutput` at +0, `tBaseMap` (t0) at +9
//! and `tGBuffer4` (t4) at +13. +10 is the pass's own history, and feeding
//! DLSS that smears.

use mhw_core::frame::Size;

use crate::dxbc::{InputKind, Reflection};
use crate::rootsig::{Layout, RangeKind};

/// Thread group edge of the temporal pass (16×16).
pub const GROUP: u32 = 16;

/// One input of the pass.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Input {
    /// Register class.
    pub kind: InputKind,
    /// HLSL name.
    pub name: &'static str,
    /// Register if the reflection lacks the name.
    pub register: u32,
    /// Table offset if the layout is unknown.
    pub offset: u32,
}

/// `tBaseMap`, this frame's colour.
pub const COLOUR: Input = Input {
    kind: InputKind::ShaderResource,
    name: "tBaseMap",
    register: 0,
    offset: 9,
};
/// `tGBuffer4`, the packed motion vectors.
pub const MOTION: Input = Input {
    kind: InputKind::ShaderResource,
    name: "tGBuffer4",
    register: 4,
    offset: 13,
};
/// `rwOutput`, the pass's output.
pub const OUTPUT: Input = Input {
    kind: InputKind::Unordered,
    name: "rwOutput",
    register: 0,
    offset: 0,
};
/// The camera constant buffer's name.
pub const CAMERA_BUFFER: &str = "CBViewProjection";
/// Its register if the reflection lacks the name.
pub const CAMERA_REGISTER: u32 = 0;

/// Where an input sits in the bound tables.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Slot {
    /// Root parameter of the table; `None`: unknown, try every bound table.
    pub root: Option<u32>,
    /// Descriptor offset from the table start.
    pub offset: u32,
}

/// Where `input` is, given the shader and (if known) the root signature.
/// `None` when the layout is known and does not place it.
pub fn locate(layout: Option<&Layout>, reflection: &Reflection, input: Input) -> Option<Slot> {
    let (register, space) = reflection
        .binding(input.kind, input.name)
        .map_or((input.register, 0), |b| (b.register, b.space));
    let Some(layout) = layout else {
        return Some(Slot {
            root: None,
            offset: input.offset,
        });
    };
    let kind = match input.kind {
        InputKind::ShaderResource => RangeKind::Srv,
        InputKind::Unordered => RangeKind::Uav,
        InputKind::ConstantBuffer => RangeKind::Cbv,
        InputKind::Sampler => RangeKind::Sampler,
        InputKind::Other(_) => return None,
    };
    let slot = layout.table_slot(kind, register, space)?;
    Some(Slot {
        root: Some(slot.root),
        offset: slot.offset,
    })
}

/// The register of the camera buffer.
pub fn camera_register(reflection: &Reflection) -> u32 {
    reflection
        .binding(InputKind::ConstantBuffer, CAMERA_BUFFER)
        .map_or(CAMERA_REGISTER, |b| b.register)
}

/// The rectangle resolved: the groups' coverage clamped to the output.
///
/// The group count rounds up and the engine's targets are not multiples of
/// sixteen (3840 at 0.667 is 2561 wide, 161 groups cover 2576). Everything
/// that converts NDC to pixels divides by the width the camera rasterised
/// into, so the rounded figure made vectors 0.6% too long.
pub fn render_size(groups: [u32; 3], output: Size) -> Size {
    Size::new(
        groups[0].saturating_mul(GROUP).min(output.w),
        groups[1].saturating_mul(GROUP).min(output.h),
    )
}

/// The smallest render ratio the mod supports.
pub const SMALLEST_RATIO: f32 = 0.5;

/// The smallest a texture can be and still be one of the engine's
/// full-frame targets on `display`: half of it each way (the smallest
/// ratio), less the sixteen pixels the engine may cut the width down by,
/// never under 64. Relative to the display, because fixed thresholds
/// (960 wide, 1280×720) were right at 4K and missed every target at
/// 1920×1080 and a ratio of 0.5.
pub fn frame_floor(display: Size) -> Size {
    let floor = |side: u32| {
        ((side as f32 * SMALLEST_RATIO) as u32)
            .saturating_sub(16)
            .max(64)
    };
    Size::new(floor(display.w), floor(display.h))
}

/// Whether a `width × height` texture reaches `floor`.
pub fn reaches(floor: Size, width: u64, height: u32) -> bool {
    width >= u64::from(floor.w) && height >= floor.h
}
#[cfg(test)]
mod tests {
    use super::*;
    use crate::dxbc::{fixture::container, reflect};
    use crate::rootsig::{Param, RangeDesc};

    fn reflection() -> Reflection {
        reflect(&container(
            0x4353,
            &[
                ("CBScreen", 0, 1),
                ("CBViewProjection", 0, 0),
                ("tBaseMap", 2, 0),
                ("tBlendMap", 2, 1),
                ("tGBuffer4", 2, 4),
                ("rwOutput", 4, 0),
            ],
            &[],
        ))
        .unwrap()
    }

    fn layout() -> Layout {
        let range = |kind, count, offset| RangeDesc {
            kind,
            count,
            base_register: 0,
            space: 0,
            offset,
        };
        Layout::new(&[
            Param::Table(vec![
                range(RangeKind::Uav, 8, 0),
                range(RangeKind::Srv, 31, 9),
            ]),
            Param::Cbv {
                register: 0,
                space: 0,
            },
        ])
    }

    #[test]
    fn inputs_by_name_through_the_layout() {
        let (reflection, layout) = (reflection(), layout());
        let at = |input| locate(Some(&layout), &reflection, input);
        assert_eq!(
            at(COLOUR),
            Some(Slot {
                root: Some(0),
                offset: 9
            })
        );
        assert_eq!(
            at(MOTION),
            Some(Slot {
                root: Some(0),
                offset: 13
            })
        );
        assert_eq!(
            at(OUTPUT),
            Some(Slot {
                root: Some(0),
                offset: 0
            })
        );
        assert_eq!(camera_register(&reflection), 0);
        assert_eq!(layout.root_cbv(camera_register(&reflection), 0), Some(1));
    }

    #[test]
    fn measured_offsets_without_a_layout() {
        let reflection = reflection();
        assert_eq!(
            locate(None, &reflection, MOTION),
            Some(Slot {
                root: None,
                offset: 13
            })
        );
        // A layout that does not place the register says so.
        let empty = Layout::new(&[]);
        assert_eq!(locate(Some(&empty), &reflection, COLOUR), None);
    }

    #[test]
    fn render_size_is_clamped_to_the_output() {
        let output = Size::new(2561, 1441);
        assert_eq!(render_size([161, 91, 1], output), output);
        assert_eq!(render_size([80, 45, 1], output), Size::new(1280, 720));
        assert_eq!(render_size([u32::MAX, 1, 1], output).w, 2561);
    }

    #[test]
    fn the_floor_admits_half_ratio_targets_at_1080p() {
        let floor = frame_floor(Size::new(1920, 1080));
        assert_eq!(floor, Size::new(944, 524));
        assert!(reaches(floor, 960, 540));
        assert!(!reaches(floor, 512, 512));
        assert!(!reaches(floor, 2048, 256));
        assert_eq!(frame_floor(Size::new(0, 0)), Size::new(64, 64));
    }
}
