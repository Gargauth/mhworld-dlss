//! Typed views of the engine's objects, with the fields the mod uses named
//! (reflection names where the RE recovered them).
//!
//! Every accessor returns `Option`/`bool` and never panics on bad memory;
//! offsets are the gate's derived ones ([`crate::sites::field`]) or the
//! documented unanchored ones ([`crate::layout`]).

use mhw_core::frame::Size;
use mhw_core::math::Vec2;

use crate::gate::Addresses;
use crate::layout::{
    camera, descriptor, device, dlss_context, native, render_config, taa, texture,
};
use crate::object::{slot_value, Object, Validated};
use crate::sites::{field, global};
use crate::viewport::ViewportSnapshot;

static RENDER_CONFIG: Validated = Validated::new();
static CAMERA: Validated = Validated::new();
static RENDER_SETTINGS: Validated = Validated::new();
static DEVICE: Validated = Validated::new();

/// Extent of the render settings object read: past its last field used.
const RENDER_SETTINGS_SIZE: usize = 0xea00;

fn singleton(addresses: &Addresses, slot: usize, len: usize, cache: &Validated) -> Option<Object> {
    cache.object(slot_value(addresses.global(slot))?, len)
}

/// `sRender`: the render configuration (`[0x1451c4480]`).
#[derive(Clone, Copy, Debug)]
pub struct RenderConfig {
    object: Object,
    at: &'static Addresses,
}

impl RenderConfig {
    /// The live object, once the engine has made it.
    pub fn get(at: &'static Addresses) -> Option<RenderConfig> {
        let object = singleton(
            at,
            global::RENDER_CONFIG,
            render_config::SIZE,
            &RENDER_CONFIG,
        )?;
        Some(RenderConfig { object, at })
    }

    /// Its address.
    pub fn address(&self) -> usize {
        self.object.base()
    }

    fn f(&self, index: usize) -> usize {
        self.at.field(index)
    }

    /// The device (`+0x78`).
    pub fn device(&self) -> Option<Device> {
        let pointer = self.object.get::<usize>(self.f(field::CFG_78))?;
        Some(Device {
            object: DEVICE.object(pointer, device::SIZE)?,
        })
    }

    /// `mScreenSize` (`+0x198`): the display size, the viewport rect.
    pub fn screen_size(&self) -> Option<Size> {
        let at = self.f(field::CFG_198);
        let w = self.object.get::<u32>(at)?;
        let h = self
            .object
            .get::<u32>(at + render_config::SCREEN_HEIGHT_AFTER_WIDTH)?;
        Some(Size::new(w, h))
    }

    /// `mBackBufferSize` (`+0x1f448`).
    pub fn back_buffer_size(&self) -> Option<Size> {
        let at = self.f(field::CFG_1F448);
        let w = self.object.get::<u32>(at)?;
        let h = self
            .object
            .get::<u32>(at + render_config::BACK_BUFFER_HEIGHT_AFTER_WIDTH)?;
        Some(Size::new(w, h))
    }

    /// `mUseDLSS` (`+0xa5`, constant 1).
    pub fn use_dlss(&self) -> Option<bool> {
        self.object.get::<u8>(self.f(field::CFG_A5)).map(|b| b != 0)
    }

    /// `mUseDLISP` (`+0xa6`, constant 1).
    pub fn use_dlisp(&self) -> Option<bool> {
        self.object.get::<u8>(self.f(field::CFG_A6)).map(|b| b != 0)
    }

    /// `mDLPerfQuality` (`+0xa8`): the mode the game asks DLSS 1 for.
    pub fn dl_perf_quality(&self) -> Option<u32> {
        self.object.get(self.f(field::CFG_A8))
    }

    /// The flags of the last optimal-settings answer (`+0xb4`).
    pub fn optimal_flags(&self) -> Option<u32> {
        self.object.get(self.f(field::CFG_B4))
    }

    /// `mUseCAS` (`+0xbc`).
    pub fn use_cas(&self) -> Option<bool> {
        self.object.get::<u8>(self.f(field::CFG_BC)).map(|b| b != 0)
    }

    /// `mCASUserSharpness` (`+0xc0`).
    pub fn cas_sharpness(&self) -> Option<f32> {
        self.object.get(self.f(field::CFG_C0))
    }

    /// The engine's own scale (`+0x1f0`, set by Image Quality: 0.75 / 0.9
    /// / 1.0).
    pub fn own_scale(&self) -> Option<f32> {
        self.object.get(self.f(field::CFG_1F0))
    }

    /// Write the engine's own scale.
    pub fn set_own_scale(&self, value: f32) -> bool {
        self.object.put(self.f(field::CFG_1F0), value)
    }

    /// `ContentScalePF` (`+0x1f8`): last frame's content scale.
    pub fn content_scale_pf(&self) -> Option<f32> {
        self.object.get(self.f(field::CFG_1F8))
    }

    /// The scale changed this frame (`+0x1fc`; makes the TAA history
    /// invalid).
    pub fn scale_changed(&self) -> Option<bool> {
        self.object
            .get::<u8>(self.f(field::CFG_1FC))
            .map(|b| b != 0)
    }

    /// The render ratio asked for (`+0x200`).
    pub fn live_ratio(&self) -> Option<f32> {
        self.object.get(self.f(field::CFG_200))
    }

    /// Write the render ratio directly (1.0 only; anything else goes
    /// through the engine's setter).
    pub fn set_live_ratio(&self, value: f32) -> bool {
        self.object.put(self.f(field::CFG_200), value)
    }

    /// The ratio the targets are built for (`+0x204`); a difference from
    /// `+0x200` rebuilds them next frame.
    pub fn built_ratio(&self) -> Option<f32> {
        self.object.get(self.f(field::CFG_204))
    }

    /// `own scale × ratio`, clamped (`+0x208`).
    pub fn product(&self) -> Option<f32> {
        self.object.get(self.f(field::CFG_208))
    }

    /// The render frame counter (`+0x1f208`).
    pub fn render_frame(&self) -> Option<u32> {
        self.object.get(render_config::RENDER_FRAME)
    }

    /// The output gamma (`+0x1c8`); the final encode divides by it.
    pub fn output_gamma(&self) -> Option<f32> {
        self.object.get(render_config::OUTPUT_GAMMA)
    }

    /// `fGammaCorrectEx` of the final encode (`+0x1cc`).
    pub fn output_gamma_ex(&self) -> Option<f32> {
        self.object.get(render_config::OUTPUT_GAMMA_EX)
    }

    /// Whether this frame is encoded for an HDR (PQ) swapchain (`+0x21a`).
    pub fn output_pq(&self) -> Option<bool> {
        self.object
            .get::<u8>(render_config::OUTPUT_PQ)
            .map(|b| b != 0)
    }

    /// `mpPrimaryScene` (`+0x1f450`).
    pub fn primary_scene(&self) -> Option<usize> {
        self.object.get(self.f(field::CFG_1F450))
    }

    /// The dynamic-resolution switch (`+0x7b3b0`).
    pub fn dynamic_resolution(&self) -> Option<u8> {
        self.object.get(self.f(field::CFG_7B3B0))
    }

    /// Write the dynamic-resolution switch.
    pub fn set_dynamic_resolution(&self, value: u8) -> bool {
        self.object.put(self.f(field::CFG_7B3B0), value)
    }

    /// `mDynamicResolutionMode` (`+0x7b3b4`).
    pub fn dynamic_resolution_mode(&self) -> Option<u32> {
        self.object.get(self.f(field::CFG_7B3B4))
    }

    /// The object, for the engine setter call.
    pub(crate) fn base(&self) -> usize {
        self.object.base()
    }
}

/// `sCamera` (`[0x1451c4400]`).
#[derive(Clone, Copy, Debug)]
pub struct Camera {
    object: Object,
    at: &'static Addresses,
}

impl Camera {
    /// The live object.
    pub fn get(at: &'static Addresses) -> Option<Camera> {
        let object = singleton(at, global::CAMERA, camera::SIZE, &CAMERA)?;
        Some(Camera { object, at })
    }

    /// Its address.
    pub fn address(&self) -> usize {
        self.object.base()
    }

    /// The jitter in engine units (`+0x38/+0x3c`).
    pub fn jitter(&self) -> Option<Vec2> {
        let x = self.object.get(self.at.field(field::CAM_38))?;
        let y = self.object.get(self.at.field(field::CAM_3C))?;
        Some(Vec2::new(x, y))
    }

    /// Write the jitter in engine units.
    pub fn set_jitter(&self, units: Vec2) -> bool {
        self.object.put(self.at.field(field::CAM_38), units.x)
            && self.object.put(self.at.field(field::CAM_3C), units.y)
    }

    /// Frame stamp of the engine's last `setJitter` (`+0x40`).
    pub fn jitter_stamp(&self) -> Option<u32> {
        self.object.get(camera::JITTER_STAMP)
    }

    /// Blend `t` (`+0x44`).
    pub fn blend(&self) -> Option<f32> {
        self.object.get(camera::BLEND)
    }

    /// The global rect (`+0xd50..`): `(left, top, right, bottom)`.
    pub fn rect(&self) -> Option<[i32; 4]> {
        let at = self.at.field(field::CAM_D50);
        Some([
            self.object.get(at)?,
            self.object.get(at + 4)?,
            self.object.get(at + 8)?,
            self.object.get(at + 12)?,
        ])
    }

    /// Which of the eight viewports `address` is.
    pub fn viewport_index(&self, address: usize) -> Option<usize> {
        let first = self.object.base() + camera::VIEWPORTS;
        let offset = address.checked_sub(first)?;
        let index = offset / crate::layout::viewport::SIZE;
        (offset % crate::layout::viewport::SIZE == 0 && index < camera::VIEWPORT_COUNT)
            .then_some(index)
    }

    /// Viewport `index`, read in one piece.
    pub fn viewport(&self, index: usize) -> Option<ViewportSnapshot> {
        if index >= camera::VIEWPORT_COUNT {
            return None;
        }
        let mut bytes = [0u8; crate::layout::viewport::SIZE];
        let at = camera::VIEWPORTS + index * crate::layout::viewport::SIZE;
        self.object
            .copy(at, &mut bytes)
            .then(|| ViewportSnapshot::parse(&bytes))
            .flatten()
    }
}

/// A viewport anywhere (the camera setup's argument), read in one piece.
pub fn viewport_at(address: usize) -> Option<ViewportSnapshot> {
    let object = Object::readable(address, crate::layout::viewport::SIZE)?;
    let mut bytes = [0u8; crate::layout::viewport::SIZE];
    object
        .copy(0, &mut bytes)
        .then(|| ViewportSnapshot::parse(&bytes))
        .flatten()
}

/// The active tonemap/luminance filter (`[0x1451cbc08]`, guarded by the
/// critical section at `0x1451cbc48`; `docs/re/ReVolumetrics.md` §3).
/// Read and written only through [`Tonemap::with`], which holds that lock:
/// the filter's destructor clears the slot under it.
#[derive(Clone, Copy, Debug)]
pub struct Tonemap {
    object: Object,
    at: &'static Addresses,
}

/// Extent of the tonemap filter read: past its last field used.
const TONEMAP_SIZE: usize = 0x240;

impl Tonemap {
    /// Run `f` on the live filter with its lock held; `None` when there is
    /// none (between areas, before the first frame).
    pub fn with<R>(at: &'static Addresses, f: impl FnOnce(&Tonemap) -> R) -> Option<R> {
        // SAFETY: a gate-verified global: the game's initialised critical
        // section, in the image's data for the life of the process.
        let _lock = unsafe { mhw_win::ForeignLock::enter(at.global(global::TONEMAP_LOCK)) };
        let object = Object::writable(slot_value(at.global(global::TONEMAP))?, TONEMAP_SIZE)?;
        Some(f(&Tonemap { object, at }))
    }

    /// Its address.
    pub fn address(&self) -> usize {
        self.object.base()
    }

    /// `mDownsampleVolume` (`+0x210`): the volumetric fog composited from a
    /// half-resolution, blurred copy (`true`, the game's default) or from
    /// the full-resolution upsample.
    pub fn downsample_volume(&self) -> Option<bool> {
        self.object
            .get::<u8>(self.at.field(field::TONEMAP_210))
            .map(|b| b != 0)
    }

    /// Write `mDownsampleVolume`.
    pub fn set_downsample_volume(&self, on: bool) -> bool {
        self.object
            .put(self.at.field(field::TONEMAP_210), u8::from(on))
    }
}

/// Render settings (`[0x1451c4368]`).
#[derive(Clone, Copy, Debug)]
pub struct RenderSettings {
    object: Object,
    at: &'static Addresses,
}

impl RenderSettings {
    /// The live object.
    pub fn get(at: &'static Addresses) -> Option<RenderSettings> {
        let object = singleton(
            at,
            global::RENDER_SETTINGS,
            RENDER_SETTINGS_SIZE,
            &RENDER_SETTINGS,
        )?;
        Some(RenderSettings { object, at })
    }

    /// Its address.
    pub fn address(&self) -> usize {
        self.object.base()
    }

    /// `mSamplerLodBias` (`+0x1a8`): the sampler bank materials bake.
    pub fn sampler_bank(&self) -> Option<u32> {
        self.object.get(self.at.field(field::RS_1A8))
    }

    /// Select a sampler bank.
    pub fn set_sampler_bank(&self, bank: u32) -> bool {
        self.object.put(self.at.field(field::RS_1A8), bank)
    }

    /// `mTextureTargetMode` (`+0x260`).
    pub fn texture_target_mode(&self) -> Option<u32> {
        self.object.get(self.at.field(field::RS_260))
    }

    /// The anti-aliasing word (`+0xe9e4`): low byte TAA, high byte FXAA.
    pub fn anti_aliasing(&self) -> Option<u16> {
        self.object.get(self.at.field(field::RS_E9E4))
    }

    /// Write the anti-aliasing word.
    pub fn set_anti_aliasing(&self, word: u16) -> bool {
        self.object.put(self.at.field(field::RS_E9E4), word)
    }

    /// The FXAA switch alone (`+0xe9e5`).
    pub fn fxaa(&self) -> Option<bool> {
        self.object
            .get::<u8>(self.at.field(field::RS_E9E5))
            .map(|b| b != 0)
    }

    /// The four dither switches: `mSSLRDitherResolve` (`+0xb43f`),
    /// `mSSLRGBufferJitter` (`+0xb440`), `mContactShadowEnableNoise`
    /// (`+0xb500`), `mFacialContactShadowEnableNoise` (`+0xb501`).
    pub fn dithers(&self) -> Option<[u8; 4]> {
        let [a, b, c, d] = self.dither_offsets();
        Some([
            self.object.get(a)?,
            self.object.get(b)?,
            self.object.get(c)?,
            self.object.get(d)?,
        ])
    }

    /// Write the four dither switches.
    pub fn set_dithers(&self, values: [u8; 4]) -> bool {
        self.dither_offsets()
            .iter()
            .zip(values)
            .all(|(at, value)| self.object.put(*at, value))
    }

    fn dither_offsets(&self) -> [usize; 4] {
        [
            self.at.field(field::RS_B43F),
            self.at.field(field::RS_B440),
            self.at.field(field::RS_B500),
            self.at.field(field::RS_B501),
        ]
    }

    /// `mCheckerboardAlphaUnrollNear/Far` (`+0xe964/+0xe968`; nothing in
    /// the engine reads them).
    pub fn checkerboard_unroll(&self) -> Option<(f32, f32)> {
        Some((
            self.object.get(self.at.field(field::RS_E964))?,
            self.object.get(self.at.field(field::RS_E968))?,
        ))
    }

    /// The motion-blur switch (`+0xe9ec`).
    pub fn motion_blur(&self) -> Option<bool> {
        self.object
            .get::<u8>(crate::layout::render_settings::MOTION_BLUR)
            .map(|b| b != 0)
    }
}

/// `uTemporalAAFilter2`, as the SS enqueue hands it over.
#[derive(Clone, Copy, Debug)]
pub struct TaaFilter {
    object: Object,
    at: &'static Addresses,
}

impl TaaFilter {
    /// The filter at `address`, checked now.
    pub fn at(at: &'static Addresses, address: usize) -> Option<TaaFilter> {
        let len = taa::SIZE.max(at.field(field::TAA_1A8) + 8);
        Some(TaaFilter {
            object: Object::writable(address, len)?,
            at,
        })
    }

    /// Its address.
    pub fn address(&self) -> usize {
        self.object.base()
    }

    /// Colour texture object (`+0x150`).
    pub fn colour(&self) -> Option<usize> {
        self.object.get(self.at.field(field::TAA_150))
    }

    /// History ring slot `index` (`+0x168 + 8·index`).
    pub fn history(&self, index: usize) -> Option<usize> {
        (index < taa::HISTORY_SLOTS)
            .then(|| self.object.get(self.at.field(field::TAA_168) + 8 * index))
            .flatten()
    }

    /// Dense velocity texture object (`+0x1a8`; SS mode only).
    pub fn dense_velocity(&self) -> Option<usize> {
        self.object.get(self.at.field(field::TAA_1A8))
    }

    /// `mBlendRate` (`+0x180`).
    pub fn blend_rate(&self) -> Option<f32> {
        self.object.get(taa::BLEND_RATE)
    }

    /// `mJitterEnable` (`+0x184`).
    pub fn jitter_enable(&self) -> Option<bool> {
        self.object.get::<u8>(taa::JITTER_ENABLE).map(|b| b != 0)
    }

    /// History invalid (`+0x185`).
    pub fn history_invalid(&self) -> Option<bool> {
        self.object.get::<u8>(taa::HISTORY_INVALID).map(|b| b != 0)
    }

    /// `mSharpen` (`+0x187`).
    pub fn sharpen(&self) -> Option<bool> {
        self.object.get::<u8>(taa::SHARPEN).map(|b| b != 0)
    }

    /// Write `mSharpen`.
    pub fn set_sharpen(&self, on: bool) -> bool {
        self.object.put(taa::SHARPEN, u8::from(on))
    }

    /// `mSharpenAmount` (`+0x198`).
    pub fn sharpen_amount(&self) -> Option<f32> {
        self.object.get(taa::SHARPEN_AMOUNT)
    }
}

/// The DX12 device object (`[sRender+0x78]`).
#[derive(Clone, Copy, Debug)]
pub struct Device {
    object: Object,
}

impl Device {
    /// Its address.
    pub fn address(&self) -> usize {
        self.object.base()
    }

    /// Something was presented (`+0x10d9`).
    pub fn presented(&self) -> Option<bool> {
        self.object.get::<u8>(device::PRESENTED).map(|b| b != 0)
    }

    /// The wait helper's latch (`+0x10da`).
    pub fn wait_latch(&self) -> Option<bool> {
        self.object.get::<u8>(device::WAIT_LATCH).map(|b| b != 0)
    }

    /// Write the latch.
    pub fn set_wait_latch(&self, on: bool) -> bool {
        self.object.put(device::WAIT_LATCH, u8::from(on))
    }

    /// The frame-latency waitable handle (`+0x10f0`).
    pub fn waitable(&self) -> Option<usize> {
        self.object.get(device::WAITABLE)
    }

    /// The game's DLSS object (`+0x42700`).
    pub fn dlss_context(&self) -> Option<usize> {
        self.object.get(device::DLSS_CONTEXT)
    }

    /// "A DLSS feature exists" (`+0x42718`).
    pub fn dlss_ready(&self) -> Option<bool> {
        self.object.get::<u8>(device::DLSS_READY).map(|b| b != 0)
    }
}

/// The game's DLSS object (`device+0x42700`).
#[derive(Clone, Copy, Debug)]
pub(crate) struct DlssContext {
    object: Object,
    at: &'static Addresses,
}

impl DlssContext {
    pub(crate) fn at(at: &'static Addresses, address: usize) -> Option<DlssContext> {
        let len = dlss_context::SIZE.max(at.field(field::DLSS_CTX_40) + 8);
        Some(DlssContext {
            object: Object::writable(address, len)?,
            at,
        })
    }

    /// Store the two feature handles.
    pub(crate) fn set_handles(&self, supersampling: usize, image_signal: usize) -> bool {
        self.object.put(dlss_context::SUPERSAMPLING, supersampling)
            && self
                .object
                .put(self.at.field(field::DLSS_CTX_40), image_signal)
    }
}

/// The evaluate descriptor.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Descriptor {
    object: Object,
}

impl Descriptor {
    pub(crate) fn at(address: usize, at: &'static Addresses) -> Option<Descriptor> {
        let len = descriptor::SIZE
            .max(at.field(field::SS_DESC_2C) + 16)
            .max(at.field(field::ISP_DESC_2C) + 16);
        Some(Descriptor {
            object: Object::readable(address, len)?,
        })
    }

    pub(crate) fn pointer(&self, offset: usize) -> usize {
        self.object.get(offset).unwrap_or(0)
    }

    pub(crate) fn float(&self, offset: usize) -> Option<f32> {
        self.object.get(offset)
    }

    pub(crate) fn rect(&self, offset: usize) -> Option<[f32; 4]> {
        Some([
            self.object.get(offset)?,
            self.object.get(offset + 4)?,
            self.object.get(offset + 8)?,
            self.object.get(offset + 12)?,
        ])
    }
}

/// The `ID3D12Resource*` behind an engine texture object
/// (`[[texture+0x60]+0x20]`), or null.
pub fn resource_of(texture_object: usize) -> *mut std::ffi::c_void {
    Object::readable(texture_object, texture::NATIVE + 8)
        .and_then(|t| t.follow(texture::NATIVE, native::SIZE, false))
        .and_then(|n| n.get::<usize>(native::RESOURCE))
        .unwrap_or(0) as *mut std::ffi::c_void
}
