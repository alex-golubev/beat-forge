//! Step 1 — audio chain smoke test.
//!
//! Open the default output device and play a pure 440 Hz sine for a couple of seconds.
//! The point isn't the tone itself but getting familiar with the audio callback and its
//! real-time discipline: no allocations, locks, I/O or panics inside the callback.

use std::time::Duration;

use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{FromSample, Sample, SizedSample};

const FREQUENCY_HZ: f32 = 440.0; // note A4 — the "reference" tone
const AMPLITUDE: f32 = 0.2; // keep it quiet
const DURATION_SECS: f32 = 2.0;
const FADE_SECS: f32 = 0.005; // 5 ms fade in/out to avoid clicks

fn main() -> anyhow::Result<()> {
    // Host — the entry point into the OS audio API (CoreAudio on macOS).
    let host = cpal::default_host();
    let device = host
        .default_output_device()
        .ok_or_else(|| anyhow::anyhow!("no default output device found"))?;

    println!("Device: {}", device.description()?.name());

    // Default config: sample rate, channel count, sample format.
    let config = device.default_output_config()?;
    println!("Config: {config:?}");

    // The sample format depends on the device (usually f32 on macOS).
    // Dispatch to a typed handler for the concrete format.
    match config.sample_format() {
        cpal::SampleFormat::F32 => run::<f32>(&device, &config.into()),
        cpal::SampleFormat::I16 => run::<i16>(&device, &config.into()),
        cpal::SampleFormat::U16 => run::<u16>(&device, &config.into()),
        other => Err(anyhow::anyhow!("unsupported sample format: {other:?}")),
    }
}

fn run<T>(device: &cpal::Device, config: &cpal::StreamConfig) -> anyhow::Result<()>
where
    T: Sample + SizedSample + FromSample<f32>,
{
    let sample_rate = config.sample_rate as f32;
    let channels = config.channels as usize;

    // Phase accumulator: add this step to the phase on every sample.
    // The sine is generated on the fly, with no tables and no allocations — ideal for the callback.
    let phase_increment = FREQUENCY_HZ / sample_rate;
    let mut phase: f32 = 0.0;

    // Anti-click envelope: count the sample index and multiply by a gain that ramps up at
    // the start and down at the end. No waveform discontinuity means no click.
    let total_samples = (DURATION_SECS * sample_rate) as u64;
    let fade_samples = (FADE_SECS * sample_rate) as u64;
    let mut n: u64 = 0;

    let mut next_sample = move || {
        let gain = if n < fade_samples {
            n as f32 / fade_samples as f32 // fade-in: 0 -> 1
        } else if n >= total_samples {
            0.0 // past the end — silence
        } else if n >= total_samples - fade_samples {
            (total_samples - n) as f32 / fade_samples as f32 // fade-out: 1 -> 0
        } else {
            1.0 // sustain — full volume
        };

        let value = (phase * std::f32::consts::TAU).sin() * AMPLITUDE * gain;
        phase = (phase + phase_increment).fract(); // keep phase in [0, 1) to avoid error growth
        n += 1;
        value
    };

    let err_fn = |err| eprintln!("audio stream error: {err}");

    let stream = device.build_output_stream(
        *config,
        move |output: &mut [T], _: &cpal::OutputCallbackInfo| {
            // === AUDIO CALLBACK (real-time) ===
            // No allocations / locks / I/O / panics here.
            // Walk the buffer frame by frame: same sample into every channel (mono -> stereo).
            for frame in output.chunks_mut(channels) {
                let sample = T::from_sample(next_sample());
                for out in frame.iter_mut() {
                    *out = sample;
                }
            }
        },
        err_fn,
        None,
    )?;

    stream.play()?;
    println!("Playing {FREQUENCY_HZ} Hz for {DURATION_SECS} s...");
    // Keep the stream alive: sound plays as long as `stream` is not dropped. Sleep a bit
    // longer than the tone so the fade-out renders to silence before the stream closes.
    std::thread::sleep(Duration::from_secs_f32(DURATION_SECS + 0.1));

    Ok(())
}