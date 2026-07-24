# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## What this is

`beat-forge` is an early-stage DAW / groovebox (FL Studio direction: patterns and beats) written in
Rust. It is built in deliberately small steps — a playable engine first, UI much later. Steps 1–2 of
the roadmap are done (sample playback with polyphony, Enter-key trigger), followed by a code-review
pass; step 3 (transport/clock, BPM, sample-accurate position) is next. Its foundation is already in
place: the engine counts frames and can schedule an event on an exact one.

Design docs live in `docs/` (written in Russian, and **gitignored** — they are local-only):
`roadmap.md` (step plan), `architecture.md` (decisions + library stack), `language-choice.md`,
`ui-and-tauri.md` (UI-layer tradeoffs), `code-review-step2.md` (the step-2 review: what was fixed,
how, and what is still open). Read `docs/roadmap.md` before starting feature work — it defines what
"the next step" means and records what each completed step actually shipped.

## Commands

```bash
cargo run                      # play the built-in synthesized blip
cargo run -- path/to/file.wav  # load and trigger a real WAV
cargo build                    # build the whole workspace
cargo check -p beat-forge-core # fast type-check of the engine crate alone
cargo clippy --all-targets
cargo fmt
cargo test                     # 27 unit tests (20 in core, 7 in app)
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
never mutexes. The module layout follows the same split: `sample.rs` loads and owns audio data on
the control thread, `engine.rs` renders it under real-time constraints, `lib.rs` holds the shared
`Frame` type and the re-exports. The pair returned by `engine()`:

- `Engine` lives inside the cpal audio callback. `process(&mut [Frame])` advances one block: it
  drains the command queue into a preallocated `pending` list, then renders the block as a series of
  spans split at event boundaries, so a scheduled event lands on its exact frame instead of being
  quantized to frame 0. Rendering mixes a fixed voice pool onto a stereo bus; when every voice is
  busy it steals the one furthest into the sample (for a decaying one-shot that is also the
  quietest).
- `Trigger` lives on the control thread. `fire()` starts the sample as soon as the engine sees it —
  live input has no timestamp worth honouring, and deferring it to an exact frame would buy accuracy
  with latency. `fire_at(frame)` targets an absolute engine frame (see `Engine::frame()`) and is what
  the sequencer will use. Both are `#[must_use] -> bool`: a full queue drops the command, and
  blocking the control thread instead is not an option, so the caller must at least be able to see
  it.

**Inside the audio callback: no allocation, no locks, no `println!`, no I/O, no `unwrap()` on
fallible I/O.** Any new engine feature must preallocate its state at construction time. When adding
a control-thread → audio-thread interaction, extend the `Command` enum and handle it in
`collect_commands()` rather than sharing state some other way.

Timing must be **sample-accurate** (counted in frames), never derived from OS timers — otherwise the
beat drifts. This matters starting with the step-3 transport work.

### Known gaps (deliberate, not oversights)

- No mixer/limiter: overlapping voices can sum past [-1.0, 1.0], so the engine's bus clips.
  Scheduled for roadmap step 5. The host clamps per channel in `write_frame` — that is a guard
  against violating `FromSample`'s documented `-1.0 <= s < 1.0` precondition, not gain staging, and
  it stays there once a real limiter lands in the engine.
- Voice stealing restarts the stolen voice without a fade. The discontinuity equals that voice's
  current amplitude, and the stolen voice is the quietest one, so it is a soft tick rather than a
  crack. A proper fade needs per-voice gain with a ramp — i.e. the mixer, step 5.
- Resampling happens once at load (`Sample::resample_to`, a windowed sinc on the control thread),
  so the engine can assume its material is always at the device rate. Two consequences: changing the
  output device mid-session would leave the sample converted for the old rate (unreachable today —
  the stream is built once at startup), and per-voice pitch, when it arrives, is a *separate*
  mechanism — a cheap RT interpolator for a musical effect, not a second copy of this one.

## Conventions

- Code comments, doc comments, and console/log output are **English**. Design docs in `docs/` are
  Russian; chat with the user is Russian.
- Doc comments (`//!` at module level, `///` on public items) are used throughout `core` to explain
  *why* — especially real-time constraints. Match that density on new public API.
- `anyhow::Result` for fallible setup/loading paths; the real-time path returns no errors.