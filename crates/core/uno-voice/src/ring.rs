//! Fixed-window rolling audio buffer addressed by absolute sample index.
//!
//! Mirrors the `audiosender.historySeconds = 10` ring the Quark engine keeps on Yandex
//! devices: enough audio to hand the recognizer the sound that happened *before* a trigger,
//! to trim a barge-in at the right word, and to dump the last seconds of microphone input
//! when something goes wrong.
//!
//! Samples are 16 kHz mono `f32` in `[-1.0, 1.0]`, matching [`crate::audio::TARGET_SAMPLE_RATE`].

use std::collections::VecDeque;

use crate::audio::TARGET_SAMPLE_RATE;

/// Rolling buffer of 16 kHz mono audio.
///
/// Unlike a plain `Vec` with `drain(..)`, pushing is O(1) per sample and the buffer never
/// grows past its capacity. Positions are exposed as absolute sample indices so a consumer
/// can reason about "the 300 ms before the trigger" without tracking offsets itself.
#[derive(Clone, Debug)]
pub struct AudioRing {
    buf: VecDeque<f32>,
    cap: usize,
    written: u64,
}

impl AudioRing {
    /// Creates a ring holding at most `capacity_samples` samples.
    pub fn new(capacity_samples: usize) -> Self {
        let cap = capacity_samples.max(1);
        Self {
            buf: VecDeque::with_capacity(cap),
            cap,
            written: 0,
        }
    }

    /// Creates a ring holding at most `seconds` of audio at 16 kHz.
    pub fn with_seconds(seconds: f32) -> Self {
        let samples = (seconds.max(0.0) * TARGET_SAMPLE_RATE as f32) as usize;
        Self::new(samples)
    }

    /// Appends samples, dropping the oldest ones once the capacity is reached.
    pub fn push(&mut self, samples: &[f32]) {
        if samples.is_empty() {
            return;
        }
        for &sample in samples {
            if self.buf.len() == self.cap {
                self.buf.pop_front();
            }
            self.buf.push_back(sample);
        }
        self.written = self.written.saturating_add(samples.len() as u64);
    }

    /// Total number of samples ever pushed. Monotonic; never reset by [`Self::clear`].
    pub fn written(&self) -> u64 {
        self.written
    }

    /// Number of samples currently held.
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// Whether the ring currently holds no samples.
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// Duration currently held, in milliseconds.
    pub fn buffered_ms(&self) -> u32 {
        let ms = self.buf.len() as u64 * 1000 / u64::from(TARGET_SAMPLE_RATE);
        u32::try_from(ms).unwrap_or(u32::MAX)
    }

    /// Absolute index of the oldest sample still held.
    pub fn oldest(&self) -> u64 {
        self.written - self.buf.len() as u64
    }

    /// Copies the absolute range `[from, to)`.
    ///
    /// Returns `None` when the range is empty, extends past the newest sample, or starts
    /// before the oldest sample still buffered (the data has already been dropped).
    pub fn slice(&self, from: u64, to: u64) -> Option<Vec<f32>> {
        let oldest = self.oldest();
        if from < oldest || from >= to || to > self.written {
            return None;
        }
        let start = usize::try_from(from - oldest).ok()?;
        let len = usize::try_from(to - from).ok()?;
        Some(self.buf.iter().skip(start).take(len).copied().collect())
    }

    /// Copies the newest `ms` milliseconds, or everything buffered when shorter.
    pub fn last_ms(&self, ms: u32) -> Vec<f32> {
        let want = ms_to_samples(ms).min(self.buf.len());
        let start = self.buf.len() - want;
        self.buf.iter().skip(start).copied().collect()
    }

    /// Copies the `ms` milliseconds ending at the absolute position `at`.
    ///
    /// Returns an empty vector when `at` is not covered by the ring any more.
    pub fn window_before(&self, at: u64, ms: u32) -> Vec<f32> {
        let want = u64::try_from(ms_to_samples(ms)).unwrap_or(u64::MAX);
        let from = at.saturating_sub(want);
        self.slice(from, at).unwrap_or_default()
    }

    /// Copies all buffered samples, oldest first.
    pub fn snapshot(&self) -> Vec<f32> {
        self.buf.iter().copied().collect()
    }

    /// Drops all buffered samples. The absolute counter keeps increasing.
    pub fn clear(&mut self) {
        self.buf.clear();
    }
}

fn ms_to_samples(ms: u32) -> usize {
    (u64::from(ms) * u64::from(TARGET_SAMPLE_RATE) / 1000) as usize
}
