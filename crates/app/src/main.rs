//! The host layer: opens the cpal output device and bridges the outside world to the engine.
//!
//! Loads a sample, resamples it to the device rate, then runs the audio stream and fires the
//! sample on every Enter press. This is where control-thread -> audio-thread messaging lives:
//! the keyboard loop pushes triggers over a lock-free queue that the audio callback drains,
//! and stream errors travel back the other way through an atomic.
//!
//! The engine renders a stereo bus; this layer owns the mapping onto the device's channels.
//!
//! Usage:
//!   cargo run -- path/to/sample.wav # play a real file
//!   cargo run # no file: use a built-in test blip

use std::io::{self, BufRead};
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, Ordering};

use beat_forge_core::{Frame, Sample, engine};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

/// Number of overlapping sample instances allowed at once.
const POLYPHONY: usize = 8;

/// Stream event codes. cpal may invoke the error callback from the audio thread, so what
/// happened travels to the control thread as a plain integer — see [`report_stream_error`].
/// Only the distinctions the operator can act on are kept; the message text is dropped.
const ERR_NONE: u32 = 0;
const ERR_DEVICE_LOST: u32 = 1;
const ERR_STREAM_INVALIDATED: u32 = 2;
const ERR_DEVICE_CHANGED: u32 = 3;
const ERR_XRUN: u32 = 4;
const ERR_OTHER: u32 = 5;

/// Frames rendered per pass inside the callback. cpal does not guarantee a fixed block
/// length, so the buffer is preallocated at this size and longer blocks take several
/// passes — the callback itself must never allocate.
const SCRATCH_FRAMES: usize = 2048;

fn main() -> anyhow::Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device found"))?;
    println!("Device: {}", device.description()?.name());

    let config = device.default_output_config()?;
    let device_rate = config.sample_rate();
    println!("Config: {config:?}");

    let sample = match std::env::args().nth(1) {
        Some(path) => {
            println!("Loading {path}");
            Sample::load_wav(path)?
        }
        None => {
            println!("No file given — using a built-in test blip.");
            Sample::blip(device_rate)
        }
    };

    // The engine reads one source frame per output frame, so anything not at the device's
    // rate would play at the wrong pitch. Converted here, once, off the audio thread.
    let sample = sample.resample_to(device_rate);
    if sample.source_sample_rate() != sample.sample_rate() {
        println!(
            "Resampled {} Hz -> {} Hz",
            sample.source_sample_rate(),
            sample.sample_rate()
        );
    }
    if sample.source_channels() > 2 {
        eprintln!(
            "warning: sample has {} channels — only the first two were kept",
            sample.source_channels()
        );
    }

    let (engine, mut trigger) = engine(sample, POLYPHONY);
    let stream_error = Arc::new(AtomicU32::new(ERR_NONE));

    // Bound to the device's native sample format; the stream must stay alive below.
    let errors = Arc::clone(&stream_error);
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => run::<f32>(&device, &config.into(), engine, errors)?,
        cpal::SampleFormat::I16 => run::<i16>(&device, &config.into(), engine, errors)?,
        cpal::SampleFormat::U16 => run::<u16>(&device, &config.into(), engine, errors)?,
        other => anyhow::bail!("unsupported sample format: {other:?}"),
    };

    println!("Ready. Press Enter to trigger, type 'q' then Enter to quit.");
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        // This loop is blocked on stdin, so a stream failure surfaces on the next keypress
        // rather than the moment it happens. A watcher thread would close that gap and is
        // not worth it while the whole UI is one blocking read.
        report_stream_error(&stream_error);
        match line?.trim() {
            "q" | "quit" => break,
            // Live input, so no timestamp worth honouring: fire as soon as possible and
            // accept block-boundary quantization rather than buying accuracy with latency.
            _ => {
                if !trigger.fire() {
                    eprintln!("warning: command queue full — trigger dropped");
                }
            }
        }
    }

    drop(stream); // stop audio before exiting
    Ok(())
}

/// Write one stereo engine frame to one device frame.
///
/// Mono devices get a fold-down, and anything past the first pair stays silent rather
/// than duplicating the front channels into surrounds.
///
/// The clamp is not cosmetic: `FromSample` documents that it assumes `-1.0 <= s < 1.0` and
/// "will overflow otherwise". Integer targets happen to saturate through an `as` cast, but
/// that is an implementation detail — the 24-bit conversion builds its value unchecked, and
/// an f32 device receives whatever it is given. The engine's bus has no gain staging yet, so
/// two overlapping hits of a normalized sample already exceed the range; overs are the
/// normal case here, not an anomaly. A real limiter replaces this in the mixer step.
fn write_frame<T: SizedSample + FromSample<f32>>(out: &mut [T], frame: Frame) {
    // Per channel, so a mono device folds down what a stereo device would actually hear.
    let left = frame[0].clamp(-1.0, 1.0);
    let right = frame[1].clamp(-1.0, 1.0);

    match out.len() {
        0 => {}
        1 => out[0] = T::from_sample((left + right) * 0.5),
        _ => {
            out[0] = T::from_sample(left);
            out[1] = T::from_sample(right);
            for o in &mut out[2..] {
                *o = T::from_sample(0.0);
            }
        }
    }
}

/// Print and clear whatever the audio thread reported, if anything.
fn report_stream_error(errors: &AtomicU32) {
    let message = match errors.swap(ERR_NONE, Ordering::Relaxed) {
        ERR_NONE => return,
        ERR_DEVICE_LOST => "output device went away — audio has stopped",
        ERR_STREAM_INVALIDATED => "stream config is no longer valid — restart to recover",
        ERR_DEVICE_CHANGED => "audio route changed; the stream was rerouted and kept playing",
        ERR_XRUN => "buffer under/overrun — audio glitched",
        _ => "the audio stream failed",
    };
    eprintln!("audio: {message}");
}

fn run<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut engine: beat_forge_core::Engine,
    errors: Arc<AtomicU32>,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    anyhow::ensure!(channels >= 1, "device reports zero output channels");

    // Allocated on the control thread — never inside the callback.
    let mut scratch = vec![[0.0f32; 2]; SCRATCH_FRAMES];

    // cpal may call this from the audio thread, where `eprintln!` would take the stderr lock
    // and allocate — the one place in the project that still broke real-time discipline.
    // Storing a code is wait-free; the control thread does the printing. Only the latest
    // failure survives, which is all the operator can act on anyway.
    let err_fn = move |err: cpal::Error| {
        let code = match err.kind() {
            cpal::ErrorKind::DeviceNotAvailable | cpal::ErrorKind::HostUnavailable => {
                ERR_DEVICE_LOST
            }
            cpal::ErrorKind::StreamInvalidated => ERR_STREAM_INVALIDATED,
            cpal::ErrorKind::DeviceChanged => ERR_DEVICE_CHANGED,
            cpal::ErrorKind::Xrun => ERR_XRUN,
            _ => ERR_OTHER,
        };
        // Latest wins: a burst of xruns collapses into one report, which is all the
        // operator would act on anyway.
        errors.store(code, Ordering::Relaxed);
    };

    let stream = device.build_output_stream(
        *config,
        move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
            // === AUDIO CALLBACK (real-time) ===
            for block in output.chunks_mut(SCRATCH_FRAMES * channels) {
                let frames = block.len() / channels;
                let bus = &mut scratch[..frames];
                engine.process(bus);
                for (out_frame, &frame) in block.chunks_mut(channels).zip(bus.iter()) {
                    write_frame(out_frame, frame);
                }
            }
        },
        err_fn,
        None,
    )?;

    stream.play()?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stereo_device_gets_the_pair_untouched() {
        let mut out = [0.0f32; 2];
        write_frame(&mut out, [0.25, -0.5]);
        assert_eq!(out, [0.25, -0.5], "must be transparent inside the range");
    }

    #[test]
    fn overs_are_clamped_per_channel() {
        let mut out = [0.0f32; 2];
        write_frame(&mut out, [3.0, -7.0]);
        assert_eq!(out, [1.0, -1.0]);
    }

    #[test]
    fn mono_device_gets_a_fold_down() {
        let mut out = [0.0f32; 1];
        write_frame(&mut out, [1.0, 0.0]);
        assert_eq!(out, [0.5]);
    }

    #[test]
    fn mono_fold_down_cannot_escape_the_range() {
        // Clamping happens per channel first, so the fold-down averages what a stereo
        // device would actually have heard rather than the raw sum.
        let mut out = [0.0f32; 1];
        write_frame(&mut out, [4.0, 2.0]);
        assert_eq!(out, [1.0]);
    }

    #[test]
    fn channels_past_the_first_pair_stay_silent() {
        let mut out = [9.0f32; 4];
        write_frame(&mut out, [0.5, -0.5]);
        assert_eq!(out, [0.5, -0.5, 0.0, 0.0]);
    }

    #[test]
    fn zero_channels_is_a_no_op() {
        let mut out: [f32; 0] = [];
        write_frame(&mut out, [1.0, 1.0]);
    }

    #[test]
    fn integer_output_reaches_full_scale_without_wrapping() {
        // Pins down the integer path, which the clamp alone does not change: an `as` cast
        // already saturates. The clamp's value here is that the guarantee stops depending
        // on that — `FromSample` documents the range as a precondition, and the 24-bit
        // conversion it offers is genuinely unchecked.
        let mut out = [0i16; 2];
        write_frame(&mut out, [5.0, -5.0]);
        assert_eq!(out, [i16::MAX, i16::MIN]);
    }
}
