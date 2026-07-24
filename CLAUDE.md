# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`beat-forge` is an early-stage DAW / groovebox (FL Studio direction: patterns and beats) written in
Rust. It is built in deliberately small steps — a playable engine first, UI much later. Steps 1–2 of
the roadmap are done (sample playback with polyphony, Enter-key trigger); step 3 (transport/clock,
BPM, sample-accurate position) is next.

Design docs live in `docs/` (written in Russian, and **gitignored** — they are local-only):
`roadmap.md` (step plan), `architecture.md` (decisions + library stack), `language-choice.md`,
`ui-and-tauri.md` (UI-layer tradeoffs). Read `docs/roadmap.md` before starting feature work — it
defines what "the next step" means and records what each completed step actually shipped.

## Commands

```bash
cargo run                      # play the built-in synthesized blip
cargo run -- path/to/file.wav  # load and trigger a real WAV
cargo build                    # build the whole workspace
cargo check -p beat-forge-core # fast type-check of the engine crate alone
cargo clippy --all-targets
cargo fmt
cargo test                     # no tests exist yet
cargo test -p beat-forge-core <name>   # run a single test by name substring
```

While the app runs: Enter fires the sample, `q` + Enter quits.

## Architecture

Cargo workspace (edition 2024, resolver 3) with two crates:

- **`crates/core`** (`beat-forge-core`) — the engine: audio, sequencing, DSP. **Must never know
  anything about UI.** This is a hard rule, not a preference: it keeps the eventual UI choice
  (egui vs Tauri) reversible. Deps: `hound` (WAV), `rtrb` (lock-free queues), `anyhow`.
- **`crates/app`** (`beat-forge` binary) — thin host layer: opens the `cpal` device, builds the
  output stream, reads stdin. Deps: `cpal`, `beat-forge-core`.

### The thread split (the central design decision)

Everything is organized around separating the **real-time audio thread** from **everything else**
(UI, file loading, control logic). They communicate only through **lock-free SPSC queues** (`rtrb`),
never mutexes. In `core/src/lib.rs` this is the `Engine` / `Trigger` pair returned by `engine()`:

- `Engine` lives inside the cpal audio callback. `pump()` drains pending commands once per block;
  `next_sample()` renders one mono frame by mixing a fixed voice pool (with voice stealing when all
  voices are busy).
- `Trigger` lives on the control thread and pushes `Command`s into the ring buffer. Pushes are
  dropped silently if the queue is full — that is correct, not a bug to "fix" with blocking.

**Inside the audio callback: no allocation, no locks, no `println!`, no I/O, no `unwrap()` on
fallible I/O.** Any new engine feature must preallocate its state at construction time. When adding
a control-thread → audio-thread interaction, extend the `Command` enum and handle it in `pump()`
rather than sharing state some other way.

Timing must be **sample-accurate** (counted in frames), never derived from OS timers — otherwise the
beat drifts. This matters starting with the step-3 transport work.

### Known gaps (deliberate, not oversights)

- No mixer/limiter: overlapping voices can sum past [-1.0, 1.0] and clip. Scheduled for roadmap
  step 5.
- No resampling: if the sample rate differs from the device rate, playback is pitch-shifted. The app
  prints a warning rather than resampling.

## Conventions

- Code comments, doc comments, and console/log output are **English**. Design docs in `docs/` are
  Russian; chat with the user is Russian.
- Doc comments (`//!` at module level, `///` on public items) are used throughout `core` to explain
  *why* — especially real-time constraints. Match that density on new public API.
- `anyhow::Result` for fallible setup/loading paths; the real-time path returns no errors.