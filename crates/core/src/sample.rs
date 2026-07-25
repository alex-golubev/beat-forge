//! Decoding and in-memory representation of audio material.
//!
//! Nothing here runs under real-time constraints: loading happens on the control thread and
//! is free to allocate and to fail. The engine only ever reads what this module produced.
//!
//! Resampling works in two number systems at once — sample indices, which are integers, and
//! continuous positions and weights, which are not — and crosses between them on every output
//! frame. `std` offers no lossless conversion for those pairs because none exists, so the cast
//! lints are switched off here and only here; a cast appearing anywhere else in the workspace
//! is still reported.
#![allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss
)]

use std::error::Error;
use std::fmt;
use std::fs::File;
use std::io::{self, BufReader, Read};
use std::path::{Path, PathBuf};

use crate::Frame;

/// What can go wrong turning a byte stream into a [`Sample`].
///
/// Separate from [`LoadError`] because [`Sample::from_reader`] opens no files and so cannot
/// fail the way [`Sample::load_wav`] can — folding both into one enum would mean a variant
/// that is unreachable for half its callers.
///
/// The decoder's own failure carries its cause as a boxed [`Error`] rather than naming
/// `hound::Error`. Putting a dependency's type in a public signature makes that dependency
/// part of this crate's contract: swapping the decoder — for FLAC or AIFF support, say —
/// would then be a breaking change forced by an implementation detail. Callers that really
/// want the concrete type can still `downcast_ref` for it; they just do it at their own risk
/// instead of on our promise.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum DecodeError {
    /// The decoder rejected the stream. The `source` carries the specific reason.
    ///
    /// Deliberately not split into "corrupt file" and "I/O failure": `hound` reports a
    /// missing RIFF tag, a data chunk that ends early and a genuine read failure all as its
    /// own `IoError`, and the only thing separating them is the message text. Classifying on
    /// that would be a guess dressed up as a type, so the reason stays in the cause chain
    /// where it is accurate, and this variant claims only what is actually known.
    #[error("could not read this as a WAV")]
    Unreadable(#[source] Box<dyn Error + Send + Sync>),
    /// The header parsed but claims no channels, so there is no audio to read.
    #[error("WAV header declares zero channels")]
    NoChannels,
    /// Bit depth outside 1..=32. Zero would underflow the scaling shift; above 32 is not a
    /// depth `hound` produces.
    #[error("unsupported bit depth: {0} bits")]
    BitDepth(u16),
    /// Sample rate outside `1000..=768_000`. The bounds exist because [`Sample::resample_to`]
    /// scales length by `target / sample_rate`, and that field is untrusted.
    #[error("unsupported sample rate: {0} Hz")]
    SampleRate(u32),
}

impl DecodeError {
    /// Box a `hound` failure as the cause of [`DecodeError::Unreadable`].
    ///
    /// The one place that sees the concrete type, and where it stops: boxing it here is what
    /// keeps `hound` out of this crate's public signatures.
    fn from_hound(err: hound::Error) -> Self {
        Self::Unreadable(Box::new(err))
    }
}

/// What can go wrong loading a [`Sample`] from a path.
///
/// Both variants carry the path: by the time this reaches a user, *which* file failed is the
/// first thing they need, and the underlying `io::Error` does not carry it.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum LoadError {
    /// The file could not be opened at all — missing, unreadable, not a file.
    #[error("opening {}", path.display())]
    Open {
        /// The path that was asked for.
        path: PathBuf,
        /// Why the operating system refused it.
        #[source]
        source: io::Error,
    },
    /// The file opened, but its contents are not a sample this engine will load.
    #[error("decoding {}", path.display())]
    Decode {
        /// The path that was asked for.
        path: PathBuf,
        /// What the decoder rejected.
        #[source]
        source: DecodeError,
    },
}

/// Decoded PCM, kept in its source channel layout.
///
/// Mono stays mono: drum one-shots are overwhelmingly mono, and widening them on load
/// would double the memory of the common case for nothing. An enum rather than a
/// `channels: u16` field so the compiler forces every consumer to handle both layouts
/// instead of trusting each one to compute the right stride.
///
/// Crate-private on purpose. This is the engine's storage layout, not a contract: a UI
/// wants a peak envelope it can draw, not a buffer it has to reduce itself, so handing it
/// out would pin the layout to whatever the first consumer did with it. Adding a
/// multi-channel variant or moving to a flat interleaved buffer stays a local change while
/// nothing outside can name the type.
pub(crate) enum Frames {
    Mono(Vec<f32>),
    Stereo(Vec<Frame>),
}

/// Layout and length, never the samples themselves.
///
/// `#[derive(Debug)]` would be actively harmful here: a 250 ms blip prints as 60 KB of
/// floats, and a three-minute stereo track as hundreds of megabytes — enough to bury the
/// assertion message that asked for it.
impl fmt::Debug for Frames {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let layout = match self {
            Frames::Mono(_) => "Mono",
            Frames::Stereo(_) => "Stereo",
        };
        write!(f, "{layout}({} frames)", self.len())
    }
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
///
/// The fields are private because the type carries an invariant: `sample_rate` is always one
/// [`Sample::resample_to`] can safely scale by. That is checked once, where untrusted data
/// enters, and public fields would make the check optional — a hand-built
/// `Sample { sample_rate: 1, .. }` would walk straight past it and ask the resampler for
/// hundreds of gigabytes. Construction therefore goes through [`Sample::load_wav`],
/// [`Sample::from_reader`] or [`Sample::blip`], and every accessor below returns a `Copy`
/// scalar, so nothing can desync the data from its declared rate afterwards either.
// `sample_rate` and `source_sample_rate` repeat the type's name, which clippy dislikes. In
// audio "sample rate" is one indivisible term, and shortening it to `rate` would lose which
// of the two rates is meant at every call site.
#[allow(clippy::struct_field_names)]
#[derive(Debug)]
pub struct Sample {
    pub(crate) frames: Frames,
    pub(crate) sample_rate: u32,
    pub(crate) source_channels: u16,
    pub(crate) source_sample_rate: u32,
}

impl Sample {
    /// Sample rate the data is *currently* at, in Hz — after any resampling.
    #[must_use]
    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Channel count of the source file, kept only so the host can warn about material that
    /// was reduced on load.
    #[must_use]
    pub fn source_channels(&self) -> u16 {
        self.source_channels
    }

    /// Sample rate of the source file, kept only so the host can report a conversion.
    #[must_use]
    pub fn source_sample_rate(&self) -> u32 {
        self.source_sample_rate
    }

    /// Load a WAV file as f32 PCM, preserving mono/stereo layout.
    ///
    /// Files with more than two channels are truncated to the first two; a correct
    /// surround downmix needs per-format coefficients and is not worth it yet.
    ///
    /// # Errors
    ///
    /// [`LoadError::Open`] if the file cannot be opened, and [`LoadError::Decode`] wrapping
    /// whatever [`Sample::from_reader`] rejected. Both carry the path, which the underlying
    /// `io::Error` does not.
    pub fn load_wav(path: impl AsRef<Path>) -> Result<Self, LoadError> {
        let path = path.as_ref();
        // Opening is ours now rather than hound's, so the path has to be put back into the
        // error — a bare "No such file or directory" names nothing the operator can act on.
        let file = File::open(path).map_err(|source| LoadError::Open {
            path: path.to_path_buf(),
            source,
        })?;
        Self::from_reader(BufReader::new(file)).map_err(|source| LoadError::Decode {
            path: path.to_path_buf(),
            source,
        })
    }

    /// Decode a WAV from any reader — the parsing half of [`Sample::load_wav`].
    ///
    /// Split out because decoding is the part that faces untrusted input, and therefore the
    /// part worth testing exhaustively: a reader can be a `Cursor` over bytes, so those tests
    /// need no filesystem, no temporary files and no cleanup. It also leaves room for material
    /// that never was a file on its own — a sample pack read out of an archive.
    ///
    /// # Errors
    ///
    /// [`DecodeError::Unreadable`] if the bytes are not a WAV the decoder accepts — a missing
    /// RIFF tag, a data chunk that ends early, a read failure underneath. Otherwise the header
    /// parsed but declares something that will not load: [`DecodeError::NoChannels`],
    /// [`DecodeError::BitDepth`] outside `1..=32`, or [`DecodeError::SampleRate`] outside
    /// `1000..=768_000`.
    pub fn from_reader(reader: impl Read) -> Result<Self, DecodeError> {
        let mut reader = hound::WavReader::new(reader).map_err(DecodeError::from_hound)?;
        let spec = reader.spec();

        // Untrusted input: a corrupt header must surface as an error, never a panic.
        // `bits_per_sample == 0` in particular would underflow the shift below.
        if spec.channels == 0 {
            return Err(DecodeError::NoChannels);
        }
        if !(1..=32).contains(&spec.bits_per_sample) {
            return Err(DecodeError::BitDepth(spec.bits_per_sample));
        }
        // Both bounds exist because `resample_to` scales by `target / sample_rate`: a low rate
        // inflates the sample (1 Hz would ask for 48000x its own length), a high one widens the
        // resampling kernel by that same factor. The range is deliberately wider than anything
        // musical — lo-fi material at 5512 Hz is what a groovebox is for, not a corrupt header.
        if !(1_000..=768_000).contains(&spec.sample_rate) {
            return Err(DecodeError::SampleRate(spec.sample_rate));
        }

        let raw: Vec<f32> = match spec.sample_format {
            hound::SampleFormat::Float => reader
                .samples::<f32>()
                .collect::<Result<_, _>>()
                .map_err(DecodeError::from_hound)?,
            hound::SampleFormat::Int => {
                // A power of two, so `f32` holds it exactly at every depth up to 32 — and
                // `powi` gets there without a lossy cast that would need excusing.
                let scale = 2.0f32.powi(i32::from(spec.bits_per_sample) - 1);
                reader
                    .samples::<i32>()
                    .map(|s| s.map(|v| v as f32 / scale))
                    .collect::<Result<_, _>>()
                    .map_err(DecodeError::from_hound)?
            }
        };

        let channels = usize::from(spec.channels);
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
    #[must_use = "resampling consumes the sample and returns a new one; \
                  dropping the result loses the audio"]
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
    #[must_use]
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
    // Exact comparison is the point here, not an oversight: these fixtures are built from
    // values the arithmetic reproduces bit for bit, and an epsilon would assert strictly less.
    // Where a result is genuinely approximate — resampled audio — the tests below use a
    // tolerance explicitly.
    #![allow(clippy::float_cmp)]

    use super::*;
    use hound::{SampleFormat, WavSpec, WavWriter};
    use std::io::Cursor;

    /// Encode a WAV in memory. Everything below decodes bytes rather than files, so the
    /// loader's untrusted-input paths can be exercised without touching the filesystem.
    fn wav(spec: WavSpec, write: impl FnOnce(&mut WavWriter<&mut Cursor<Vec<u8>>>)) -> Vec<u8> {
        let mut cursor = Cursor::new(Vec::new());
        let mut writer = WavWriter::new(&mut cursor, spec).expect("spec is writable");
        write(&mut writer);
        writer.finalize().expect("finalize");
        cursor.into_inner()
    }

    fn int_wav(channels: u16, rate: u32, bits: u16, samples: &[i32]) -> Vec<u8> {
        let spec = WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: bits,
            sample_format: SampleFormat::Int,
        };
        wav(spec, |w| {
            for &s in samples {
                w.write_sample(s).expect("write");
            }
        })
    }

    fn float_wav(channels: u16, rate: u32, samples: &[f32]) -> Vec<u8> {
        let spec = WavSpec {
            channels,
            sample_rate: rate,
            bits_per_sample: 32,
            sample_format: SampleFormat::Float,
        };
        wav(spec, |w| {
            for &s in samples {
                w.write_sample(s).expect("write");
            }
        })
    }

    fn decode(bytes: Vec<u8>) -> Result<Sample, DecodeError> {
        Sample::from_reader(Cursor::new(bytes))
    }

    /// The error a decode is expected to fail with.
    fn decode_err(bytes: Vec<u8>, why: &str) -> DecodeError {
        decode(bytes).expect_err(why)
    }

    fn stereo_data(s: &Sample) -> &[Frame] {
        match &s.frames {
            Frames::Stereo(d) => d,
            Frames::Mono(_) => panic!("expected stereo"),
        }
    }

    #[test]
    fn sixteen_bit_mono_reaches_full_scale_without_exceeding_it() {
        let s = decode(int_wav(
            1,
            44_100,
            16,
            &[i32::from(i16::MAX), 0, i32::from(i16::MIN)],
        ))
        .expect("valid file");
        let d = mono_data(&s);
        assert!((d[0] - 1.0).abs() < 1e-4, "peak positive: {}", d[0]);
        assert_eq!(d[1], 0.0);
        assert_eq!(d[2], -1.0, "the negative rail is exactly -1.0");
        assert_eq!(s.sample_rate, 44_100);
        assert_eq!(s.source_channels, 1);
    }

    #[test]
    fn eight_bit_arrives_signed() {
        // WAV stores 8-bit PCM *unsigned* with a 128 bias; hound removes it. If that ever
        // stopped being true, silence would decode as full-scale positive.
        let s = decode(int_wav(1, 22_050, 8, &[127, 0, -128])).expect("valid file");
        let d = mono_data(&s);
        assert!(d[0] > 0.99, "peak positive: {}", d[0]);
        assert_eq!(d[1], 0.0, "silence must stay silence, not +1.0");
        assert_eq!(d[2], -1.0);
    }

    #[test]
    fn twenty_four_bit_uses_the_full_depth() {
        // Guards the `1 << (bits - 1)` scale: a wrong shift here still "works" but quietly
        // loads everything 256x too loud or too quiet.
        let s = decode(int_wav(1, 48_000, 24, &[8_388_607, 0, -8_388_608])).expect("valid file");
        let d = mono_data(&s);
        assert!((d[0] - 1.0).abs() < 1e-6, "peak positive: {}", d[0]);
        assert_eq!(d[2], -1.0);
    }

    #[test]
    fn float_samples_pass_through_untouched() {
        let s = decode(float_wav(1, 48_000, &[0.25, -0.5, 0.0])).expect("valid file");
        assert_eq!(mono_data(&s), [0.25, -0.5, 0.0]);
    }

    #[test]
    fn stereo_is_deinterleaved_not_flattened() {
        let s = decode(float_wav(2, 48_000, &[1.0, -1.0, 0.5, -0.5])).expect("valid file");
        assert_eq!(stereo_data(&s), [[1.0, -1.0], [0.5, -0.5]]);
        assert_eq!(s.source_channels, 2);
    }

    #[test]
    fn extra_channels_are_truncated_to_the_first_pair() {
        let s = decode(float_wav(4, 48_000, &[0.1, 0.2, 0.3, 0.4])).expect("valid file");
        assert_eq!(
            stereo_data(&s),
            [[0.1, 0.2]],
            "surround channels are dropped"
        );
        assert_eq!(
            s.source_channels, 4,
            "the original count survives so the host can warn"
        );
    }

    #[test]
    fn a_sample_rate_below_the_range_is_rejected() {
        // Not a taste call: `resample_to` scales length by `target / sample_rate`, so a 1 Hz
        // header turns a 39 KB file into a request for 192 GB.
        let err = decode_err(int_wav(1, 1, 16, &[0; 8]), "a 1 Hz header must not load");
        // Matches the variant, not the message: the reason is now part of the type, so
        // rewording the text can no longer quietly turn this into a test of nothing.
        assert!(matches!(err, DecodeError::SampleRate(1)), "got {err:?}");
    }

    #[test]
    fn a_sample_rate_above_the_range_is_rejected() {
        // The other end costs CPU rather than memory: the kernel widens as 1/ratio.
        let err = decode_err(int_wav(1, 4_000_000, 16, &[0; 8]), "4 MHz must not load");
        assert!(
            matches!(err, DecodeError::SampleRate(4_000_000)),
            "got {err:?}"
        );
    }

    #[test]
    fn the_bounds_themselves_still_load() {
        // The range has to stay wide enough for real material — vintage lo-fi at the bottom,
        // exotic high-rate gear at the top. Pins the edges against a careless tightening.
        for rate in [1_000, 5_512, 8_363, 44_100, 48_000, 192_000, 768_000] {
            assert!(
                decode(int_wav(1, rate, 16, &[0; 8])).is_ok(),
                "{rate} Hz is legitimate material and must load"
            );
        }
    }

    #[test]
    fn garbage_is_an_error_rather_than_a_panic() {
        let err = decode_err(b"NOTAWAVFILEATALL".to_vec(), "garbage must not load");
        assert!(matches!(err, DecodeError::Unreadable(_)), "got {err:?}");
    }

    #[test]
    fn a_truncated_file_is_an_error_rather_than_a_panic() {
        let mut bytes = int_wav(1, 44_100, 16, &[100, 200, 300]);
        bytes.truncate(bytes.len() - 3);
        let err = decode_err(bytes, "a truncated file must not load");
        assert!(matches!(err, DecodeError::Unreadable(_)), "got {err:?}");
    }

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
