//! Streaming utterance segmenter with pre-roll, neural speech probability, and reports.
//!
//! Speech onset requires *two* independent signals: RMS energy above the adaptive noise
//! floor and RNNoise speech probability above a threshold. Energy alone cannot tell a
//! voice from music or a keyboard, which is exactly the failure mode that makes a local
//! assistant transcribe the room instead of the user.

use std::collections::VecDeque;

use crate::audio::{TARGET_SAMPLE_RATE, rms_level};

/// Tuning parameters for real-time utterance segmentation.
#[derive(Clone, Copy, Debug)]
pub struct VadConfig {
    /// Minimum RMS energy to count as speech above the adaptive noise floor.
    pub speech_threshold: f32,
    /// Minimum neural speech probability (`0.0..=1.0`) to count as speech.
    pub speech_prob: f32,
    /// Minimum speech duration (ms) before an utterance is eligible to commit.
    pub min_speech_ms: u32,
    /// Trailing silence (ms) after speech that triggers automatic turn commit.
    pub silence_commit_ms: u32,
    /// Hard cap on a single utterance (ms).
    pub max_utterance_ms: u32,
    /// Pre-roll audio kept before speech onset (ms) so the first syllable is never clipped.
    pub pre_roll_ms: u32,
    /// Trailing silence kept in the committed utterance (ms).
    pub sound_after_trigger_ms: u32,
}

impl Default for VadConfig {
    fn default() -> Self {
        Self {
            speech_threshold: 0.008,
            speech_prob: 0.5,
            min_speech_ms: 100,
            silence_commit_ms: 700,
            max_utterance_ms: 24_000,
            pre_roll_ms: 300,
            sound_after_trigger_ms: 200,
        }
    }
}

/// What ended an utterance.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum VadEnd {
    /// Trailing silence reached `silence_commit_ms`.
    #[default]
    Silence,
    /// The utterance hit `max_utterance_ms`.
    MaxDuration,
    /// The host forced the commit.
    Manual,
}

/// What the segmenter actually delivered for a committed utterance.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct VadReport {
    /// Pre-roll audio actually available before speech onset.
    pub actual_before_ms: u32,
    /// Trailing silence actually included after speech ended.
    pub actual_after_ms: u32,
    /// Mean voiced RMS divided by the tracked noise floor.
    pub rms_ratio: f32,
    /// What ended the utterance.
    pub ended_by: VadEnd,
}

/// Output of feeding a 16 kHz mono chunk into [`EnergyVad`].
#[derive(Debug)]
pub enum VadDecision {
    /// Idle or still collecting speech; includes smoothed visual level in `0.0..=1.0`.
    Continue {
        /// Smoothed level for UI meters.
        level: f32,
        /// Set on the frame where speech began.
        speech_started: bool,
        /// Whether this frame was classified as voiced.
        voiced: bool,
    },
    /// A full spoken utterance is ready for transcription.
    UtteranceReady {
        /// 16 kHz mono audio, pre-roll and trailing silence included.
        audio: Vec<f32>,
        /// What was actually delivered.
        report: VadReport,
    },
}

/// Streaming utterance segmenter operating on 16 kHz mono f32 audio.
pub struct EnergyVad {
    config: VadConfig,
    pre_roll_cap: usize,
    noise_floor: f32,
    in_speech: bool,
    speech_samples: usize,
    silence_samples: usize,
    extra_silence_ms: u32,
    pre_roll: VecDeque<f32>,
    utterance: Vec<f32>,
    onset_pre_roll_samples: usize,
    voiced_rms_sum: f64,
    voiced_frames: u64,
}

impl EnergyVad {
    /// Creates a segmenter from explicit tuning.
    pub fn new(config: VadConfig) -> Self {
        let pre_roll_cap = ms_to_samples(config.pre_roll_ms);
        Self {
            config,
            pre_roll_cap,
            noise_floor: 0.004,
            in_speech: false,
            speech_samples: 0,
            silence_samples: 0,
            extra_silence_ms: 0,
            pre_roll: VecDeque::with_capacity(pre_roll_cap),
            utterance: Vec::new(),
            onset_pre_roll_samples: 0,
            voiced_rms_sum: 0.0,
            voiced_frames: 0,
        }
    }

    /// Current tuning.
    pub fn config(&self) -> VadConfig {
        self.config
    }

    /// Whether an utterance is currently being collected.
    pub fn is_in_speech(&self) -> bool {
        self.in_speech
    }

    /// Voiced audio collected in the current utterance, in milliseconds.
    pub fn voiced_ms(&self) -> u32 {
        samples_to_ms(self.speech_samples)
    }

    /// Trailing silence accumulated since the last voiced frame, in milliseconds.
    pub fn silence_ms(&self) -> u32 {
        samples_to_ms(self.silence_samples)
    }

    /// Extends the current pause budget when mid-utterance text looks incomplete.
    pub fn extend_silence(&mut self, extra_ms: u32) {
        self.extra_silence_ms = self.extra_silence_ms.max(extra_ms);
    }

    /// Returns the currently buffered speech audio (16 kHz mono) for live partial transcription.
    pub fn current_utterance(&self) -> &[f32] {
        &self.utterance
    }

    /// Feeds a chunk together with the neural speech probability from the denoiser.
    pub fn push_frame(&mut self, rms: f32, speech_prob: f32, samples: &[f32]) -> VadDecision {
        if samples.is_empty() {
            return VadDecision::Continue {
                level: 0.0,
                speech_started: false,
                voiced: false,
            };
        }

        let effective_threshold = self.config.speech_threshold.max(self.noise_floor * 2.2);
        let is_voiced = if self.in_speech {
            (rms >= effective_threshold * 0.45
                && speech_prob >= (self.config.speech_prob * 0.45).max(0.20))
                || speech_prob >= self.config.speech_prob.max(0.65)
        } else {
            rms >= effective_threshold && speech_prob >= self.config.speech_prob
        };
        let visual_level = (rms * 7.5).clamp(0.0, 1.0);

        if !self.in_speech {
            if is_voiced {
                self.in_speech = true;
                self.onset_pre_roll_samples = self.pre_roll.len();
                self.speech_samples = samples.len();
                self.silence_samples = 0;
                self.extra_silence_ms = 0;
                self.voiced_rms_sum = f64::from(rms) * samples.len() as f64;
                self.voiced_frames = samples.len() as u64;
                self.utterance.clear();
                self.utterance.extend(self.pre_roll.drain(..));
                self.utterance.extend_from_slice(samples);
                return VadDecision::Continue {
                    level: visual_level,
                    speech_started: true,
                    voiced: true,
                };
            }

            // Track ambient noise floor slowly when idle.
            self.noise_floor = (self.noise_floor * 0.96 + rms * 0.04).clamp(0.001, 0.03);
            for &sample in samples {
                if self.pre_roll.len() >= self.pre_roll_cap {
                    self.pre_roll.pop_front();
                }
                self.pre_roll.push_back(sample);
            }
            return VadDecision::Continue {
                level: visual_level,
                speech_started: false,
                voiced: false,
            };
        }

        // Currently inside speech.
        self.utterance.extend_from_slice(samples);
        if is_voiced {
            self.speech_samples += samples.len();
            self.silence_samples = 0;
            self.extra_silence_ms = 0;
            self.voiced_rms_sum += f64::from(rms) * samples.len() as f64;
            self.voiced_frames += samples.len() as u64;
        } else {
            self.silence_samples += samples.len();
        }

        let max_samples = ms_to_samples(self.config.max_utterance_ms);
        let adapted_silence_ms =
            if self.config.silence_commit_ms >= 500 && self.voiced_ms() >= 3_500 {
                self.config.silence_commit_ms.saturating_mul(125) / 100
            } else {
                self.config.silence_commit_ms
            };
        let silence_limit = ms_to_samples(adapted_silence_ms.saturating_add(self.extra_silence_ms));
        let min_speech = ms_to_samples(self.config.min_speech_ms);

        if self.utterance.len() >= max_samples || self.silence_samples >= silence_limit {
            if self.speech_samples >= min_speech {
                let ended_by = if self.utterance.len() >= max_samples {
                    VadEnd::MaxDuration
                } else {
                    VadEnd::Silence
                };
                let report = self.finish_report(ended_by);
                let audio = self.trim_trailing_silence();
                self.reset_turn_state();
                return VadDecision::UtteranceReady { audio, report };
            }
            self.reset_turn_state();
        }

        VadDecision::Continue {
            level: visual_level,
            speech_started: false,
            voiced: is_voiced,
        }
    }

    /// Feeds samples and computes RMS internally, assuming perfect speech probability.
    ///
    /// Convenience for offline segmentation and tests; the live path should prefer
    /// [`Self::push_frame`] so the neural probability can veto music and noise.
    pub fn push_samples(&mut self, samples: &[f32]) -> VadDecision {
        let rms = rms_level(samples);
        self.push_frame(rms, 1.0, samples)
    }

    /// Forces the currently buffered speech to commit immediately (e.g. push-to-talk release).
    pub fn force_commit(&mut self) -> Option<(Vec<f32>, VadReport)> {
        let min_speech = ms_to_samples(self.config.min_speech_ms);
        let result = if self.in_speech && self.speech_samples >= min_speech {
            let report = self.finish_report(VadEnd::Manual);
            let audio = self.trim_trailing_silence();
            Some((audio, report))
        } else {
            None
        };
        self.reset_turn_state();
        result
    }

    /// Discards any in-progress utterance without emitting it.
    pub fn reset(&mut self) {
        self.reset_turn_state();
        self.pre_roll.clear();
    }

    fn finish_report(&self, ended_by: VadEnd) -> VadReport {
        let noise_floor = self.noise_floor.max(1e-6);
        let mean_voiced_rms = if self.voiced_frames > 0 {
            self.voiced_rms_sum / self.voiced_frames as f64
        } else {
            0.0
        };
        VadReport {
            actual_before_ms: samples_to_ms(self.onset_pre_roll_samples),
            actual_after_ms: samples_to_ms(
                self.silence_samples
                    .min(ms_to_samples(self.config.sound_after_trigger_ms)),
            ),
            rms_ratio: (mean_voiced_rms / f64::from(noise_floor)) as f32,
            ended_by,
        }
    }

    fn trim_trailing_silence(&mut self) -> Vec<f32> {
        let keep = ms_to_samples(self.config.sound_after_trigger_ms);
        let trim = self.silence_samples.saturating_sub(keep);
        if trim > 0 {
            let new_len = self.utterance.len().saturating_sub(trim);
            self.utterance.truncate(new_len);
        }
        std::mem::take(&mut self.utterance)
    }

    fn reset_turn_state(&mut self) {
        self.in_speech = false;
        self.speech_samples = 0;
        self.silence_samples = 0;
        self.extra_silence_ms = 0;
        self.utterance.clear();
        self.onset_pre_roll_samples = 0;
        self.voiced_rms_sum = 0.0;
        self.voiced_frames = 0;
    }
}

fn ms_to_samples(ms: u32) -> usize {
    (u64::from(ms) * u64::from(TARGET_SAMPLE_RATE) / 1000) as usize
}

fn samples_to_ms(samples: usize) -> u32 {
    let ms = samples as u64 * 1000 / u64::from(TARGET_SAMPLE_RATE);
    u32::try_from(ms).unwrap_or(u32::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base_cfg() -> VadConfig {
        VadConfig {
            speech_threshold: 0.02,
            speech_prob: 0.5,
            min_speech_ms: 100,
            silence_commit_ms: 200,
            max_utterance_ms: 5_000,
            pre_roll_ms: 50,
            sound_after_trigger_ms: 50,
        }
    }

    #[test]
    fn hysteresis_holds_quiet_consonants_without_chopping() {
        let mut vad = EnergyVad::new(base_cfg());
        let chunk = vec![0.1_f32; 1600]; // 100 ms

        assert!(matches!(
            vad.push_frame(0.1, 0.8, &chunk),
            VadDecision::Continue {
                speech_started: true,
                ..
            }
        ));

        // Softer consonant below onset threshold (0.011 < 0.02, prob 0.30 < 0.5) stays voiced.
        let decision = vad.push_frame(0.011, 0.30, &chunk);
        assert!(matches!(
            decision,
            VadDecision::Continue { voiced: true, .. }
        ));
        assert_eq!(vad.silence_ms(), 0);

        // True silence still triggers commit after silence_commit_ms.
        let _ = vad.push_frame(0.0, 0.0, &chunk);
        assert!(matches!(
            vad.push_frame(0.0, 0.0, &chunk),
            VadDecision::UtteranceReady { .. }
        ));
    }

    #[test]
    fn extend_silence_delays_commit_and_resets_on_new_speech() {
        let mut vad = EnergyVad::new(base_cfg());
        let chunk = vec![0.1_f32; 1600]; // 100 ms

        vad.push_frame(0.1, 0.8, &chunk);
        vad.extend_silence(300); // total 500 ms

        // 200 ms silence does NOT commit while extended.
        assert!(matches!(
            vad.push_frame(0.0, 0.0, &chunk),
            VadDecision::Continue { .. }
        ));
        assert!(matches!(
            vad.push_frame(0.0, 0.0, &chunk),
            VadDecision::Continue { .. }
        ));

        // Resuming speech clears extra_silence_ms back to base 200 ms.
        vad.push_frame(0.1, 0.8, &chunk);
        assert!(matches!(
            vad.push_frame(0.0, 0.0, &chunk),
            VadDecision::Continue { .. }
        ));
        assert!(matches!(
            vad.push_frame(0.0, 0.0, &chunk),
            VadDecision::UtteranceReady { .. }
        ));
    }
}
