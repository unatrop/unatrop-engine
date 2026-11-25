//! Pure-Rust PCM audio framing, mono downmix, and 16 kHz resampling.

/// Target sample rate expected by Whisper and VAD (16 kHz mono).
pub const TARGET_SAMPLE_RATE: u32 = 16_000;

/// Raw PCM16 audio frame pushed from any host audio capture backend (cpal, mobile, web).
#[derive(Clone, Debug)]
pub struct AudioFrame {
    pub pcm16: Vec<i16>,
    pub sample_rate: u32,
    pub channels: u16,
}

impl AudioFrame {
    pub fn new(pcm16: Vec<i16>, sample_rate: u32, channels: u16) -> Self {
        Self {
            pcm16,
            sample_rate,
            channels: channels.max(1),
        }
    }
}

/// Downmixes interleaved PCM16 to normalized mono `f32` at the native sample rate.
///
/// Use this when the consumer can handle the native rate — the denoiser, for example,
/// prefers 48 kHz because RNNoise runs there and resampling twice only loses signal.
pub fn to_mono_f32(frame: &AudioFrame) -> Vec<f32> {
    if frame.pcm16.is_empty() {
        return Vec::new();
    }

    let channels = usize::from(frame.channels.max(1));
    let mono_len = frame.pcm16.len() / channels;
    let mut mono = Vec::with_capacity(mono_len);

    if channels == 1 {
        for &sample in &frame.pcm16 {
            mono.push(f32::from(sample) / 32768.0);
        }
    } else {
        let inv_ch = 1.0 / (channels as f32 * 32768.0);
        for chunk in frame.pcm16.chunks_exact(channels) {
            let sum: f32 = chunk.iter().map(|&s| f32::from(s)).sum();
            mono.push(sum * inv_ch);
        }
    }

    mono
}

/// Converts interleaved PCM16 audio of any sample rate / channel count into normalized
/// `[-1.0, 1.0]` mono f32 samples at 16 kHz.
pub fn to_mono_f32_16khz(frame: &AudioFrame) -> Vec<f32> {
    if frame.sample_rate == 0 {
        return Vec::new();
    }

    let mono = to_mono_f32(frame);
    if frame.sample_rate == TARGET_SAMPLE_RATE || mono.len() < 2 {
        return mono;
    }

    let ratio = frame.sample_rate as f64 / TARGET_SAMPLE_RATE as f64;
    let out_len = ((mono.len() as f64) / ratio).floor().max(1.0) as usize;
    let mut out = Vec::with_capacity(out_len);

    for i in 0..out_len {
        let src_pos = i as f64 * ratio;
        let idx = src_pos.floor() as usize;
        let frac = (src_pos - idx as f64) as f32;
        let s0 = mono[idx.min(mono.len() - 1)];
        let s1 = mono[(idx + 1).min(mono.len() - 1)];
        out.push(s0 + (s1 - s0) * frac);
    }
    out
}

/// Computes root-mean-square signal energy in `[0.0, 1.0]`.
pub fn rms_level(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum_sq: f64 = samples.iter().map(|&s| f64::from(s) * f64::from(s)).sum();
    ((sum_sq / samples.len() as f64).sqrt() as f32).clamp(0.0, 1.0)
}
