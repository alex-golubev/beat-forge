//! beat-forge audio engine core.
//!
//! UI-agnostic playback engine. The design splits into two sides:
//! - [`Engine`] runs inside the real-time audio callback and must never allocate,
//!   lock, or block.
//! - [`Trigger`] lives on the control thread and pushes commands to the engine over a
//!   lock-free single-producer/single-consumer queue.

use std::path::Path;

use rtrb::{Consumer, Producer, RingBuffer};

/// A decoded, mono audio sample.
pub struct Sample {
    /// Mono PCM frames, nominally in [-1.0, 1.0].
    pub frames: Vec<f32>,
    /// Sample rate the data was recorded at, in Hz.
    pub sample_rate: u32,
}

impl Sample {
    /// Load a WAV file and downmix it to mono f32.
    pub fn load_wav(path: impl AsRef<Path>) -> anyhow::Result<Self> {
        let mut reader = hound::WavReader::open(path)?;
        let spec = reader.spec();
        let channels = spec.channels as usize;

        // Read every sample as f32, normalizing integer formats to [-1.0, 1.0].
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

        // Downmix interleaved channels to mono by averaging.
        let frames = if channels <= 1 {
            raw
        } else {
            raw.chunks(channels)
                .map(|frame| frame.iter().sum::<f32>() / channels as f32)
                .collect()
        };

        Ok(Self {
            frames,
            sample_rate: spec.sample_rate,
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
        Self { frames, sample_rate }
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
    // Fixed-capacity, lock-free command queue (single producer, single consumer).
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
        // Reuse a free voice, or steal the first one if all are busy (voice stealing).
        let slot = self.voices.iter().position(|v| !v.active).unwrap_or(0);
        self.voices[slot].pos = 0;
        self.voices[slot].active = true;
    }

    /// Produce the next mono output sample by mixing all active voices.
    ///
    /// The mix can exceed [-1.0, 1.0] when many voices overlap; a proper mixer with
    /// gain staging arrives in a later step.
    pub fn next_sample(&mut self) -> f32 {
        let len = self.sample.frames.len();
        let mut mix = 0.0;
        for i in 0..self.voices.len() {
            if !self.voices[i].active {
                continue;
            }
            let pos = self.voices[i].pos;
            if pos < len {
                mix += self.sample.frames[pos];
                self.voices[i].pos = pos + 1;
            } else {
                self.voices[i].active = false;
            }
        }
        mix
    }

    /// Sample rate the loaded sample was recorded at, in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample.sample_rate
    }
}
