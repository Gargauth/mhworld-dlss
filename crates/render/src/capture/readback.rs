//! Readback layout and the in-flight queue of the capture service.

/// `D3D12_TEXTURE_DATA_PITCH_ALIGNMENT`.
pub const PITCH_ALIGNMENT: u32 = 256;
/// `D3D12_RESOURCE_STATE_COPY_SOURCE`.
pub const COPY_SOURCE: u32 = 0x800;

/// Bytes per pixel of the uncompressed formats a capture may meet; `None`
/// for block-compressed, planar, video and unknown formats.
pub fn bytes_per_pixel(format: u32) -> Option<u32> {
    Some(match format {
        // R32G32B32A32_*
        1..=4 => 16,
        // R32G32B32_*
        5..=8 => 12,
        // R16G16B16A16_*, R32G32_*, R32G8X24 / D32_FLOAT_S8X24 family
        9..=22 => 8,
        // R10G10B10A2_*, R11G11B10, R8G8B8A8_*, R16G16_*, R32_*, R24G8 / D24S8
        23..=47 => 4,
        // R8G8_*, R16_*, D16
        48..=59 => 2,
        // R8_*, A8
        60..=65 => 1,
        // R9G9B9E5_SHAREDEXP
        67 => 4,
        // B5G6R5, B5G5R5A1
        85 | 86 => 2,
        // B8G8R8A8_*, B8G8R8X8_*, R10G10B10_XR_BIAS_A2
        87..=93 => 4,
        // B4G4R4A4
        115 => 2,
        _ => return None,
    })
}

/// A row's bytes rounded up to the copy pitch alignment.
pub fn row_pitch(row_bytes: u32) -> Option<u32> {
    row_bytes.checked_next_multiple_of(PITCH_ALIGNMENT)
}

/// How a texture's mip 0 lies in a readback buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    /// Bytes of pixels per row.
    pub row_bytes: u32,
    /// Bytes from row to row.
    pub row_pitch: u32,
    /// Rows.
    pub rows: u32,
    /// Buffer size: every row padded (`row_pitch × rows`).
    pub size: u64,
}

/// The layout of a `width × height` texture of `format`.
pub fn layout(format: u32, width: u32, height: u32) -> Option<Layout> {
    if width == 0 || height == 0 {
        return None;
    }
    let row_bytes = width.checked_mul(bytes_per_pixel(format)?)?;
    let row_pitch = row_pitch(row_bytes)?;
    Some(Layout {
        row_bytes,
        row_pitch,
        rows: height,
        size: u64::from(row_pitch) * u64::from(height),
    })
}

/// Row `y` of `row_bytes` bytes in a buffer of `pitch`-spaced rows.
pub fn row(bytes: &[u8], pitch: u32, row_bytes: u32, y: u32) -> Option<&[u8]> {
    if row_bytes > pitch {
        return None;
    }
    let start = usize::try_from(u64::from(pitch) * u64::from(y)).ok()?;
    bytes.get(start..start.checked_add(row_bytes as usize)?)
}

/// Whether a resource in `state` must be transitioned to be copied from:
/// a read state that includes `COPY_SOURCE` (e.g. `GENERIC_READ`) may be
/// copied from as it is.
pub fn needs_transition(state: u32) -> bool {
    state & COPY_SOURCE == 0
}

/// Work waiting for the GPU: entries recorded under a ticket, given a fence
/// value when the list holding them is submitted, ready once the fence
/// reaches it. Oldest first throughout.
#[derive(Debug)]
pub struct Inflight<T> {
    entries: Vec<(u64, Option<u64>, T)>,
}

impl<T> Default for Inflight<T> {
    fn default() -> Self {
        Inflight {
            entries: Vec::new(),
        }
    }
}

impl<T> Inflight<T> {
    /// Add `entry` under `ticket` (no fence yet).
    pub fn push(&mut self, ticket: u64, entry: T) {
        self.entries.push((ticket, None, entry));
    }

    /// The list of `ticket` was submitted; its work completes at `fence`.
    pub fn assign(&mut self, ticket: u64, fence: u64) {
        for (t, value, _) in &mut self.entries {
            if *t == ticket {
                *value = Some(fence);
            }
        }
    }

    /// Entries whose fence value is at most `completed`, oldest first.
    pub fn take_ready(&mut self, completed: u64) -> Vec<T> {
        let mut ready = Vec::new();
        let mut kept = Vec::with_capacity(self.entries.len());
        for (ticket, fence, entry) in self.entries.drain(..) {
            if fence.is_some_and(|value| value <= completed) {
                ready.push(entry);
            } else {
                kept.push((ticket, fence, entry));
            }
        }
        self.entries = kept;
        ready
    }

    /// Entries in flight.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether nothing is in flight.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Every entry, for accounting.
    pub fn iter(&self) -> impl Iterator<Item = &T> {
        self.entries.iter().map(|(_, _, entry)| entry)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixel_sizes_of_the_engines_formats() {
        assert_eq!(bytes_per_pixel(26), Some(4), "R11G11B10_FLOAT");
        assert_eq!(bytes_per_pixel(10), Some(8), "R16G16B16A16_FLOAT");
        assert_eq!(bytes_per_pixel(34), Some(4), "R16G16_FLOAT");
        assert_eq!(bytes_per_pixel(40), Some(4), "D32_FLOAT");
        assert_eq!(bytes_per_pixel(61), Some(1), "R8_UNORM");
        assert_eq!(bytes_per_pixel(87), Some(4), "B8G8R8A8_UNORM");
        assert_eq!(bytes_per_pixel(2), Some(16));
        assert_eq!(bytes_per_pixel(71), None, "BC1");
        assert_eq!(bytes_per_pixel(0), None);
    }

    #[test]
    fn rows_are_pitched_to_256_bytes() {
        assert_eq!(row_pitch(0), Some(0));
        assert_eq!(row_pitch(1), Some(256));
        assert_eq!(row_pitch(256), Some(256));
        assert_eq!(row_pitch(257), Some(512));
        assert_eq!(row_pitch(u32::MAX), None);
        // 1920 × R11G11B10: 7680 bytes, already aligned.
        let l = layout(26, 1920, 1080).unwrap();
        assert_eq!(
            (l.row_bytes, l.row_pitch, l.size),
            (7680, 7680, 7680 * 1080)
        );
        // 1366 × RGBA16F: 10928 -> 11008.
        let l = layout(10, 1366, 768).unwrap();
        assert_eq!((l.row_bytes, l.row_pitch), (10928, 11008));
        assert_eq!(l.size, 11008 * 768);
        assert_eq!(layout(26, 0, 10), None);
        assert_eq!(layout(71, 64, 64), None);
    }

    #[test]
    fn row_access_is_bounded() {
        let bytes = vec![0u8; 512];
        assert_eq!(row(&bytes, 256, 12, 1).map(<[u8]>::len), Some(12));
        assert_eq!(row(&bytes, 256, 12, 2), None);
        assert_eq!(row(&bytes, 8, 12, 0), None, "row wider than its pitch");
    }

    #[test]
    fn copy_source_bit_decides_the_barrier() {
        assert!(needs_transition(0x4), "render target");
        assert!(needs_transition(0), "common");
        assert!(!needs_transition(0x800));
        assert!(
            !needs_transition(0xac3),
            "GENERIC_READ includes COPY_SOURCE"
        );
    }

    #[test]
    fn inflight_entries_complete_in_order_once_fenced() {
        let mut queue = Inflight::default();
        queue.push(1, "a");
        queue.push(1, "b");
        queue.push(2, "c");
        queue.push(3, "d");
        assert!(queue.take_ready(u64::MAX).is_empty(), "nothing submitted");
        queue.assign(1, 10);
        queue.assign(2, 11);
        assert_eq!(queue.take_ready(9), Vec::<&str>::new());
        assert_eq!(queue.take_ready(10), ["a", "b"]);
        assert_eq!(queue.take_ready(100), ["c"]);
        assert_eq!(queue.len(), 1);
        assert!(
            !queue.is_empty(),
            "never submitted: held, bounded by the caller's cap"
        );
    }
}
