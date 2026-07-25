# beat-forge

An early-stage DAW written in Rust, built in deliberately small steps — engine first, UI much
later, on the theory that an instrument nobody can hear yet is easier to redesign than one already
wearing an interface. What exactly it grows into is still an open question, and the engine is
being built so that it stays one.

## Status

Sample playback with polyphony. Load a WAV, press Enter, hear it; overlapping hits mix on a stereo
bus, and an event can be scheduled to land on an exact frame rather than at the start of whatever
buffer the device happened to ask for.

Next is the transport — BPM, a beat grid, sample-accurate position — followed by a step sequencer,
a mixer, a synth, effects, and only then a UI.

## Try it

```sh
cargo run                      # play the built-in synthesized blip
cargo run -- path/to/file.wav  # load and trigger a real WAV
```

While it runs: <kbd>Enter</kbd> fires the sample, `q` then <kbd>Enter</kbd> quits.

A file at a sample rate other than the device's is converted once, on load — the engine reads one
source frame per output frame, so material at the wrong rate would otherwise play at the wrong
pitch. Files with more than two channels keep their first two.

## Layout

A Cargo workspace with two crates:

- **`crates/core`** (`beat-forge-core`) — the engine: audio, sequencing, DSP. Knows nothing about
  UI, and is not allowed to. That rule is what keeps the eventual UI choice reversible.
- **`crates/app`** (`beat-forge`) — a thin host: opens the audio device, builds the output stream,
  reads the keyboard.

## The design, in one paragraph

Everything is organized around separating the real-time audio thread from everything else — UI,
file loading, control logic. The two sides talk only through lock-free single-producer queues,
never mutexes: inside the audio callback there is no allocation, no locking, no I/O and no
printing, because any one of them can take a lock or a page fault and turn a missed deadline into
an audible glitch. State the engine needs is preallocated when it is built. Timing is counted in
frames rather than read from an OS clock, so the beat cannot drift.

## Building

Requires Rust 1.85 or newer (edition 2024); `rust-toolchain.toml` asks rustup for a current stable
toolchain, so a fresh clone needs no setup beyond rustup itself.

On Linux, `cpal` links against ALSA and needs its headers:

```sh
sudo apt-get install libasound2-dev
```

macOS (CoreAudio) and Windows (WASAPI) need nothing extra.

```sh
cargo build                            # build the workspace
cargo test                             # 38 unit tests
cargo clippy --all-targets             # must report zero
cargo doc -p beat-forge-core --no-deps # must report zero
```

## License

Dual-licensed under either of

- Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE))
- MIT license ([LICENSE-MIT](LICENSE-MIT))

at your option. Unless you explicitly state otherwise, any contribution intentionally submitted
for inclusion in this project by you, as defined in the Apache-2.0 license, shall be dual licensed
as above, without any additional terms or conditions.
