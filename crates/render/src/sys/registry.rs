//! The process-wide registries the hooks feed, and the small facts they
//! share (resource descriptions, the pass clock, the frame-size floor).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, LazyLock};

use mhw_core::frame::Size;
use parking_lot::Mutex;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{
    ID3D12Device, D3D12_DESCRIPTOR_HEAP_TYPE, D3D12_RESOURCE_DESC,
    D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL,
};

use crate::addrmap::AddressMap;
use crate::depth::DepthTargets;
use crate::descriptors::DescriptorRegistry;
use crate::inputs::InputBoard;
use crate::rootsig::Layout;
use crate::states::StateTracker;
use crate::taa;
use crate::upload::UploadMap;

/// How a resource was created, recorded at creation so a stale address
/// gives a wrong answer instead of a call on freed memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResourceInfo {
    /// `D3D12_RESOURCE_DIMENSION`.
    pub dimension: u32,
    /// Width.
    pub width: u64,
    /// Height.
    pub height: u32,
    /// Mip levels.
    pub mips: u16,
    /// `DXGI_FORMAT`.
    pub format: u32,
    /// Sample count.
    pub samples: u32,
    /// `D3D12_RESOURCE_FLAGS`.
    pub flags: u32,
}

impl ResourceInfo {
    pub(crate) fn of(desc: &D3D12_RESOURCE_DESC) -> ResourceInfo {
        ResourceInfo {
            dimension: desc.Dimension.0 as u32,
            width: desc.Width,
            height: desc.Height,
            mips: desc.MipLevels,
            format: desc.Format.0 as u32,
            samples: desc.SampleDesc.Count,
            flags: desc.Flags.0 as u32,
        }
    }

    /// Width × height, saturated to `u32`.
    pub fn size(&self) -> Size {
        Size::new(u32::try_from(self.width).unwrap_or(u32::MAX), self.height)
    }

    /// Created as a depth-stencil target.
    pub fn is_depth(&self) -> bool {
        self.flags & D3D12_RESOURCE_FLAG_ALLOW_DEPTH_STENCIL.0 as u32 != 0
    }
}

/// Smallest texture whose description is kept.
const DESCRIBED_MIN: u64 = 256;

pub(crate) static DESCS: LazyLock<Mutex<AddressMap<usize, ResourceInfo>>> =
    LazyLock::new(Default::default);
pub(crate) static DESCRIPTORS: LazyLock<DescriptorRegistry> =
    LazyLock::new(DescriptorRegistry::new);
pub(crate) static STATES: LazyLock<Mutex<StateTracker>> = LazyLock::new(Default::default);
pub(crate) static UPLOAD: LazyLock<Mutex<UploadMap>> = LazyLock::new(Default::default);
pub(crate) static DEPTHS: LazyLock<Mutex<DepthTargets>> = LazyLock::new(Default::default);
/// Depth-stencil view CPU handle -> resource: what `OMSetRenderTargets`
/// binds is a handle, and only view creation says what is behind it.
pub(crate) static DSV_HANDLES: LazyLock<Mutex<AddressMap<usize, usize>>> =
    LazyLock::new(Default::default);
/// Render-target view CPU handle -> resource, for the HUD-less detection.
pub(crate) static RTV_HANDLES: LazyLock<Mutex<AddressMap<usize, usize>>> =
    LazyLock::new(Default::default);
pub(crate) static ROOT_SIGNATURES: LazyLock<Mutex<AddressMap<usize, Arc<Layout>>>> =
    LazyLock::new(Default::default);
pub(crate) static BOARD: LazyLock<Mutex<InputBoard>> = LazyLock::new(Default::default);

/// The pass clock: temporal dispatches handled so far. Depth bindings are
/// stamped with it.
pub(crate) static FRAME: AtomicU64 = AtomicU64::new(0);
/// The frame-size floor, packed `w << 32 | h`; starts at a 960×540 display's.
static FLOOR: AtomicU64 = AtomicU64::new((464 << 32) | 254);
/// Presents so far: the clock our own objects are retired by.
static PRESENTS: AtomicU64 = AtomicU64::new(0);
/// Descriptor increments per `D3D12_DESCRIPTOR_HEAP_TYPE`.
static STRIDES: [AtomicUsize; 4] = [const { AtomicUsize::new(0) }; 4];

/// Record a created resource's description (textures of at least 256×256).
pub(crate) fn described(resource: usize, desc: &D3D12_RESOURCE_DESC) {
    if resource == 0 || desc.Width < DESCRIBED_MIN || u64::from(desc.Height) < DESCRIBED_MIN {
        return;
    }
    DESCS.lock().insert(resource, ResourceInfo::of(desc));
}

/// How `resource` was created, if it was created while hooked and is at
/// least 256×256. Never touches the resource.
pub fn describe(resource: usize) -> Option<ResourceInfo> {
    if resource == 0 {
        return None;
    }
    DESCS.lock().get(&resource).copied()
}

/// Tell the crate the engine's display size, so "could be a full-frame
/// target" is judged against it (and the HUD-less scene target must have
/// it). Call when the engine's output size is known or changes (before it
/// builds targets for it).
pub fn note_display(display: Size) {
    let floor = taa::frame_floor(display);
    FLOOR.store(
        (u64::from(floor.w) << 32) | u64::from(floor.h),
        Ordering::Relaxed,
    );
    super::hudless::note_display(display);
}

/// Count a present.
pub(crate) fn tick_presents() {
    PRESENTS.fetch_add(1, Ordering::Relaxed);
}

/// Presents so far.
pub(crate) fn presents() -> u64 {
    PRESENTS.load(Ordering::Relaxed)
}

/// Whether a texture this size could be a full-frame target.
pub(crate) fn frame_sized(width: u64, height: u32) -> bool {
    let floor = FLOOR.load(Ordering::Relaxed);
    taa::reaches(Size::new((floor >> 32) as u32, floor as u32), width, height)
}

/// Whether views of `resource` are worth following, and its serial if so:
/// frame-sized, not depth (depth comes from DSVs, and shadow maps are
/// sampled by thousands of draws), and witnessed.
pub(crate) fn worth_following(resource: usize) -> Option<u64> {
    let info = describe(resource)?;
    if info.is_depth() || !frame_sized(info.width, info.height) {
        return None;
    }
    super::alive::serial_of(resource)
}

/// Descriptor increment for `kind`, asked of `device` once.
///
/// # Safety
/// `device` must be null or a live `ID3D12Device`.
pub(crate) unsafe fn stride(device: *mut c_void, kind: D3D12_DESCRIPTOR_HEAP_TYPE) -> usize {
    let Some(slot) = usize::try_from(kind.0)
        .ok()
        .and_then(|index| STRIDES.get(index))
    else {
        return 0;
    };
    let known = slot.load(Ordering::Relaxed);
    if known != 0 {
        return known;
    }
    // SAFETY: caller guarantees a live device or null.
    let Some(device) = (unsafe { ID3D12Device::from_raw_borrowed(&device) }) else {
        return 0;
    };
    // SAFETY: a live device; the call has no preconditions.
    let stride = unsafe { device.GetDescriptorHandleIncrementSize(kind) } as usize;
    slot.store(stride, Ordering::Relaxed);
    stride
}

/// The cached increment for `kind` (0 until a device was asked).
pub(crate) fn known_stride(kind: D3D12_DESCRIPTOR_HEAP_TYPE) -> usize {
    usize::try_from(kind.0)
        .ok()
        .and_then(|index| STRIDES.get(index))
        .map_or(0, |slot| slot.load(Ordering::Relaxed))
}

/// The pass clock.
pub(crate) fn frame() -> u64 {
    FRAME.load(Ordering::Relaxed)
}
