//! Dev-only capture service: copies of named resources and GPU colour
//! statistics, read back without stalling the engine.
//!
//! Copies are recorded on the engine's open list (moving each resource from
//! its stated state to `COPY_SOURCE` and back), into readback buffers of
//! this crate. When the engine submits that list, a fence of this crate is
//! signalled on the same queue right behind it; [`poll`] hands back the
//! copies whose fence value has completed, oldest first, and never waits.
//!
//! [`record_stats`] runs a reduction compute pass over a colour resource on
//! the engine's list (state restored as for every recording here): per
//! 16×16 group sums, minima, maxima and counts, and a 258-bin histogram of
//! Rec. 709 luma with eight bins per octave from 2^-16 to 2^16 (plus one
//! below and one above). The CPU finishes the reduction ([`stats::reduce`])
//! and the percentiles come from the histogram ([`stats::percentile`]).
//!
//! Windows-only entry points: `record`, `poll`, `record_stats`,
//! `poll_stats`.

use std::ffi::c_void;

pub mod readback;
pub mod stats;

#[cfg(windows)]
pub use crate::sys::capture::{poll, poll_stats, record, record_stats};

/// One resource to copy back. `state` is its current state on `list`.
#[derive(Debug, Clone)]
pub struct Item {
    /// A name for the result (`colour`, `depth`, …).
    pub name: String,
    /// `ID3D12Resource*`.
    pub resource: *mut c_void,
    /// `D3D12_RESOURCE_STATES` it is in at this point of the list.
    pub state: u32,
}

/// A completed copy.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Captured {
    /// The request it belongs to.
    pub request: u64,
    /// The item's name.
    pub name: String,
    /// `DXGI_FORMAT` of the bytes (a depth-stencil resource's depth plane
    /// is reported in its copyable format; `0` for a buffer).
    pub format: u32,
    /// Width in pixels (bytes for a buffer).
    pub width: u32,
    /// Height in rows.
    pub height: u32,
    /// Bytes from one row to the next in `bytes` (256-aligned for
    /// textures).
    pub row_pitch: u32,
    /// The frame given to `record`.
    pub frame: u64,
    /// `row_pitch × height` bytes, mip 0 / plane 0.
    pub bytes: Vec<u8>,
}

impl Captured {
    /// Row `y` without its padding, if the format's pixel size is known.
    pub fn row(&self, y: u32) -> Option<&[u8]> {
        let bpp = if self.format == 0 {
            1
        } else {
            readback::bytes_per_pixel(self.format)?
        };
        readback::row(&self.bytes, self.row_pitch, self.width.checked_mul(bpp)?, y)
    }
}

/// Colour statistics of one resource (linear values as stored).
#[derive(Debug, Clone, Copy, PartialEq, Default)]
pub struct ColourStats {
    /// Per-channel minimum over finite pixels (0 when there are none).
    pub min: [f32; 3],
    /// Per-channel maximum over finite pixels.
    pub max: [f32; 3],
    /// Per-channel mean over finite pixels.
    pub mean: [f32; 3],
    /// Median Rec. 709 luma.
    pub p50: f32,
    /// 99th percentile luma.
    pub p99: f32,
    /// 99.9th percentile luma.
    pub p999: f32,
    /// Fraction of all pixels with max channel > 1.0.
    pub above_one: f32,
    /// Pixels with a NaN or infinite channel.
    pub nan_or_inf: u64,
}

/// Why a capture was not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureError {
    /// Not possible yet (no device seen, or too much already in flight).
    NotReady,
    /// The resource cannot be captured this way; the text says why.
    Unsupported(String),
    /// A D3D12 call failed; the text says which.
    Device(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::NotReady => f.write_str("capture not ready"),
            CaptureError::Unsupported(why) => write!(f, "unsupported: {why}"),
            CaptureError::Device(why) => write!(f, "device: {why}"),
        }
    }
}

impl std::error::Error for CaptureError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_are_read_without_padding() {
        let captured = Captured {
            request: 1,
            name: "colour".into(),
            format: 28,
            width: 3,
            height: 2,
            row_pitch: 256,
            frame: 7,
            bytes: (0..512u32).map(|v| v as u8).collect(),
        };
        assert_eq!(captured.row(1).unwrap(), &captured.bytes[256..268]);
        assert_eq!(captured.row(2), None);
        let unknown = Captured {
            format: 71,
            ..captured.clone()
        };
        assert_eq!(unknown.row(0), None, "block-compressed: no pixel size");
        let buffer = Captured {
            format: 0,
            width: 10,
            height: 1,
            row_pitch: 10,
            bytes: vec![1; 10],
            ..captured
        };
        assert_eq!(buffer.row(0).unwrap().len(), 10);
    }
}
