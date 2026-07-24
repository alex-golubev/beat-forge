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

/// Half-width of the resampling kernel, in source frames at unity ratio.
///
/// 16 (32 taps) is where the Blackman-windowed sinc stops being the audible weak link for
/// drum material. Widening it costs load time only — nothing here runs under real-time
/// constraints — but buys less than the next digit of the file's own quality.
const RESAMPLE_HALF_TAPS: f64 = 16.0;

/// A decoded audio sample.
pub struct Sample {
    /// PCM data, nominally in [-1.0, 1.0].
    pub frames: Frames,
    /// Sample rate the data is *currently* at, in Hz — after any resampling.
    pub sample_rate: u32,
    /// Channel count of the source file, kept only so the host can warn about material
    /// that was reduced on load.
    pub source_channels: u16,
    /// Sample rate of the source file, kept only so the host can report a conversion.
    pub source_sample_rate: u32,
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
            source_sample_rate: spec.sample_rate,
        })
    }

    /// Convert the sample to `target_rate`, or return it untouched if it is already there.
    ///
    /// The engine reads exactly one source frame per output frame, so material at the wrong
    /// rate plays at the wrong pitch — 44.1 kHz on a 48 kHz device is a semitone and a half
    /// sharp. Samples are static, so the fix belongs here: convert once, on the control
    /// thread, at whatever quality we like, rather than dragging a resampler into the audio
    /// callback. (Per-voice pitch is a different feature and will want its own cheap
    /// interpolator — it is a musical effect, not a correction.)
    ///
    /// Interpolation is a Blackman-windowed sinc. Linear would do for upsampling, but it has
    /// no anti-alias filter, so downsampling — a 96 kHz one-shot on a 48 kHz device — would
    /// fold everything above the target Nyquist back into the audible band, right where
    /// cymbals keep their energy. Lowering the kernel's cutoff along with the ratio makes the
    /// same code the anti-alias filter, for free.
    pub fn resample_to(self, target_rate: u32) -> Self {
        if target_rate == 0 || target_rate == self.sample_rate {
            return self;
        }
        // Nothing to interpolate, but the result still has to *be* at the target rate.
        if self.frames.is_empty() {
            return Self {
                sample_rate: target_rate,
                ..self
            };
        }

        let ratio = f64::from(target_rate) / f64::from(self.sample_rate);
        let out_len = ((self.frames.len() as f64) * ratio).round().max(1.0) as usize;
        // Cutoff tracks the lower of the two Nyquist limits; the kernel widens to match, so
        // downsampling gets more taps rather than less filtering.
        let cutoff = ratio.min(1.0);
        let half = RESAMPLE_HALF_TAPS / cutoff;
        // Source position per output frame. Multiplied out from the index rather than
        // accumulated, so the phase cannot drift over a long sample.
        let step = f64::from(self.sample_rate) / f64::from(target_rate);

        let mut window = Vec::with_capacity(2 * half as usize + 2);
        let frames = match &self.frames {
            Frames::Mono(src) => Frames::Mono(
                (0..out_len)
                    .map(|i| {
                        kernel(i as f64 * step, half, cutoff, &mut window);
                        window.iter().map(|&(j, w)| w * at(src, j)).sum()
                    })
                    .collect(),
            ),
            Frames::Stereo(src) => Frames::Stereo(
                (0..out_len)
                    .map(|i| {
                        kernel(i as f64 * step, half, cutoff, &mut window);
                        window.iter().fold([0.0, 0.0], |acc, &(j, w)| {
                            let s = at(src, j);
                            [acc[0] + w * s[0], acc[1] + w * s[1]]
                        })
                    })
                    .collect(),
            ),
        };

        Self {
            frames,
            sample_rate: target_rate,
            ..self
        }
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
            source_sample_rate: sample_rate,
        }
    }
}

/// Fill `window` with the interpolation kernel around source position `pos`, as
/// (source frame index, weight). Indices outside the sample are kept — [`at`] reads them
/// as silence, which is what lies outside a one-shot anyway.
///
/// Weights are normalized by their own sum rather than scaled by `cutoff`: it costs one
/// division and makes a constant input come out exactly constant at any fractional phase.
fn kernel(pos: f64, half: f64, cutoff: f64, window: &mut Vec<(isize, f32)>) {
    window.clear();

    let first = (pos - half).ceil() as isize;
    let last = (pos + half).floor() as isize;
    let mut sum = 0.0;
    for j in first..=last {
        let x = pos - j as f64;
        let w = blackman(x / half) * sinc(cutoff * x);
        sum += w;
        window.push((j, w as f32));
    }

    let norm = (1.0 / sum) as f32;
    for (_, w) in window.iter_mut() {
        *w *= norm;
    }
}

/// Blackman window over `t` in [-1, 1].
fn blackman(t: f64) -> f64 {
    use std::f64::consts::PI;
    0.42 + 0.5 * (PI * t).cos() + 0.08 * (2.0 * PI * t).cos()
}

/// Normalized sinc: `sin(pi x) / (pi x)`, and 1 at zero.
fn sinc(x: f64) -> f64 {
    if x == 0.0 {
        return 1.0;
    }
    let pi_x = std::f64::consts::PI * x;
    pi_x.sin() / pi_x
}

/// Read a possibly out-of-range frame index as silence.
fn at<T: Copy + Default>(src: &[T], j: isize) -> T {
    usize::try_from(j)
        .ok()
        .and_then(|j| src.get(j))
        .copied()
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mono(data: Vec<f32>, rate: u32) -> Sample {
        Sample {
            frames: Frames::Mono(data),
            sample_rate: rate,
            source_channels: 1,
            source_sample_rate: rate,
        }
    }

    fn mono_data(s: &Sample) -> &[f32] {
        match &s.frames {
            Frames::Mono(d) => d,
            Frames::Stereo(_) => panic!("expected mono"),
        }
    }

    /// A tone at `freq`, `len` frames long, sampled at `rate`.
    fn tone(freq: f64, rate: u32, len: usize) -> Vec<f32> {
        (0..len)
            .map(|i| {
                let t = i as f64 / f64::from(rate);
                (t * freq * std::f64::consts::TAU).sin() as f32
            })
            .collect()
    }

    #[test]
    fn matching_rate_is_left_alone() {
        let out = mono(vec![0.1, -0.2, 0.3], 48_000).resample_to(48_000);
        assert_eq!(mono_data(&out), [0.1, -0.2, 0.3], "must not touch the data");
    }

    #[test]
    fn length_scales_with_the_ratio_and_the_source_rate_is_remembered() {
        let out = mono(vec![0.0; 100], 44_100).resample_to(48_000);
        assert_eq!(mono_data(&out).len(), 109); // 100 * 48000 / 44100
        assert_eq!(out.sample_rate, 48_000);
        assert_eq!(
            out.source_sample_rate, 44_100,
            "the host reports the conversion from this"
        );
    }

    #[test]
    fn a_constant_stays_constant() {
        // Catches an unnormalized kernel: the weights must sum to exactly 1 at every
        // fractional phase, not just on whole samples.
        let out = mono(vec![1.0; 200], 44_100).resample_to(48_000);
        for (i, &s) in mono_data(&out).iter().enumerate().take(170).skip(40) {
            assert!((s - 1.0).abs() < 1e-4, "frame {i} drifted to {s}");
        }
    }

    #[test]
    fn a_tone_keeps_its_frequency_and_amplitude() {
        let out = mono(tone(1_000.0, 44_100, 4_410), 44_100).resample_to(48_000);
        let expected = tone(1_000.0, 48_000, 4_800);
        // Interior only: the kernel runs off the ends, where a one-shot is silence anyway.
        for (i, (&got, &want)) in mono_data(&out)
            .iter()
            .zip(expected.iter())
            .enumerate()
            .take(4_000)
            .skip(800)
        {
            assert!((got - want).abs() < 0.01, "frame {i}: {got} vs {want}");
        }
    }

    #[test]
    fn downsampling_rejects_content_above_the_new_nyquist() {
        // The reason for a windowed sinc rather than linear interpolation: 18 kHz cannot be
        // represented at 24 kHz, and without a filter it would fold back to 6 kHz at full
        // level instead of disappearing.
        let out = mono(tone(18_000.0, 96_000, 9_600), 96_000).resample_to(24_000);
        let peak = mono_data(&out)[600..1_800]
            .iter()
            .fold(0.0f32, |m, s| m.max(s.abs()));
        assert!(peak < 0.05, "aliased back into the audible band at {peak}");
    }

    #[test]
    fn stereo_channels_stay_independent() {
        let sample = Sample {
            frames: Frames::Stereo(vec![[1.0, -0.5]; 200]),
            sample_rate: 44_100,
            source_channels: 2,
            source_sample_rate: 44_100,
        };
        let out = sample.resample_to(48_000);
        let Frames::Stereo(data) = &out.frames else {
            panic!("stereo must stay stereo");
        };
        for (i, f) in data.iter().enumerate().take(170).skip(40) {
            assert!((f[0] - 1.0).abs() < 1e-4, "frame {i} left: {}", f[0]);
            assert!((f[1] + 0.5).abs() < 1e-4, "frame {i} right: {}", f[1]);
        }
    }

    #[test]
    fn an_empty_sample_still_arrives_at_the_target_rate() {
        let out = mono(Vec::new(), 44_100).resample_to(48_000);
        assert!(out.frames.is_empty());
        assert_eq!(out.sample_rate, 48_000);
    }
}
