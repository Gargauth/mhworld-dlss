//! Resource tags: what is handed to `slSetTagForFrame`, with explicit
//! states.
//!
//! Every tag is `eValidUntilPresent`: the guide asks for that first, since
//! the other lifecycles make Streamline copy. It obliges the caller to
//! leave the resource untouched until the frame is presented, and makes the
//! `state` the one the resource is in **when Streamline reads it, at
//! Present** — the header calls the state mandatory, and a wrong one is
//! undefined behaviour rather than an error code. A resource whose state is
//! not known must not be tagged.

use std::ffi::c_void;

use mhw_core::frame::Size;

use crate::abi;

/// What a tagged resource is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum TagKind {
    /// `kBufferTypeDepth`: the engine's (inverted) depth at render size.
    Depth,
    /// `kBufferTypeMotionVectors`: decoded vectors at render size.
    MotionVectors,
    /// `kBufferTypeHUDLessColor`: the finished frame before the interface,
    /// in the back buffer's format and colour space, display size.
    HudLess,
    /// `kBufferTypeUIAlpha`: one channel of interface opacity.
    UiAlpha,
}

impl TagKind {
    /// Every kind, in [`TagKind::slot`] order.
    pub const ALL: [TagKind; 4] = [
        TagKind::Depth,
        TagKind::MotionVectors,
        TagKind::HudLess,
        TagKind::UiAlpha,
    ];

    /// `sl::BufferType`.
    pub const fn buffer_type(self) -> abi::BufferType {
        match self {
            TagKind::Depth => abi::BUFFER_DEPTH,
            TagKind::MotionVectors => abi::BUFFER_MOTION_VECTORS,
            TagKind::HudLess => abi::BUFFER_HUDLESS_COLOR,
            TagKind::UiAlpha => abi::BUFFER_UI_ALPHA,
        }
    }

    /// Index into per-kind arrays.
    pub const fn slot(self) -> usize {
        match self {
            TagKind::Depth => 0,
            TagKind::MotionVectors => 1,
            TagKind::HudLess => 2,
            TagKind::UiAlpha => 3,
        }
    }

    /// Short name for logs.
    pub const fn label(self) -> &'static str {
        match self {
            TagKind::Depth => "depth",
            TagKind::MotionVectors => "mvec",
            TagKind::HudLess => "hudless",
            TagKind::UiAlpha => "ui-alpha",
        }
    }
}

/// One resource to tag. Borrowed: no reference is taken.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagResource {
    /// The `ID3D12Resource`.
    pub resource: *mut c_void,
    /// `D3D12_RESOURCE_STATES` the resource is in when Streamline reads it
    /// (at `Present`).
    pub state: u32,
    /// Its size in pixels (the whole resource is used).
    pub size: Size,
    /// Its `DXGI_FORMAT`.
    pub format: u32,
}

/// Why a tag was not built.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TagError {
    /// Null resource.
    Null,
    /// Zero width or height.
    Empty,
}

impl TagResource {
    /// Check what can be checked without the GPU.
    pub fn validate(&self) -> Result<(), TagError> {
        if self.resource.is_null() {
            Err(TagError::Null)
        } else if self.size.is_empty() {
            Err(TagError::Empty)
        } else {
            Ok(())
        }
    }

    /// The `sl::Resource` describing it.
    pub fn to_abi(&self) -> abi::Resource {
        abi::Resource {
            base: abi::Base::new(abi::RESOURCE, 1),
            kind: abi::RESOURCE_TEX2D,
            native: self.resource,
            memory: std::ptr::null_mut(),
            view: std::ptr::null_mut(),
            state: self.state,
            width: self.size.w,
            height: self.size.h,
            native_format: self.format,
            mip_levels: 1,
            array_layers: 1,
            gpu_virtual_address: 0,
            flags: 0,
            usage: 0,
            internal_flags: 0,
            reserved: 0,
        }
    }
}

/// A `sl::ResourceTag` for `kind` pointing at `resource`, or removing the
/// tag when `resource` is null ("set to null to remove the specified tag").
pub fn resource_tag(kind: TagKind, resource: *mut abi::Resource, size: Size) -> abi::ResourceTag {
    abi::ResourceTag {
        base: abi::Base::new(abi::RESOURCE_TAG, 1),
        resource,
        kind: kind.buffer_type(),
        lifecycle: abi::LIFECYCLE_VALID_UNTIL_PRESENT,
        extent: if resource.is_null() {
            abi::Extent::default()
        } else {
            abi::Extent {
                top: 0,
                left: 0,
                width: size.w,
                height: size.h,
            }
        },
    }
}

/// What was last filed for one kind, for introspection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TagRecord {
    /// Address of the resource; 0 for a removal.
    pub resource: usize,
    /// State asserted.
    pub state: u32,
    /// Size.
    pub size: Size,
    /// `DXGI_FORMAT`.
    pub format: u32,
    /// Token index it was filed under.
    pub index: u32,
    /// Whether `slSetTagForFrame` returned `eOk`.
    pub accepted: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn depth() -> TagResource {
        TagResource {
            resource: 0x1000 as *mut c_void,
            state: 0x80 | 0x40,
            size: Size::new(1707, 960),
            format: 40,
        }
    }

    #[test]
    fn kinds_map_to_the_header_buffer_types() {
        assert_eq!(TagKind::Depth.buffer_type(), 0);
        assert_eq!(TagKind::MotionVectors.buffer_type(), 1);
        assert_eq!(TagKind::HudLess.buffer_type(), 2);
        assert_eq!(TagKind::UiAlpha.buffer_type(), 69);
        for (i, kind) in TagKind::ALL.iter().enumerate() {
            assert_eq!(kind.slot(), i);
        }
    }

    #[test]
    fn resources_carry_their_explicit_state() {
        let r = depth().to_abi();
        assert_eq!(r.state, 0xc0);
        assert_eq!((r.width, r.height, r.native_format), (1707, 960, 40));
        assert_eq!(r.base.kind, abi::RESOURCE);
        assert_eq!(r.kind, abi::RESOURCE_TEX2D);
    }

    #[test]
    fn tags_are_valid_until_present_and_nulls_remove() {
        let mut r = depth().to_abi();
        let tag = resource_tag(TagKind::Depth, &mut r, depth().size);
        assert_eq!(tag.lifecycle, abi::LIFECYCLE_VALID_UNTIL_PRESENT);
        assert_eq!(tag.extent.width, 1707);
        let removal = resource_tag(TagKind::HudLess, std::ptr::null_mut(), Size::new(1, 1));
        assert!(removal.resource.is_null());
        assert_eq!(removal.extent, abi::Extent::default());
        assert_eq!(removal.kind, abi::BUFFER_HUDLESS_COLOR);
    }

    #[test]
    fn unusable_resources_are_refused() {
        assert_eq!(depth().validate(), Ok(()));
        let null = TagResource {
            resource: std::ptr::null_mut(),
            ..depth()
        };
        assert_eq!(null.validate(), Err(TagError::Null));
        let empty = TagResource {
            size: Size::new(0, 960),
            ..depth()
        };
        assert_eq!(empty.validate(), Err(TagError::Empty));
    }
}
