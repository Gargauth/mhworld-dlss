//! Root signature layout, read rather than guessed.
//!
//! A root descriptor table arrives at the command list as a single GPU handle.
//! Which descriptor in it is `t2` is decided entirely by the root signature:
//! a table may contain several ranges, each placed at its own
//! `OffsetInDescriptorsFromTableStart`. The capture that prompted this module
//! showed exactly that -- the UAV sat at offset 0 and the six SRVs began at
//! offset 9, so the obvious "registers follow the base in order" reading was
//! wrong and would have mislabelled every input.
//!
//! D3D12 ships a deserialiser for the signature blob, so the layout can simply
//! be read back at creation time.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::ffi::c_void;
use windows::core::Interface;
use windows::Win32::Graphics::Direct3D12::{
    D3D12CreateVersionedRootSignatureDeserializer, ID3D12VersionedRootSignatureDeserializer,
    D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND, D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
};

/// One descriptor range of a root descriptor table.
#[derive(Clone, Copy)]
pub struct Range {
    /// `D3D12_DESCRIPTOR_RANGE_TYPE`: 0 SRV, 1 UAV, 2 CBV, 3 sampler.
    pub kind: i32,
    pub count: u32,
    pub base_register: u32,
    /// Offset in descriptors from the start of the table.
    pub offset: u32,
}

impl Range {
    /// The HLSL register prefix for this range's type.
    pub fn prefix(&self) -> char {
        match self.kind {
            0 => 't',
            1 => 'u',
            2 => 'b',
            3 => 's',
            _ => '?',
        }
    }
}

#[derive(Default, Clone)]
pub struct Layout {
    /// Root parameter index -> its table ranges. Non-table parameters are
    /// absent, which is how a root CBV is told from a table.
    pub tables: HashMap<u32, Vec<Range>>,
    /// Root parameter index -> the constant buffer register it supplies.
    ///
    /// Needed to find CBViewProjection, which the temporal pass binds at b0.
    /// Taking the most recently bound buffer that merely looks like a
    /// projection instead picked a shadow cascade -- 23 degrees of field of
    /// view against the camera's 83 -- and offset the wrong view.
    pub root_cbvs: HashMap<u32, u32>,
}

static LAYOUTS: Mutex<Option<HashMap<usize, Layout>>> = Mutex::new(None);

/// Deserialise a root signature blob and remember its table layout.
///
/// # Safety
/// `blob` must point to `len` readable bytes holding a serialised root
/// signature, and `signature` must be the object it produced.
pub unsafe fn record(signature: *mut c_void, blob: *const c_void, len: usize) {
    if signature.is_null() || blob.is_null() || len == 0 {
        return;
    }

    let mut raw: *mut c_void = std::ptr::null_mut();
    if D3D12CreateVersionedRootSignatureDeserializer(
        blob,
        len,
        &ID3D12VersionedRootSignatureDeserializer::IID,
        &mut raw,
    )
    .is_err()
        || raw.is_null()
    {
        return;
    }
    let deserializer = ID3D12VersionedRootSignatureDeserializer::from_raw(raw);

    let versioned = deserializer.GetUnconvertedRootSignatureDesc();
    if versioned.is_null() {
        return;
    }
    let versioned = &*versioned;

    let mut layout = Layout::default();
    // D3D_ROOT_SIGNATURE_VERSION: 1 is 1.0, 2 is 1.1, 3 is 1.2. Matched
    // numerically because the named constants are not exported here, and a
    // constant in a match arm would bind rather than compare.
    match versioned.Version.0 {
        1 => {
            let desc = &versioned.Anonymous.Desc_1_0;
            for index in 0..desc.NumParameters {
                let parameter = &*desc.pParameters.add(index as usize);
                if parameter.ParameterType
                    == windows::Win32::Graphics::Direct3D12::D3D12_ROOT_PARAMETER_TYPE_CBV
                {
                    layout
                        .root_cbvs
                        .insert(index, parameter.Anonymous.Descriptor.ShaderRegister);
                    continue;
                }
                if parameter.ParameterType != D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE {
                    continue;
                }
                let table = &parameter.Anonymous.DescriptorTable;
                let mut ranges = Vec::new();
                let mut running = 0u32;
                for slot in 0..table.NumDescriptorRanges {
                    let range = &*table.pDescriptorRanges.add(slot as usize);
                    let offset = if range.OffsetInDescriptorsFromTableStart
                        == D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND
                    {
                        running
                    } else {
                        range.OffsetInDescriptorsFromTableStart
                    };
                    running = offset + range.NumDescriptors;
                    ranges.push(Range {
                        kind: range.RangeType.0,
                        count: range.NumDescriptors,
                        base_register: range.BaseShaderRegister,
                        offset,
                    });
                }
                layout.tables.insert(index, ranges);
            }
        }
        2 | 3 => {
            let desc = &versioned.Anonymous.Desc_1_1;
            for index in 0..desc.NumParameters {
                let parameter = &*desc.pParameters.add(index as usize);
                if parameter.ParameterType
                    == windows::Win32::Graphics::Direct3D12::D3D12_ROOT_PARAMETER_TYPE_CBV
                {
                    layout
                        .root_cbvs
                        .insert(index, parameter.Anonymous.Descriptor.ShaderRegister);
                    continue;
                }
                if parameter.ParameterType != D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE {
                    continue;
                }
                let table = &parameter.Anonymous.DescriptorTable;
                let mut ranges = Vec::new();
                let mut running = 0u32;
                for slot in 0..table.NumDescriptorRanges {
                    let range = &*table.pDescriptorRanges.add(slot as usize);
                    let offset = if range.OffsetInDescriptorsFromTableStart
                        == D3D12_DESCRIPTOR_RANGE_OFFSET_APPEND
                    {
                        running
                    } else {
                        range.OffsetInDescriptorsFromTableStart
                    };
                    running = offset + range.NumDescriptors;
                    ranges.push(Range {
                        kind: range.RangeType.0,
                        count: range.NumDescriptors,
                        base_register: range.BaseShaderRegister,
                        offset,
                    });
                }
                layout.tables.insert(index, ranges);
            }
        }
        other => {
            crate::log!("[rsig] unhandled root signature version {other}");
            return;
        }
    }

    let summary: Vec<String> = layout
        .tables
        .iter()
        .map(|(root, ranges)| {
            let listed: Vec<String> = ranges
                .iter()
                .map(|r| {
                    format!(
                        "{}{}..{}@+{}",
                        r.prefix(),
                        r.base_register,
                        r.base_register + r.count - 1,
                        r.offset
                    )
                })
                .collect();
            format!("root{root}[{}]", listed.join(" "))
        })
        .collect();
    crate::log!("[rsig] {signature:p} {}", summary.join(" "));

    LAYOUTS
        .lock()
        .get_or_insert_with(HashMap::new)
        .insert(signature as usize, layout);
}

/// The recorded layout of a root signature, if it was created while hooked.
pub fn layout_of(signature: usize) -> Option<Layout> {
    LAYOUTS.lock().as_ref()?.get(&signature).cloned()
}
