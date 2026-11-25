# uno-voice

Host-agnostic ядро голосового мода: шумодав, VAD, сегментация, кольцевой
буфер, локальный STT и контракты TTS/воспроизведения. Микрофон и динамик
остаются в хосте (cpal/WASAPI/WebAudio) — здесь только то, что должно работать
одинаково везде.

## Голос Алисы (только синтез)

Фича `alice-tts` добавляет один HTTP-клиент к локальному мосту
`tools/alice-uniproxy/serve.py`:

- `AliceTts` (`TtsEngine`) — `POST /say` → WAV → `SpeechUtterance`.

**Модель/диалог Алисы не используется.** Распознанный текст идёт сразу в
синтез; для этого есть `VoicePipeline::spawn_tts()`:

```rust
use std::sync::Arc;
use uno_voice::{AliceTts, OnnxStt, VoicePipeline};

let stt = Arc::new(OnnxStt::new(models_dir));
let tts = Arc::new(AliceTts::from_env().with_voice("kolya"));
let (handle, mut events) = VoicePipeline::spawn_tts(rt.handle(), Default::default(), stt, tts);
```

Хост пушит `AudioFrame` через `handle.push_audio(...)`, на
`VoicePipelineEvent::SpeechStarted` / `BargeIn` останавливает плеер, на
`VoicePipelineEvent::Speaking` отдаёт `SpeechUtterance` в свой `TtsPlayer`.
`VoicePipeline` отбрасывает устаревшие результаты, если пользователь начал
говорить, пока фраза ещё синтезировалась.

Проверка без микрофона:

```bash
python tools/alice-uniproxy/serve.py
cargo run -p uno-voice --features alice-tts --example alice_say -- "привет" kolya
cargo test -p uno-voice --features alice-tts
```

## Что остаётся хосту

- захват PCM16 и push в `VoicePipelineHandle`;
- реализация `TtsPlayer` (реальное проигрывание и учёт `played_ms`);
- wake word (`wake.rs` — пока только трейт) и AEC (`aec.rs` — только статус).
