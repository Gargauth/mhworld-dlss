//! Offsets that no instruction in the gate names, from the RE reports of
//! Steam build 15539686.
//!
//! Anchored offsets are derived from the game's code at the gate
//! ([`crate::sites::field`]); these are the rest, documented where they were
//! found. Names are the engine's reflection names where the RE recovered
//! them.
//!
//! In another build they are trusted as far as the code can vouch for them:
//! an object here that has anchored fields too (`render_config`, `camera`,
//! `render_settings`, `taa`, `descriptor`, `dlss_context`) stands the gate
//! down when one of them moved ([`crate::anchors::AnchorProblem::Moved`]),
//! since its layout changed. The objects without an anchored field
//! (`viewport`, `device`, `capture`, `texture`, `native`, `view`) are used
//! as the reference build has them, unchecked.

/// `sRender` (render config, `[0x1451c4480]`; `ReFramePresent.md`,
/// `ReJitterCamera.md` §2.1).
pub mod render_config {
    /// Object size (`0x140fba3a0` allocates `0x7b710`).
    pub const SIZE: usize = 0x7b710;
    /// `mScreenSize.h`: the field after `mScreenSize.w` (anchored `+0x198`).
    pub const SCREEN_HEIGHT_AFTER_WIDTH: usize = 4;
    /// `mBackBufferSize.h` after `.w` (anchored `+0x1f448`).
    pub const BACK_BUFFER_HEIGHT_AFTER_WIDTH: usize = 4;
    /// The render frame counter: `++` at every kick (`0x142293280`), the
    /// stamp `sCamera::setJitter` writes. One per presented frame.
    pub const RENDER_FRAME: usize = 0x1f208;
    /// User sharpness added to the optimal answer's for EvalISP.
    pub const USER_SHARPNESS: usize = 0xac;
    /// Output gamma (`f32`, 2.2 from the ctor `0x142290e05`): the encode's
    /// `fGammaCorrect` is its reciprocal (`0x142297b1d`;
    /// `ReFramePresent.md` §5b).
    pub const OUTPUT_GAMMA: usize = 0x1c8;
    /// `fGammaCorrectEx` (`f32`, 1.0 from the ctor).
    pub const OUTPUT_GAMMA_EX: usize = 0x1cc;
    /// HDR output in use this frame (`u8`, copied from `+0x219` at the
    /// kick, `0x14229342e`): the encode is `…PQSingle` (`0x142297d78`).
    pub const OUTPUT_PQ: usize = 0x21a;
}

/// `sCamera` (`[0x1451c4400]`, DTI size `0xd80`; `ReJitterCamera.md` §2.1).
pub mod camera {
    /// Object size.
    pub const SIZE: usize = 0xd80;
    /// Frame stamp of the last `setJitter` (`sRender+0x1f208`).
    pub const JITTER_STAMP: usize = 0x40;
    /// Blend `t`: when `> 0` the engine lerps view/proj with the previous
    /// frame's (`0x14228f43e`).
    pub const BLEND: usize = 0x44;
    /// The first of the viewports.
    pub const VIEWPORTS: usize = 0x50;
    /// Number of viewports.
    pub const VIEWPORT_COUNT: usize = 8;
}

/// `sCamera::Viewport` (DTI size `0x1a0`; `ReJitterCamera.md` §2.1, §2.7).
pub mod viewport {
    /// Size, and the stride between viewports.
    pub const SIZE: usize = 0x1a0;
    /// `cCamera*`.
    pub const CAMERA: usize = 0x08;
    /// Own render target (render-to-texture); 0 for the screen.
    pub const TARGET: usize = 0x18;
    /// Active byte; the camera setup returns at once when 0.
    pub const ACTIVE: usize = 0x20;
    /// Kind: split-screen part 0..9, `0xa` = the global rect.
    pub const KIND: usize = 0x24;
    /// Pass the viewport is drawn in (0 main, 1 sub display).
    pub const PASS: usize = 0x25;
    /// Rect `(left, top, right, bottom)` as four `i32`; the jitter divisor.
    pub const RECT: usize = 0x28;
    /// View matrix (row-major, row vectors), unjittered.
    pub const VIEW: usize = 0xa0;
    /// Projection, unjittered, standard depth.
    pub const PROJ: usize = 0xe0;
    /// Last frame's view (the engine's copy).
    pub const PREV_VIEW: usize = 0x120;
    /// Last frame's projection.
    pub const PREV_PROJ: usize = 0x160;
}

/// The DX12 device object (`[sRender+0x78]`, size `0x42720`;
/// `ReFramePresent.md` §2.2, `ReDlss1Path.md` §2.2).
pub mod device {
    /// Object size.
    pub const SIZE: usize = 0x42720;
    /// Set after every `Present` (`0x1423aa1ed`): the wait helper waits
    /// only once something was presented.
    pub const PRESENTED: usize = 0x10d9;
    /// The wait helper's latch: 1 = never wait on the frame-latency
    /// waitable again. Only the ctor (0) and the helper's 1 s timeout (1)
    /// write it.
    pub const WAIT_LATCH: usize = 0x10da;
    /// `GetFrameLatencyWaitableObject` handle.
    pub const WAITABLE: usize = 0x10f0;
    /// The game's DLSS object (`0x48` bytes).
    pub const DLSS_CONTEXT: usize = 0x42700;
    /// "A DLSS feature exists" (`ctx+0x38 || ctx+0x40`).
    pub const DLSS_READY: usize = 0x42718;
}

/// The game's DLSS object (`device+0x42700`; `ReDlss1Path.md` §2.3).
pub mod dlss_context {
    /// Size.
    pub const SIZE: usize = 0x48;
    /// Supersampling feature handle (the ISP handle at `+0x40` is
    /// anchored).
    pub const SUPERSAMPLING: usize = 0x38;
}

/// `uTemporalAAFilter2` (`ReDlss1Path.md` §2.4, `RePostProcess.md` §2.3).
/// Colour `+0x150`, history ring `+0x168` and dense velocity `+0x1a8` are
/// anchored.
pub mod taa {
    /// Extent read (through the dense velocity pointer).
    pub const SIZE: usize = 0x1b0;
    /// `mBlendRate` (0.95 by default).
    pub const BLEND_RATE: usize = 0x180;
    /// `mJitterEnable`.
    pub const JITTER_ENABLE: usize = 0x184;
    /// History invalid: set by a history rebuild or a scale change, cleared
    /// by the next draw. The game's only reset signal.
    pub const HISTORY_INVALID: usize = 0x185;
    /// `mSharpen` (1 by default): the post-resolve `TemporalAA2_Sharpen`
    /// pass, cleared by the engine only with the supersampling flag.
    pub const SHARPEN: usize = 0x187;
    /// `mSharpenAmount` (0.2 by default).
    pub const SHARPEN_AMOUNT: usize = 0x198;
    /// History slots in the ring.
    pub const HISTORY_SLOTS: usize = 3;
}

/// The 64-byte evaluate descriptor (`ReDlss1Path.md` §2.4). The rect
/// (`+0x2c`) and ISP output (`+0x18`) are anchored.
pub mod descriptor {
    /// Extent.
    pub const SIZE: usize = 0x40;
    /// Colour texture object.
    pub const COLOUR: usize = 0x08;
    /// Motion vector texture object (SS: `taa+0x1a8`).
    pub const MOTION: usize = 0x10;
    /// Output texture object.
    pub const OUTPUT: usize = 0x18;
    /// Depth texture object (SS: the R32F depth copy).
    pub const DEPTH: usize = 0x20;
    /// Blend rate, 0 when the history is invalid: `Reset = blend ≤ 0`.
    pub const BLEND: usize = 0x28;
    /// Sharpness (ISP: driver + user).
    pub const SHARPNESS: usize = 0x3c;
}

/// The capture the SS enqueue receives (`ReDlss1Path.md` §5).
pub mod capture {
    /// Extent.
    pub const SIZE: usize = 0x40;
    /// `uTemporalAAFilter2*`.
    pub const TAA: usize = 0x08;
    /// `&view` (the scene view).
    pub const VIEW: usize = 0x30;
    /// `&reset`: the TAA draw's copy of `taa+0x185` before it clears it.
    pub const RESET: usize = 0x38;
}

/// The engine's texture object (`ReDlss1Path.md` §2.5).
pub mod texture {
    /// Native shader-resource view object.
    pub const NATIVE: usize = 0x60;
    /// Native depth view object (type-6 textures only; null for the R32F
    /// depth copy, which is what crashes the SS path's barrier planner).
    pub const DEPTH_NATIVE: usize = 0xf0;
    /// Extent.
    pub const SIZE: usize = 0xf8;
}

/// The native view object behind a texture.
pub mod native {
    /// Reference count (`lock inc dword [+8]`, `0x142553fe0`).
    pub const REFCOUNT: usize = 0x08;
    /// The `ID3D12Resource*`.
    pub const RESOURCE: usize = 0x20;
    /// Extent.
    pub const SIZE: usize = 0x28;
}

/// The scene view (`ReDlss1Path.md` §2.5, `0x14255ce10`).
pub mod view {
    /// The view's targets object.
    pub const TARGETS: usize = 0x628;
    /// In the targets object: the R32F depth copy texture.
    pub const DEPTH_COPY: usize = 0xa0;
}

/// Render settings (`[0x1451c4368]`; `RePostProcess.md`).
pub mod render_settings {
    /// Motion blur switch (`0x14043ed6c`), unanchored.
    pub const MOTION_BLUR: usize = 0xe9ec;
    /// The anti-aliasing word value "TAA only": `+0xe9e4 = 1` (TAA),
    /// `+0xe9e5 = 0` (FXAA).
    pub const TAA_ONLY: u16 = 0x0001;
    /// Sampler bank selecting the rebuilt bank 1.
    pub const BANK_ONE: u32 = 1;
}
