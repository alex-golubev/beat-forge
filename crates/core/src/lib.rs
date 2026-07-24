//! beat-forge audio engine core.
//!
//! UI-agnostic playback engine. The design splits into two sides:
//! - [`Engine`] runs inside the real-time audio callback and must never allocate,
//!   lock, or block.
//! - [`Trigger`] lives on the control thread and pushes commands to the engine over a
//!   lock-free single-producer/single-consumer queue.
//!
//! The internal bus is stereo: [`Engine::process`] always produces [`Frame`]s whatever the
//! source layout. Mapping that pair onto the output device is the host layer's job.

use std::path::Path;

use rtrb::{Consumer, Producer, RingBuffer};

/// One stereo frame: `[left, right]`.
///
/// An array rather than a flat `&mut [f32]`, so a slice's length is a frame count with no
/// `len % 2 == 0` invariant to uphold.
pub type Frame = [f32; 2];

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

/// How many commands can be in flight between the two threads.
const COMMAND_CAPACITY: usize = 64;

/// How many scheduled events the engine can hold before their frame arrives.
///
/// Preallocated, because the audio thread cannot grow it.
const PENDING_CAPACITY: usize = 64;

/// Messages sent from the control thread to the real-time engine.
///
/// The two variants exist because live input and scheduled input want opposite things.
/// A keypress has no trustworthy timestamp — placing it on an exact frame would mean
/// deferring it into the future, buying accuracy with latency, which is the wrong trade
/// for playing by hand. A sequenced event does know its frame and must land on it.
enum Command {
    /// Start at the beginning of whichever block the engine sees this in.
    Trigger,
    /// Start at this absolute engine frame.
    TriggerAt(u64),
}

/// A single playing instance of the sample.
struct Voice {
    pos: usize,
    active: bool,
}

/// Real-time playback engine. Lives inside the audio callback.
///
/// Mixes a fixed pool of voices, each an independent playback of the same sample.
/// Everything here is allocation- and lock-free.
pub struct Engine {
    sample: Sample,
    voices: Vec<Voice>,
    commands: Consumer<Command>,
    /// Absolute start frames of events whose time has not come yet.
    pending: Vec<u64>,
    /// Frames rendered so far — the engine's timebase.
    frame: u64,
}

/// Control-side handle used to trigger the sample from another thread.
pub struct Trigger {
    tx: Producer<Command>,
}

impl Trigger {
    /// Fire the sample as soon as the engine picks the command up.
    ///
    /// Returns `false` if the command queue was full and the trigger was dropped. A lost
    /// note is impossible to diagnose after the fact, so the result is deliberately not
    /// ignorable — blocking here instead is not an option on the control thread.
    #[must_use]
    pub fn fire(&mut self) -> bool {
        self.tx.push(Command::Trigger).is_ok()
    }

    /// Fire the sample at an absolute engine frame (see [`Engine::frame`]).
    ///
    /// A frame already in the past is clamped to the start of the current block. Returns
    /// `false` if the command queue was full.
    #[must_use]
    pub fn fire_at(&mut self, frame: u64) -> bool {
        self.tx.push(Command::TriggerAt(frame)).is_ok()
    }
}

/// Create an [`Engine`] and its paired [`Trigger`] handle.
///
/// `polyphony` is the number of overlapping sample instances allowed at once.
pub fn engine(sample: Sample, polyphony: usize) -> (Engine, Trigger) {
    let (tx, rx) = RingBuffer::new(COMMAND_CAPACITY);
    let voices = (0..polyphony.max(1))
        .map(|_| Voice {
            pos: 0,
            active: false,
        })
        .collect();
    let engine = Engine {
        sample,
        voices,
        commands: rx,
        pending: Vec::with_capacity(PENDING_CAPACITY),
        frame: 0,
    };
    (engine, Trigger { tx })
}

impl Engine {
    /// Frames rendered since the engine was created — the timebase everything schedules
    /// against. Counted in frames, never derived from an OS timer, so it cannot drift.
    pub fn frame(&self) -> u64 {
        self.frame
    }

    /// Advance the engine by one block: dispatch due events and render `out.len()` frames.
    ///
    /// Events land on their exact frame because the block is rendered as a series of spans
    /// split at event boundaries. The engine owns that split; a host that drained commands
    /// itself and then asked for one flat render could only ever start notes on frame 0.
    ///
    /// The mix can exceed [-1.0, 1.0] when many voices overlap; a proper mixer with gain
    /// staging arrives in a later step.
    pub fn process(&mut self, out: &mut [Frame]) {
        self.collect_commands();

        let block_end = self.frame + out.len() as u64;
        let mut cursor = 0usize;
        loop {
            let next = self.next_event(self.frame + cursor as u64, block_end);
            let split = match next {
                Some(at) => (at - self.frame) as usize,
                None => out.len(),
            };
            if split > cursor {
                self.render_span(&mut out[cursor..split]);
                cursor = split;
            }
            match next {
                Some(at) => self.fire_pending_at(at),
                None => break,
            }
        }

        self.frame = block_end;
    }

    /// Move queued commands into `pending`, resolving each to an absolute frame.
    fn collect_commands(&mut self) {
        while let Ok(cmd) = self.commands.pop() {
            let at = match cmd {
                Command::Trigger => self.frame,
                // Already past: the moment has gone, so play it now rather than never.
                Command::TriggerAt(at) => at.max(self.frame),
            };
            if self.pending.len() < PENDING_CAPACITY {
                self.pending.push(at);
            }
        }
    }

    /// Earliest pending event in `[from, to)`, if any.
    fn next_event(&self, from: u64, to: u64) -> Option<u64> {
        self.pending
            .iter()
            .copied()
            .filter(|&at| at >= from && at < to)
            .min()
    }

    /// Start a voice for every pending event at exactly `at` and drop them.
    fn fire_pending_at(&mut self, at: u64) {
        let mut i = 0;
        while i < self.pending.len() {
            if self.pending[i] == at {
                self.pending.swap_remove(i);
                self.start_voice();
            } else {
                i += 1;
            }
        }
    }

    fn start_voice(&mut self) {
        let slot = match self.voices.iter().position(|v| !v.active) {
            Some(free) => free,
            // Pool exhausted: steal whichever voice is furthest into the sample. For a
            // decaying one-shot that is also the quietest, so it is both the least missed
            // and the one whose abrupt restart makes the smallest discontinuity.
            //
            // `pos` stops being a valid proxy once voices can play different samples —
            // comparing raw positions across lengths is meaningless.
            None => self
                .voices
                .iter()
                .enumerate()
                .max_by_key(|(_, v)| v.pos)
                .map_or(0, |(i, _)| i),
        };
        self.voices[slot].pos = 0;
        self.voices[slot].active = true;
    }

    /// Render one span, overwriting it so a caller cannot forget to clear the buffer.
    fn render_span(&mut self, out: &mut [Frame]) {
        out.fill([0.0, 0.0]);

        let len = self.sample.frames.len();
        for voice in self.voices.iter_mut() {
            if !voice.active {
                continue;
            }
            let n = len.saturating_sub(voice.pos).min(out.len());
            // Matched per voice per block, never per sample.
            match &self.sample.frames {
                Frames::Mono(data) => {
                    let src = &data[voice.pos..voice.pos + n];
                    for (o, &x) in out[..n].iter_mut().zip(src) {
                        o[0] += x;
                        o[1] += x;
                    }
                }
                Frames::Stereo(data) => {
                    let src = &data[voice.pos..voice.pos + n];
                    for (o, s) in out[..n].iter_mut().zip(src) {
                        o[0] += s[0];
                        o[1] += s[1];
                    }
                }
            }
            voice.pos += n;
            if voice.pos >= len {
                voice.active = false;
            }
        }
    }

    /// Sample rate the loaded sample was recorded at, in Hz.
    pub fn sample_rate(&self) -> u32 {
        self.sample.sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample(frames: Frames) -> Sample {
        let source_channels = match &frames {
            Frames::Mono(_) => 1,
            Frames::Stereo(_) => 2,
        };
        Sample {
            frames,
            sample_rate: 48_000,
            source_channels,
        }
    }

    #[test]
    fn mono_lands_equally_in_both_channels() {
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0, 0.5])), 4);
        assert!(trig.fire());

        // Pre-filled with 9.0: the third frame is past the end of the sample, so it also
        // checks that render overwrites instead of leaving stale data.
        let mut out = [[9.0; 2]; 3];
        eng.process(&mut out);
        assert_eq!(out, [[1.0, 1.0], [0.5, 0.5], [0.0, 0.0]]);
    }

    #[test]
    fn stereo_keeps_channels_apart() {
        let (mut eng, mut trig) = engine(sample(Frames::Stereo(vec![[1.0, -1.0]])), 4);
        assert!(trig.fire());

        let mut out = [[0.0; 2]; 1];
        eng.process(&mut out);
        assert_eq!(out, [[1.0, -1.0]]);
    }

    #[test]
    fn playback_is_continuous_across_blocks() {
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0, 2.0, 3.0, 4.0])), 4);
        assert!(trig.fire());

        let mut out = [[0.0; 2]; 4];
        eng.process(&mut out[..1]);
        eng.process(&mut out[1..]);
        assert_eq!(out, [[1.0, 1.0], [2.0, 2.0], [3.0, 3.0], [4.0, 4.0]]);
    }

    #[test]
    fn overlapping_voices_sum() {
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0, 1.0])), 4);
        assert!(trig.fire());

        let mut out = [[0.0; 2]; 1];
        eng.process(&mut out);

        // Second voice starts while the first is still playing.
        assert!(trig.fire());
        eng.process(&mut out);
        assert_eq!(out, [[2.0, 2.0]]);
    }

    #[test]
    fn exhausted_pool_steals_instead_of_dropping() {
        const POLYPHONY: usize = 2;
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0; 8])), POLYPHONY);
        for _ in 0..POLYPHONY + 1 {
            assert!(trig.fire());
        }

        // The third trigger steals rather than being dropped: two voices sound, not three.
        let mut out = [[0.0; 2]; 1];
        eng.process(&mut out);
        assert_eq!(out, [[POLYPHONY as f32, POLYPHONY as f32]]);
    }

    #[test]
    fn stealing_takes_the_oldest_voice_not_a_fixed_slot() {
        // Built so that the oldest voice is *not* slot 0 — otherwise the old
        // "always steal slot 0" policy would pass this test too.
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0; 10])), 2);

        assert!(trig.fire()); // -> slot 0
        eng.process(&mut [[0.0; 2]; 3]);
        assert!(trig.fire()); // -> slot 1
        eng.process(&mut [[0.0; 2]; 8]); // slot 0 runs out and frees up here
        assert!(trig.fire()); // -> slot 0 again, now the *fresh* one
        eng.process(&mut [[0.0; 2]; 1]);

        assert_eq!(eng.voices[0].pos, 1, "just started");
        assert_eq!(eng.voices[1].pos, 9, "nearly finished");

        // Pool is full: the near-finished voice must go, not the one that just started.
        assert!(trig.fire());
        eng.process(&mut [[0.0; 2]; 1]);
        assert_eq!(eng.voices[0].pos, 2, "kept playing");
        assert_eq!(eng.voices[1].pos, 1, "stolen and restarted");
    }

    #[test]
    fn empty_sample_does_not_panic() {
        let (mut eng, mut trig) = engine(sample(Frames::Mono(Vec::new())), 2);
        assert!(trig.fire());

        let mut out = [[0.0; 2]; 2];
        eng.process(&mut out);
        assert_eq!(out, [[0.0, 0.0]; 2]);
    }

    #[test]
    fn frame_counter_advances_by_block_length() {
        let (mut eng, _trig) = engine(sample(Frames::Mono(vec![1.0])), 2);
        assert_eq!(eng.frame(), 0);
        eng.process(&mut [[0.0; 2]; 3]);
        eng.process(&mut [[0.0; 2]; 5]);
        assert_eq!(eng.frame(), 8);
    }

    #[test]
    fn scheduled_trigger_starts_on_its_exact_frame() {
        // The whole point of splitting the block: not quantized to frame 0.
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0; 2])), 4);
        assert!(trig.fire_at(2));

        let mut out = [[0.0; 2]; 5];
        eng.process(&mut out);
        assert_eq!(
            out,
            [[0.0, 0.0], [0.0, 0.0], [1.0, 1.0], [1.0, 1.0], [0.0, 0.0]]
        );
    }

    #[test]
    fn scheduled_trigger_waits_for_a_later_block() {
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0; 2])), 4);
        assert!(trig.fire_at(5));

        let mut first = [[0.0; 2]; 3];
        eng.process(&mut first);
        assert_eq!(first, [[0.0, 0.0]; 3], "must stay silent until frame 5");

        // Second block covers frames 3..7, so the event lands on its index 2.
        let mut second = [[0.0; 2]; 4];
        eng.process(&mut second);
        assert_eq!(second, [[0.0, 0.0], [0.0, 0.0], [1.0, 1.0], [1.0, 1.0]]);
    }

    #[test]
    fn several_events_in_one_block_each_land_separately() {
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0])), 4);
        assert!(trig.fire_at(1));
        assert!(trig.fire_at(3));

        let mut out = [[0.0; 2]; 5];
        eng.process(&mut out);
        assert_eq!(
            out,
            [[0.0, 0.0], [1.0, 1.0], [0.0, 0.0], [1.0, 1.0], [0.0, 0.0]]
        );
    }

    #[test]
    fn schedule_in_the_past_plays_now_rather_than_never() {
        let (mut eng, mut trig) = engine(sample(Frames::Mono(vec![1.0])), 4);
        eng.process(&mut [[0.0; 2]; 10]);

        assert!(trig.fire_at(4)); // already gone
        let mut out = [[0.0; 2]; 2];
        eng.process(&mut out);
        assert_eq!(out, [[1.0, 1.0], [0.0, 0.0]]);
    }

    #[test]
    fn full_queue_reports_the_drop() {
        let (_eng, mut trig) = engine(sample(Frames::Mono(vec![1.0])), 4);
        for _ in 0..COMMAND_CAPACITY {
            assert!(trig.fire());
        }
        assert!(
            !trig.fire(),
            "queue is full — the caller must be able to see it"
        );
    }
}
