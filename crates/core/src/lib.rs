//! beat-forge audio engine core.
//!
//! UI-agnostic playback engine. The design splits into two sides:
//! - [`Engine`] runs inside the real-time audio callback and must never allocate,
//!   lock, or block.
//! - [`Trigger`] lives on the control thread and pushes commands to the engine over a
//!   lock-free single-producer/single-consumer queue.
//!
//! The internal bus is stereo: [`Engine::process`] always produces [`Frame`]s whatever the
//! source layout. Mapping that pair onto the output device is the host layer's job.
//!
//! The same split runs through the module layout: [`sample`] loads and owns audio data on
//! the control thread, [`engine`] renders it under real-time constraints.

mod engine;
mod sample;

pub use engine::{Engine, Trigger, engine};
pub use sample::{Frames, Sample};

/// One stereo frame: `[left, right]`.
///
/// An array rather than a flat `&mut [f32]`, so a slice's length is a frame count with no
/// `len % 2 == 0` invariant to uphold.
///
/// Lives at the crate root because both sides speak it: it is the unit of stored audio and
/// the unit of rendered output.
pub type Frame = [f32; 2];
