//! Knowing that a resource has not been destroyed.
//!
//! Everything handed on is held as a bare address taken from a descriptor, a
//! depth-stencil view or a binding, and all of those outlive what they name:
//! the engine rebuilds its targets whenever the render ratio changes, and the
//! allocator puts new ones where the old ones stood. Any object can carry
//! private data, and private data set as an interface is released by the
//! runtime when the object is destroyed. So each texture worth following gets
//! a small COM object attached at creation, a *witness*, whose last
//! `Release` is the destruction notice. A witness carries a serial, and so
//! does every record made of the resource; an address is believed only while
//! a witness for it lives *and* its serial matches, which also catches a new
//! resource at a recycled address. Nothing here holds a reference to the
//! resource.

use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::LazyLock;

use parking_lot::Mutex;
use windows::core::{IUnknown, Interface, GUID, HRESULT};
use windows::Win32::Foundation::{E_NOINTERFACE, E_POINTER, S_OK};
use windows::Win32::Graphics::Direct3D12::ID3D12Object;

use crate::addrmap::AddressMap;

/// Resources with a living witness, and the serial each was given. A leaf
/// lock: nothing is released while it is held (a release calls back here).
static ALIVE: LazyLock<Mutex<AddressMap<usize, u64>>> = LazyLock::new(Default::default);
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);
static WATCHED: AtomicU64 = AtomicU64::new(0);
static ENDED: AtomicU64 = AtomicU64::new(0);

/// The private-data key the witness is filed under (this project's own).
const WITNESS: GUID = GUID::from_u128(0x6d687764_6c73_7334_a11e_77697465736e);

#[repr(C)]
struct Witness {
    methods: &'static Methods,
    references: AtomicU32,
    resource: usize,
    serial: u64,
}

#[repr(C)]
struct Methods {
    query_interface:
        unsafe extern "system" fn(*mut Witness, *const GUID, *mut *mut c_void) -> HRESULT,
    add_ref: unsafe extern "system" fn(*mut Witness) -> u32,
    release: unsafe extern "system" fn(*mut Witness) -> u32,
}

static METHODS: Methods = Methods {
    query_interface,
    add_ref,
    release,
};

unsafe extern "system" fn query_interface(
    this: *mut Witness,
    wanted: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
    if out.is_null() {
        return E_POINTER;
    }
    // SAFETY: COM contract: `wanted` is null or a GUID, `out` writable,
    // `this` a live witness.
    unsafe {
        if !wanted.is_null() && *wanted == IUnknown::IID {
            add_ref(this);
            *out = this.cast();
            return S_OK;
        }
        *out = std::ptr::null_mut();
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut Witness) -> u32 {
    // SAFETY: COM contract: `this` is a live witness.
    unsafe { (*this).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut Witness) -> u32 {
    // SAFETY: COM contract: `this` is a live witness with a reference.
    let left = unsafe { (*this).references.fetch_sub(1, Ordering::AcqRel) } - 1;
    if left == 0 {
        // SAFETY: the last reference: the box made in `watch` is ours again.
        let witness = unsafe { Box::from_raw(this) };
        let mut alive = ALIVE.lock();
        // Only this witness's own entry: the address may already belong to
        // a newer resource with a witness of its own.
        if alive.get(&witness.resource) == Some(&witness.serial) {
            alive.remove(&witness.resource);
        }
        ENDED.fetch_add(1, Ordering::Relaxed);
    }
    left
}

/// Attach a witness to a resource that has just been created; returns its
/// serial, or `None` if it would not take private data (then it is not
/// believed anywhere).
///
/// # Safety
/// `raw` must be a live D3D12 object.
pub(crate) unsafe fn watch(raw: *mut c_void) -> Option<u64> {
    // SAFETY: caller guarantees a live COM object.
    let unknown = unsafe { IUnknown::from_raw_borrowed(&raw) }?;
    let object = unknown.cast::<ID3D12Object>().ok()?;
    let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
    let witness = Box::into_raw(Box::new(Witness {
        methods: &METHODS,
        references: AtomicU32::new(1),
        resource: raw as usize,
        serial,
    }));
    // Entered before attaching, so no moment exists where the witness
    // lives and the entry does not; replaces any earlier resource's.
    ALIVE.lock().insert(raw as usize, serial);
    WATCHED.fetch_add(1, Ordering::Relaxed);
    let as_interface: *mut c_void = witness.cast();
    // SAFETY: `witness` is laid out as a COM object implementing IUnknown
    // and holds one reference, ours.
    let attached = unsafe {
        object
            .SetPrivateDataInterface(&WITNESS, IUnknown::from_raw_borrowed(&as_interface))
            .is_ok()
    };
    // Our reference goes either way: attached, the runtime holds the only
    // one; not attached, this is the last and the entry goes with it.
    // SAFETY: `witness` is live and we hold one reference.
    unsafe { release(witness) };
    if !attached {
        mhw_core::log_once!(
            Warn,
            "render",
            "witness-refused",
            "a resource refused private data; it is not followed and cannot be handed on"
        );
        return None;
    }
    Some(serial)
}

/// The serial of the living resource at `resource`.
pub fn serial_of(resource: usize) -> Option<u64> {
    if resource == 0 {
        return None;
    }
    ALIVE.lock().get(&resource).copied()
}

/// Whether the resource a record was made of (`resource`, `serial`) is
/// still that resource.
pub fn is_alive(resource: usize, serial: u64) -> bool {
    serial_of(resource) == Some(serial)
}

/// `(watched, destroyed since)`.
pub(crate) fn counts() -> (u64, u64) {
    (
        WATCHED.load(Ordering::Relaxed),
        ENDED.load(Ordering::Relaxed),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_release_ends_the_entry_and_only_its_own() {
        let address = 0x1234_5000usize;
        let make = |serial: u64| {
            ALIVE.lock().insert(address, serial);
            Box::into_raw(Box::new(Witness {
                methods: &METHODS,
                references: AtomicU32::new(1),
                resource: address,
                serial,
            }))
        };
        // SAFETY: witnesses built here, released exactly as often as held.
        unsafe {
            let first = make(900_001);
            let mut out = std::ptr::null_mut();
            assert_eq!(query_interface(first, &IUnknown::IID, &mut out), S_OK);
            assert_eq!(release(first), 1);
            assert!(is_alive(address, 900_001));
            let second = make(900_002);
            assert_eq!(release(first), 0);
            assert!(
                is_alive(address, 900_002),
                "an old witness removed a newer entry"
            );
            assert_eq!(release(second), 0);
            assert!(serial_of(address).is_none());
        }
    }
}
