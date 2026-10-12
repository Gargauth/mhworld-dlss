//! The RCAS pass (Windows side of [`crate::sharpen`]).
//!
//! [`intermediate`] hands out the texture DLSS writes instead of the
//! engine's target while sharpening is on: one per (size, format), a few
//! kept (a loading screen resolves at another size than the game, and
//! rebuilding at every switch was what the old mod did), replaced ones
//! released only [`RETIRE_PRESENTS`] presents later. [`record`] reads it and
//! writes the engine's target on the engine's list, from and back to the
//! states the caller states, with the list's compute state restored.

use std::ffi::c_void;
use std::sync::LazyLock;

use mhw_core::frame::Size;
use parking_lot::Mutex;
use windows::core::{w, Interface};
use windows::Win32::Graphics::Direct3D12::*;

use super::gpu::{self, Explicit};
use super::queue::engine_queue;
use super::record::record_on;
use super::registry;
use crate::sharpen::{writable, RcasConstants, GROUP};
use crate::states;

/// Intermediates kept (game size, loading-screen size, one spare).
const KEPT: usize = 3;
/// Presents a replaced intermediate is kept: lists that wrote it may not
/// have executed yet (the GPU runs at most two or three frames behind).
pub const RETIRE_PRESENTS: u64 = 8;
/// Descriptor slots cycled through (SRV + UAV each).
const RING: usize = 16;
/// The intermediate's state between uses (DLSS writes it as a UAV).
pub const INTERMEDIATE_STATE: u32 = states::UNORDERED_ACCESS;

/// Why the pass did not run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SharpenError {
    /// Sharpness is 0 (or not a number): nothing to do.
    Off,
    /// The engine's queue (and so its device) is not known yet.
    NotReady,
    /// The target cannot be written by the pass (size or format).
    Unsupported(String),
    /// The pass's objects could not be made (shader, D3D12 text).
    Build(String),
}

impl std::fmt::Display for SharpenError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SharpenError::Off => f.write_str("sharpening off"),
            SharpenError::NotReady => f.write_str("no device yet"),
            SharpenError::Unsupported(why) => write!(f, "not sharpened: {why}"),
            SharpenError::Build(why) => write!(f, "sharpening pass not built: {why}"),
        }
    }
}

/// A texture of this crate for DLSS to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Intermediate {
    /// `ID3D12Resource*`: alive while it is handed out by [`intermediate`]
    /// and [`RETIRE_PRESENTS`] presents after it was last.
    pub resource: *mut c_void,
    /// Its state between uses ([`INTERMEDIATE_STATE`]); leave it so.
    pub state: u32,
    /// Size.
    pub size: Size,
    /// `DXGI_FORMAT`.
    pub format: u32,
}

struct Owned {
    texture: ID3D12Resource,
    size: Size,
    format: u32,
    used: u64,
}

// SAFETY: D3D12 resources are free-threaded; held under a mutex.
unsafe impl Send for Owned {}

static INTERMEDIATES: Mutex<Vec<Owned>> = Mutex::new(Vec::new());
static RETIRED: Mutex<Vec<(u64, Owned)>> = Mutex::new(Vec::new());
static FAILED_FOR: Mutex<Option<(Size, u32)>> = Mutex::new(None);

/// The texture DLSS should write for a target of `size` and `format`
/// (made at first use; the engine's device is taken from its queue).
pub fn intermediate(size: Size, format: u32) -> Result<Intermediate, SharpenError> {
    if size.is_empty() || !writable(format) {
        return Err(SharpenError::Unsupported(format!(
            "{}x{} format {format}",
            size.w, size.h
        )));
    }
    let now = registry::presents();
    {
        let mut retired = RETIRED.lock();
        retired.retain(|(at, _)| now.saturating_sub(*at) <= RETIRE_PRESENTS);
    }
    let mut kept = INTERMEDIATES.lock();
    if let Some(owned) = kept
        .iter_mut()
        .find(|o| o.size == size && o.format == format)
    {
        owned.used = now;
        return Ok(handed(owned));
    }
    if *FAILED_FOR.lock() == Some((size, format)) {
        return Err(SharpenError::Build("failed before for this size".into()));
    }
    let queue = engine_queue().ok_or(SharpenError::NotReady)?;
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: a live queue; an out pointer to a local.
    unsafe { queue.GetDevice(&mut device) }
        .map_err(|e| SharpenError::Build(format!("GetDevice: {e}")))?;
    let device = device.ok_or(SharpenError::NotReady)?;
    // SAFETY: a live device.
    let device = unsafe { super::reshade::native_interface(device) };
    let texture = gpu::texture(
        &device,
        size,
        format,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        INTERMEDIATE_STATE,
        w!("mhw-dlss DLSS output (before RCAS)"),
    )
    .map_err(|why| {
        *FAILED_FOR.lock() = Some((size, format));
        SharpenError::Build(why)
    })?;
    if kept.len() >= KEPT {
        // The least recently used goes, later.
        if let Some(oldest) = (0..kept.len()).min_by_key(|&i| kept[i].used) {
            RETIRED.lock().push((now, kept.remove(oldest)));
        }
    }
    log::info!(target: "dlss", "RCAS intermediate {}x{} format {format}", size.w, size.h);
    kept.push(Owned {
        texture,
        size,
        format,
        used: now,
    });
    kept.last().map(handed).ok_or(SharpenError::NotReady)
}

fn handed(owned: &Owned) -> Intermediate {
    Intermediate {
        resource: owned.texture.as_raw(),
        state: INTERMEDIATE_STATE,
        size: owned.size,
        format: owned.format,
    }
}

struct Pass {
    device: usize,
    heap: ID3D12DescriptorHeap,
    stride: usize,
    signature: ID3D12RootSignature,
    pipeline: ID3D12PipelineState,
    next: usize,
}

// SAFETY: D3D12 objects are free-threaded; the pass is used under its mutex.
unsafe impl Send for Pass {}

static PASS: Mutex<Option<Pass>> = Mutex::new(None);
static SHADER: LazyLock<Result<Vec<u8>, String>> = LazyLock::new(|| {
    gpu::compile(
        crate::sharpen::hlsl(),
        b"mhw_rcas\0",
        b"main\0",
        b"cs_5_0\0",
    )
});

fn build(device: &ID3D12Device) -> Result<Pass, String> {
    let code = SHADER.as_ref().map_err(Clone::clone)?;
    let ranges = [
        gpu::range(D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 0, 0, 0),
        gpu::range(D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 0, 0, 1),
    ];
    let signature = gpu::root_signature(
        device,
        &ranges,
        Some((0, 0, RcasConstants::DWORDS)),
        &[],
        D3D12_SHADER_VISIBILITY_ALL,
    )?;
    let pipeline = gpu::compute_pipeline(device, &signature, code)?;
    let (heap, stride) = gpu::heap(
        device,
        D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
        RING * 2,
        true,
    )?;
    log::info!(target: "dlss", "RCAS pass ready");
    Ok(Pass {
        device: device.as_raw() as usize,
        heap,
        stride,
        signature,
        pipeline,
        next: 0,
    })
}

/// Sharpen `input` into `output` on the engine's open `list`. `input` is
/// read (normally the [`intermediate`] DLSS just wrote) and `output` (the
/// engine's target, same size) written; each is in the stated state on
/// `list` and is left in it. Compute state, heaps and pipeline are
/// restored.
///
/// # Safety
/// `list` must be the live, open engine list recorded on this thread;
/// `input` and `output` live resources in the stated states.
pub unsafe fn record(
    list: *mut c_void,
    input: *mut c_void,
    input_state: u32,
    output: *mut c_void,
    output_state: u32,
    sharpness: f32,
) -> Result<(), SharpenError> {
    // SAFETY: caller's contract.
    let (Some(source), Some(target)) = (unsafe { gpu::resource(&input) }, unsafe {
        gpu::resource(&output)
    }) else {
        return Err(SharpenError::Unsupported("null resource".into()));
    };
    // SAFETY: live resources.
    let (from, to) = unsafe { (source.GetDesc(), target.GetDesc()) };
    let size = Size::new(u32::try_from(to.Width).unwrap_or(0), to.Height);
    let constants = RcasConstants::new(sharpness, size);
    if constants.amount <= 0.0 {
        return Err(SharpenError::Off);
    }
    let format = to.Format.0 as u32;
    if !writable(format) || (from.Width, from.Height) != (to.Width, to.Height) || input == output {
        return Err(SharpenError::Unsupported(format!(
            "input {}x{} format {}, output {}x{} format {format}",
            from.Width, from.Height, from.Format.0, to.Width, to.Height
        )));
    }
    // SAFETY: a live resource.
    let device = unsafe { gpu::device_of(target) }.map_err(SharpenError::Build)?;
    let mut guard = PASS.lock();
    if guard
        .as_ref()
        .is_none_or(|pass| pass.device != device.as_raw() as usize)
    {
        *guard = Some(build(&device).map_err(SharpenError::Build)?);
    }
    let pass = guard.as_mut().ok_or(SharpenError::NotReady)?;
    let slot = pass.next;
    pass.next = (pass.next + 1) % RING;
    let mut moves = Explicit::default();
    moves.add(
        input as usize,
        input_state,
        states::NON_PIXEL_SHADER_RESOURCE,
    );
    moves.add(output as usize, output_state, states::UNORDERED_ACCESS);
    let dwords = constants.to_dwords();
    let (groups_x, groups_y) = constants.groups();
    debug_assert_eq!(groups_x, size.w.div_ceil(GROUP));
    // SAFETY: our heap and pipeline; live resources; the open list (the
    // caller's contract), its state restored by `record_on`.
    let recorded = unsafe {
        let cpu = pass.heap.GetCPUDescriptorHandleForHeapStart().ptr + slot * 2 * pass.stride;
        let gpu_base =
            pass.heap.GetGPUDescriptorHandleForHeapStart().ptr + (slot * 2 * pass.stride) as u64;
        // Our own views: not followed by the descriptor tracking.
        let views = super::record::Guard::enter();
        device.CreateShaderResourceView(
            source,
            Some(&gpu::srv(from.Format.0 as u32)),
            D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu },
        );
        device.CreateUnorderedAccessView(
            target,
            None,
            Some(&gpu::uav(format)),
            D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: cpu + pass.stride,
            },
        );
        drop(views);
        record_on(list, None, |engine| {
            moves.forward(engine);
            engine.SetDescriptorHeaps(&[Some(pass.heap.clone())]);
            engine.SetComputeRootSignature(&pass.signature);
            engine.SetPipelineState(&pass.pipeline);
            engine.SetComputeRootDescriptorTable(0, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: gpu_base });
            engine.SetComputeRoot32BitConstants(
                1,
                RcasConstants::DWORDS,
                dwords.as_ptr().cast(),
                0,
            );
            engine.Dispatch(groups_x, groups_y, 1);
            moves.back(engine);
        })
    };
    recorded.ok_or_else(|| SharpenError::Unsupported("not a graphics command list".into()))?;
    mhw_core::log_once!(
        Info,
        "dlss",
        "rcas-first",
        "RCAS sharpening {}x{} format {format} at {:.2}",
        size.w,
        size.h,
        constants.amount
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    /// The shader is compiled at the first sharpened frame in the game;
    /// compiled here so a mistake in it fails the Windows tests instead.
    #[test]
    fn the_shader_compiles() {
        let code = super::SHADER.as_ref();
        assert!(code.is_ok(), "{:?}", code.err());
    }
}
