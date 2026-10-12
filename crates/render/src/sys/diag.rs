//! The D3D12 debug layer and DRED, when `mod` asks for them (dev settings).
//!
//! Both must be enabled before the device exists. The debug layer's
//! information queue is drained into the log (errors and corruption, with a
//! tally; id 527, a transition barrier declaring the wrong `StateBefore`, is
//! the one this crate can cause). DRED is reported once, when the device is
//! removed: page-fault allocations (a *freed* one means a descriptor named a
//! released resource) and each queue's last breadcrumbs.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use windows::core::{Interface, PCWSTR};
use windows::Win32::Graphics::Direct3D12::*;

use super::creation::DeviceOptions;

static DEBUG_ENABLED: AtomicBool = AtomicBool::new(false);
static DRED_ENABLED: AtomicBool = AtomicBool::new(false);
static DRAINING: AtomicBool = AtomicBool::new(false);
static REMOVAL_WATCHED: AtomicBool = AtomicBool::new(false);

/// `D3D12 ERROR #527`: barrier `StateBefore` mismatch.
const BARRIER_MISMATCH: i32 = 527;

/// Turn on what `options` asks for, once each, before a device exists.
pub(crate) fn enable(options: DeviceOptions) {
    if options.debug_layer && !DEBUG_ENABLED.swap(true, Ordering::SeqCst) {
        let mut debug: Option<ID3D12Debug> = None;
        // SAFETY: plain interface query; no device exists to retrofit.
        match unsafe { D3D12GetDebugInterface(&mut debug) }.map(|()| debug) {
            Ok(Some(debug)) => {
                // SAFETY: a live debug interface.
                unsafe { debug.EnableDebugLayer() };
                log::warn!(target: "render", "D3D12 debug layer on; expect it to cost performance");
            }
            // Usually the Graphics Tools optional feature is not installed.
            Ok(None) => log::warn!(target: "render", "D3D12 debug layer unavailable"),
            Err(error) => log::warn!(target: "render", "D3D12 debug layer unavailable: {error}"),
        }
    }
    if options.dred && !DRED_ENABLED.swap(true, Ordering::SeqCst) {
        let mut settings: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
        // SAFETY: plain interface query.
        match unsafe { D3D12GetDebugInterface(&mut settings) }.map(|()| settings) {
            Ok(Some(settings)) => {
                // SAFETY: a live settings interface.
                unsafe {
                    settings.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                    settings.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                }
                log::info!(target: "render", "DRED breadcrumbs and page-fault reporting on");
            }
            Ok(None) => log::warn!(target: "render", "DRED unavailable"),
            Err(error) => log::warn!(target: "render", "DRED unavailable: {error}"),
        }
    }
}

/// Start the watchers `options` asks for on the game's (native) device.
///
/// # Safety
/// `device` must be a live `ID3D12Device`.
pub(crate) unsafe fn watch(device: *mut c_void, options: DeviceOptions) {
    // SAFETY: caller's contract.
    let Some(device) = (unsafe { ID3D12Device::from_raw_borrowed(&device) }).cloned() else {
        return;
    };
    if options.debug_layer && !DRAINING.swap(true, Ordering::SeqCst) {
        match device.cast::<ID3D12InfoQueue>() {
            Ok(queue) => {
                if let Err(error) = mhw_win::spawn("mhw-render-validation", move || drain(queue)) {
                    log::warn!(target: "render", "validation drain thread not started: {error}");
                }
            }
            Err(error) => log::warn!(target: "render", "no information queue: {error}"),
        }
    }
    if options.dred && !REMOVAL_WATCHED.swap(true, Ordering::SeqCst) {
        if let Err(error) = mhw_win::spawn("mhw-render-dred", move || watch_removal(device)) {
            log::warn!(target: "render", "DRED watch thread not started: {error}");
        }
    }
}

fn drain(queue: ID3D12InfoQueue) {
    let mut tally: HashMap<i32, u64> = HashMap::new();
    let mut reported = Instant::now();
    let mut buffer: Vec<u64> = Vec::new();
    loop {
        std::thread::sleep(Duration::from_millis(250));
        // SAFETY: a live info queue; each message is copied into a buffer
        // of the size the queue asked for (u64 storage keeps it aligned).
        unsafe {
            let stored = queue.GetNumStoredMessages();
            for index in 0..stored {
                let mut length = 0usize;
                if queue.GetMessage(index, None, &mut length).is_err() || length == 0 {
                    continue;
                }
                buffer.clear();
                buffer.resize(length.div_ceil(8), 0);
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(index, Some(message), &mut length).is_err() {
                    continue;
                }
                let message = &*message;
                let severity = match message.Severity {
                    D3D12_MESSAGE_SEVERITY_CORRUPTION => "corruption",
                    D3D12_MESSAGE_SEVERITY_ERROR => "error",
                    D3D12_MESSAGE_SEVERITY_WARNING => "warning",
                    _ => continue,
                };
                *tally.entry(message.ID.0).or_default() += 1;
                let text = if message.pDescription.is_null() {
                    &[][..]
                } else {
                    std::slice::from_raw_parts(
                        message.pDescription,
                        message.DescriptionByteLength.saturating_sub(1),
                    )
                };
                log::warn!(
                    target: "render",
                    "d3d12 {severity} {}: {}",
                    message.ID.0,
                    String::from_utf8_lossy(text).trim()
                );
            }
            if stored > 0 {
                queue.ClearStoredMessages();
            }
        }
        if reported.elapsed() >= Duration::from_secs(20) && !tally.is_empty() {
            reported = Instant::now();
            let mut counts: Vec<(i32, u64)> = tally.iter().map(|(id, n)| (*id, *n)).collect();
            counts.sort_by_key(|entry| std::cmp::Reverse(entry.1));
            let top: Vec<String> = counts
                .iter()
                .take(8)
                .map(|(id, n)| format!("{id}x{n}"))
                .collect();
            log::info!(
                target: "render",
                "d3d12 message tally: {}; barrier mismatches (527): {}",
                top.join(" "),
                tally.get(&BARRIER_MISMATCH).copied().unwrap_or(0)
            );
        }
    }
}

fn watch_removal(device: ID3D12Device) {
    loop {
        std::thread::sleep(Duration::from_millis(250));
        // SAFETY: a live device.
        let Err(reason) = (unsafe { device.GetDeviceRemovedReason() }) else {
            continue;
        };
        log::error!(target: "render", "device removed: {:#010x}", reason.code().0);
        // SAFETY: a live (removed) device; DRED is read-only here.
        unsafe { report(&device) };
        return;
    }
}

/// # Safety
/// `device` must be live.
unsafe fn report(device: &ID3D12Device) {
    let data: ID3D12DeviceRemovedExtendedData = match device.cast() {
        Ok(data) => data,
        Err(error) => {
            log::error!(target: "render", "no DRED data: {error}");
            return;
        }
    };
    // SAFETY: DRED output nodes are valid while `data` lives.
    unsafe {
        match data.GetPageFaultAllocationOutput() {
            Ok(fault) => {
                log::error!(target: "render", "DRED page fault at {:#x}", fault.PageFaultVA);
                allocations("live", fault.pHeadExistingAllocationNode);
                allocations("freed", fault.pHeadRecentFreedAllocationNode);
            }
            Err(error) => log::error!(target: "render", "no DRED page fault data: {error}"),
        }
        match data.GetAutoBreadcrumbsOutput() {
            Ok(output) => breadcrumbs(output.pHeadAutoBreadcrumbNode),
            Err(error) => log::error!(target: "render", "no DRED breadcrumbs: {error}"),
        }
    }
}

/// # Safety
/// `ansi`/`wide` null or NUL-terminated.
unsafe fn name_of(ansi: *const u8, wide: PCWSTR) -> String {
    // SAFETY: caller's contract; at most 256 bytes are scanned.
    unsafe {
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
    }
    "unnamed".into()
}

/// # Safety
/// A valid DRED allocation list.
unsafe fn allocations(label: &str, mut node: *const D3D12_DRED_ALLOCATION_NODE) {
    let mut seen = 0;
    // SAFETY: caller's contract; bounded walk.
    unsafe {
        while let Some(entry) = node.as_ref() {
            if seen >= 24 {
                break;
            }
            log::error!(
                target: "render",
                "DRED {label} allocation: {} type={}",
                name_of(entry.ObjectNameA, entry.ObjectNameW),
                entry.AllocationType.0
            );
            node = entry.pNext;
            seen += 1;
        }
    }
}

/// # Safety
/// A valid DRED breadcrumb list.
unsafe fn breadcrumbs(mut node: *const D3D12_AUTO_BREADCRUMB_NODE) {
    let mut nodes = 0;
    // SAFETY: caller's contract; bounded walk; history indices below the
    // node's count.
    unsafe {
        while let Some(entry) = node.as_ref() {
            if nodes >= 8 {
                break;
            }
            let done = entry.pLastBreadcrumbValue.as_ref().copied().unwrap_or(0);
            log::error!(
                target: "render",
                "DRED queue \"{}\" list \"{}\": {done} of {} operations completed",
                name_of(entry.pCommandQueueDebugNameA, entry.pCommandQueueDebugNameW),
                name_of(entry.pCommandListDebugNameA, entry.pCommandListDebugNameW),
                entry.BreadcrumbCount
            );
            if !entry.pCommandHistory.is_null() {
                for index in done.saturating_sub(6)..(done + 2).min(entry.BreadcrumbCount) {
                    let op = *entry.pCommandHistory.add(index as usize);
                    let marker = if index == done {
                        "  <-- stopped here"
                    } else {
                        ""
                    };
                    log::error!(target: "render", "DRED   {index}: op {}{marker}", op.0);
                }
            }
            node = entry.pNext;
            nodes += 1;
        }
    }
}
