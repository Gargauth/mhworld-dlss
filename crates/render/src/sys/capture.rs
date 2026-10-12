//! The capture service (dev only; Windows side of [`crate::capture`]).
//!
//! Copies and statistics are recorded on the engine's open list into
//! readback buffers of this crate; when the engine submits that list, the
//! crate's fence is signalled behind it on the same queue
//! ([`super::submit::after_submit`]) and the work's ticket gets that value.
//! The poll functions take what the fence has passed, oldest first, without
//! waiting. Readback buffers are pooled; what may be in flight is bounded,
//! beyond that a request is answered [`CaptureError::NotReady`].

use std::ffi::c_void;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::LazyLock;

use parking_lot::Mutex;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

use super::gpu::{self, Explicit};
use super::record::{barrier, native_list, record_on, Guard};
use super::submit;
use crate::capture::readback::{needs_transition, Inflight};
use crate::capture::{stats, CaptureError, Captured, ColourStats, Item};
use crate::states::{self, Move};

/// Copies in flight at most.
const MAX_COPIES: usize = 64;
/// Bytes of copies in flight at most.
const MAX_BYTES: u64 = 1 << 30;
/// Statistics in flight at most.
const MAX_STATS: usize = 8;
/// Bytes of idle readback buffers kept for reuse at most.
const POOL_BYTES: u64 = 512 << 20;
/// Descriptor slots of the statistics pass (SRV + UAV each).
const RING: usize = 16;

struct Copy {
    request: u64,
    name: String,
    frame: u64,
    format: u32,
    width: u32,
    height: u32,
    row_pitch: u32,
    bytes: u64,
    buffer: ID3D12Resource,
}

struct StatsJob {
    request: u64,
    frame: u64,
    width: u32,
    height: u32,
    bytes: u64,
    readback: ID3D12Resource,
    /// The UAV the shader wrote; kept until the copy from it completed.
    _output: ID3D12Resource,
}

// SAFETY: D3D12 resources are free-threaded; both are held under mutexes.
unsafe impl Send for Copy {}
// SAFETY: as above.
unsafe impl Send for StatsJob {}

static COPIES: LazyLock<Mutex<Inflight<Copy>>> = LazyLock::new(Default::default);
static STATS: LazyLock<Mutex<Inflight<StatsJob>>> = LazyLock::new(Default::default);
static POOL: Mutex<Vec<(u64, ID3D12Resource)>> = Mutex::new(Vec::new());
static TICKETS: AtomicU64 = AtomicU64::new(1);

fn device_error(what: &str) -> impl Fn(String) -> CaptureError + '_ {
    move |why| CaptureError::Device(format!("{what}: {why}"))
}

/// A readback buffer of at least `bytes` (from the pool when one fits).
fn readback(device: &ID3D12Device, bytes: u64) -> Result<ID3D12Resource, CaptureError> {
    {
        let mut pool = POOL.lock();
        if let Some(at) = pool
            .iter()
            .position(|(size, _)| *size >= bytes && *size <= bytes.saturating_mul(2))
        {
            return Ok(pool.swap_remove(at).1);
        }
    }
    gpu::buffer(
        device,
        bytes,
        D3D12_HEAP_TYPE_READBACK,
        D3D12_RESOURCE_FLAG_NONE,
        states::COPY_DEST,
    )
    .map_err(device_error("readback buffer"))
}

fn give_back(buffer: ID3D12Resource) {
    // SAFETY: a live buffer.
    let size = unsafe { buffer.GetDesc() }.Width;
    let mut pool = POOL.lock();
    let held: u64 = pool.iter().map(|(s, _)| *s).sum();
    if held + size <= POOL_BYTES {
        pool.push((size, buffer));
    }
}

/// Read `bytes` from the start of a readback buffer.
fn read(buffer: &ID3D12Resource, bytes: u64) -> Option<Vec<u8>> {
    let len = usize::try_from(bytes).ok()?;
    let mut data: *mut c_void = std::ptr::null_mut();
    // SAFETY: a readback buffer of at least `bytes` whose writes completed
    // (fence passed); mapped, copied out, unmapped with nothing written.
    unsafe {
        buffer
            .Map(
                0,
                Some(&D3D12_RANGE { Begin: 0, End: len }),
                Some(&mut data),
            )
            .ok()?;
        if data.is_null() {
            return None;
        }
        let out = std::slice::from_raw_parts(data.cast::<u8>(), len).to_vec();
        buffer.Unmap(0, Some(&D3D12_RANGE { Begin: 0, End: 0 }));
        Some(out)
    }
}

/// Signal the fence behind `list` and give `ticket` its value.
fn fence_behind(list: usize, ticket: u64, fence: std::sync::Arc<submit::Fence>) {
    submit::after_submit(
        list,
        Box::new(move |queue| {
            if let Some(value) = fence.signal(queue) {
                COPIES.lock().assign(ticket, value);
                STATS.lock().assign(ticket, value);
            }
        }),
    );
}

/// A copy location naming `resource` without owning a reference.
fn location(
    resource: &ID3D12Resource,
    kind: D3D12_TEXTURE_COPY_TYPE,
    anonymous: D3D12_TEXTURE_COPY_LOCATION_0,
) -> D3D12_TEXTURE_COPY_LOCATION {
    D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: one nullable interface pointer, never released through
        // the location (`ManuallyDrop`).
        pResource: ManuallyDrop::new(unsafe { std::mem::transmute_copy(resource) }),
        Type: kind,
        Anonymous: anonymous,
    }
}

/// Plan one item: its readback buffer, layout and copy.
fn prepare(
    device: &ID3D12Device,
    request: u64,
    frame: u64,
    item: &Item,
) -> Result<(Copy, Option<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>), CaptureError> {
    // SAFETY: the caller's live resource (the item's contract).
    let resource = unsafe { gpu::resource(&item.resource) }
        .ok_or_else(|| CaptureError::Unsupported(format!("{}: null resource", item.name)))?;
    // SAFETY: a live resource.
    let desc = unsafe { resource.GetDesc() };
    let (format, width, height, row_pitch, bytes, footprint) = match desc.Dimension {
        D3D12_RESOURCE_DIMENSION_BUFFER => {
            let width = u32::try_from(desc.Width).map_err(|_| {
                CaptureError::Unsupported(format!("{}: buffer over 4 GiB", item.name))
            })?;
            (0, width, 1, width, desc.Width, None)
        }
        D3D12_RESOURCE_DIMENSION_TEXTURE2D if desc.SampleDesc.Count == 1 => {
            let mut footprint = D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default();
            let mut rows = 0u32;
            // SAFETY: out pointers to locals, one subresource.
            unsafe {
                device.GetCopyableFootprints(
                    &desc,
                    0,
                    1,
                    0,
                    Some(&mut footprint),
                    Some(&mut rows),
                    None,
                    None,
                );
            }
            let pitch = footprint.Footprint.RowPitch;
            let width = u32::try_from(desc.Width).unwrap_or(0);
            if pitch == 0 || rows == 0 || width == 0 {
                return Err(CaptureError::Unsupported(format!(
                    "{}: format {} has no copyable layout",
                    item.name, desc.Format.0
                )));
            }
            (
                footprint.Footprint.Format.0 as u32,
                width,
                rows,
                pitch,
                u64::from(pitch) * u64::from(rows),
                Some(footprint),
            )
        }
        other => {
            return Err(CaptureError::Unsupported(format!(
                "{}: dimension {} / {} samples",
                item.name, other.0, desc.SampleDesc.Count
            )))
        }
    };
    Ok((
        Copy {
            request,
            name: item.name.clone(),
            frame,
            format,
            width,
            height,
            row_pitch,
            bytes,
            buffer: readback(device, bytes)?,
        },
        footprint,
    ))
}

/// Records copies of `items` into readback buffers on the open engine
/// `list`, restoring their states.
///
/// # Safety
/// `list` must be the live, open engine list recorded on this thread; each
/// item's resource live and in its stated state on it.
pub unsafe fn record(
    list: *mut c_void,
    request: u64,
    frame: u64,
    items: &[Item],
) -> Result<(), CaptureError> {
    // SAFETY: caller's contract.
    let list = unsafe { native_list(list) }
        .ok_or_else(|| CaptureError::Unsupported("not a graphics command list".into()))?;
    let Some(first) = items.first() else {
        return Ok(());
    };
    // SAFETY: caller's contract.
    let first = unsafe { gpu::resource(&first.resource) }
        .ok_or_else(|| CaptureError::Unsupported(format!("{}: null resource", first.name)))?;
    // SAFETY: a live resource.
    let device = unsafe { gpu::device_of(first) }.map_err(device_error("device"))?;
    let fence = submit::fence(&device).map_err(device_error("fence"))?;
    {
        let inflight = COPIES.lock();
        let bytes: u64 = inflight.iter().map(|c| c.bytes).sum();
        if inflight.len() + items.len() > MAX_COPIES || bytes > MAX_BYTES {
            return Err(CaptureError::NotReady);
        }
    }
    let mut planned = Vec::with_capacity(items.len());
    let mut moves = Explicit::default();
    for item in items {
        planned.push((item, prepare(&device, request, frame, item)?));
        if needs_transition(item.state) {
            moves.add(item.resource as usize, item.state, states::COPY_SOURCE);
        }
    }
    let _guard = Guard::enter();
    // SAFETY: the open list; every resource live (caller's contract) and
    // ours; locations point at locals that outlive each call.
    unsafe {
        moves.forward(&list);
        for (item, (copy, footprint)) in &planned {
            let raw = item.resource;
            let Some(source) = gpu::resource(&raw) else {
                continue;
            };
            match footprint {
                None => list.CopyBufferRegion(&copy.buffer, 0, source, 0, copy.bytes),
                Some(footprint) => {
                    let to = location(
                        &copy.buffer,
                        D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                        D3D12_TEXTURE_COPY_LOCATION_0 {
                            PlacedFootprint: *footprint,
                        },
                    );
                    let from = location(
                        source,
                        D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                        D3D12_TEXTURE_COPY_LOCATION_0 {
                            SubresourceIndex: 0,
                        },
                    );
                    list.CopyTextureRegion(&to, 0, 0, 0, &from, None);
                }
            }
        }
        moves.back(&list);
    }
    let ticket = TICKETS.fetch_add(1, Ordering::Relaxed);
    {
        let mut inflight = COPIES.lock();
        for (_, (copy, _)) in planned {
            inflight.push(ticket, copy);
        }
    }
    fence_behind(list.as_raw() as usize, ticket, fence);
    Ok(())
}

/// Completed copies, oldest first (non-blocking).
pub fn poll() -> Vec<Captured> {
    let Some(fence) = submit::current_fence() else {
        return Vec::new();
    };
    let ready = COPIES.lock().take_ready(fence.completed());
    let mut out = Vec::with_capacity(ready.len());
    for copy in ready {
        match read(&copy.buffer, copy.bytes) {
            Some(bytes) => out.push(Captured {
                request: copy.request,
                name: copy.name,
                format: copy.format,
                width: copy.width,
                height: copy.height,
                row_pitch: copy.row_pitch,
                frame: copy.frame,
                bytes,
            }),
            None => {
                log::warn!(target: "dev", "capture {} ({}): readback map failed", copy.request, copy.name)
            }
        }
        give_back(copy.buffer);
    }
    out
}

struct StatsPass {
    device: usize,
    heap: ID3D12DescriptorHeap,
    stride: usize,
    signature: ID3D12RootSignature,
    clear: ID3D12PipelineState,
    reduce: ID3D12PipelineState,
    next: usize,
}

// SAFETY: D3D12 objects are free-threaded; used under the mutex.
unsafe impl Send for StatsPass {}

static PASS: Mutex<Option<StatsPass>> = Mutex::new(None);

fn build_pass(device: &ID3D12Device) -> Result<StatsPass, String> {
    let clear = gpu::compile(stats::HLSL, b"mhw_stats\0", b"clear\0", b"cs_5_0\0")?;
    let reduce = gpu::compile(stats::HLSL, b"mhw_stats\0", b"reduce\0", b"cs_5_0\0")?;
    let ranges = [
        gpu::range(D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 0, 0, 0),
        gpu::range(D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 0, 0, 1),
    ];
    let signature = gpu::root_signature(
        device,
        &ranges,
        Some((0, 0, 4)),
        &[],
        D3D12_SHADER_VISIBILITY_ALL,
    )?;
    let (heap, stride) = gpu::heap(
        device,
        D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
        RING * 2,
        true,
    )?;
    Ok(StatsPass {
        device: device.as_raw() as usize,
        heap,
        stride,
        clear: gpu::compute_pipeline(device, &signature, &clear)?,
        reduce: gpu::compute_pipeline(device, &signature, &reduce)?,
        signature,
        next: 0,
    })
}

/// GPU reduction over the colour resource `item` (luma percentiles from a
/// histogram, log2 bins 2^-16..2^16), recorded on the open engine `list`
/// with its state restored.
///
/// # Safety
/// As [`record`].
pub unsafe fn record_stats(
    list: *mut c_void,
    request: u64,
    frame: u64,
    item: &Item,
) -> Result<(), CaptureError> {
    // SAFETY: caller's contract.
    let resource = unsafe { gpu::resource(&item.resource) }
        .ok_or_else(|| CaptureError::Unsupported(format!("{}: null resource", item.name)))?;
    // SAFETY: a live resource.
    let desc = unsafe { resource.GetDesc() };
    if desc.Dimension != D3D12_RESOURCE_DIMENSION_TEXTURE2D || desc.SampleDesc.Count != 1 {
        return Err(CaptureError::Unsupported(format!(
            "{}: not a single-sample 2D texture",
            item.name
        )));
    }
    let view = stats::view_format(desc.Format.0 as u32).ok_or_else(|| {
        CaptureError::Unsupported(format!(
            "{}: format {} is not readable colour",
            item.name, desc.Format.0
        ))
    })?;
    let (width, height) = (u32::try_from(desc.Width).unwrap_or(0), desc.Height);
    if STATS.lock().len() >= MAX_STATS {
        return Err(CaptureError::NotReady);
    }
    // SAFETY: a live resource.
    let device = unsafe { gpu::device_of(resource) }.map_err(device_error("device"))?;
    let fence = submit::fence(&device).map_err(device_error("fence"))?;
    let bytes = (stats::buffer_dwords(width, height) * 4) as u64;
    let output = gpu::buffer(
        &device,
        bytes,
        D3D12_HEAP_TYPE_DEFAULT,
        D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
        states::COMMON,
    )
    .map_err(device_error("statistics buffer"))?;
    let readback = readback(&device, bytes)?;

    let mut guard = PASS.lock();
    if guard
        .as_ref()
        .is_none_or(|p| p.device != device.as_raw() as usize)
    {
        *guard = Some(build_pass(&device).map_err(device_error("statistics pass"))?);
    }
    let pass = guard.as_mut().ok_or(CaptureError::NotReady)?;
    let slot = pass.next;
    pass.next = (pass.next + 1) % RING;
    let mut source = Explicit::default();
    if item.state & states::NON_PIXEL_SHADER_RESOURCE == 0 {
        source.add(
            item.resource as usize,
            item.state,
            states::NON_PIXEL_SHADER_RESOURCE,
        );
    }
    let out = output.as_raw() as usize;
    let to_write = barrier(Move {
        resource: out,
        from: states::COMMON,
        to: states::UNORDERED_ACCESS,
    });
    let to_copy = barrier(Move {
        resource: out,
        from: states::UNORDERED_ACCESS,
        to: states::COPY_SOURCE,
    });
    let constants = stats::constants(width, height);
    let (groups_x, groups_y) = stats::groups(width, height);
    let clear_groups = (stats::BINS as u32).div_ceil(stats::CLEAR_GROUP);
    // SAFETY: our heap, pipelines and buffers; the caller's live resource in
    // its stated state; the open list, its state restored by `record_on`.
    let recorded = unsafe {
        let cpu = pass.heap.GetCPUDescriptorHandleForHeapStart().ptr + slot * 2 * pass.stride;
        let table =
            pass.heap.GetGPUDescriptorHandleForHeapStart().ptr + (slot * 2 * pass.stride) as u64;
        {
            let _views = Guard::enter();
            device.CreateShaderResourceView(
                resource,
                Some(&gpu::srv(view)),
                D3D12_CPU_DESCRIPTOR_HANDLE { ptr: cpu },
            );
            device.CreateUnorderedAccessView(
                &output,
                None,
                Some(&gpu::raw_uav(bytes)),
                D3D12_CPU_DESCRIPTOR_HANDLE {
                    ptr: cpu + pass.stride,
                },
            );
        }
        record_on(list, None, |engine| {
            source.forward(engine);
            engine.ResourceBarrier(&[to_write]);
            engine.SetDescriptorHeaps(&[Some(pass.heap.clone())]);
            engine.SetComputeRootSignature(&pass.signature);
            engine.SetComputeRootDescriptorTable(0, D3D12_GPU_DESCRIPTOR_HANDLE { ptr: table });
            engine.SetComputeRoot32BitConstants(1, 4, constants.as_ptr().cast(), 0);
            engine.SetPipelineState(&pass.clear);
            engine.Dispatch(clear_groups, 1, 1);
            engine.ResourceBarrier(&[gpu::uav_barrier(&output)]);
            engine.SetPipelineState(&pass.reduce);
            engine.Dispatch(groups_x, groups_y, 1);
            engine.ResourceBarrier(&[to_copy]);
            engine.CopyBufferRegion(&readback, 0, &output, 0, bytes);
            source.back(engine);
            engine.as_raw() as usize
        })
    };
    drop(guard);
    let native =
        recorded.ok_or_else(|| CaptureError::Unsupported("not a graphics command list".into()))?;
    let ticket = TICKETS.fetch_add(1, Ordering::Relaxed);
    STATS.lock().push(
        ticket,
        StatsJob {
            request,
            frame,
            width,
            height,
            bytes,
            readback,
            _output: output,
        },
    );
    fence_behind(native, ticket, fence);
    Ok(())
}

/// Completed statistics `(request, frame, stats)`, oldest first
/// (non-blocking).
pub fn poll_stats() -> Vec<(u64, u64, ColourStats)> {
    let Some(fence) = submit::current_fence() else {
        return Vec::new();
    };
    let ready = STATS.lock().take_ready(fence.completed());
    let mut out = Vec::with_capacity(ready.len());
    for job in ready {
        let reduced = read(&job.readback, job.bytes).and_then(|bytes| {
            let (words, _) = bytes.as_chunks::<4>();
            let dwords: Vec<u32> = words.iter().map(|b| u32::from_le_bytes(*b)).collect();
            stats::reduce(&dwords, job.width, job.height)
        });
        match reduced {
            Some(result) => out.push((job.request, job.frame, result)),
            None => log::warn!(target: "dev", "statistics {}: readback unusable", job.request),
        }
        give_back(job.readback);
    }
    out
}
