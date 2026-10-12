//! The `NVSDK_NGX_Parameter` ABI: vtable slots, typed values and parameter
//! names.
//!
//! `NVSDK_NGX_Parameter` is a C++ object (`nvsdk_ngx_params.h`): eight `Set`
//! overloads `(u64, f32, f64, u32, i32, ID3D11Resource*, ID3D12Resource*,
//! void*)`, the same eight as `Get`, then `Reset`. MSVC lays out the
//! overloads of one name in **reverse** declaration order, so the slots are
//! `void*, ID3D12Resource*, ID3D11Resource*, i32, u32, f64, f32, u64` for
//! each group. The game's own wrappers confirm every offset
//! (`docs/re/ReDlss1Path.md` §2.1, e.g. `SetD3d12Resource` = `[vtbl+8]` at
//! `0x14273e741`). Calling a declared-order slot is not caught by the callee:
//! the 0.2 code once wrote a float through the `ID3D12Resource*` setter and
//! read back 5.6e-11, because the float travels in XMM2 and that overload
//! reads R8.
//!
//! This module is pure; [`Parameters`] (x86-64 only) makes the calls.

use std::ffi::CStr;

pub mod names;
#[cfg(target_arch = "x86_64")]
mod object;

#[cfg(target_arch = "x86_64")]
pub use object::{Parameters, SlotCheck};

/// A slot of the `NVSDK_NGX_Parameter` vtable, in MSVC layout.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(usize)]
pub enum Slot {
    /// `Set(const char*, void*)`.
    SetVoidPointer = 0,
    /// `Set(const char*, ID3D12Resource*)`.
    SetD3d12Resource = 1,
    /// `Set(const char*, ID3D11Resource*)`.
    SetD3d11Resource = 2,
    /// `Set(const char*, int)`.
    SetI32 = 3,
    /// `Set(const char*, unsigned int)`.
    SetU32 = 4,
    /// `Set(const char*, double)`.
    SetF64 = 5,
    /// `Set(const char*, float)`.
    SetF32 = 6,
    /// `Set(const char*, unsigned long long)`.
    SetU64 = 7,
    /// `Get(const char*, void**)`.
    GetVoidPointer = 8,
    /// `Get(const char*, ID3D12Resource**)`.
    GetD3d12Resource = 9,
    /// `Get(const char*, ID3D11Resource**)`.
    GetD3d11Resource = 10,
    /// `Get(const char*, int*)`.
    GetI32 = 11,
    /// `Get(const char*, unsigned int*)`.
    GetU32 = 12,
    /// `Get(const char*, double*)`.
    GetF64 = 13,
    /// `Get(const char*, float*)`.
    GetF32 = 14,
    /// `Get(const char*, unsigned long long*)`.
    GetU64 = 15,
    /// `Reset()`.
    Reset = 16,
}

impl Slot {
    /// Index into the vtable.
    pub const fn index(self) -> usize {
        self as usize
    }

    /// Byte offset into the vtable, as it appears in disassembly.
    pub const fn offset(self) -> usize {
        self.index() * std::mem::size_of::<u64>()
    }
}

/// A value to store in a parameter object, typed by the overload it goes
/// through.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Value {
    /// `int` (`SetI`).
    I32(i32),
    /// `unsigned int` (`SetUI`).
    U32(u32),
    /// `float` (`SetF`).
    F32(f32),
    /// `double` (`SetD`).
    F64(f64),
    /// `unsigned long long` (`SetULL`).
    U64(u64),
    /// `ID3D12Resource*`, as an address; 0 clears the entry.
    D3d12Resource(usize),
    /// `void*`, as an address.
    VoidPointer(usize),
}

impl Value {
    /// The setter this value is stored through.
    pub const fn set_slot(self) -> Slot {
        match self {
            Value::I32(_) => Slot::SetI32,
            Value::U32(_) => Slot::SetU32,
            Value::F32(_) => Slot::SetF32,
            Value::F64(_) => Slot::SetF64,
            Value::U64(_) => Slot::SetU64,
            Value::D3d12Resource(_) => Slot::SetD3d12Resource,
            Value::VoidPointer(_) => Slot::SetVoidPointer,
        }
    }
}

/// One `Set` call: a parameter name and its value.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Write {
    /// The parameter name (`nvsdk_ngx_defs.h`).
    pub name: &'static CStr,
    /// The value and, by its variant, the overload.
    pub value: Value,
}

impl Write {
    /// `name = value`.
    pub const fn new(name: &'static CStr, value: Value) -> Write {
        Write { name, value }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Vtable offsets observed in the game's `NVSDK_NGX_Parameter_*`
    /// wrappers (`docs/re/ReDlss1Path.md` §2.1).
    const OBSERVED: [(Slot, usize); 17] = [
        (Slot::SetVoidPointer, 0x00),
        (Slot::SetD3d12Resource, 0x08),
        (Slot::SetD3d11Resource, 0x10),
        (Slot::SetI32, 0x18),
        (Slot::SetU32, 0x20),
        (Slot::SetF64, 0x28),
        (Slot::SetF32, 0x30),
        (Slot::SetU64, 0x38),
        (Slot::GetVoidPointer, 0x40),
        (Slot::GetD3d12Resource, 0x48),
        (Slot::GetD3d11Resource, 0x50),
        (Slot::GetI32, 0x58),
        (Slot::GetU32, 0x60),
        (Slot::GetF64, 0x68),
        (Slot::GetF32, 0x70),
        (Slot::GetU64, 0x78),
        (Slot::Reset, 0x80),
    ];

    #[test]
    fn slots_match_the_offsets_in_the_games_wrappers() {
        for (slot, offset) in OBSERVED {
            assert_eq!(slot.offset(), offset, "{slot:?}");
        }
        // The 0.2 constants, confirmed by the RE.
        assert_eq!(Slot::GetI32.index(), 11);
        assert_eq!(Slot::GetU32.index(), 12);
        assert_eq!(Slot::SetI32.index(), 3);
        assert_eq!(Slot::SetU32.index(), 4);
    }

    #[test]
    fn each_group_is_the_declared_order_reversed() {
        // nvsdk_ngx_params.h declares: u64, f32, f64, u32, i32, D3D11, D3D12, void*.
        let declared_set = [
            Slot::SetU64,
            Slot::SetF32,
            Slot::SetF64,
            Slot::SetU32,
            Slot::SetI32,
            Slot::SetD3d11Resource,
            Slot::SetD3d12Resource,
            Slot::SetVoidPointer,
        ];
        let declared_get = [
            Slot::GetU64,
            Slot::GetF32,
            Slot::GetF64,
            Slot::GetU32,
            Slot::GetI32,
            Slot::GetD3d11Resource,
            Slot::GetD3d12Resource,
            Slot::GetVoidPointer,
        ];
        for (position, slot) in declared_set.iter().enumerate() {
            assert_eq!(slot.index(), 7 - position, "{slot:?}");
        }
        for (position, slot) in declared_get.iter().enumerate() {
            assert_eq!(slot.index(), 8 + 7 - position, "{slot:?}");
        }
    }

    #[test]
    fn values_go_through_the_setter_of_their_type() {
        assert_eq!(Value::I32(-1).set_slot(), Slot::SetI32);
        assert_eq!(Value::U32(1).set_slot(), Slot::SetU32);
        assert_eq!(Value::F32(0.5).set_slot(), Slot::SetF32);
        assert_eq!(Value::F64(0.5).set_slot(), Slot::SetF64);
        assert_eq!(Value::U64(1).set_slot(), Slot::SetU64);
        assert_eq!(
            Value::D3d12Resource(0x1000).set_slot(),
            Slot::SetD3d12Resource
        );
        assert_eq!(Value::VoidPointer(0x1000).set_slot(), Slot::SetVoidPointer);
    }
}
