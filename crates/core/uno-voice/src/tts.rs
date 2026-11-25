//! Text-to-speech contract and the Alice/uniproxy HTTP client.
//!
//! The synth itself lives in the `alice-uniproxy` Python sidecar (WebSocket to `uni.ws`,
//! then Ogg/Opus -> 16-bit PCM WAV). This crate only owns the host-independent part:
//! turning a WAV response into a [`SpeechUtterance`] that a host `TtsPlayer` can play.

#[cfg(feature = "alice-tts")]
use std::sync::atomic::{AtomicU64, Ordering};
#[cfg(feature = "alice-tts")]
use std::time::Duration;

use async_trait::async_trait;
#[cfg(feature = "alice-tts")]
use futures_util::StreamExt;
#[cfg(feature = "alice-tts")]
use serde::Serialize;
use tokio::sync::mpsc;

use crate::playback::SpeechUtterance;
use crate::stt::VoiceError;

/// Default address of the local `tools/alice-uniproxy/serve.py` sidecar.
pub const DEFAULT_ALICE_TTS_URL: &str = "http://127.0.0.1:8766";

/// Voice used when the caller does not pick one (confirmed working in uniproxy).
pub const ALICE_DEFAULT_VOICE: &str = "shitova.us";

/// Receiver yielding synthesized audio chunks in playback order.
pub type TtsChunkStream = mpsc::UnboundedReceiver<Result<SpeechUtterance, VoiceError>>;

/// Per-utterance synthesis options. `None` means "engine default".
#[derive(Clone, Debug, Default)]
pub struct TtsOptions {
    /// Voice id, for example `shitova.us`, `jane` or `kolya`.
    pub voice: Option<String>,
    /// BCP-47 language tag, for example `ru-RU`.
    pub lang: Option<String>,
    /// Quality preset understood by the engine (`UltraHigh` for Alice).
    pub quality: Option<String>,
}

/// Framework-independent text-to-speech engine.
#[async_trait]
pub trait TtsEngine: Send + Sync {
    /// Synthesizes `text` into a mono utterance ready for host playback.
    async fn synthesize(
        &self,
        text: &str,
        options: &TtsOptions,
    ) -> Result<SpeechUtterance, VoiceError>;

    /// Streams audio as soon as chunks are available, so playback can start before the full
    /// response is decoded. Engines without streaming support inherit the one-chunk default.
    async fn synthesize_stream(
        &self,
        text: &str,
        options: &TtsOptions,
    ) -> Result<TtsChunkStream, VoiceError> {
        let utterance = self.synthesize(text, options).await?;
        let (tx, rx) = mpsc::unbounded_channel();
        let _ = tx.send(Ok(utterance));
        Ok(rx)
    }
}

/// Alice TTS through the local uniproxy HTTP sidecar.
///
/// Requires the `alice-tts` feature. The sidecar converts Ogg/Opus from `TTS.Generate`
/// into WAV so no Opus decoder has to be linked into the voice core.
#[cfg(feature = "alice-tts")]
pub struct AliceTts {
    endpoint: String,
    client: reqwest::Client,
    voice: String,
    next_id: AtomicU64,
}

#[cfg(feature = "alice-tts")]
impl AliceTts {
    /// Creates a client pointed at `endpoint` (for example `http://127.0.0.1:8766`).
    pub fn new(endpoint: impl Into<String>) -> Self {
        Self {
            endpoint: endpoint.into().trim_end_matches('/').to_owned(),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(90))
                .build()
                .unwrap_or_else(|_| reqwest::Client::new()),
            voice: ALICE_DEFAULT_VOICE.to_owned(),
            next_id: AtomicU64::new(1),
        }
    }

    /// Creates a client from `ALICE_TTS_URL`, falling back to [`DEFAULT_ALICE_TTS_URL`].
    pub fn from_env() -> Self {
        Self::new(std::env::var("ALICE_TTS_URL").unwrap_or_else(|_| DEFAULT_ALICE_TTS_URL.into()))
    }

    /// Overrides the voice used when [`TtsOptions::voice`] is `None`.
    pub fn with_voice(mut self, voice: impl Into<String>) -> Self {
        self.voice = voice.into();
        self
    }

    /// Returns the configured default voice.
    pub fn voice(&self) -> &str {
        &self.voice
    }
}

#[cfg(feature = "alice-tts")]
#[derive(Serialize)]
struct SayRequest<'a> {
    text: &'a str,
    voice: &'a str,
    #[serde(skip_serializing_if = "Option::is_none")]
    lang: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    quality: Option<&'a str>,
}

#[cfg(feature = "alice-tts")]
const STREAM_CHUNK_FRAMES: usize = 2_400; // 50 ms at 48 kHz

#[cfg(feature = "alice-tts")]
#[async_trait]
impl TtsEngine for AliceTts {
    async fn synthesize(
        &self,
        text: &str,
        options: &TtsOptions,
    ) -> Result<SpeechUtterance, VoiceError> {
        let url = format!("{}/say", self.endpoint);
        let request = SayRequest {
            text,
            voice: options.voice.as_deref().unwrap_or(&self.voice),
            lang: options.lang.as_deref(),
            quality: options.quality.as_deref(),
        };
        let body = serde_json::to_vec(&request)
            .map_err(|err| VoiceError::Tts(format!("serialize request: {err}")))?;
        let response = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|err| VoiceError::Tts(format!("POST {url}: {err}")))?;

        let status = response.status();
        let bytes = response
            .bytes()
            .await
            .map_err(|err| VoiceError::Tts(format!("read {url}: {err}")))?;
        if !status.is_success() {
            return Err(VoiceError::Tts(format!(
                "HTTP {status}: {}",
                error_snippet(&bytes)
            )));
        }

        let (sample_rate, samples) = decode_wav_mono(&bytes)?;
        Ok(SpeechUtterance::new(
            self.next_id.fetch_add(1, Ordering::Relaxed),
            sample_rate,
            samples,
        ))
    }

    async fn synthesize_stream(
        &self,
        text: &str,
        options: &TtsOptions,
    ) -> Result<TtsChunkStream, VoiceError> {
        let url = format!("{}/say", self.endpoint);
        let request = SayRequest {
            text,
            voice: options.voice.as_deref().unwrap_or(&self.voice),
            lang: options.lang.as_deref(),
            quality: options.quality.as_deref(),
        };
        let body = serde_json::to_vec(&request)
            .map_err(|err| VoiceError::Tts(format!("serialize request: {err}")))?;
        let response = self
            .client
            .post(&url)
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .map_err(|err| VoiceError::Tts(format!("POST {url}: {err}")))?;

        let status = response.status();
        if !status.is_success() {
            let bytes = response
                .bytes()
                .await
                .map_err(|err| VoiceError::Tts(format!("read {url}: {err}")))?;
            return Err(VoiceError::Tts(format!(
                "HTTP {status}: {}",
                error_snippet(&bytes)
            )));
        }

        let (tx, rx) = mpsc::unbounded_channel();
        let mut decoder = WavStreamDecoder::new();
        let mut stream = response.bytes_stream();
        let mut emitted = false;

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|err| VoiceError::Tts(format!("stream {url}: {err}")))?;
            let samples = match decoder.push(&chunk) {
                Ok(samples) => samples,
                Err(err) => {
                    let _ = tx.send(Err(err));
                    return Ok(rx);
                }
            };
            emitted |= push_chunks(&tx, &decoder, &self.next_id, &samples);
            if tx.is_closed() {
                return Ok(rx);
            }
        }

        let tail = match decoder.finish() {
            Ok(tail) => tail,
            Err(err) => {
                let _ = tx.send(Err(err));
                return Ok(rx);
            }
        };
        emitted |= push_chunks(&tx, &decoder, &self.next_id, &tail);
        if !emitted {
            let _ = tx.send(Err(VoiceError::Tts("empty audio".into())));
        }
        Ok(rx)
    }
}

#[cfg(feature = "alice-tts")]
fn push_chunks(
    tx: &mpsc::UnboundedSender<Result<SpeechUtterance, VoiceError>>,
    decoder: &WavStreamDecoder,
    next_id: &AtomicU64,
    samples: &[f32],
) -> bool {
    let mut sent = false;
    for piece in samples.chunks(STREAM_CHUNK_FRAMES) {
        if piece.is_empty() {
            continue;
        }
        let utterance = SpeechUtterance::new(
            next_id.fetch_add(1, Ordering::Relaxed),
            decoder.sample_rate(),
            piece.to_vec(),
        );
        if tx.send(Ok(utterance)).is_err() {
            break;
        }
        sent = true;
    }
    sent
}

#[cfg(feature = "alice-tts")]
fn error_snippet(bytes: &[u8]) -> String {
    let end = bytes.len().min(300);
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

/// Decodes a RIFF/WAVE buffer into mono `f32` samples.
///
/// Supports 8/16/24/32-bit PCM and 32-bit IEEE float, any channel count, and keeps the
/// file's sample rate. Alice's sidecar returns 16-bit PCM, but accepting the other common
/// formats makes this useful for any TTS engine that speaks WAV.
pub fn decode_wav_mono(bytes: &[u8]) -> Result<(u32, Vec<f32>), VoiceError> {
    if bytes.len() < 12 || &bytes[..4] != b"RIFF" || &bytes[8..12] != b"WAVE" {
        return Err(VoiceError::Tts("not a RIFF/WAVE buffer".into()));
    }

    let mut cursor = 12usize;
    let mut fmt: Option<(u16, u16, u32, u16)> = None; // format, channels, rate, bits
    let mut data: Option<&[u8]> = None;
    while cursor + 8 <= bytes.len() {
        let id = &bytes[cursor..cursor + 4];
        let size = u32::from_le_bytes(
            bytes[cursor + 4..cursor + 8]
                .try_into()
                .map_err(|_| VoiceError::Tts("bad WAV chunk size".into()))?,
        ) as usize;
        let start = cursor + 8;
        let end = start.saturating_add(size).min(bytes.len());
        if id == b"fmt " {
            if end < start + 16 {
                return Err(VoiceError::Tts("truncated WAV fmt chunk".into()));
            }
            fmt = Some((
                u16::from_le_bytes(bytes[start..start + 2].try_into().unwrap()),
                u16::from_le_bytes(bytes[start + 2..start + 4].try_into().unwrap()),
                u32::from_le_bytes(bytes[start + 4..start + 8].try_into().unwrap()),
                u16::from_le_bytes(bytes[start + 14..start + 16].try_into().unwrap()),
            ));
        } else if id == b"data" {
            data = Some(&bytes[start..end]);
        }
        cursor = start.saturating_add(size).saturating_add(size & 1);
    }

    let (format, channels, sample_rate, bits) =
        fmt.ok_or_else(|| VoiceError::Tts("WAV has no fmt chunk".into()))?;
    let data = data.ok_or_else(|| VoiceError::Tts("WAV has no data chunk".into()))?;
    if channels == 0 || sample_rate == 0 || bits == 0 {
        return Err(VoiceError::Tts("invalid WAV format header".into()));
    }
    let bytes_per_sample = usize::from(bits) / 8;
    if bytes_per_sample == 0 || bits % 8 != 0 {
        return Err(VoiceError::Tts("unsupported WAV bit depth".into()));
    }

    let frame_size = bytes_per_sample * usize::from(channels);
    let mut samples = Vec::with_capacity(data.len() / frame_size);
    for frame in data.chunks_exact(frame_size) {
        let mut sum = 0.0f32;
        for channel in 0..usize::from(channels) {
            let raw = &frame[channel * bytes_per_sample..(channel + 1) * bytes_per_sample];
            sum += decode_sample(format, bits, raw)?;
        }
        samples.push(sum / f32::from(channels));
    }
    Ok((sample_rate, samples))
}

fn validate_wav_format(format: u16, bits: u16) -> Result<(), VoiceError> {
    match (format, bits) {
        (1, 8 | 16 | 24 | 32) | (3, 32) => Ok(()),
        _ => Err(VoiceError::Tts(format!(
            "unsupported WAV format {format}, {bits} bit"
        ))),
    }
}

fn decode_sample(format: u16, bits: u16, raw: &[u8]) -> Result<f32, VoiceError> {
    validate_wav_format(format, bits)?;
    Ok(match (format, bits) {
        (1, 8) => (raw[0] as f32 - 128.0) / 128.0,
        (1, 16) => {
            let value = i16::from_le_bytes(raw[..2].try_into().unwrap());
            f32::from(value) / 32_768.0
        }
        (1, 24) => {
            let value = i32::from(raw[0]) | (i32::from(raw[1]) << 8) | (i32::from(raw[2]) << 16);
            let value = if value & 0x0080_0000 != 0 {
                value | !0x00ff_ffff
            } else {
                value
            };
            value as f32 / 8_388_608.0
        }
        (1, 32) => {
            let value = i32::from_le_bytes(raw[..4].try_into().unwrap());
            value as f32 / 2_147_483_648.0
        }
        (3, 32) => f32::from_le_bytes(raw[..4].try_into().unwrap()),
        _ => unreachable!("validated above"),
    })
}

/// Incremental WAV reader for chunked HTTP responses.
///
/// The header is parsed once; then every complete PCM frame is converted and handed to the
/// caller as soon as it arrives. This is what lets playback start before the whole file is
/// downloaded.
#[cfg(feature = "alice-tts")]
struct WavStreamDecoder {
    header: Vec<u8>,
    data: Vec<u8>,
    consumed: usize,
    header_done: bool,
    sample_rate: u32,
    channels: u16,
    bits: u16,
    format: u16,
}

#[cfg(feature = "alice-tts")]
impl WavStreamDecoder {
    fn new() -> Self {
        Self {
            header: Vec::with_capacity(64),
            data: Vec::new(),
            consumed: 0,
            header_done: false,
            sample_rate: 0,
            channels: 0,
            bits: 0,
            format: 0,
        }
    }

    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    fn push(&mut self, bytes: &[u8]) -> Result<Vec<f32>, VoiceError> {
        if !self.header_done {
            self.header.extend_from_slice(bytes);
            if let Some(data_start) = self.parse_header()? {
                self.data.extend_from_slice(&self.header[data_start..]);
                self.header.clear();
                self.header_done = true;
            }
        } else {
            self.data.extend_from_slice(bytes);
        }
        self.take_samples()
    }

    fn finish(&mut self) -> Result<Vec<f32>, VoiceError> {
        if !self.header_done {
            return Err(VoiceError::Tts("WAV stream ended before data chunk".into()));
        }
        self.take_samples()
    }

    fn parse_header(&mut self) -> Result<Option<usize>, VoiceError> {
        if self.header.len() < 12 {
            return Ok(None);
        }
        if &self.header[..4] != b"RIFF" || &self.header[8..12] != b"WAVE" {
            return Err(VoiceError::Tts("not a RIFF/WAVE buffer".into()));
        }

        let mut cursor = 12usize;
        let mut fmt: Option<(u16, u16, u32, u16)> = None;
        while cursor + 8 <= self.header.len() {
            let id = &self.header[cursor..cursor + 4];
            let size = u32::from_le_bytes(self.header[cursor + 4..cursor + 8].try_into().unwrap())
                as usize;
            let start = cursor + 8;
            if id == b"fmt " {
                if size < 16 {
                    return Err(VoiceError::Tts("truncated WAV fmt chunk".into()));
                }
                if self.header.len() < start + 16 {
                    return Ok(None);
                }
                fmt = Some((
                    u16::from_le_bytes(self.header[start..start + 2].try_into().unwrap()),
                    u16::from_le_bytes(self.header[start + 2..start + 4].try_into().unwrap()),
                    u32::from_le_bytes(self.header[start + 4..start + 8].try_into().unwrap()),
                    u16::from_le_bytes(self.header[start + 14..start + 16].try_into().unwrap()),
                ));
            } else if id == b"data" {
                let (format, channels, sample_rate, bits) =
                    fmt.ok_or_else(|| VoiceError::Tts("WAV has no fmt chunk".into()))?;
                if channels == 0 || sample_rate == 0 || bits == 0 {
                    return Err(VoiceError::Tts("invalid WAV format header".into()));
                }
                validate_wav_format(format, bits)?;
                self.sample_rate = sample_rate;
                self.channels = channels;
                self.bits = bits;
                self.format = format;
                return Ok(Some(start));
            }
            cursor = start.saturating_add(size).saturating_add(size & 1);
        }
        Ok(None)
    }

    fn take_samples(&mut self) -> Result<Vec<f32>, VoiceError> {
        if !self.header_done {
            return Ok(Vec::new());
        }
        let bytes_per_sample = usize::from(self.bits) / 8;
        let frame_size = bytes_per_sample * usize::from(self.channels);
        if frame_size == 0 {
            return Err(VoiceError::Tts("invalid WAV frame size".into()));
        }
        let available = self.data.len().saturating_sub(self.consumed);
        let frames = available / frame_size;
        if frames == 0 {
            return Ok(Vec::new());
        }
        let take = frames * frame_size;
        let mut samples = Vec::with_capacity(frames);
        for frame in self.data[self.consumed..self.consumed + take].chunks_exact(frame_size) {
            let mut sum = 0.0f32;
            for channel in 0..usize::from(self.channels) {
                let raw = &frame[channel * bytes_per_sample..(channel + 1) * bytes_per_sample];
                sum += decode_sample(self.format, self.bits, raw)?;
            }
            samples.push(sum / f32::from(self.channels));
        }
        self.consumed += take;
        if self.consumed == self.data.len() {
            self.data.clear();
            self.consumed = 0;
        } else if self.consumed > 64 * 1024 {
            self.data.drain(..self.consumed);
            self.consumed = 0;
        }
        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wav(rate: u32, channels: u16, samples: &[i16]) -> Vec<u8> {
        let data_len = samples.len() * 2;
        let mut out = Vec::with_capacity(44 + data_len);
        out.extend_from_slice(b"RIFF");
        out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        out.extend_from_slice(b"WAVEfmt ");
        out.extend_from_slice(&16u32.to_le_bytes());
        out.extend_from_slice(&1u16.to_le_bytes()); // PCM
        out.extend_from_slice(&channels.to_le_bytes());
        out.extend_from_slice(&rate.to_le_bytes());
        out.extend_from_slice(&(rate * u32::from(channels) * 2).to_le_bytes());
        out.extend_from_slice(&(channels * 2).to_le_bytes());
        out.extend_from_slice(&16u16.to_le_bytes());
        out.extend_from_slice(b"data");
        out.extend_from_slice(&(data_len as u32).to_le_bytes());
        for sample in samples {
            out.extend_from_slice(&sample.to_le_bytes());
        }
        out
    }

    #[test]
    fn decodes_pcm16_mono() {
        let bytes = wav(48_000, 1, &[0, 16_384, -16_384]);
        let (rate, samples) = decode_wav_mono(&bytes).expect("valid wav");
        assert_eq!(rate, 48_000);
        assert_eq!(samples.len(), 3);
        assert!((samples[1] - 0.5).abs() < 0.001);
        assert!((samples[2] + 0.5).abs() < 0.001);
    }

    #[test]
    fn stereo_is_downmixed_to_mono() {
        let bytes = wav(16_000, 2, &[0, 16_384, -16_384, 0]);
        let (rate, samples) = decode_wav_mono(&bytes).expect("valid wav");
        assert_eq!(rate, 16_000);
        assert_eq!(samples.len(), 2);
        assert!((samples[0] - 0.25).abs() < 0.001);
        assert!((samples[1] + 0.25).abs() < 0.001);
    }

    #[test]
    fn rejects_non_wav() {
        let err = decode_wav_mono(b"not audio at all").unwrap_err();
        assert!(err.to_string().contains("RIFF"));
    }

    #[cfg(feature = "alice-tts")]
    #[test]
    fn wav_stream_decoder_emits_samples_from_partial_chunks() {
        let bytes = wav(48_000, 1, &[0, 16_384, -16_384]);
        let mut decoder = WavStreamDecoder::new();

        assert!(
            decoder
                .push(&bytes[..40])
                .expect("partial header")
                .is_empty()
        );
        assert!(decoder.sample_rate() == 0);

        let samples = decoder.push(&bytes[40..]).expect("rest of stream");
        assert_eq!(decoder.sample_rate(), 48_000);
        assert_eq!(samples.len(), 3);
        assert!((samples[1] - 0.5).abs() < 0.001);
        assert!((samples[2] + 0.5).abs() < 0.001);
    }
}
