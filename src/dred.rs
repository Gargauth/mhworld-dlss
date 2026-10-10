//! Asking the driver what went wrong.
//!
//! The faults this library can cause are GPU-side: an invalid barrier, a
//! descriptor pointing at a released resource, a shader reading past the end
//! of a texture. None of them fail at the call that records them. The command
//! list closes cleanly, `EvaluateFeature` returns success, and the process
//! dies a few hundred frames later with nothing in the log.
//!
//! Device Removed Extended Data is built for exactly this. Enabled before the
//! device is created, the driver keeps a breadcrumb trail of completed GPU
//! operations and, on a page fault, the offending virtual address together
//! with the allocations that surround it. It needs no debug layer and no
//! Graphics Tools install, so it works on the machine the game actually runs
//! on.

use std::sync::atomic::{AtomicBool, Ordering};
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::*;

static ENABLED: AtomicBool = AtomicBool::new(false);
static REPORTED: AtomicBool = AtomicBool::new(false);

/// Turn on breadcrumbs and page fault reporting.
///
/// This has to happen before the device is created, which is why it is called
/// from the `D3D12CreateDevice` forwarder rather than from the device hooks.
pub fn enable() {
    if ENABLED.swap(true, Ordering::Relaxed) {
        return;
    }
    unsafe {
        let mut settings: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
        match D3D12GetDebugInterface(&mut settings) {
            Ok(()) => match settings {
                Some(settings) => {
                    settings.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                    settings.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                    crate::log!("[dred] breadcrumbs and page fault reporting on");
                }
                None => crate::log!("[dred] no settings interface"),
            },
            Err(error) => crate::log!("[dred] unavailable: {error}"),
        }
    }
}

/// Watch a device and report once it is removed.
///
/// A removed device is terminal: the game cannot recover, so the only thing
/// worth doing is writing down why before the process goes.
pub fn watch(device: ID3D12Device) {
    std::thread::spawn(move || loop {
        std::thread::sleep(std::time::Duration::from_millis(250));
        let removed = unsafe { device.GetDeviceRemovedReason() };
        let Err(reason) = removed else {
            continue;
        };
        if REPORTED.swap(true, Ordering::Relaxed) {
            return;
        }
        let code = reason.code().0;
        crate::log!("[dred] device removed: {code:#010x} {}", describe(code));
        unsafe { report(&device) };
        return;
    });
}

/// A DRED object name, which the driver supplies as ANSI or wide or not at
/// all.
unsafe fn name_of(ansi: *const u8, wide: windows::core::PCWSTR) -> String {
    if !ansi.is_null() {
        let mut end = 0;
        while end < 256 && *ansi.add(end) != 0 {
            end += 1;
        }
        if end > 0 {
            return String::from_utf8_lossy(std::slice::from_raw_parts(ansi, end)).into_owned();
        }
    }
    if !wide.is_null() {
        if let Ok(text) = wide.to_string() {
            if !text.is_empty() {
                return text;
            }
        }
    }
    "unnamed".into()
}

fn describe(reason: i32) -> &'static str {
    match reason as u32 {
        0x887a0006 => "DEVICE_HUNG -- the GPU stopped responding to our own work",
        0x887a0005 => "DEVICE_REMOVED",
        0x887a0007 => "DEVICE_RESET",
        0x887a0020 => "DRIVER_INTERNAL_ERROR",
        0x887a000d => "INVALID_CALL -- an invalid command reached the GPU",
        _ => "unrecognised",
    }
}

unsafe fn report(device: &ID3D12Device) {
    let data: ID3D12DeviceRemovedExtendedData = match device.cast() {
        Ok(data) => data,
        Err(error) => {
            crate::log!("[dred] no extended data: {error}");
            return;
        }
    };

    match data.GetPageFaultAllocationOutput() {
        Ok(fault) => {
            crate::log!("[dred] page fault at {:#x}", fault.PageFaultVA);
            list_allocations("live", fault.pHeadExistingAllocationNode);
            list_allocations("freed", fault.pHeadRecentFreedAllocationNode);
        }
        Err(error) => crate::log!("[dred] no page fault data: {error}"),
    }

    match data.GetAutoBreadcrumbsOutput() {
        Ok(output) => breadcrumbs(output.pHeadAutoBreadcrumbNode),
        Err(error) => crate::log!("[dred] no breadcrumbs: {error}"),
    }
}

/// The allocations around a faulting address.
///
/// A resource in the "freed" list is the answer on its own: it means something
/// handed the GPU a descriptor for a resource that had already been released.
unsafe fn list_allocations(label: &str, mut node: *const D3D12_DRED_ALLOCATION_NODE) {
    let mut seen = 0;
    while !node.is_null() && seen < 24 {
        let entry = &*node;
        crate::log!(
            "[dred]   {label} allocation: {} type={}",
            name_of(entry.ObjectNameA, entry.ObjectNameW),
            entry.AllocationType.0
        );
        node = entry.pNext;
        seen += 1;
    }
    if seen == 0 {
        crate::log!("[dred]   no {label} allocations recorded");
    }
}

/// The last GPU operations each queue completed.
///
/// The trail stops at the operation that faulted, so the final entries name
/// the pass that did it -- a dispatch of ours, or something the engine was
/// doing with a resource we had moved out from under it.
unsafe fn breadcrumbs(mut node: *const D3D12_AUTO_BREADCRUMB_NODE) {
    let mut nodes = 0;
    while !node.is_null() && nodes < 8 {
        let entry = &*node;
        let done = if entry.pLastBreadcrumbValue.is_null() {
            0
        } else {
            *entry.pLastBreadcrumbValue
        };
        crate::log!(
            "[dred] queue \"{}\" list \"{}\": {} of {} operations completed",
            name_of(entry.pCommandQueueDebugNameA, entry.pCommandQueueDebugNameW),
            name_of(entry.pCommandListDebugNameA, entry.pCommandListDebugNameW),
            done,
            entry.BreadcrumbCount
        );
        // Only the tail matters: everything before it finished.
        let first = done.saturating_sub(6);
        let last = (done + 2).min(entry.BreadcrumbCount);
        if !entry.pCommandHistory.is_null() {
            for index in first..last {
                let op = *entry.pCommandHistory.add(index as usize);
                let marker = if index == done {
                    " <-- stopped here"
                } else {
                    ""
                };
                crate::log!("[dred]   {index}: {}{marker}", operation(op));
            }
        }
        node = entry.pNext;
        nodes += 1;
    }
    if nodes == 0 {
        crate::log!("[dred] no breadcrumb nodes recorded");
    }
}

fn operation(op: D3D12_AUTO_BREADCRUMB_OP) -> String {
    const NAMES: &[&str] = &[
        "SetMarker",
        "BeginEvent",
        "EndEvent",
        "DrawInstanced",
        "DrawIndexedInstanced",
        "ExecuteIndirect",
        "Dispatch",
        "CopyBufferRegion",
        "CopyTextureRegion",
        "CopyResource",
        "CopyTiles",
        "ResolveSubresource",
        "ClearRenderTargetView",
        "ClearUnorderedAccessView",
        "ClearDepthStencilView",
        "ResourceBarrier",
        "ExecuteBundle",
        "Present",
        "ResolveQueryData",
        "BeginSubmission",
        "EndSubmission",
        "DecodeFrame",
        "ProcessFrames",
        "AtomicCopyBufferUint",
        "AtomicCopyBufferUint64",
        "ResolveSubresourceRegion",
        "WriteBufferImmediate",
        "DecodeFrame1",
        "SetProtectedResourceSession",
        "DecodeFrame2",
        "ProcessFrames1",
        "BuildRaytracingAccelerationStructure",
        "EmitRaytracingAccelerationStructurePostbuildInfo",
        "CopyRaytracingAccelerationStructure",
        "SetPredication",
        "Reserved35",
        "DispatchRays",
        "InitializeMetaCommand",
        "ExecuteMetaCommand",
        "EstimateMotion",
        "ResolveMotionVectorHeap",
        "SetPipelineState1",
        "InitializeExtensionCommand",
        "ExecuteExtensionCommand",
    ];
    NAMES
        .get(op.0 as usize)
        .map(|name| (*name).to_string())
        .unwrap_or_else(|| format!("op {}", op.0))
}
