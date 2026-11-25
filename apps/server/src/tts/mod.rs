//! Pure-Rust Alice `uni.ws` TTS client + Ogg/Opus -> PCM16 decoder with connection pooling.

pub(crate) mod orchestrator;

use std::collections::VecDeque;
use std::io::Cursor;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use ogg::reading::PacketReader;
use opus_decoder::OpusDecoder;
use serde_json::{Value, json};
use tokio::net::TcpStream;
use tokio::sync::{Mutex, Semaphore};
use tokio_tungstenite::tungstenite::Message as WsMessage;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream, connect_async};
use uuid::Uuid;

const UNIPROXY_URL: &str = "wss://uniproxy.alice.ya.ru/uni.ws";
const DEFAULT_VOICE: &str = "shitova.us";
const DEFAULT_LANG: &str = "ru-RU";
const DEFAULT_QUALITY: &str = "High";
const DEFAULT_EMOTION: &str = "neutral";
const OPUS_SAMPLE_RATE: u32 = 48_000;
const MAX_FRAME_SAMPLES: usize = 5760; // 120ms @ 48kHz
const MAX_CONCURRENT_SYNTHESIS: usize = 2;
const MAX_CACHE_ENTRIES: usize = 32;
const MAX_CACHE_TEXT_BYTES: usize = 160;
const MAX_TTS_TEXT_BYTES: usize = 4096;
const MAX_OGG_AUDIO_BYTES: usize = 16 * 1024 * 1024;
const MAX_DECODED_PCM_SAMPLES: usize = (OPUS_SAMPLE_RATE as usize) * 180;

type WsStream = WebSocketStream<MaybeTlsStream<TcpStream>>;
type TtsCacheKey = (String, String, String, String, String);

#[derive(Clone, Debug, PartialEq, Eq)]
struct CachedAudio {
    sample_rate: u32,
    pcm_s16le: Vec<u8>,
    duration_ms: u32,
}

struct TtsCache {
    entries: VecDeque<(TtsCacheKey, CachedAudio)>,
    max_entries: usize,
}

impl TtsCache {
    fn new(max_entries: usize) -> Self {
        Self {
            entries: VecDeque::with_capacity(max_entries),
            max_entries,
        }
    }

    fn get(&mut self, key: &TtsCacheKey) -> Option<CachedAudio> {
        let pos = self.entries.iter().position(|(k, _)| k == key)?;
        let (k, audio) = self.entries.remove(pos)?;
        let cloned = audio.clone();
        self.entries.push_back((k, audio));
        Some(cloned)
    }

    fn insert(&mut self, key: TtsCacheKey, audio: CachedAudio) {
        if self.max_entries == 0 {
            return;
        }
        if let Some(pos) = self.entries.iter().position(|(k, _)| k == &key) {
            self.entries.remove(pos);
        } else if self.entries.len() >= self.max_entries {
            self.entries.pop_front();
        }
        self.entries.push_back((key, audio));
    }
}

pub struct SynthesizedAudio {
    pub utterance_id: u64,
    pub sample_rate: u32,
    pub pcm_s16le: Vec<u8>,
    pub duration_ms: u32,
}

/// Pooled `uni.ws` TTS client. Reuses an open TLS+WebSocket connection across turns
const MAX_POOLED_CONNS: usize = 3;

/// to avoid handshake latency, reconnecting transparently on drop.
pub struct AliceTtsPool {
    idle: Mutex<Vec<WsStream>>,
    next_id: AtomicU64,
    synth_limit: Arc<Semaphore>,
    cache: StdMutex<TtsCache>,
}

impl Default for AliceTtsPool {
    fn default() -> Self {
        Self::new()
    }
}

impl AliceTtsPool {
    pub fn new() -> Self {
        Self {
            idle: Mutex::new(Vec::with_capacity(MAX_POOLED_CONNS)),
            next_id: AtomicU64::new(1),
            synth_limit: Arc::new(Semaphore::new(MAX_CONCURRENT_SYNTHESIS)),
            cache: StdMutex::new(TtsCache::new(MAX_CACHE_ENTRIES)),
        }
    }

    async fn take_conn(&self) -> Result<WsStream, String> {
        if let Some(ws) = self.idle.lock().await.pop() {
            return Ok(ws);
        }
        connect().await
    }

    async fn return_conn(&self, ws: WsStream) {
        let mut guard = self.idle.lock().await;
        if guard.len() < MAX_POOLED_CONNS {
            guard.push(ws);
        }
    }

    /// Pre-warms a pooled `uni.ws` TLS+WebSocket connection in the background.
    pub async fn prewarm(&self) {
        if !self.idle.lock().await.is_empty() {
            return;
        }
        if let Ok(ws) = connect().await {
            self.return_conn(ws).await;
        }
    }

    pub async fn synthesize(
        &self,
        text: &str,
        voice: Option<&str>,
        lang: Option<&str>,
        quality: Option<&str>,
        emotion: Option<&str>,
    ) -> Result<SynthesizedAudio, String> {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return Err("empty tts text".to_owned());
        }
        if trimmed.len() > MAX_TTS_TEXT_BYTES {
            return Err(format!(
                "tts text exceeds limit ({MAX_TTS_TEXT_BYTES} bytes)"
            ));
        }

        let voice = voice.unwrap_or(DEFAULT_VOICE);
        let lang = lang.unwrap_or(DEFAULT_LANG);
        let quality = quality.unwrap_or(DEFAULT_QUALITY);
        let emotion = emotion.unwrap_or(DEFAULT_EMOTION);

        let cache_key = (trimmed.len() <= MAX_CACHE_TEXT_BYTES).then(|| {
            (
                trimmed.to_owned(),
                voice.to_owned(),
                lang.to_owned(),
                quality.to_owned(),
                emotion.to_owned(),
            )
        });

        if let Some(ref key) = cache_key
            && let Ok(mut guard) = self.cache.lock()
            && let Some(cached) = guard.get(key)
        {
            let utterance_id = self.next_id.fetch_add(1, Ordering::Relaxed);
            return Ok(SynthesizedAudio {
                utterance_id,
                sample_rate: cached.sample_rate,
                pcm_s16le: cached.pcm_s16le,
                duration_ms: cached.duration_ms,
            });
        }

        let _permit = self
            .synth_limit
            .acquire()
            .await
            .map_err(|_| "tts limiter closed".to_owned())?;

        for attempt in 0..2 {
            let mut ws = if attempt == 0 {
                self.take_conn().await?
            } else {
                connect().await?
            };
            match run_tts_generate(&mut ws, trimmed, voice, lang, quality, emotion).await {
                Ok(ogg_bytes) => {
                    self.return_conn(ws).await;
                    let (pcm_s16le, duration_ms) =
                        tokio::task::spawn_blocking(move || decode_ogg_opus_to_pcm16le(&ogg_bytes))
                            .await
                            .map_err(|e| format!("tts decode join error: {e}"))??;

                    if let Some(key) = cache_key
                        && let Ok(mut guard) = self.cache.lock()
                    {
                        guard.insert(
                            key,
                            CachedAudio {
                                sample_rate: OPUS_SAMPLE_RATE,
                                pcm_s16le: pcm_s16le.clone(),
                                duration_ms,
                            },
                        );
                    }

                    let utterance_id = self.next_id.fetch_add(1, Ordering::Relaxed);
                    return Ok(SynthesizedAudio {
                        utterance_id,
                        sample_rate: OPUS_SAMPLE_RATE,
                        pcm_s16le,
                        duration_ms,
                    });
                }
                Err(err) if attempt == 0 => {
                    tracing::debug!("reconnecting uni.ws after error: {err}");
                }
                Err(err) => {
                    return Err(err);
                }
            }
        }
        Err("tts synthesis failed after reconnect".to_owned())
    }
}

fn build_event(namespace: &str, name: &str, payload: Value) -> String {
    json!({
        "event": {
            "header": {
                "namespace": namespace,
                "name": name,
                "messageId": Uuid::new_v4().to_string(),
                "requestId": Uuid::new_v4().to_string(),
            },
            "payload": payload,
        }
    })
    .to_string()
}

/// Opens a pooled `uni.ws` connection.
///
/// `TTS.Generate` works anonymously: we verified that neither `SynchronizeState`, Vins nor an
/// Alice dialog session is needed. Skipping the old handshake removes up to six seconds of
/// dead time from a cold connection and from `prewarm()`.
fn validate_uniproxy_url(raw: &str) -> Result<(String, String), String> {
    let trimmed = raw.trim();
    if trimmed.is_empty()
        || trimmed.chars().any(char::is_control)
        || trimmed.chars().any(char::is_whitespace)
    {
        return Err("invalid ALICE_UNIPROXY_URL".to_owned());
    }
    let Some((scheme, rest)) = trimmed.split_once("://") else {
        return Err("invalid ALICE_UNIPROXY_URL scheme".to_owned());
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err("invalid ALICE_UNIPROXY_URL authority".to_owned());
    }
    let host = if authority.starts_with('[') {
        authority
            .find(']')
            .map(|idx| &authority[..=idx])
            .unwrap_or("")
    } else {
        authority.split(':').next().unwrap_or("")
    };
    if host.is_empty() {
        return Err("invalid ALICE_UNIPROXY_URL host".to_owned());
    }
    let is_loopback =
        host.eq_ignore_ascii_case("localhost") || host == "127.0.0.1" || host == "[::1]";
    let allowed =
        scheme.eq_ignore_ascii_case("wss") || (scheme.eq_ignore_ascii_case("ws") && is_loopback);
    if !allowed {
        return Err("ALICE_UNIPROXY_URL must use wss:// (or ws:// on loopback)".to_owned());
    }
    Ok((trimmed.to_owned(), host.to_owned()))
}

async fn connect() -> Result<WsStream, String> {
    let raw = std::env::var("ALICE_UNIPROXY_URL").unwrap_or_else(|_| UNIPROXY_URL.to_owned());
    let (url, host) = validate_uniproxy_url(&raw)?;
    let (ws, _) = tokio::time::timeout(Duration::from_secs(10), connect_async(&url))
        .await
        .map_err(|_| format!("uni.ws connect timeout ({host})"))?
        .map_err(|_| format!("uni.ws connect error ({host})"))?;
    Ok(ws)
}

async fn run_tts_generate(
    ws: &mut WsStream,
    text: &str,
    voice: &str,
    lang: &str,
    quality: &str,
    emotion: &str,
) -> Result<Vec<u8>, String> {
    let req = build_event(
        "TTS",
        "Generate",
        json!({
            "voice": voice,
            "lang": lang,
            "format": "audio/opus",
            "emotion": emotion,
            "quality": quality,
            "text": text,
        }),
    );
    ws.send(WsMessage::Text(req.into()))
        .await
        .map_err(|e| format!("uni.ws TTS.Generate send error: {e}"))?;

    let mut audio = Vec::new();
    let mut stream_id: Option<u32> = None;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        let step = tokio::time::timeout(remaining.min(Duration::from_secs(4)), ws.next()).await;
        let Some(frame_res) = (match step {
            Ok(item) => item,
            Err(_) => break,
        }) else {
            return Err("uni.ws closed".to_owned());
        };
        let frame = frame_res.map_err(|e| format!("uni.ws recv error: {e}"))?;
        match frame {
            WsMessage::Binary(bytes) => {
                if bytes.len() > 4 {
                    let candidate = u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
                    if stream_id.is_none() || stream_id == Some(candidate) {
                        let payload = &bytes[4..];
                        if audio.len().saturating_add(payload.len()) > MAX_OGG_AUDIO_BYTES {
                            return Err(format!(
                                "uni.ws audio exceeded {MAX_OGG_AUDIO_BYTES} bytes"
                            ));
                        }
                        audio.extend_from_slice(payload);
                    }
                }
            }
            WsMessage::Text(txt) => {
                if txt.contains("streamcontrol") {
                    break;
                }
                if let Ok(val) = serde_json::from_str::<Value>(&txt) {
                    let directive = &val["directive"];
                    if let Some(sid) = directive["header"]["streamId"].as_u64() {
                        stream_id = u32::try_from(sid).ok();
                    }
                    if let Some(err) = directive["payload"]["error"]["message"].as_str() {
                        return Err(format!("uni.ws TTS error: {err}"));
                    }
                }
            }
            WsMessage::Close(_) => return Err("uni.ws closed".to_owned()),
            _ => {}
        }
    }

    if audio.is_empty() {
        return Err("uni.ws returned empty audio".to_owned());
    }
    Ok(audio)
}

fn decode_ogg_opus_to_pcm16le(ogg_bytes: &[u8]) -> Result<(Vec<u8>, u32), String> {
    let pcm_i16 = decode_ogg_opus_mono(ogg_bytes)?;
    let duration_ms = ((pcm_i16.len() as u64) * 1000 / u64::from(OPUS_SAMPLE_RATE))
        .min(u64::from(u32::MAX)) as u32;
    let mut pcm_s16le = vec![0u8; pcm_i16.len() * 2];
    for (chunk, &sample) in pcm_s16le.as_chunks_mut::<2>().0.iter_mut().zip(&pcm_i16) {
        *chunk = sample.to_le_bytes();
    }
    Ok((pcm_s16le, duration_ms))
}

fn decode_ogg_opus_mono(ogg_bytes: &[u8]) -> Result<Vec<i16>, String> {
    let mut reader = PacketReader::new(Cursor::new(ogg_bytes));
    let mut channels: usize = 1;
    let mut decoder: Option<OpusDecoder> = None;
    let mut pcm = vec![0i16; MAX_FRAME_SAMPLES * 2];
    let mut mono_out = Vec::new();

    while let Some(packet) = reader
        .read_packet()
        .map_err(|e| format!("ogg read error: {e}"))?
    {
        if packet.data.starts_with(b"OpusHead") {
            if packet.data.len() >= 10 {
                channels = usize::from(packet.data[9]).clamp(1, 2);
            }
            decoder = Some(
                OpusDecoder::new(OPUS_SAMPLE_RATE, channels)
                    .map_err(|e| format!("opus init error: {e:?}"))?,
            );
            continue;
        }
        if packet.data.starts_with(b"OpusTags") {
            continue;
        }

        let dec = match decoder.as_mut() {
            Some(d) => d,
            None => {
                let d = OpusDecoder::new(OPUS_SAMPLE_RATE, 1)
                    .map_err(|e| format!("opus init error: {e:?}"))?;
                decoder.insert(d)
            }
        };

        let samples = dec
            .decode(
                &packet.data,
                &mut pcm[..MAX_FRAME_SAMPLES * channels],
                false,
            )
            .map_err(|e| format!("opus decode error: {e:?}"))?;

        if mono_out.len().saturating_add(samples) > MAX_DECODED_PCM_SAMPLES {
            return Err(format!(
                "decoded TTS audio exceeds {MAX_DECODED_PCM_SAMPLES} samples"
            ));
        }
        if channels == 1 {
            mono_out.extend_from_slice(&pcm[..samples]);
        } else {
            let (pairs, _) = pcm[..samples * 2].as_chunks::<2>();
            mono_out.extend(
                pairs
                    .iter()
                    .map(|&[l, r]| ((i32::from(l) + i32::from(r)) / 2) as i16),
            );
        }
    }

    if mono_out.is_empty() {
        return Err("decoded zero opus samples".to_owned());
    }
    Ok(mono_out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(text: &str) -> TtsCacheKey {
        (
            text.to_owned(),
            DEFAULT_VOICE.to_owned(),
            DEFAULT_LANG.to_owned(),
            DEFAULT_QUALITY.to_owned(),
            DEFAULT_EMOTION.to_owned(),
        )
    }

    fn audio(tag: u8) -> CachedAudio {
        CachedAudio {
            sample_rate: OPUS_SAMPLE_RATE,
            pcm_s16le: vec![tag; 4],
            duration_ms: 10,
        }
    }

    #[test]
    fn tts_cache_promotes_hits_and_evicts_oldest_entry() {
        let mut cache = TtsCache::new(3);
        cache.insert(key("first"), audio(1));
        cache.insert(key("second"), audio(2));
        cache.insert(key("third"), audio(3));

        // Promote "first" to most recently used; "second" becomes oldest.
        assert_eq!(cache.get(&key("first")), Some(audio(1)));

        cache.insert(key("fourth"), audio(4));
        assert_eq!(cache.get(&key("second")), None);
        assert_eq!(cache.get(&key("first")), Some(audio(1)));
        assert_eq!(cache.get(&key("third")), Some(audio(3)));
        assert_eq!(cache.get(&key("fourth")), Some(audio(4)));
    }
}
