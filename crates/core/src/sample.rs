//! Decoding and in-memory representation of audio material.
//!
//! Nothing here runs under real-time constraints: loading happens on the control thread and
//! is free to allocate and to fail. The engine only ever reads what this module produced.

use std::path::Path;

use crate::Frame;

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
