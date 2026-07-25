//! The real-time side: voice pool, event scheduling and rendering.
//!
//! Everything below runs inside the audio callback, with the single exception of
//! [`Trigger`], which is the control thread's only way in. No allocation, no locks, no I/O
//! past this line — state that the engine needs is preallocated when it is built.

use std::fmt;

use rtrb::{Consumer, Producer, RingBuffer};

use crate::{Frame, Frames, Sample};

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

/// Summarises the state an engine is usually inspected for, not its storage.
///
/// Written out rather than derived because two of the fields print badly: the voice pool is
/// mostly idle slots, and `rtrb`'s `Debug` dumps pointers and cache padding. Formatting never
/// happens inside the audio callback, so nothing here is bound by real-time rules.
///
/// The cost of writing it out is that a field added later is not picked up automatically —
/// the transport arriving in step 3 will want its position listed here too.
impl fmt::Debug for Engine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Engine")
            .field("frame", &self.frame)
            .field(
                "active_voices",
                &self.voices.iter().filter(|v| v.active).count(),
            )
            .field("polyphony", &self.voices.len())
            .field("pending", &self.pending.len())
            .field("sample", &self.sample)
            // `..` rather than `finish()`: the command queue is deliberately left out, and
            // saying so is more honest than printing a complete-looking struct.
            .finish_non_exhaustive()
    }
}

/// Control-side handle used to trigger the sample from another thread.
pub struct Trigger {
    tx: Producer<Command>,
}

/// Reports the one thing worth knowing: whether the queue is backing up.
///
/// `rtrb`'s own `Debug` prints the ring buffer's pointers, cache padding and `PhantomData`,
/// which says nothing about whether triggers are getting through.
impl fmt::Debug for Trigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Trigger")
            .field("free_slots", &self.tx.slots())
            .field("capacity", &COMMAND_CAPACITY)
            .finish()
    }
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
#[must_use = "the engine and its trigger are the only handles to the sample just consumed"]
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
    #[must_use]
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
                Some(at) => {
                    // `next_event` only returns frames below `block_end`, so this always fits
                    // and `try_from` never takes the fallback. Written as a conversion that
                    // can fail rather than a cast that cannot complain: the block length is a
                    // safe answer if the invariant ever breaks, where a truncating cast would
                    // silently render the wrong span and a panic would kill the audio thread.
                    let offset = usize::try_from(at - self.frame).unwrap_or(out.len());
                    debug_assert!(
                        offset < out.len(),
                        "event at {at} is outside the block ending at {block_end}"
                    );
                    offset
                }
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
        for voice in &mut self.voices {
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
}

#[cfg(test)]
mod tests {
    // The engine's arithmetic is exact — it sums and copies stored samples without scaling —
    // so the expected buffers are compared bit for bit on purpose. Voice counts are small
    // integers that `f32` holds exactly, so widening one to build an expectation loses nothing.
    #![allow(clippy::float_cmp, clippy::cast_precision_loss)]

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
            source_sample_rate: 48_000,
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
        for _ in 0..=POLYPHONY {
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
