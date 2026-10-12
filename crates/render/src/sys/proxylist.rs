//! The list as the engine holds it, when ReShade wraps it.
//!
//! NGX is the one place where a native list is the wrong thing to hand over.
//! NGX creates descriptor heaps and command objects on the device it holds
//! and then uses them with the list it is given. With ReShade loaded that
//! device is ReShade's proxy (the game initialises NGX with the device as it
//! holds it), so what NGX creates are ReShade's proxies, and it then calls
//! `SetDescriptorHeaps` with a proxy heap on the native list it was handed:
//! D3D12 is passed an object it did not make and faults inside D3D12Core at
//! the first DLSS frame. Measured in all four combinations: a proxy list
//! works whichever device NGX was initialised with; a native list faults or
//! fails.
//!
//! So NGX must get the list from the same side of ReShade as its device.
//! The difficulty is that the temporal pass runs from a vtable hook on the
//! *native* list, and nothing leads from a native list back to its proxy:
//! ReShade keeps no such record. What does exist is the call stack. The
//! engine's `Dispatch` goes through ReShade's proxy before it reaches ours,
//! so one hook on the proxy's `Dispatch` slot notes, for the duration of
//! that call and on that thread, which proxy is being dispatched on. The
//! native hook underneath can then ask ([`engine_list`]), and the answer is
//! checked: the proxy must unwrap to that very native list.
//!
//! NGX then sets its own descriptor heaps through that proxy, and ReShade
//! remembers the heaps of every list it wraps; a restore of the engine's
//! heaps on the native list underneath would leave that memory pointing at
//! NGX's heap. So a second hook, on the proxy's `SetDescriptorHeaps`, keeps
//! the heaps the engine last set on each proxy, as it passed them (ReShade's
//! proxies), and they are set again through the proxy after NGX
//! ([`set_heaps_through`]), from a snapshot taken *before* NGX ran: NGX's
//! own calls pass the same hook and overwrite the record.

use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};

use mhw_win::{patch_vtable, slot, vtable_of};
use parking_lot::Mutex;
use windows::core::{Interface, GUID};
use windows::Win32::Graphics::Direct3D12::{
    ID3D12CommandAllocator, ID3D12Device, ID3D12GraphicsCommandList,
    ID3D12GraphicsCommandList_Vtbl, D3D12_COMMAND_LIST_TYPE_DIRECT,
};

use super::queue::engine_queue;
use super::reshade::{self, peel};
use super::vt::{original, DispatchFn, SetHeapsFn};
use crate::proxystate::{dispatching, Dispatching, Heaps, HeldHeaps};

/// `__uuidof(D3D12Device)` in ReShade's `d3d12_device.hpp`: ReShade leaves
/// a pointer to its proxy device on the native device as private data under
/// the proxy class's own GUID.
const PROXY_DEVICE: GUID = GUID::from_u128(0x2523aff4_978b_4939_ba16_8ee876a4cb2a);

/// The proxy's own `Dispatch` (non-zero once hooked).
static O_PROXY_DISPATCH: AtomicUsize = AtomicUsize::new(0);
/// The proxy's own `SetDescriptorHeaps`.
static O_PROXY_SET_HEAPS: AtomicUsize = AtomicUsize::new(0);
/// The heaps the engine last set on each proxy list (bounded).
static HELD: Mutex<Option<HeldHeaps>> = Mutex::new(None);
/// A watch was attempted on a device that has a ReShade proxy; it is not
/// attempted again, whatever came of it.
static ATTEMPTED: AtomicBool = AtomicBool::new(false);
static PROXY_DISPATCHES: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn proxy_dispatch(this: *mut c_void, x: u32, y: u32, z: u32) {
    PROXY_DISPATCHES.fetch_add(1, Ordering::Relaxed);
    // Bound to a variable, so that it lasts for the call and not for the
    // statement.
    let _dispatching = Dispatching::enter(this as usize);
    // SAFETY: stored by `patch_vtable` before the slot was written; the
    // caller's own arguments.
    unsafe { original::<DispatchFn>(&O_PROXY_DISPATCH)(this, x, y, z) }
}

unsafe extern "system" fn proxy_set_heaps(
    this: *mut c_void,
    count: u32,
    heaps: *const *mut c_void,
) {
    let given = match count {
        0 => Some(&[][..]),
        1..=2 if !heaps.is_null() => {
            // SAFETY: the caller passes `count` heap pointers.
            Some(unsafe { std::slice::from_raw_parts(heaps as *const usize, count as usize) })
        }
        _ => None,
    };
    if let Some(held) = given.and_then(Heaps::new) {
        HELD.lock()
            .get_or_insert_with(HeldHeaps::default)
            .record(this as usize, held);
    }
    // SAFETY: stored by `patch_vtable` before the slot was written; the
    // caller's own arguments.
    unsafe { original::<SetHeapsFn>(&O_PROXY_SET_HEAPS)(this, count, heaps) }
}

/// Whether ReShade's command list proxy is being watched (its `Dispatch`
/// slot is hooked).
pub fn proxy_lists_watched() -> bool {
    O_PROXY_DISPATCH.load(Ordering::Relaxed) != 0
}

/// How many dispatches have come through ReShade's command list proxy.
pub fn proxy_dispatches() -> u64 {
    PROXY_DISPATCHES.load(Ordering::Relaxed)
}

/// Hook ReShade's command list proxy, once ReShade has a proxy over the
/// engine's device (found through the engine's graphics queue, see
/// [`super::note_presenting_queue`]).
///
/// Idempotent and cheap: one relaxed load when ReShade is absent, or once
/// an attempt was made; until ReShade wraps the engine's device a
/// `GetPrivateData`. Call where a swapchain is created (ReShade may have
/// come in after the device); render calls it itself after the game's
/// device creation. Creates and releases a command list through ReShade:
/// never under the loader lock.
pub fn watch_proxy_lists() {
    if !reshade::reshade_present() || ATTEMPTED.load(Ordering::Relaxed) {
        return;
    }
    let Some(queue) = engine_queue() else {
        return;
    };
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: a live queue held by the registry; a typed out parameter.
    if unsafe { queue.GetDevice(&mut device) }.is_err() {
        return;
    }
    if let Some(device) = device {
        // SAFETY: a live native device (the queue was unwrapped when noted).
        unsafe { watch(&device) };
    }
}

/// [`watch_proxy_lists`] for a native device in hand.
///
/// # Safety
/// `native` must be a native (unwrapped) device; not under the loader lock.
pub(crate) unsafe fn watch(native: &ID3D12Device) {
    if !reshade::reshade_present() || ATTEMPTED.load(Ordering::Relaxed) {
        return;
    }
    let mut proxy: *mut c_void = std::ptr::null_mut();
    let mut size = std::mem::size_of::<*mut c_void>() as u32;
    // SAFETY: a live device; the out buffer is one pointer, as `size` says.
    let found = unsafe {
        native.GetPrivateData(
            &PROXY_DEVICE,
            &mut size,
            Some(&mut proxy as *mut *mut c_void as *mut c_void),
        )
    };
    if found.is_err() || proxy.is_null() || size as usize != std::mem::size_of::<*mut c_void>() {
        // No proxy on this device (yet): ReShade came in after it was made,
        // so the engine's lists are native and there is nothing to watch,
        // or it wraps the device only once the creation call returns.
        return;
    }
    if ATTEMPTED.swap(true, Ordering::SeqCst) {
        return;
    }
    // SAFETY: ReShade's proxy device, alive as long as the native device it
    // is stored on.
    let Some(device) = (unsafe { ID3D12Device::from_raw_borrowed(&proxy) }) else {
        return;
    };
    // SAFETY: plain object creation on a live device, not under the loader
    // lock (caller's contract).
    let list = unsafe {
        let allocator: ID3D12CommandAllocator = match device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
        {
            Ok(allocator) => allocator,
            Err(error) => {
                log::warn!(target: "render", "no command allocator from ReShade's proxy device {proxy:p}: {error}");
                return;
            }
        };
        let list: ID3D12GraphicsCommandList = match device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &allocator,
            None,
        ) {
            Ok(list) => list,
            Err(error) => {
                log::warn!(target: "render", "no command list from ReShade's proxy device {proxy:p}: {error}");
                return;
            }
        };
        let _ = list.Close();
        list
    };
    // SAFETY: a live list.
    if !unsafe { reshade::is_reshade_proxy(list.as_raw()) } {
        log::info!(
            target: "render",
            "a list created through ReShade's proxy device {proxy:p} is not a proxy, so the \
             engine's lists are not either"
        );
        return;
    }
    // Every proxy list shares the vtable of the one just made.
    // SAFETY: a live object; its first word is its vtable.
    let vtable = unsafe { vtable_of(list.as_raw()) };
    // `Dispatch` last: `proxy_lists_watched` reads its original.
    let hooks: [(&str, usize, *const c_void, &AtomicUsize); 2] = [
        (
            "SetDescriptorHeaps",
            slot!(ID3D12GraphicsCommandList_Vtbl, SetDescriptorHeaps),
            proxy_set_heaps as *const c_void,
            &O_PROXY_SET_HEAPS,
        ),
        (
            "Dispatch",
            slot!(ID3D12GraphicsCommandList_Vtbl, Dispatch),
            proxy_dispatch as *const c_void,
            &O_PROXY_DISPATCH,
        ),
    ];
    for (name, index, replacement, stored) in hooks {
        // SAFETY: the slot index is the interface's own and the
        // replacement has that slot's signature.
        match unsafe { patch_vtable(vtable, index, replacement, stored) } {
            Ok(()) => log::info!(
                target: "render",
                "hooked slot {index} {name} on ReShade's command list proxy (vtable {vtable:p})"
            ),
            Err(error) => {
                log::warn!(
                    target: "render",
                    "could not hook {name} on ReShade's command list proxy: {error}"
                );
                return;
            }
        }
    }
}

/// The list as the engine holds it: the ReShade proxy whose `Dispatch` is
/// on this thread's stack if it unwraps to `native`, otherwise `native`.
/// One relaxed load with ReShade absent; one thread-local read (and a
/// `QueryInterface` while a proxy dispatches) otherwise.
///
/// # Safety
/// `native` must be a live command list.
pub(crate) unsafe fn engine_list(native: *mut c_void) -> *mut c_void {
    if !reshade::reshade_present() {
        return native;
    }
    let proxy = dispatching() as *mut c_void;
    if proxy.is_null() || proxy == native {
        return native;
    }
    // SAFETY: the proxy's `Dispatch` is on this thread's stack, so it is
    // alive; checked, not assumed: it must answer with this very list.
    if unsafe { peel(proxy) } == Some(native) {
        proxy
    } else {
        native
    }
}

/// The descriptor heaps the engine last set on this proxy list, as it passed
/// them. Take it before NGX is given the list. `None` when not watched or
/// none were set.
pub(crate) fn held_heaps(proxy: *mut c_void) -> Option<Heaps> {
    HELD.lock().as_ref()?.get(proxy as usize)
}

/// Set `heaps` on `proxy` again through the proxy's own slot: our hook and
/// then ReShade's method, which unwraps them, sets them on the native list
/// and updates its record. Returns how many were set (none for an empty
/// snapshot).
///
/// # Safety
/// `proxy` must be a live ReShade command list proxy, open for recording on
/// this thread, and `heaps` a snapshot of its own [`held_heaps`].
pub(crate) unsafe fn set_heaps_through(proxy: *mut c_void, heaps: Heaps) -> u32 {
    if heaps.count() == 0 {
        return 0;
    }
    let pointers = heaps.as_slice();
    // SAFETY: a live COM object's first word is its vtable; the slot is
    // `SetDescriptorHeaps` of the interface; the heaps are those the engine
    // set on this list, which it keeps alive while recording it.
    unsafe {
        let set: SetHeapsFn = std::mem::transmute::<usize, SetHeapsFn>(
            *vtable_of(proxy).add(slot!(ID3D12GraphicsCommandList_Vtbl, SetDescriptorHeaps)),
        );
        set(
            proxy,
            heaps.count(),
            pointers.as_ptr() as *const *mut c_void,
        );
    }
    heaps.count()
}
