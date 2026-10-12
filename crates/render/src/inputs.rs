//! What the temporal pass gives DLSS, and handing it over frame by frame.
//!
//! Everything in one [`DlssInputs`] is taken from **one** temporal dispatch:
//! the textures from its descriptor table, the camera from the constant
//! buffer that very dispatch binds. The engine records on several threads,
//! so a value set aside elsewhere (a shared "last camera") can belong to the
//! next frame by the time it is read; this was a measured intermittent third
//! of a pixel of error. The record is published whole, under one lock, and
//! taken whole.
//!
//! [`InputBoard`] keeps the last few records. The consumer (the upscale slot,
//! later on the render thread) takes the newest record made on its own
//! command list, else the newest one at all, each at most once and never one
//! older than the last taken. Which case applied is reported, not assumed.

use std::sync::Arc;

use mhw_core::frame::Size;
use mhw_core::math::Mat4;

use crate::depth::Match;
use crate::dxbc::ConstantBuffer;
use crate::states::State;

/// Largest camera constant buffer copied (`CBViewProjection` is 1072 bytes).
pub const CAMERA_CB_MAX: usize = 2048;
/// Records kept.
const RING: usize = 8;

/// A texture handed over, as identified.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TextureRef {
    /// `ID3D12Resource*` (native, not a layer's proxy).
    pub resource: usize,
    /// Liveness serial at identification; check before use.
    pub serial: u64,
    /// `DXGI_FORMAT` it was created with.
    pub format: u32,
    /// Width × height of mip 0.
    pub size: Size,
    /// `D3D12_RESOURCE_STATES` at the dispatch on its list, if known.
    pub state: Option<State>,
}

/// The camera constant buffer the temporal dispatch bound.
#[derive(Clone)]
pub struct CameraCb {
    /// Root parameter it was bound at.
    pub root: u32,
    /// GPU virtual address.
    pub address: u64,
    /// The shader's declaration of it, for field names.
    pub layout: Option<Arc<ConstantBuffer>>,
    len: usize,
    bytes: [u8; CAMERA_CB_MAX],
}

impl std::fmt::Debug for CameraCb {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CameraCb")
            .field("root", &self.root)
            .field("address", &format_args!("{:#x}", self.address))
            .field("layout", &self.layout.as_ref().map(|l| l.name.as_str()))
            .field("len", &self.len)
            .finish()
    }
}

impl CameraCb {
    /// A copy of `bytes` (truncated to [`CAMERA_CB_MAX`]).
    pub fn new(
        root: u32,
        address: u64,
        layout: Option<Arc<ConstantBuffer>>,
        bytes: &[u8],
    ) -> CameraCb {
        let len = bytes.len().min(CAMERA_CB_MAX);
        let mut copy = [0u8; CAMERA_CB_MAX];
        copy[..len].copy_from_slice(&bytes[..len]);
        CameraCb {
            root,
            address,
            layout,
            len,
            bytes: copy,
        }
    }

    /// The bytes read.
    pub fn bytes(&self) -> &[u8] {
        &self.bytes[..self.len]
    }

    /// The bytes of field `name` (`fProj`), by the shader's declaration.
    pub fn field(&self, name: &str) -> Option<&[u8]> {
        let variable = self.layout.as_ref()?.variable(name)?;
        let start = variable.offset as usize;
        self.bytes()
            .get(start..start.checked_add(variable.size as usize)?)
    }

    /// Field `name` as a row-major 4×4 matrix.
    pub fn mat4(&self, name: &str) -> Option<Mat4> {
        let bytes = self.field(name)?.get(..64)?;
        let mut values = [0f32; 16];
        for (value, word) in values.iter_mut().zip(bytes.as_chunks::<4>().0) {
            *value = f32::from_le_bytes(*word);
        }
        Some(Mat4::from_array(values))
    }
}

/// One temporal dispatch's inputs.
#[derive(Debug, Clone)]
pub struct DlssInputs {
    /// Publication number, strictly increasing (assigned by the board).
    pub seq: u64,
    /// Native command list the dispatch was recorded on.
    pub list: usize,
    /// Thread groups of the dispatch (16×16 each).
    pub groups: [u32; 3],
    /// Rectangle resolved: the dispatch's coverage clamped to its output.
    pub render: Size,
    /// `tBaseMap`: this frame's colour (R11G11B10_FLOAT).
    pub colour: TextureRef,
    /// `tGBuffer4`: packed motion vectors (R32_UINT).
    pub packed_motion: TextureRef,
    /// Decoded motion vectors (R16G16_FLOAT, render pixels, MV scale 1),
    /// in `NON_PIXEL_SHADER_RESOURCE`; `None` when decoding is off or failed.
    pub motion: Option<TextureRef>,
    /// `rwOutput`: the pass's own output (render size).
    pub output: TextureRef,
    /// The scene depth, if one matched.
    pub depth: Option<TextureRef>,
    /// Which rule matched the depth.
    pub depth_match: Option<Match>,
    /// The camera buffer the dispatch bound (`CBViewProjection`).
    pub camera: Option<CameraCb>,
}

impl DlssInputs {
    /// Every texture, for liveness checks and keeping references.
    pub fn textures(&self) -> impl Iterator<Item = &TextureRef> {
        [
            Some(&self.colour),
            Some(&self.packed_motion),
            Some(&self.output),
        ]
        .into_iter()
        .chain([self.motion.as_ref(), self.depth.as_ref()])
        .flatten()
    }
}

/// How a taken record relates to the taker's list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Association {
    /// Recorded on the same command list.
    SameList,
    /// Recorded on another list (it has always been one list in this
    /// engine; on two, the record may be another frame's).
    OtherList,
}

/// Board counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct BoardStats {
    /// Records published.
    pub published: u64,
    /// Records taken from the taker's list.
    pub taken_same_list: u64,
    /// Records taken from another list.
    pub taken_other_list: u64,
    /// Takes that found nothing new.
    pub empty: u64,
    /// Records superseded before anyone took them.
    pub skipped: u64,
}

#[derive(Debug, Clone)]
struct Record {
    inputs: DlssInputs,
    taken: bool,
}

/// The last few records. Not synchronised.
#[derive(Debug, Default)]
pub struct InputBoard {
    slots: [Option<Record>; RING],
    next: u64,
    last_taken: u64,
    stats: BoardStats,
}

impl InputBoard {
    /// Publish a record; its `seq` is assigned here and returned.
    pub fn publish(&mut self, mut inputs: DlssInputs) -> u64 {
        self.next += 1;
        inputs.seq = self.next;
        let index = (self.next % RING as u64) as usize;
        if let Some(old) = self.slots[index].take() {
            if !old.taken && old.inputs.seq > self.last_taken {
                self.stats.skipped += 1;
            }
        }
        self.slots[index] = Some(Record {
            inputs,
            taken: false,
        });
        self.stats.published += 1;
        self.next
    }

    /// Take the newest untaken record newer than the last one taken,
    /// preferring one recorded on `list`.
    pub fn take(&mut self, list: usize) -> Option<(DlssInputs, Association)> {
        let last = self.last_taken;
        let eligible = |record: &&mut Record| !record.taken && record.inputs.seq > last;
        let newest_on_list = self
            .slots
            .iter()
            .flatten()
            .filter(|r| !r.taken && r.inputs.seq > last && r.inputs.list == list)
            .map(|r| r.inputs.seq)
            .max();
        let newest = self
            .slots
            .iter()
            .flatten()
            .filter(|r| !r.taken && r.inputs.seq > last)
            .map(|r| r.inputs.seq)
            .max();
        let (seq, association) = match (newest_on_list, newest) {
            (Some(seq), _) => (seq, Association::SameList),
            (None, Some(seq)) => (seq, Association::OtherList),
            (None, None) => {
                self.stats.empty += 1;
                return None;
            }
        };
        let record = self
            .slots
            .iter_mut()
            .flatten()
            .filter(eligible)
            .find(|r| r.inputs.seq == seq)?;
        record.taken = true;
        self.last_taken = seq;
        match association {
            Association::SameList => self.stats.taken_same_list += 1,
            Association::OtherList => self.stats.taken_other_list += 1,
        }
        Some((record.inputs.clone(), association))
    }

    /// The newest record, taken or not.
    pub fn latest(&self) -> Option<&DlssInputs> {
        self.slots
            .iter()
            .flatten()
            .map(|r| &r.inputs)
            .max_by_key(|inputs| inputs.seq)
    }

    /// Counters.
    pub fn stats(&self) -> BoardStats {
        self.stats
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texture(resource: usize) -> TextureRef {
        TextureRef {
            resource,
            serial: 1,
            format: 26,
            size: Size::new(16, 16),
            state: None,
        }
    }

    fn inputs(list: usize, colour: usize) -> DlssInputs {
        DlssInputs {
            seq: 0,
            list,
            groups: [1, 1, 1],
            render: Size::new(16, 16),
            colour: texture(colour),
            packed_motion: texture(0x20),
            motion: None,
            output: texture(0x30),
            depth: Some(texture(0x40)),
            depth_match: Some(Match::Render),
            camera: None,
        }
    }

    #[test]
    fn each_record_is_taken_once_preferring_the_takers_list() {
        let mut board = InputBoard::default();
        board.publish(inputs(0xa, 1));
        board.publish(inputs(0xb, 2));
        let (taken, how) = board.take(0xa).unwrap();
        assert_eq!((taken.colour.resource, how), (1, Association::SameList));
        let (taken, how) = board.take(0xb).unwrap();
        assert_eq!((taken.colour.resource, how), (2, Association::SameList));
        assert!(board.take(0xa).is_none());
        assert_eq!(board.stats().empty, 1);
    }

    #[test]
    fn never_older_than_the_last_taken() {
        let mut board = InputBoard::default();
        board.publish(inputs(0xa, 1));
        board.publish(inputs(0xb, 2));
        // The slot on list b takes the newer one; list a's older record is
        // then a past frame and is not handed out.
        assert_eq!(board.take(0xb).unwrap().0.colour.resource, 2);
        assert!(board.take(0xa).is_none());
    }

    #[test]
    fn falls_back_to_another_list_and_says_so() {
        let mut board = InputBoard::default();
        let seq = board.publish(inputs(0xa, 1));
        let (taken, how) = board.take(0xc).unwrap();
        assert_eq!((taken.seq, how), (seq, Association::OtherList));
        assert_eq!(board.stats().taken_other_list, 1);
    }

    #[test]
    fn untaken_records_are_counted_when_overwritten() {
        let mut board = InputBoard::default();
        for colour in 0..(RING + 3) {
            board.publish(inputs(0xa, colour));
        }
        assert_eq!(board.stats().skipped, 3);
        assert_eq!(board.latest().unwrap().colour.resource, RING + 2);
        assert_eq!(board.take(0xa).unwrap().0.colour.resource, RING + 2);
    }

    #[test]
    fn camera_fields_by_declaration() {
        let layout = Arc::new(ConstantBuffer {
            name: "CBViewProjection".into(),
            size: 128,
            variables: vec![crate::dxbc::Variable {
                name: "fProj".into(),
                offset: 64,
                size: 64,
            }],
        });
        let mut bytes = vec![0u8; 128];
        for index in 0..16 {
            let at = 64 + index * 4;
            bytes[at..at + 4].copy_from_slice(&(index as f32).to_le_bytes());
        }
        let camera = CameraCb::new(2, 0x1000, Some(layout), &bytes);
        let proj = camera.mat4("fProj").unwrap();
        assert_eq!(proj.rows[2], [8.0, 9.0, 10.0, 11.0]);
        assert!(camera.mat4("fView").is_none());
        let short = CameraCb::new(2, 0x1000, camera.layout.clone(), &bytes[..100]);
        assert!(short.field("fProj").is_none());
        assert_eq!(inputs(1, 1).textures().count(), 4);
    }
}
