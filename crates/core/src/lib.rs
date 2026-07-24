//! beat-forge audio engine core.
//!
//! UI-agnostic playback engine. The design splits into two sides:
//! - [`Engine`] runs inside the real-time audio callback and must never allocate,
//!   lock, or block.
//! - [`Trigger`] lives on the control thread and pushes commands to the engine over a
//!   lock-free single-producer/single-consumer queue.
//!
//! The internal bus is stereo: [`Engine::render`] always produces [`Frame`]s whatever the
//! source layout. Mapping that pair onto the output device is the host layer's job.

use std::path::Path;

use rtrb::{Consumer, Producer, RingBuffer};

/// One stereo frame: `[left, right]`.
///
/// An array rather than a flat `&mut [f32]`, so a slice's length is a frame count with no
/// `len % 2 == 0` invariant to uphold.
pub type Frame = [f32; 2];

/// Decoded PCM, kept in its source channel layout.
///
/// Mono stays mono: drum one-shots are overwhelmingly mono, and widening them on load
/// would double the memory of the common case for nothing. An enum rather than a
/// `channels: u16` field so the compiler forces every consumer to handle both layouts
/// instead of trusting each one to compute the right stride.
pub enum Frames {
    Mono(Vec<f32>),
    Stereo(Vec<Frame>),
}

impl Frames {
    /// Number of frames, independent of channel layout.
    pub fn len(&self) -> usize {
        match self {
            Frames::Mono(data) => data.len(),
            Frames::Stereo(data) => data.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// A decoded audio sample.
pub struct Sample {
    /// PCM data, nominally in [-1.0, 1.0].
    pub frames: Frames,
    /// Sample rate the data was recorded at, in Hz.
    pub sample_rate: u32,
    /// Channel count of the source file, kept only so the host can warn about material
    /// that was reduced on load.
    pub source_channels: u16,
}

impl Sample {
    /// Load a WAV file as f32 PCM, preserving mono/stereo layout.
    ///
    /// Files with more than two channels are truncated to the first two; a correct
    /// surround downmix needs per-format coefficients and is not worth it yet.
    pub fn load_wav(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let mut reader = hound::WavReader::open(path)?;
        let spec = reader.spec();

        // Untrusted input: a corrupt header must surface as an error, never a panic.
        // `bits_per_sample == 0` in particular would underflow the shift below.
        anyhow::ensure!(spec.channels >= 1, "WAV header declares zero channels");
        anyhow::ensure!(
            (1..=32).contains(&spec.bits_per_sample),
            "unsupported bit depth: {} bits",
            spec.bits_per_sample
        );
        anyhow::ensure!(
            spec.sample_rate > 0,
            "WAV header declares a zero sample rate"
        );

        let raw: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader.samples::<f32>().collect::<Result<_, _>>()?,
            hound::SampleFormat::Int => {
                let scale = (1i64 << (spec.bits_per_sample - 1)) as f32;
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|v| v as f32 / scale))
                    .collect::<Result<_, _>>()?
            }
        };

        let channels = spec.channels as usize;
        let frames = if channels == 1 {
            Frames::Mono(raw)
        } else {
            // `chunks_exact` discards a trailing partial frame from a truncated file.
            Frames::Stereo(raw.chunks_exact(channels).map(|f| [f[0], f[1]]).collect())
        };

        Ok(Self {
            frames,
            sample_rate: spec.sample_rate,
            source_channels: spec.channels,
        })
    }

    /// Synthesize a short percussive blip (a decaying sine) so the engine can be tested
    /// without a real audio file on hand.
    pub fn blip(sample_rate: u32) -> Self {
        let len = sample_rate as usize / 4; // 250 ms
        let freq = 220.0;
        let frames = (0..len)
            .map(|i| {
                let t = i as f32 / sample_rate as f32;
                let decay = (-t * 12.0).exp();
                (t * freq * std::f32::consts::TAU).sin() * decay * 0.4
            })
            .collect();
        Self {
            frames: Frames::Mono(frames),
            sample_rate,
            source_channels: 1,
        }
    }
}

/// Messages sent from the control thread to the real-time engine.
enum Command {
    Trigger,
}

/// A single playing instance of the sample.
struct Voice {
    pos: usize,
    active: bool,
}

/// Real-time playback engine. Lives inside the audio callback.
///
/// Mixes a fixed pool of voices, each an independent playback of the same sample.
/// Everything here is allocation- and lock-free.
pub struct Engine {
    sample: Sample,
    voices: Vec<Voice>,
    commands: Consumer<Command>,
}

/// Control-side handle used to trigger the sample from another thread.
pub struct Trigger {
    tx: Producer<Command>,
}

impl Trigger {
    /// Fire the sample once. Silently dropped if the command queue is full.
    pub fn fire(&mut self) {
        let _ = self.tx.push(Command::Trigger);
    }
}

/// Create an [`Engine`] and its paired [`Trigger`] handle.
///
/// `polyphony` is the number of overlapping sample instances allowed at once.
pub fn engine(sample: Sample, polyphony: usize) -> (Engine, Trigger) {
    let (tx, rx) = RingBuffer::new(64);
    let voices = (0..polyphony.max(1))
        .map(|_| Voice {
            pos: 0,
            active: false,
        })
        .collect();
    let engine = Engine {
        sample,
        voices,
        commands: rx,
    };
    (engine, Trigger { tx })
}

impl Engine {
    /// Drain pending commands. Call once at the start of each audio block.
    pub fn pump(&mut self) {
        while let Ok(cmd) = self.commands.pop() {
            match cmd {
                Command::Trigger => self.start_voice(),
            }
        }
    }

    fn start_voice(&mut self) {
        // Voice stealing: when the pool is exhausted, slot 0 loses its note.
        let slot = self.voices.iter().position(|v| !v.active).unwrap_or(0);
        self.voices[slot].pos = 0;
        self.voices[slot].active = true;
    }

    /// Render `out.len()` stereo frames, mixing every active voice.
    ///
    /// Overwrites `out` rather than accumulating, so a caller cannot forget to clear it.
    /// Rendering a sub-slice is well-defined and is how event-accurate scheduling will
    /// split a block later.
    ///
    /// The mix can exceed [-1.0, 1.0] when many voices overlap; a proper mixer with gain
    /// staging arrives in a later step.
    pub fn render(&mut self, out: &mut [Frame]) {
        out.fill([0.0, 0.0]);

        let len = self.sample.frames.len();
        for voice in self.voices.iter_mut() {
            if !voice.active {
                continue;
            }
            let n = len.saturating_sub(voice.pos).min(out.len());
            // Matched per voice per block, never per sample.
            match &self.sample.frames {
                Frames::Mono(data) => {
                    let src = &data[voice.pos..voice.pos + n];
                    for (o, &x) in out[..n].iter_mut().zip(src) {
                        o[0] += x;
                        o[1] += x;
                    }
                }
                Frames::Stereo(data) => {
                    let src = &data[voice.pos..voice.pos + n];
                    for (o, s) in out[..n].iter_mut().zip(src) {
                        o[0] += s[0];
                        o[1] += s[1];
                    }
                }
            }
            voice.pos += n;
            if voice.pos >= len {
                voice.active = false;
            }
        }
    }

    /// Sample rate the loaded sample was recorded at, in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample.sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(frames: Frames) -> Sample {
        let source_channels = match &frames {
            Frames::Mono(_) => 1,
            Frames::Stereo(_) => 2,
        };
        Sample {
            frames,
            sample_rate: 48_000,
            source_channels,
        }
    }

    #[test]
    fn mono_lands_equally_in_both_channels() {
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0, 0.5])), 4);
        trig.fire();
        eng.pump();

        // Pre-filled with 9.0: the third frame is past the end of the sample, so it also
        // checks that render overwrites instead of leaving stale data.
        let mut out = [[9.0; 2]; 3];
        eng.render(&mut out);
        assert_eq!(out, [[1.0, 1.0], [0.5, 0.5], [0.0, 0.0]]);
    }

    #[test]
    fn stereo_keeps_channels_apart() {
        let (mut eng, mut trig) = engine(sample(Frames::Stereo(vec![[1.0, -1.0]])), 4);
        trig.fire();
        eng.pump();

        let mut out = [[0.0; 2]; 1];
        eng.render(&mut out);
        assert_eq!(out, [[1.0, -1.0]]);
    }

    #[test]
    fn sub_slice_rendering_is_continuous() {
        // The contract event-accurate scheduling will rely on.
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0, 2.0, 3.0, 4.0])), 4);
        trig.fire();
        eng.pump();

        let mut out = [[0.0; 2]; 4];
        eng.render(&mut out[..1]);
        eng.render(&mut out[1..]);
        assert_eq!(out, [[1.0, 1.0], [2.0, 2.0], [3.0, 3.0], [4.0, 4.0]]);
    }

    #[test]
    fn overlapping_voices_sum() {
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0, 1.0])), 4);
        trig.fire();
        eng.pump();

        let mut out = [[0.0; 2]; 1];
        eng.render(&mut out);

        // Second voice starts while the first is still playing.
        trig.fire();
        eng.pump();
        eng.render(&mut out);
        assert_eq!(out, [[2.0, 2.0]]);
    }

    #[test]
    fn exhausted_pool_steals_instead_of_dropping() {
        const POLYPHONY: usize = 2;
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0; 8])), POLYPHONY);
        for _ in 0..POLYPHONY + 1 {
            trig.fire();
        }
        eng.pump();

        // The third trigger steals rather than being dropped: two voices sound, not three.
        let mut out = [[0.0; 2]; 1];
        eng.render(&mut out);
        assert_eq!(out, [[POLYPHONY as f32, POLYPHONY as f32]]);
    }

    #[test]
    fn empty_sample_does_not_panic() {
        let (mut eng, mut trig) = engine(sample(Frames::Mono(Vec::new())), 2);
        trig.fire();
        eng.pump();

        let mut out = [[0.0; 2]; 2];
        eng.render(&mut out);
        assert_eq!(out, [[0.0, 0.0]; 2]);
    }
}
