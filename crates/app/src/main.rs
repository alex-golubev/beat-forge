//! Step 2 — load a WAV and trigger it in real time.
//!
//! Wires the beat-forge engine into a cpal output stream and fires the sample on every
//! Enter press. This is where control-thread -> audio-thread messaging first appears:
//! the keyboard loop pushes triggers over a lock-free queue that the audio callback drains.
//!
//! The engine renders a stereo bus; this layer owns the mapping onto the device's channels.
//!
//! Usage:
//!   cargo run -- path/to/sample.wav # play a real file
//!   cargo run # no file: use a built-in test blip

use std::io::{self, BufRead};

use beat_forge_core::{Frame, Sample, engine};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

/// Number of overlapping sample instances allowed at once.
const POLYPHONY: usize = 8;

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

    // Without resampling, a rate mismatch shifts the pitch. Resampling is a later step.
    if sample.sample_rate != device_rate {
        eprintln!(
            "warning: sample is {} Hz but device is {} Hz — pitch will be off",
            sample.sample_rate, device_rate
        );
    }
    if sample.source_channels > 2 {
        eprintln!(
            "warning: sample has {} channels — only the first two were kept",
            sample.source_channels
        );
    }

    let (engine, mut trigger) = engine(sample, POLYPHONY);

    // Bound to the device's native sample format; the stream must stay alive below.
    let stream = match config.sample_format() {
        cpal::SampleFormat::F32 => run::<f32>(&device, &config.into(), engine)?,
        cpal::SampleFormat::I16 => run::<i16>(&device, &config.into(), engine)?,
        cpal::SampleFormat::U16 => run::<u16>(&device, &config.into(), engine)?,
        other => anyhow::bail!("unsupported sample format: {other:?}"),
    };

    println!("Ready. Press Enter to trigger, type 'q' then Enter to quit.");
    let stdin = io::stdin();
    for line in stdin.lock().lines() {
        match line?.trim() {
            "q" | "quit" => break,
            _ => trigger.fire(),
        }
    }

    drop(stream); // stop audio before exiting
    Ok(())
}

/// Write one stereo engine frame to one device frame.
///
/// Mono devices get a fold-down, and anything past the first pair stays silent rather
/// than duplicating the front channels into surrounds.
fn write_frame<T: SizedSample + FromSample<f32>>(out: &mut [T], frame: Frame) {
    match out.len() {
        0 => {}
        1 => out[0] = T::from_sample((frame[0] + frame[1]) * 0.5),
        _ => {
            out[0] = T::from_sample(frame[0]);
            out[1] = T::from_sample(frame[1]);
            for o in &mut out[2..] {
                *o = T::from_sample(0.0);
            }
        }
    }
}

fn run<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut engine: beat_forge_core::Engine,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    anyhow::ensure!(channels >= 1, "device reports zero output channels");

    // Allocated on the control thread — never inside the callback.
    let mut scratch = vec![[0.0f32; 2]; SCRATCH_FRAMES];
    let err_fn = |err| eprintln!("audio stream error: {err}");

    let stream = device.build_output_stream(
        *config,
        move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
            // === AUDIO CALLBACK (real-time) ===
            engine.pump();
            for block in output.chunks_mut(SCRATCH_FRAMES * channels) {
                let frames = block.len() / channels;
                let bus = &mut scratch[..frames];
                engine.render(bus);
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
