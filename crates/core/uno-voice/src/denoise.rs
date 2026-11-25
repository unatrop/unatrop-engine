//! Real-time neural noise & music suppression (RNNoise via `nnnoiseless`) + high-pass filter.
//!
//! RNNoise runs at 48 kHz on 480-sample (10 ms) frames and returns a speech probability
//! alongside the denoised signal. Microphones on Windows/macOS almost always deliver 48 kHz,
//! so [`VoiceDenoiser::process_frame`] denoises at the native rate and decimates once with a
//! proper low-pass, instead of the lossy 48 → 16 → 48 round trip.

use nnnoiseless::DenoiseState;

use crate::audio::{AudioFrame, to_mono_f32, to_mono_f32_16khz};

/// 10 ms at 48 kHz (required by RNNoise).
const RNNOISE_FRAME_48K: usize = DenoiseState::FRAME_SIZE; // 480
/// 1st-order high-pass at ~100 Hz, `fs = 48 kHz` -> alpha = 1 / (1 + 2*pi*100/48000).
const HP_ALPHA_48K: f32 = 0.98708;
/// Frames quieter than this skip RNNoise completely to save CPU during silence/DTX.
const SILENCE_PEAK_THRESHOLD: f32 = 1e-4;
/// Fixed low-pass tap count and power-of-two ring capacity for zero-shift decimation.
const FIR_TAPS: usize = 31;
const FIR_RING_CAP: usize = 32;
const FIR_RING_MASK: usize = FIR_RING_CAP - 1;

/// Real-time neural denoiser that strips background music, beats, fans, and keyboard clicks
/// while returning a neural speech probability (`0.0..=1.0`) per processed chunk.
pub struct VoiceDenoiser {
    state: Box<DenoiseState<'static>>,
    /// High-pass filter state (cuts sub-100 Hz music bass/rumble).
    hp_prev_in: f32,
    hp_prev_out: f32,
    /// Pending 48 kHz input samples waiting to fill a 480-sample frame.
    in_buf: Vec<f32>,
    /// Last 16 kHz sample for smooth 3x linear upsampling (16 kHz input path).
    prev_sample: f32,
    /// Stateful 48 kHz -> 16 kHz low-pass + decimation.
    decimator: FirDecimator,
}

impl VoiceDenoiser {
    /// Creates a denoiser with empty filter state.
    pub fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            hp_prev_in: 0.0,
            hp_prev_out: 0.0,
            in_buf: Vec::with_capacity(RNNOISE_FRAME_48K),
            prev_sample: 0.0,
            decimator: FirDecimator::new(3),
        }
    }

    /// Resets internal buffers (e.g. on session cancel).
    pub fn reset(&mut self) {
        self.state = DenoiseState::new();
        self.hp_prev_in = 0.0;
        self.hp_prev_out = 0.0;
        self.in_buf.clear();
        self.prev_sample = 0.0;
        self.decimator.reset();
    }

    /// Denoises a raw capture frame and returns `(denoised_16khz, max_neural_vad_prob)`.
    ///
    /// 48 kHz frames take the native path: mono downmix, high-pass, RNNoise, one low-passed
    /// decimation. Other rates are resampled to 16 kHz first and take the 3x upsample path.
    pub fn process_frame(&mut self, frame: &AudioFrame) -> (Vec<f32>, f32) {
        match frame.sample_rate {
            48_000 => {
                let mono = to_mono_f32(frame);
                self.process_48k(&mono)
            }
            16_000 => {
                let mono = to_mono_f32(frame);
                self.process(&mono)
            }
            _ => {
                let mono_16k = to_mono_f32_16khz(frame);
                self.process(&mono_16k)
            }
        }
    }

    /// Denoises 48 kHz mono `f32` samples and returns 16 kHz mono output.
    pub fn process_48k(&mut self, samples_48k: &[f32]) -> (Vec<f32>, f32) {
        let (denoised_48k, max_vad) = self.push_48k(samples_48k);
        (self.decimator.process(&denoised_48k), max_vad)
    }

    /// Processes a chunk of 16 kHz mono `f32` samples in `[-1.0, 1.0]`.
    ///
    /// Returns `(denoised_16khz_samples, max_neural_vad_prob)`.
    /// `max_neural_vad_prob` is in `0.0..=1.0`, where values `>= 0.5` indicate human speech
    /// and lower values indicate silence, ambient noise, or background music.
    pub fn process(&mut self, samples_16k: &[f32]) -> (Vec<f32>, f32) {
        let mut upsampled = Vec::with_capacity(samples_16k.len() * 3);
        let mut prev = self.prev_sample;
        for &cur in samples_16k {
            upsampled.push(prev + (cur - prev) * (1.0 / 3.0));
            upsampled.push(prev + (cur - prev) * (2.0 / 3.0));
            upsampled.push(cur);
            prev = cur;
        }
        self.prev_sample = prev;
        self.process_48k(&upsampled)
    }

    /// High-passes and denoises at 48 kHz, returning 48 kHz output and the max speech probability.
    fn push_48k(&mut self, samples_48k: &[f32]) -> (Vec<f32>, f32) {
        self.in_buf.reserve(samples_48k.len());
        for &x in samples_48k {
            let y = HP_ALPHA_48K * (self.hp_prev_out + x - self.hp_prev_in);
            self.hp_prev_in = x;
            self.hp_prev_out = y;
            self.in_buf.push(y);
        }

        let num_frames = self.in_buf.len() / RNNOISE_FRAME_48K;
        if num_frames == 0 {
            return (Vec::new(), 0.0);
        }

        let mut out = Vec::with_capacity(num_frames * RNNOISE_FRAME_48K);
        let mut frame_in = [0.0_f32; RNNOISE_FRAME_48K];
        let mut frame_out = [0.0_f32; RNNOISE_FRAME_48K];
        let mut max_vad: f32 = 0.0;

        for frame_index in 0..num_frames {
            let start = frame_index * RNNOISE_FRAME_48K;
            let frame_slice = &self.in_buf[start..start + RNNOISE_FRAME_48K];
            let peak = frame_slice.iter().fold(0.0_f32, |m, &s| m.max(s.abs()));
            if peak < SILENCE_PEAK_THRESHOLD {
                out.extend(std::iter::repeat_n(0.0_f32, RNNOISE_FRAME_48K));
                continue;
            }

            for (slot, &sample) in frame_in.iter_mut().zip(frame_slice) {
                *slot = sample * 32767.0;
            }

            let vad = self.state.process_frame(&mut frame_out, &frame_in);
            if vad > max_vad {
                max_vad = vad;
            }

            out.extend(frame_out.iter().map(|&s| (s / 32767.0).clamp(-1.0, 1.0)));
        }

        self.in_buf.drain(..num_frames * RNNOISE_FRAME_48K);
        (out, max_vad)
    }
}

impl Default for VoiceDenoiser {
    fn default() -> Self {
        Self::new()
    }
}

/// Windowed-sinc low-pass followed by integer decimation, keeping state in a fixed circular array.
struct FirDecimator {
    taps: [f32; FIR_TAPS],
    ring: [f32; FIR_RING_CAP],
    write_idx: usize,
    factor: usize,
    phase: usize,
}

impl FirDecimator {
    fn new(factor: usize) -> Self {
        let factor = factor.max(1);
        // Cut just below the post-decimation Nyquist to leave a transition band.
        let cutoff = 0.5 / factor as f32 * 0.9;
        let taps_vec = design_low_pass(FIR_TAPS, cutoff);
        let mut taps = [0.0_f32; FIR_TAPS];
        taps.copy_from_slice(&taps_vec);
        Self {
            // Pre-filled with zeros so the filter emits a sample for every `factor` inputs
            // from the very first chunk, instead of swallowing the first `FIR_TAPS` samples.
            ring: [0.0; FIR_RING_CAP],
            write_idx: 0,
            taps,
            factor,
            phase: 0,
        }
    }

    fn reset(&mut self) {
        self.ring.fill(0.0);
        self.write_idx = 0;
        self.phase = 0;
    }

    fn process(&mut self, input: &[f32]) -> Vec<f32> {
        let mut out = Vec::with_capacity(input.len() / self.factor + 1);
        for &sample in input {
            self.ring[self.write_idx] = sample;
            self.write_idx = (self.write_idx + 1) & FIR_RING_MASK;

            self.phase += 1;
            if self.phase == self.factor {
                self.phase = 0;
                let oldest = (self.write_idx + FIR_RING_CAP - FIR_TAPS) & FIR_RING_MASK;
                let mut acc = 0.0;
                for (i, &tap) in self.taps.iter().enumerate() {
                    acc += tap * self.ring[(oldest + i) & FIR_RING_MASK];
                }
                out.push(acc);
            }
        }
        out
    }
}

/// Hann-windowed sinc low-pass with unity DC gain. `cutoff` is in cycles per sample.
fn design_low_pass(len: usize, cutoff: f32) -> Vec<f32> {
    let len = len | 1;
    let mid = (len - 1) as f32 / 2.0;
    let mut taps: Vec<f32> = (0..len)
        .map(|index| {
            let n = index as f32 - mid;
            let sinc = if n.abs() < f32::EPSILON {
                2.0 * cutoff
            } else {
                (2.0 * std::f32::consts::PI * cutoff * n).sin() / (std::f32::consts::PI * n)
            };
            let window =
                0.5 - 0.5 * (2.0 * std::f32::consts::PI * index as f32 / (len - 1) as f32).cos();
            sinc * window
        })
        .collect();

    let sum: f32 = taps.iter().sum();
    if sum.abs() > f32::EPSILON {
        for tap in &mut taps {
            *tap /= sum;
        }
    }
    taps
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_fast_path_skips_rnnoise_and_preserves_length() {
        let mut denoiser = VoiceDenoiser::new();
        let silence = vec![0.0_f32; RNNOISE_FRAME_48K * 2];
        let (out, vad) = denoiser.process_48k(&silence);
        assert_eq!(out.len(), silence.len() / 3);
        assert_eq!(vad, 0.0);
        assert!(out.iter().all(|&s| s == 0.0));
    }

    #[test]
    fn fir_decimator_has_unity_dc_gain_and_resets() {
        let mut dec = FirDecimator::new(3);
        let ones = vec![1.0_f32; 120];
        let out = dec.process(&ones);
        assert_eq!(out.len(), 40);
        for &s in out.iter().skip(15) {
            assert!((s - 1.0).abs() < 1e-4, "expected ~1.0, got {s}");
        }

        dec.reset();
        let zeros = vec![0.0_f32; 30];
        let reset_out = dec.process(&zeros);
        assert!(reset_out.iter().all(|&s| s == 0.0));
    }
}
