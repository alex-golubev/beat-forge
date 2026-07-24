//! Step 2 — load a WAV and trigger it in real time.
//!
//! Wires the beat-forge engine into a cpal output stream and fires the sample on every
//! Enter press. This is where control-thread -> audio-thread messaging first appears:
//! the keyboard loop pushes triggers over a lock-free queue that the audio callback drains.
//!
//! Usage:
//!   cargo run -- path/to/sample.wav # play a real file
//!   cargo run # no file: use a built-in test blip

use std::io::{self, BufRead};

use beat_forge_core::{Sample, engine};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, SizedSample};

/// Number of overlapping sample instances allowed at once.
const POLYPHONY: usize = 8;

fn main() -> anyhow::Result<()> {
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device found"))?;
    println!("Device: {}", device.description()?.name());

    let config = device.default_output_config()?;
    let device_rate = config.sample_rate();
    println!("Config: {config:?}");

    // Load a sample from the first CLI argument, or fall back to a synthesized blip.
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

    let (engine, mut trigger) = engine(sample, POLYPHONY);

    // Build the stream for the device's native sample format; keep it alive below.
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

fn run<T>(
    device: &cpal::Device,
    config: &cpal::StreamConfig,
    mut engine: beat_forge_core::Engine,
) -> anyhow::Result<cpal::Stream>
where
    T: SizedSample + FromSample<f32>,
{
    let channels = config.channels as usize;
    let err_fn = |err| eprintln!("audio stream error: {err}");

    let stream = device.build_output_stream(
        *config,
        move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
            // === AUDIO CALLBACK (real-time) ===
            // Drain triggers once per block, then render the buffer frame by frame.
            engine.pump();
            for frame in output.chunks_mut(channels) {
                let sample = T::from_sample(engine.next_sample());
                for out in frame.iter_mut() {
                    *out = sample;
                }
            }
        },
        err_fn,
        None,
    )?;

    stream.play()?;
    Ok(stream)
}
