//! The mod-facing side of the game's two DLSS evaluations.
//!
//! The engine records its DLSS work as a command (type `0x17`) whose
//! translator (`0x14259dae0`, on the engine's **render thread**) applies the
//! command's barriers, calls EvalSS or EvalISP with the native command list,
//! and afterwards sets its descriptor heaps and root state again
//! (`ReDlss1Path.md` §2.4). The detours turn those calls into
//! [`SupersamplingCall`] / [`ImageSignalCall`] and hand them to the
//! registered [`UpscaleHandler`]:
//!
//! * same thread, synchronously, inside the engine's recording of that
//!   list: whatever the handler records lands between the engine's
//!   barriers and the rest of its frame;
//! * resources are borrowed: the engine owns them, no reference is added;
//! * resource states at entry are given in the call (`input_state`,
//!   `output_state`); the handler must leave them so.
//!
//! The types are plain data with raw COM pointers so that this crate does
//! not depend on another wave-2 crate.

use std::ffi::c_void;

use mhw_core::frame::Size;

/// `D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE`: what the engine puts
/// the inputs in (its state 4).
pub const STATE_NON_PIXEL_SHADER_RESOURCE: u32 = 0x40;
/// `D3D12_RESOURCE_STATE_UNORDERED_ACCESS`: the output (its state `0x400`).
pub const STATE_UNORDERED_ACCESS: u32 = 0x8;

/// The handle CreateFeatures stores for the supersampling feature
/// (`ctx+0x38`): a recognisable constant, never a pointer.
pub const FAKE_SUPERSAMPLING: usize = 0x0000_d155_0000_0001;
/// The handle stored for the image-signal-processing feature (`ctx+0x40`).
pub const FAKE_IMAGE_SIGNAL: usize = 0x0000_d155_0000_0009;

/// Whether `handle` is one of the stand-ins: the game passes them to
/// `NVSDK_NGX_D3D12_ReleaseFeature` when it rebuilds its targets, and the
/// driver must never see them.
pub fn is_stand_in_handle(handle: usize) -> bool {
    handle == FAKE_SUPERSAMPLING || handle == FAKE_IMAGE_SIGNAL
}

/// The rectangle the engine put in the descriptor: left, top, right,
/// bottom, in render pixels of the input.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    /// Left edge.
    pub left: f32,
    /// Top edge.
    pub top: f32,
    /// Right edge (exclusive).
    pub right: f32,
    /// Bottom edge (exclusive).
    pub bottom: f32,
}

impl Rect {
    /// From the four floats at the descriptor's `+0x2c`.
    pub fn from_floats([left, top, right, bottom]: [f32; 4]) -> Rect {
        Rect {
            left,
            top,
            right,
            bottom,
        }
    }

    /// Width and height, truncated as the engine's EvalSS does
    /// (`(int)(right − left)`); empty for a degenerate or non-finite rect.
    pub fn size(&self) -> Size {
        let side = |from: f32, to: f32| {
            let length = to - from;
            if length.is_finite() && length >= 1.0 {
                length as u32
            } else {
                0
            }
        };
        Size::new(side(self.left, self.right), side(self.top, self.bottom))
    }

    /// Whether the rect starts at the origin and has a size.
    pub fn is_origin_based(&self) -> bool {
        self.left == 0.0 && self.top == 0.0 && !self.size().is_empty()
    }
}

/// EvalSS (`0x142383cf0`): the supersampling pass, flags 3 only.
///
/// What the game would have handed DLSS 1: colour before its own temporal
/// filter, the dense velocity it computed from `tGBuffer4`
/// (`R32G32_FLOAT`, render size, convention to be measured), the R32F depth
/// copy (needs the depth alias), and a TAA history slot as output — which
/// the engine copies back into its main colour afterwards, so the output
/// must be written (a copy of the colour at least).
#[derive(Clone, Copy, Debug)]
pub struct SupersamplingCall {
    /// `ID3D12GraphicsCommandList*` being recorded.
    pub list: *mut c_void,
    /// `ID3D12Resource*` colour (`R11G11B10_FLOAT`), render size.
    pub colour: *mut c_void,
    /// `ID3D12Resource*` dense motion vectors (`R32G32_FLOAT`).
    pub motion: *mut c_void,
    /// `ID3D12Resource*` depth (`R32_FLOAT` copy).
    pub depth: *mut c_void,
    /// `ID3D12Resource*` output: the TAA history slot.
    pub output: *mut c_void,
    /// Rect of the input.
    pub rect: Rect,
    /// The game's reset: blend rate ≤ 0 (`taa+0x185` set or the history
    /// rebuilt).
    pub game_reset: bool,
    /// Engine render frame (`sRender+0x1f208`).
    pub frame: u64,
    /// State of colour, motion and depth at entry.
    pub input_state: u32,
    /// State of the output at entry.
    pub output_state: u32,
}

/// EvalISP (`0x142383990`): the upscale point (pass 16), after DOF, motion
/// blur, tonemapping and (when on) FXAA; output `UpscaleOut` at display size,
/// which the engine uses for the rest of the frame.
#[derive(Clone, Copy, Debug)]
pub struct ImageSignalCall {
    /// `ID3D12GraphicsCommandList*` being recorded.
    pub list: *mut c_void,
    /// `ID3D12Resource*` colour, render size (main colour after post).
    pub colour: *mut c_void,
    /// `ID3D12Resource*` output (`UpscaleOut`), display size.
    pub output: *mut c_void,
    /// Rect of the input.
    pub rect: Rect,
    /// Sharpness the game asked for (driver + user).
    pub sharpness: f32,
    /// The game's reset, latched by the last temporal pass since the
    /// previous ISP call: `taa+0x185` as the TAA draw saw it, or a blend
    /// rate ≤ 0. Consumed by this call.
    pub game_reset: bool,
    /// Engine render frame (`sRender+0x1f208`).
    pub frame: u64,
    /// Display size (`mScreenSize`), `0×0` if unreadable.
    pub display: Size,
    /// State of the colour at entry.
    pub input_state: u32,
    /// State of the output at entry.
    pub output_state: u32,
}

/// What `mod` does at the game's DLSS evaluations.
///
/// Called on the engine's render thread, synchronously, inside the
/// recording of `call.list`; must not block on the main thread (the main
/// thread waits for the render thread before it submits). Return whether
/// the call was handled; either way the engine is told it succeeded (the
/// game's DLSS 1 features do not exist to fall back on).
pub trait UpscaleHandler: Send + Sync {
    /// EvalSS. The default does nothing: with flags 2 it is never called.
    fn supersampling(&self, call: &SupersamplingCall) -> bool {
        let _ = call;
        false
    }

    /// EvalISP.
    fn image_signal(&self, call: &ImageSignalCall) -> bool;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_the_stand_ins_are_stand_ins() {
        assert!(is_stand_in_handle(FAKE_SUPERSAMPLING));
        assert!(is_stand_in_handle(FAKE_IMAGE_SIGNAL));
        assert!(!is_stand_in_handle(0));
        assert!(!is_stand_in_handle(0x7ff6_0000_1000));
    }

    #[test]
    fn the_rect_size_truncates_like_evalss() {
        let rect = Rect::from_floats([0.0, 0.0, 2560.9, 1440.2]);
        assert_eq!(rect.size(), Size::new(2560, 1440));
        assert!(rect.is_origin_based());
        let offset = Rect::from_floats([8.0, 0.0, 100.0, 50.0]);
        assert_eq!(offset.size(), Size::new(92, 50));
        assert!(!offset.is_origin_based());
        assert!(Rect::from_floats([5.0, 5.0, 1.0, 1.0]).size().is_empty());
        assert!(Rect::from_floats([0.0, 0.0, f32::NAN, 9.0])
            .size()
            .is_empty());
    }
}
