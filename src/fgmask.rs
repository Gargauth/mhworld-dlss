//! A constant, one-channel mask to hand DLSS-G, for finding out what two of
//! its inputs do before anything is built on them.
//!
//! The artefact it is for is a world-space label -- the nameplate over an NPC
//! -- that ghosts when the camera pans. Two inputs might address it and
//! neither is documented well enough to build on:
//!
//! * `kBufferTypeNoWarpMask` (54), *"Mask for pixels to skip warping"*. That
//!   one line in `sl_core_types.h` is all the documentation there is, and
//!   neither programming guide mentions it. Whether the plugin reads it, what
//!   size and format it wants, and what a pixel that was not warped looks
//!   like in a generated frame, are all unmeasured.
//! * `kBufferTypeUIAlpha` (69), a **single channel of opacity and nothing
//!   else**. It is not `kBufferTypeUIColorAndAlpha` (23), which needs the
//!   interface's colour as well. The alpha-only tag needs coverage alone,
//!   which could in principle be derived from the difference between the
//!   HUD-less capture and the finished interface buffer.
//!
//! Deriving a per-pixel mask is a compute pass at the end of the interface,
//! which is real machinery. This is the experiment that says whether it is
//! worth building: a texture cleared to a constant, optionally only inside a
//! centred box, tagged as one or the other. No shader, no descriptor heap
//! bound, nothing of the engine's read or changed -- a clear and two barriers
//! on a texture of our own.
//!
//! The box is what makes it a preview rather than only a liveness test. With
//! the label in the middle of the screen and the mask set inside a central
//! box, the label gets the treatment a derived mask would give it while the
//! scene outside the box is interpolated as today, and the scene *inside* the
//! box shows what the treatment costs.

use parking_lot::Mutex;
use std::ffi::c_void;
use windows::core::Interface;
use windows::Win32::Foundation::RECT;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::{DXGI_FORMAT_R8_UNORM, DXGI_SAMPLE_DESC};

/// `R8_UNORM`, 61. One channel, which is what both tags describe.
pub const FORMAT: u32 = DXGI_FORMAT_R8_UNORM.0 as u32;

/// The state the mask rests in, and the state it is tagged with. The guide
/// asks for `NON_PIXEL_SHADER_RESOURCE` on the snippet's inputs; both read
/// bits are set so either kind of read is legal.
pub const READ_STATE: D3D12_RESOURCE_STATES = D3D12_RESOURCE_STATES(
    D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE.0 | D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE.0,
);

struct Mask {
    texture: ID3D12Resource,
    /// Held because the view lives in it.
    _heap: ID3D12DescriptorHeap,
    view: D3D12_CPU_DESCRIPTOR_HANDLE,
    size: (u32, u32),
    /// What the texture was last cleared to, as recorded on a list: the value
    /// and the box, bit for bit. `None` until the first clear.
    painted: Option<(u32, u32)>,
}

unsafe impl Send for Mask {}
unsafe impl Sync for Mask {}

/// One mask, never released while the session lives: a tag naming it is valid
/// until Present, on a queue that is not ours, so dropping it on a size change
/// would be a use-after-free decided by timing.
static MASKS: Mutex<Vec<Mask>> = Mutex::new(Vec::new());

/// What `paint` did, for the caller to log as an observation.
pub struct Painted {
    pub resource: *mut c_void,
    pub size: (u32, u32),
    /// The rectangle cleared to the value, in pixels: left, top, right, bottom.
    pub region: (i32, i32, i32, i32),
    /// Whether a clear was recorded on the list by this call. False means the
    /// texture already held this content from an earlier frame.
    pub recorded: bool,
}

/// Make sure the mask holds `value` inside a centred box covering `boxed` of
/// each dimension and zero outside it, recording the clear on `list` if it
/// does not already, or if `again` asks for the clear to be repeated.
///
/// # Safety
/// `list` must be an open `ID3D12GraphicsCommandList` that will execute before
/// the frame is presented.
pub unsafe fn paint(
    list: *mut c_void,
    size: (u32, u32),
    value: f32,
    boxed: f32,
    again: bool,
) -> Result<Painted, String> {
    if size.0 == 0 || size.1 == 0 {
        return Err("the display size is not known".into());
    }
    let recorder = ID3D12GraphicsCommandList::from_raw_borrowed(&list)
        .ok_or_else(|| String::from("no command list to record the clear on"))?;
    let mut masks = MASKS.lock();
    let index = match masks.iter().position(|mask| mask.size == size) {
        Some(index) => index,
        None => {
            let device = crate::bindings::device()
                .ok_or_else(|| String::from("the engine's device is not known"))?;
            masks.push(build(&device, size).map_err(|error| format!("{error}"))?);
            masks.len() - 1
        }
    };
    let mask = &mut masks[index];

    let value = value.clamp(0.0, 1.0);
    let boxed = boxed.clamp(0.0, 1.0);
    let region = region(size, boxed);
    let key = (value.to_bits(), boxed.to_bits());
    let recorded = again || mask.painted != Some(key);
    if recorded {
        recorder.ResourceBarrier(&[transition(
            &mask.texture,
            READ_STATE,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        )]);
        // Zero everywhere, then the value inside the box. A clear does not
        // touch the output-merger bindings, so the engine's own targets are
        // still bound when it returns.
        recorder.ClearRenderTargetView(mask.view, &[0.0, 0.0, 0.0, 0.0], None);
        if region.2 > region.0 && region.3 > region.1 {
            let rect = RECT {
                left: region.0,
                top: region.1,
                right: region.2,
                bottom: region.3,
            };
            recorder.ClearRenderTargetView(mask.view, &[value, 0.0, 0.0, 0.0], Some(&[rect]));
        }
        recorder.ResourceBarrier(&[transition(
            &mask.texture,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            READ_STATE,
        )]);
        mask.painted = Some(key);
    }
    Ok(Painted {
        resource: mask.texture.as_raw(),
        size,
        region,
        recorded,
    })
}

/// The centred rectangle covering `boxed` of each dimension, in pixels: left,
/// top, right, bottom. Rounded outwards and kept inside the texture.
fn region(size: (u32, u32), boxed: f32) -> (i32, i32, i32, i32) {
    let boxed = boxed.clamp(0.0, 1.0);
    let half = (size.0 as f32 * boxed / 2.0, size.1 as f32 * boxed / 2.0);
    let centre = (size.0 as f32 / 2.0, size.1 as f32 / 2.0);
    (
        (centre.0 - half.0).floor().max(0.0) as i32,
        (centre.1 - half.1).floor().max(0.0) as i32,
        (centre.0 + half.0).ceil().min(size.0 as f32) as i32,
        (centre.1 + half.1).ceil().min(size.1 as f32) as i32,
    )
}

unsafe fn build(device: &ID3D12Device, size: (u32, u32)) -> windows::core::Result<Mask> {
    let heap_properties = D3D12_HEAP_PROPERTIES {
        Type: D3D12_HEAP_TYPE_DEFAULT,
        ..Default::default()
    };
    let description = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Alignment: 0,
        Width: size.0 as u64,
        Height: size.1,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R8_UNORM,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
        Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET,
    };
    let mut texture: Option<ID3D12Resource> = None;
    device.CreateCommittedResource(
        &heap_properties,
        D3D12_HEAP_FLAG_NONE,
        &description,
        READ_STATE,
        None,
        &mut texture,
    )?;
    let texture = texture.ok_or_else(windows::core::Error::from_win32)?;
    let heap: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
        Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
        NumDescriptors: 1,
        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
        NodeMask: 0,
    })?;
    let view = heap.GetCPUDescriptorHandleForHeapStart();
    device.CreateRenderTargetView(&texture, None, view);
    Ok(Mask {
        texture,
        _heap: heap,
        view,
        size,
        painted: None,
    })
}

unsafe fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: std::mem::transmute_copy(resource),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::region;

    #[test]
    fn the_whole_screen_is_the_whole_texture() {
        assert_eq!(region((3840, 2160), 1.0), (0, 0, 3840, 2160));
        // Out of range is clamped rather than trusted.
        assert_eq!(region((3840, 2160), 7.0), (0, 0, 3840, 2160));
    }

    #[test]
    fn a_box_is_centred_and_inside() {
        assert_eq!(region((3840, 2160), 0.5), (960, 540, 2880, 1620));
        let (left, top, right, bottom) = region((2561, 1440), 0.3);
        assert!(left > 0 && top > 0 && right < 2561 && bottom < 1440);
        assert_eq!(left + right, 2561);
        assert_eq!(top + bottom, 1440);
    }

    #[test]
    fn nothing_is_an_empty_rectangle() {
        let (left, top, right, bottom) = region((3840, 2160), 0.0);
        assert!(right <= left && bottom <= top);
    }
}
