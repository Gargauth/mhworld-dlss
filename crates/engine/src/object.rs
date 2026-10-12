//! Bounds- and page-checked access to game objects.
//!
//! An [`Object`] is a base address and an extent whose pages were found
//! committed (readable, and writable for [`Object::writable`]) when it was
//! made. Field reads and writes are then bounds-checked against the extent
//! and never panic. The check cannot close the window in which the game
//! frees the object afterwards (see `mhw_win::mem`): objects are made fresh
//! for every hook call, except the engine's singletons, whose pointers are
//! re-validated whenever they change ([`Validated`]).

use std::sync::atomic::{AtomicUsize, Ordering};

use mhw_win::mem;

/// Types any bit pattern of which is a valid value.
pub(crate) trait Plain: Copy {}

impl Plain for u8 {}
impl Plain for u16 {}
impl Plain for u32 {}
impl Plain for u64 {}
impl Plain for usize {}
impl Plain for i32 {}
impl Plain for f32 {}

/// A checked view of game memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Object {
    base: usize,
    len: usize,
    writable: bool,
}

impl Object {
    /// `len` readable bytes at `base`; `None` for null or unreadable.
    pub(crate) fn readable(base: usize, len: usize) -> Option<Object> {
        (base != 0 && mem::is_readable(base, len)).then_some(Object {
            base,
            len,
            writable: false,
        })
    }

    /// `len` readable and writable bytes at `base`.
    pub(crate) fn writable(base: usize, len: usize) -> Option<Object> {
        (base != 0 && mem::is_readable(base, len) && mem::is_writable(base, len)).then_some(
            Object {
                base,
                len,
                writable: true,
            },
        )
    }

    /// The base address.
    pub(crate) fn base(&self) -> usize {
        self.base
    }

    fn span(&self, offset: usize, size: usize) -> Option<usize> {
        let end = offset.checked_add(size)?;
        (end <= self.len).then(|| self.base + offset)
    }

    /// The `T` at `offset`, if inside the extent.
    pub(crate) fn get<T: Plain>(&self, offset: usize) -> Option<T> {
        let address = self.span(offset, std::mem::size_of::<T>())?;
        // SAFETY: inside the extent checked readable when the object was
        // made (module docs for the remaining window); `T` is `Plain`.
        Some(unsafe { (address as *const T).read_unaligned() })
    }

    /// Write `value` at `offset`; `false` outside the extent or for a
    /// read-only object.
    pub(crate) fn put<T: Plain>(&self, offset: usize, value: T) -> bool {
        let Some(address) = self.span(offset, std::mem::size_of::<T>()) else {
            return false;
        };
        if !self.writable {
            return false;
        }
        // SAFETY: inside the extent checked writable when the object was
        // made; the fields written are plain data the engine reads.
        unsafe { (address as *mut T).write_unaligned(value) };
        true
    }

    /// Copy `out.len()` bytes from `offset`.
    pub(crate) fn copy(&self, offset: usize, out: &mut [u8]) -> bool {
        let Some(address) = self.span(offset, out.len()) else {
            return false;
        };
        // SAFETY: inside the extent checked readable; `out` is ours.
        unsafe { std::ptr::copy_nonoverlapping(address as *const u8, out.as_mut_ptr(), out.len()) };
        true
    }

    /// The address of the field at `offset`, if inside the extent.
    pub(crate) fn address_of(&self, offset: usize, size: usize) -> Option<usize> {
        self.span(offset, size)
    }

    /// The pointer stored at `offset`, as a fresh object of `len` bytes.
    pub(crate) fn follow(&self, offset: usize, len: usize, writable: bool) -> Option<Object> {
        let pointer = self.get::<usize>(offset)?;
        if writable {
            Object::writable(pointer, len)
        } else {
            Object::readable(pointer, len)
        }
    }
}

/// The pointer a singleton was last validated at, so that a hot path does
/// not query the pages of an unchanged pointer again.
#[derive(Debug)]
pub(crate) struct Validated(AtomicUsize);

impl Validated {
    /// Nothing validated yet.
    pub(crate) const fn new() -> Validated {
        Validated(AtomicUsize::new(0))
    }

    /// The writable object at `pointer`, checked only when the pointer
    /// differs from the last one validated.
    pub(crate) fn object(&self, pointer: usize, len: usize) -> Option<Object> {
        if pointer == 0 {
            return None;
        }
        if self.0.load(Ordering::Acquire) == pointer {
            return Some(Object {
                base: pointer,
                len,
                writable: true,
            });
        }
        let object = Object::writable(pointer, len)?;
        self.0.store(pointer, Ordering::Release);
        Some(object)
    }
}

/// The pointer stored in a global slot (the slot is in the image's data,
/// checked readable on every call: one region query).
pub(crate) fn slot_value(slot: usize) -> Option<usize> {
    // SAFETY: a pointer-sized read of image data, checked readable.
    unsafe { mem::read::<usize>(slot) }.filter(|pointer| *pointer != 0)
}
