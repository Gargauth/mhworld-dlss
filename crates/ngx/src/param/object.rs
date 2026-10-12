//! Calls into a live `NVSDK_NGX_Parameter` through its vtable.
//!
//! The methods are C++ virtual member functions; on x64 Windows `this` is the
//! first argument and the convention is the ordinary Microsoft x64 one,
//! spelled `extern "win64"` here so the same code runs (and is tested against
//! a stand-in object) on an x86-64 host.

use std::ffi::{c_char, c_void, CStr};
use std::ptr::NonNull;

use super::{names, Slot, Value, Write};
use crate::status::Status;

type GetFn<T> = unsafe extern "win64" fn(*mut c_void, *const c_char, *mut T) -> u32;
type ResetFn = unsafe extern "win64" fn(*mut c_void);

/// A borrowed `NVSDK_NGX_Parameter*`.
///
/// Copyable and non-owning: whoever obtained the object (`AllocateParameters`
/// / `GetCapabilityParameters` / the game's `GetParameters`) destroys it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parameters(NonNull<c_void>);

// SAFETY: a parameter object is a plain key/value store with no thread
// affinity; the crate only uses one from one thread at a time (under the
// feature lock or inside the game's own call).
unsafe impl Send for Parameters {}

/// The result of [`Parameters::verify_slots`].
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct SlotCheck {
    /// What came back for the integer written (1234 when the slots are right).
    pub int: Option<i32>,
    /// What came back for the float written (0.5 when the slots are right).
    pub float: Option<f32>,
}

impl SlotCheck {
    /// Whether both values came back as written.
    pub fn held(&self) -> bool {
        self.int == Some(1234) && self.float == Some(0.5)
    }
}

impl Parameters {
    /// Wrap a raw pointer; `None` for null.
    ///
    /// # Safety
    /// A non-null `raw` must be a live `NVSDK_NGX_Parameter` (MSVC layout)
    /// that stays alive for every use of the returned value.
    pub unsafe fn from_raw(raw: *mut c_void) -> Option<Parameters> {
        NonNull::new(raw).map(Parameters)
    }

    /// The raw pointer.
    pub fn as_raw(self) -> *mut c_void {
        self.0.as_ptr()
    }

    /// The function in `slot`.
    ///
    /// # Safety
    /// The object is live (type contract of `from_raw`).
    unsafe fn entry(self, slot: Slot) -> usize {
        // SAFETY: a live C++ object starts with its vtable pointer, and the
        // `NVSDK_NGX_Parameter` vtable has `Slot::Reset + 1` entries.
        unsafe {
            let vtable = *(self.0.as_ptr() as *const *const usize);
            *vtable.add(slot.index())
        }
    }

    /// Store `value` under `name` through the overload of its type.
    ///
    /// # Safety
    /// The object is live. A resource or pointer value must be what NGX
    /// expects under `name` and stay alive while NGX may use it.
    pub unsafe fn set(self, name: &CStr, value: Value) {
        let this = self.as_raw();
        let key = name.as_ptr();
        // SAFETY: `entry` reads the slot matching the value's type; each
        // transmute gives that slot its declared C++ signature.
        unsafe {
            let entry = self.entry(value.set_slot());
            match value {
                Value::I32(v) => std::mem::transmute::<
                    usize,
                    unsafe extern "win64" fn(*mut c_void, *const c_char, i32),
                >(entry)(this, key, v),
                Value::U32(v) => std::mem::transmute::<
                    usize,
                    unsafe extern "win64" fn(*mut c_void, *const c_char, u32),
                >(entry)(this, key, v),
                Value::F32(v) => std::mem::transmute::<
                    usize,
                    unsafe extern "win64" fn(*mut c_void, *const c_char, f32),
                >(entry)(this, key, v),
                Value::F64(v) => std::mem::transmute::<
                    usize,
                    unsafe extern "win64" fn(*mut c_void, *const c_char, f64),
                >(entry)(this, key, v),
                Value::U64(v) => std::mem::transmute::<
                    usize,
                    unsafe extern "win64" fn(*mut c_void, *const c_char, u64),
                >(entry)(this, key, v),
                Value::D3d12Resource(v) | Value::VoidPointer(v) => {
                    std::mem::transmute::<
                        usize,
                        unsafe extern "win64" fn(*mut c_void, *const c_char, *mut c_void),
                    >(entry)(this, key, v as *mut c_void)
                }
            }
        }
    }

    /// Apply every write in order.
    ///
    /// # Safety
    /// As [`Parameters::set`] for each write.
    pub unsafe fn apply(self, writes: &[Write]) {
        for write in writes {
            // SAFETY: forwarded to the caller.
            unsafe { self.set(write.name, write.value) };
        }
    }

    /// Read through `slot`, whose out-parameter type is `T`.
    ///
    /// # Safety
    /// The object is live and `T` is the out type of the `Get` in `slot`.
    unsafe fn get<T: Copy + Default>(self, slot: Slot, name: &CStr) -> Option<T> {
        let mut value = T::default();
        // SAFETY: the caller pairs `slot` with `T`; the entry is a function
        // pointer of the declared signature.
        let status = unsafe {
            let call: GetFn<T> = std::mem::transmute_copy(&self.entry(slot));
            call(self.as_raw(), name.as_ptr(), &mut value)
        };
        Status(status).is_success().then_some(value)
    }

    /// `Get(name, int*)`; `None` when NGX reports a failure (e.g. unset).
    ///
    /// # Safety
    /// The object is live.
    pub unsafe fn get_i32(self, name: &CStr) -> Option<i32> {
        // SAFETY: `GetI32` writes an `int`.
        unsafe { self.get(Slot::GetI32, name) }
    }

    /// `Get(name, unsigned int*)`.
    ///
    /// # Safety
    /// The object is live.
    pub unsafe fn get_u32(self, name: &CStr) -> Option<u32> {
        // SAFETY: `GetU32` writes an `unsigned int`.
        unsafe { self.get(Slot::GetU32, name) }
    }

    /// `Get(name, float*)`.
    ///
    /// # Safety
    /// The object is live.
    pub unsafe fn get_f32(self, name: &CStr) -> Option<f32> {
        // SAFETY: `GetF32` writes a `float`.
        unsafe { self.get(Slot::GetF32, name) }
    }

    /// `Get(name, double*)`.
    ///
    /// # Safety
    /// The object is live.
    pub unsafe fn get_f64(self, name: &CStr) -> Option<f64> {
        // SAFETY: `GetF64` writes a `double`.
        unsafe { self.get(Slot::GetF64, name) }
    }

    /// `Get(name, unsigned long long*)`.
    ///
    /// # Safety
    /// The object is live.
    pub unsafe fn get_u64(self, name: &CStr) -> Option<u64> {
        // SAFETY: `GetU64` writes an `unsigned long long`.
        unsafe { self.get(Slot::GetU64, name) }
    }

    /// `Get(name, void**)`, as an address.
    ///
    /// # Safety
    /// The object is live.
    pub unsafe fn get_void_pointer(self, name: &CStr) -> Option<usize> {
        // SAFETY: `GetVoidPointer` writes a pointer, read as its address.
        unsafe { self.get(Slot::GetVoidPointer, name) }
    }

    /// `Get(name, ID3D12Resource**)`, as an address (no reference taken).
    ///
    /// # Safety
    /// The object is live.
    pub unsafe fn get_d3d12_resource(self, name: &CStr) -> Option<usize> {
        // SAFETY: `GetD3d12Resource` writes a pointer, read as its address.
        unsafe { self.get(Slot::GetD3d12Resource, name) }
    }

    /// `Reset()`: clear every entry.
    ///
    /// # Safety
    /// The object is live and nothing NGX evaluates still needs its entries.
    pub unsafe fn reset(self) {
        // SAFETY: slot `Reset` is `void Reset()`.
        unsafe {
            let call: ResetFn = std::mem::transmute::<usize, ResetFn>(self.entry(Slot::Reset));
            call(self.as_raw());
        }
    }

    /// Write an int and a float and read both back: if either does not come
    /// back the object is not laid out as this module assumes (another
    /// implementation's class), and nothing must be set through it.
    ///
    /// # Safety
    /// The object is live; two `mhw.probe.*` entries are left in it.
    pub unsafe fn verify_slots(self) -> SlotCheck {
        // SAFETY: forwarded to the caller.
        unsafe {
            self.set(names::PROBE_INT, Value::I32(1234));
            self.set(names::PROBE_FLOAT, Value::F32(0.5));
            SlotCheck {
                int: self.get_i32(names::PROBE_INT),
                float: self.get_f32(names::PROBE_FLOAT),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    //! A stand-in `NVSDK_NGX_Parameter` with the MSVC vtable built by hand
    //! from the header's declaration order, independently of [`Slot`].

    use std::cell::RefCell;
    use std::collections::HashMap;

    use super::*;

    #[derive(Clone, Copy, Debug, PartialEq)]
    enum Stored {
        U64(u64),
        F32(f32),
        F64(f64),
        U32(u32),
        I32(i32),
        D3d11(usize),
        D3d12(usize),
        Void(usize),
    }

    #[repr(C)]
    struct Fake {
        vtable: *const usize,
        store: RefCell<HashMap<String, Stored>>,
        resets: RefCell<u32>,
    }

    unsafe fn fake<'a>(this: *mut c_void) -> &'a Fake {
        // SAFETY: only called with the address of a live `Fake`.
        unsafe { &*(this as *const Fake) }
    }

    unsafe fn key(name: *const c_char) -> String {
        // SAFETY: the code under test passes NUL-terminated names.
        unsafe { CStr::from_ptr(name) }
            .to_string_lossy()
            .into_owned()
    }

    unsafe fn put(this: *mut c_void, name: *const c_char, value: Stored) {
        // SAFETY: forwarded.
        unsafe { fake(this).store.borrow_mut().insert(key(name), value) };
    }

    macro_rules! setter {
        ($name:ident, $ty:ty, $variant:ident, $conv:expr) => {
            unsafe extern "win64" fn $name(this: *mut c_void, name: *const c_char, value: $ty) {
                // SAFETY: called through the fake's own vtable.
                unsafe { put(this, name, Stored::$variant($conv(value))) }
            }
        };
    }
    macro_rules! getter {
        ($name:ident, $ty:ty, $variant:ident, $conv:expr) => {
            unsafe extern "win64" fn $name(
                this: *mut c_void,
                name: *const c_char,
                out: *mut $ty,
            ) -> u32 {
                // SAFETY: called through the fake's own vtable.
                unsafe {
                    match fake(this).store.borrow().get(&key(name)) {
                        Some(Stored::$variant(value)) => {
                            *out = $conv(*value);
                            1
                        }
                        _ => 0xBAD0_0000,
                    }
                }
            }
        };
    }

    fn same<T>(value: T) -> T {
        value
    }
    fn addr(value: *mut c_void) -> usize {
        value as usize
    }
    fn ptr(value: usize) -> *mut c_void {
        value as *mut c_void
    }

    setter!(set_u64, u64, U64, same);
    setter!(set_f32, f32, F32, same);
    setter!(set_f64, f64, F64, same);
    setter!(set_u32, u32, U32, same);
    setter!(set_i32, i32, I32, same);
    setter!(set_d3d11, *mut c_void, D3d11, addr);
    setter!(set_d3d12, *mut c_void, D3d12, addr);
    setter!(set_void, *mut c_void, Void, addr);
    getter!(get_u64, u64, U64, same);
    getter!(get_f32, f32, F32, same);
    getter!(get_f64, f64, F64, same);
    getter!(get_u32, u32, U32, same);
    getter!(get_i32, i32, I32, same);
    getter!(get_d3d11, *mut c_void, D3d11, ptr);
    getter!(get_d3d12, *mut c_void, D3d12, ptr);
    getter!(get_void, *mut c_void, Void, ptr);

    unsafe extern "win64" fn reset(this: *mut c_void) {
        // SAFETY: called through the fake's own vtable.
        let object = unsafe { fake(this) };
        object.store.borrow_mut().clear();
        *object.resets.borrow_mut() += 1;
    }

    /// The vtable MSVC builds: each overload group in reverse declaration
    /// order (`u64, f32, f64, u32, i32, D3D11, D3D12, void*` declared).
    fn msvc_vtable() -> Vec<usize> {
        let declared_set = [
            set_u64 as *const () as usize,
            set_f32 as *const () as usize,
            set_f64 as *const () as usize,
            set_u32 as *const () as usize,
            set_i32 as *const () as usize,
            set_d3d11 as *const () as usize,
            set_d3d12 as *const () as usize,
            set_void as *const () as usize,
        ];
        let declared_get = [
            get_u64 as *const () as usize,
            get_f32 as *const () as usize,
            get_f64 as *const () as usize,
            get_u32 as *const () as usize,
            get_i32 as *const () as usize,
            get_d3d11 as *const () as usize,
            get_d3d12 as *const () as usize,
            get_void as *const () as usize,
        ];
        let mut vtable: Vec<usize> = declared_set.iter().rev().copied().collect();
        vtable.extend(declared_get.iter().rev());
        vtable.push(reset as *const () as usize);
        vtable
    }

    fn with_fake(test: impl FnOnce(Parameters, &Fake)) {
        let vtable = msvc_vtable();
        let object = Fake {
            vtable: vtable.as_ptr(),
            store: RefCell::default(),
            resets: RefCell::new(0),
        };
        // SAFETY: `object` is a live stand-in with the MSVC layout.
        let parameters =
            unsafe { Parameters::from_raw(&object as *const Fake as *mut c_void) }.unwrap();
        test(parameters, &object);
    }

    fn stored(object: &Fake, name: &str) -> Option<Stored> {
        object.store.borrow().get(name).copied()
    }

    #[test]
    fn every_typed_value_lands_in_its_own_overload() {
        with_fake(|parameters, object| {
            // SAFETY: the stand-in is live for the closure.
            unsafe {
                parameters.set(c"i", Value::I32(-7));
                parameters.set(c"u", Value::U32(7));
                parameters.set(c"f", Value::F32(0.25));
                parameters.set(c"d", Value::F64(-2.5));
                parameters.set(c"l", Value::U64(1 << 40));
                parameters.set(c"r", Value::D3d12Resource(0x1234_5000));
                parameters.set(c"p", Value::VoidPointer(0x9876_0000));
            }
            assert_eq!(stored(object, "i"), Some(Stored::I32(-7)));
            assert_eq!(stored(object, "u"), Some(Stored::U32(7)));
            assert_eq!(stored(object, "f"), Some(Stored::F32(0.25)));
            assert_eq!(stored(object, "d"), Some(Stored::F64(-2.5)));
            assert_eq!(stored(object, "l"), Some(Stored::U64(1 << 40)));
            assert_eq!(stored(object, "r"), Some(Stored::D3d12(0x1234_5000)));
            assert_eq!(stored(object, "p"), Some(Stored::Void(0x9876_0000)));
        });
    }

    #[test]
    fn typed_getters_read_their_own_overload() {
        with_fake(|parameters, _| {
            // SAFETY: the stand-in is live for the closure.
            unsafe {
                parameters.apply(&[
                    Write::new(c"i", Value::I32(-7)),
                    Write::new(c"u", Value::U32(7)),
                    Write::new(c"f", Value::F32(0.25)),
                    Write::new(c"d", Value::F64(-2.5)),
                    Write::new(c"l", Value::U64(1 << 40)),
                    Write::new(c"r", Value::D3d12Resource(0x1234_5000)),
                    Write::new(c"p", Value::VoidPointer(0x9876_0000)),
                ]);
                assert_eq!(parameters.get_i32(c"i"), Some(-7));
                assert_eq!(parameters.get_u32(c"u"), Some(7));
                assert_eq!(parameters.get_f32(c"f"), Some(0.25));
                assert_eq!(parameters.get_f64(c"d"), Some(-2.5));
                assert_eq!(parameters.get_u64(c"l"), Some(1 << 40));
                assert_eq!(parameters.get_d3d12_resource(c"r"), Some(0x1234_5000));
                assert_eq!(parameters.get_void_pointer(c"p"), Some(0x9876_0000));
                // A failure status reads as None, not as a zero.
                assert_eq!(parameters.get_i32(c"missing"), None);
                assert_eq!(parameters.get_f32(c"i"), None);
            }
        });
    }

    #[test]
    fn the_slot_check_holds_on_the_msvc_layout_and_reset_clears() {
        with_fake(|parameters, object| {
            // SAFETY: the stand-in is live for the closure.
            unsafe {
                let check = parameters.verify_slots();
                assert!(check.held(), "{check:?}");
                parameters.reset();
                assert_eq!(*object.resets.borrow(), 1);
                assert_eq!(parameters.get_i32(names::PROBE_INT), None);
            }
        });
    }

    #[test]
    fn the_slot_check_catches_swapped_overloads() {
        // An int and a float overload trading places, which is what a wrong
        // slot count looks like from outside. Only same-sized types are
        // swapped so the stand-in itself never writes past an out-parameter.
        let mut vtable = msvc_vtable();
        vtable.swap(Slot::SetI32.index(), Slot::SetF32.index());
        vtable.swap(Slot::GetI32.index(), Slot::GetF32.index());
        let object = Fake {
            vtable: vtable.as_ptr(),
            store: RefCell::default(),
            resets: RefCell::new(0),
        };
        // SAFETY: the stand-in is live and each swapped entry writes exactly
        // four bytes, the size of the out-parameter it is called with.
        let check = unsafe {
            Parameters::from_raw(&object as *const Fake as *mut c_void)
                .unwrap()
                .verify_slots()
        };
        assert!(!check.held(), "{check:?}");
    }
}
