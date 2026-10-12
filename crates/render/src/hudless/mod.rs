//! HUD-less image for frame generation: the scene target just before the
//! first GUI draw, converted to the swapchain's format with the engine's own
//! output encode.
//!
//! * [`detect`]: when to copy (learned from each frame's encode, validated
//!   after the fact);
//! * [`encode`]: how the copy reaches the backbuffer's colour space (the
//!   engine's `System_CopyToBackBuffer*` pixel shader, or an own gamma/PQ
//!   shader);
//! * Windows: [`enable`], [`set_output_transform`], [`begin_frame`],
//!   [`take`], [`status`].
//!
//! The copy is recorded on the engine's list before the first GUI draw; the
//! conversion is recorded on a command list of this crate and executed on
//! the same queue right after the engine's list that holds the copy, so the
//! engine's graphics state is never touched. The converted image is kept in
//! a ring of three, so the one handed out stays unmodified until its frame
//! is presented.

pub mod detect;
pub mod encode;

pub use detect::{Bound, Capture, Detector, Learned};
pub use encode::OutputTransform;

#[cfg(windows)]
pub use crate::sys::hudless::{
    begin_frame, enable, set_output_transform, status, take, Encoder, HudlessImage, Status,
};
