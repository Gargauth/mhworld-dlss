//! Knowing that a resource has not been destroyed.
//!
//! Everything this library hands to NGX it holds as a bare address: the
//! colour, the motion vectors and the output come out of the descriptor
//! registry, the depth out of the depth-stencil views, and all of those
//! outlive what they name. The engine destroys and rebuilds its render
//! targets whenever the render ratio changes -- twice on the way from the
//! title screen into play -- and an address that named a target before a
//! rebuild names freed memory after it, or a different resource that the
//! allocator has put in the same place.
//!
//! A view slot that was rewritten, a copy that aged out, a depth target bound
//! this frame: each is a reason to believe a resource is current and none is
//! a proof. Acting on a wrong one is an evaluation that fails and a command
//! list the engine cannot close.
//!
//! Direct3D 12 offers the proof. Any object can carry private data, and
//! private data set as an interface is released by the runtime when the
//! object is destroyed. So each texture worth following gets a small COM
//! object of ours attached at creation -- a witness -- whose last `Release`
//! is the destruction notice, delivered by the runtime, exactly once, at the
//! moment it happens. A witness carries a serial number, and so does every
//! record made of the resource while it lived. An address is believed only
//! if a witness for it is still alive *and* its serial is the one the record
//! was made under, which also catches a new resource at a recycled address.
//!
//! Nothing here holds a reference to the resource, so nothing here keeps a
//! target alive that the engine wants gone.

use crate::fasthash::AddressMap;
use parking_lot::Mutex;
use std::ffi::c_void;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use windows::core::{IUnknown, Interface, GUID, HRESULT};
use windows::Win32::Graphics::Direct3D12::ID3D12Object;

/// Resources with a living witness, and the serial each was given.
///
/// A leaf lock: nothing is called with it held, and in particular no
/// resource is ever released with it held, since a release is what calls
/// back into here.
static ALIVE: Mutex<Option<AddressMap<usize, u64>>> = Mutex::new(None);
static NEXT_SERIAL: AtomicU64 = AtomicU64::new(1);
static WATCHED: AtomicU64 = AtomicU64::new(0);
static ENDED: AtomicU64 = AtomicU64::new(0);

/// The private-data key the witness is filed under. This project's own.
const WITNESS: GUID = GUID::from_u128(0x6d687764_6c73_7334_a11e_77697465736e);

const S_OK: HRESULT = HRESULT(0);
const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002_u32 as i32);
const E_POINTER: HRESULT = HRESULT(0x8000_4003_u32 as i32);

/// The object attached to a resource. Laid out as a COM object: a pointer to
/// a table of `IUnknown`'s three methods comes first.
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
    if !wanted.is_null() && *wanted == IUnknown::IID {
        add_ref(this);
        *out = this as *mut c_void;
        return S_OK;
    }
    *out = std::ptr::null_mut();
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut Witness) -> u32 {
    (*this).references.fetch_add(1, Ordering::Relaxed) + 1
}

unsafe extern "system" fn release(this: *mut Witness) -> u32 {
    let left = (*this).references.fetch_sub(1, Ordering::AcqRel) - 1;
    if left == 0 {
        // The runtime has let go, which it does when the resource is
        // destroyed. Only this witness's own entry is removed: the address
        // may already belong to a newer resource with a witness of its own.
        let witness = Box::from_raw(this);
        if let Some(alive) = ALIVE.lock().as_mut() {
            if alive.get(&witness.resource) == Some(&witness.serial) {
                alive.remove(&witness.resource);
            }
        }
        ENDED.fetch_add(1, Ordering::Relaxed);
    }
    left
}

/// Attach a witness to a resource that has just been created.
///
/// # Safety
/// `raw` must be a live COM interface pointer to a Direct3D 12 object.
pub unsafe fn watch(raw: *mut c_void) {
    let Some(unknown) = IUnknown::from_raw_borrowed(&raw) else {
        return;
    };
    let Ok(object) = unknown.cast::<ID3D12Object>() else {
        return;
    };
    let serial = NEXT_SERIAL.fetch_add(1, Ordering::Relaxed);
    let witness = Box::into_raw(Box::new(Witness {
        methods: &METHODS,
        references: AtomicU32::new(1),
        resource: raw as usize,
        serial,
    }));
    // Entered before it is attached, so there is no moment at which the
    // witness exists and the entry does not. Replaces whatever an earlier
    // resource at this address left, which is the point.
    ALIVE
        .lock()
        .get_or_insert_with(AddressMap::default)
        .insert(raw as usize, serial);
    WATCHED.fetch_add(1, Ordering::Relaxed);

    let as_interface = witness as *mut c_void;
    let attached = object
        .SetPrivateDataInterface(&WITNESS, IUnknown::from_raw_borrowed(&as_interface))
        .is_ok();
    // Our own reference goes either way. Attached, the runtime now holds the
    // only one. Not attached, this is the last, and the entry goes with it --
    // so a resource that could not be watched is simply not believed.
    release(witness);
    if !attached {
        static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
        if !SAID.swap(true, Ordering::Relaxed) {
            crate::log!(
                "[alive] a resource would not take private data, so it is not followed and \
                 cannot be handed to DLSS"
            );
        }
    }
}

/// The serial of the living resource at this address, if there is one.
pub fn serial_of(resource: usize) -> Option<u64> {
    if resource == 0 {
        return None;
    }
    ALIVE
        .lock()
        .as_ref()
        .and_then(|alive| alive.get(&resource).copied())
}

/// Whether the resource a record was made of is still that resource.
pub fn is(resource: usize, serial: u64) -> bool {
    serial_of(resource) == Some(serial)
}

/// For the log: how many are being watched, and how many have ended.
pub fn summary() -> String {
    format!(
        "{} resources watched, {} of them since destroyed",
        WATCHED.load(Ordering::Relaxed),
        ENDED.load(Ordering::Relaxed)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The witness by itself, with the runtime's part played by hand: taking
    /// a reference, and letting the last one go.
    #[test]
    fn the_last_release_ends_the_entry_and_only_its_own() {
        let address = 0x1234_5000usize;
        let make = |serial: u64| {
            ALIVE
                .lock()
                .get_or_insert_with(AddressMap::default)
                .insert(address, serial);
            Box::into_raw(Box::new(Witness {
                methods: &METHODS,
                references: AtomicU32::new(1),
                resource: address,
                serial,
            }))
        };
        unsafe {
            let first = make(900_001);
            assert!(is(address, 900_001));

            let mut out = std::ptr::null_mut();
            assert_eq!(query_interface(first, &IUnknown::IID, &mut out), S_OK);
            assert_eq!(out as *mut Witness, first);
            assert_eq!(
                query_interface(first, &ID3D12Object::IID, &mut out),
                E_NOINTERFACE
            );
            assert_eq!(release(first), 1);
            assert!(is(address, 900_001), "ended while a reference was held");

            // A newer resource at the same address before the old one's
            // witness has gone: the old one's end must not take the new entry.
            let second = make(900_002);
            assert_eq!(release(first), 0);
            assert!(is(address, 900_002), "an old witness removed a newer entry");
            assert!(!is(address, 900_001));

            assert_eq!(release(second), 0);
            assert!(serial_of(address).is_none());
        }
    }

    /// Against the real runtime: a texture is watched, is known while it
    /// lives, and stops being known the moment its last reference goes --
    /// with nothing of ours having told it so. Skipped where no Direct3D 12
    /// device can be made.
    #[test]
    fn the_runtime_tells_us_when_a_texture_is_destroyed() {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
        use windows::Win32::Graphics::Direct3D12::*;
        use windows::Win32::Graphics::Dxgi::Common::{
            DXGI_FORMAT_R11G11B10_FLOAT, DXGI_SAMPLE_DESC,
        };
        unsafe {
            let mut device: Option<ID3D12Device> = None;
            if D3D12CreateDevice(None, D3D_FEATURE_LEVEL_11_0, &mut device).is_err() {
                return;
            }
            let Some(device) = device else {
                return;
            };
            let make = || {
                let mut texture: Option<ID3D12Resource> = None;
                device
                    .CreateCommittedResource(
                        &D3D12_HEAP_PROPERTIES {
                            Type: D3D12_HEAP_TYPE_DEFAULT,
                            ..Default::default()
                        },
                        D3D12_HEAP_FLAG_NONE,
                        &D3D12_RESOURCE_DESC {
                            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                            Width: 1024,
                            Height: 512,
                            DepthOrArraySize: 1,
                            MipLevels: 1,
                            Format: DXGI_FORMAT_R11G11B10_FLOAT,
                            SampleDesc: DXGI_SAMPLE_DESC {
                                Count: 1,
                                Quality: 0,
                            },
                            Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
                            ..Default::default()
                        },
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        None,
                        &mut texture,
                    )
                    .expect("a render target could be created");
                texture.expect("and was returned")
            };

            let texture = make();
            let address = texture.as_raw() as usize;
            // In this test binary `D3D12CreateDevice` is this library's own
            // export, which installs the render hooks -- so the texture may
            // already have a witness from `hook_create_committed`. Watching
            // it again replaces that one, and the old witness's end must not
            // take the new entry with it, which the assertions below cover.
            watch(texture.as_raw());
            let serial = serial_of(address).expect("a watched texture is known");
            assert!(is(address, serial));

            // A second reference and its release change nothing.
            let again = texture.clone();
            drop(again);
            assert!(is(address, serial));

            // The last one does, and it is the runtime that says so.
            drop(texture);
            assert!(
                serial_of(address).is_none(),
                "a destroyed texture is still believed"
            );
            assert!(!is(address, serial));

            // A second texture is a second serial, whatever its address.
            let other = make();
            watch(other.as_raw());
            let next = serial_of(other.as_raw() as usize).expect("known");
            assert_ne!(next, serial);
            assert!(
                !is(other.as_raw() as usize, serial),
                "an old record names a new resource"
            );
        }
    }
}
