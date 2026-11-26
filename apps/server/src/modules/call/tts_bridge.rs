//! Server-side streaming TTS orchestration, sentence ordering, tool announcements, and echo memory.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};

use tokio::sync::mpsc;
use tokio::task::{AbortHandle, JoinHandle};
use uno_daemon_protocol::pb;
use uno_voice::{
    TtsHints, extract_ready_sentences_with_hints, finalize_tts_tail_with_hints, is_echo_of_spoken,
    tts_tool_announcement,
};

use crate::modules::{sessions::SessionEntry, tts::TtsService};

const ECHO_MEMORY_WINDOW: Duration = Duration::from_secs(5);
const TOOL_ANNOUNCE_DELAY: Duration = Duration::from_millis(350);

struct OrderedTtsQueue {
    turn_gen: u64,
    next_seq: u64,
    play_seq: u64,
    ready: BTreeMap<u64, Option<pb::TtsAudioChunk>>,
    in_flight: Vec<AbortHandle>,
}

/// Manages ordered sentence synthesis and streaming delivery of `TtsAudioChunk` frames over `/v1/call/ws`.
pub(crate) struct CallTtsPipeline {
    tts_pool: Arc<TtsService>,
    out_tx: mpsc::Sender<pb::CallServerMsg>,
    generation: Arc<AtomicU64>,
    ordered: Arc<StdMutex<OrderedTtsQueue>>,
    last_spoken: Arc<StdMutex<(String, Option<Instant>)>>,
}

impl CallTtsPipeline {
    pub(crate) fn new(tts_pool: Arc<TtsService>, out_tx: mpsc::Sender<pb::CallServerMsg>) -> Self {
        Self {
            tts_pool,
            out_tx,
            generation: Arc::new(AtomicU64::new(0)),
            ordered: Arc::new(StdMutex::new(OrderedTtsQueue {
                turn_gen: 0,
                next_seq: 0,
                play_seq: 0,
                ready: BTreeMap::new(),
                in_flight: Vec::new(),
            })),
            last_spoken: Arc::new(StdMutex::new((String::new(), None))),
        }
    }

    pub(crate) fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Starts a fresh TTS generation, aborting any pending synthesis and optionally emitting `tts_clear`.
    pub(crate) fn begin_turn(&self, send_clear: bool) -> u64 {
        let gen_id = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        if let Ok(mut ord) = self.ordered.lock() {
            ord.turn_gen = gen_id;
            ord.next_seq = 0;
            ord.play_seq = 0;
            ord.ready.clear();
            for handle in ord.in_flight.drain(..) {
                handle.abort();
            }
        }
        if send_clear {
            let _ = self.out_tx.try_send(pb::CallServerMsg {
                msg: Some(pb::call_server_msg::Msg::TtsClear(pb::Empty {})),
            });
        }
        gen_id
    }

    /// Stops current TTS synthesis and playback immediately and returns state to listening.
    pub(crate) fn interrupt(&self) {
        self.begin_turn(true);
        let _ = self.out_tx.try_send(pb::CallServerMsg {
            msg: Some(pb::call_server_msg::Msg::State(
                pb::CallState::Listening as i32,
            )),
        });
    }

    /// Queues a single spoken chunk for concurrent synthesis with strict ordered emission.
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn enqueue_sentence(
        &self,
        text: String,
        voice: Option<String>,
        lang: Option<String>,
        turn_gen: u64,
        tag: Option<u64>,
        is_final: bool,
    ) {
        let trimmed = text.trim().to_owned();
        if trimmed.is_empty() || self.generation.load(Ordering::Acquire) != turn_gen {
            return;
        }
        if let Ok(mut spoken) = self.last_spoken.lock() {
            if !spoken.0.is_empty() {
                spoken.0.push(' ');
            }
            spoken.0.push_str(&trimmed);
            if spoken.0.len() > 2048 {
                let trim_from = spoken
                    .0
                    .char_indices()
                    .find(|&(idx, _)| idx >= spoken.0.len() - 1024)
                    .map_or(0, |(idx, _)| idx);
                spoken.0.drain(..trim_from);
            }
            spoken.1 = Some(Instant::now());
        }

        let seq = {
            let Ok(mut ord) = self.ordered.lock() else {
                return;
            };
            if ord.turn_gen != turn_gen {
                return;
            }
            let seq = ord.next_seq;
            ord.next_seq += 1;
            seq
        };

        let tts_pool = Arc::clone(&self.tts_pool);
        let out_tx = self.out_tx.clone();
        let generation = Arc::clone(&self.generation);
        let ordered = Arc::clone(&self.ordered);
        let last_spoken = Arc::clone(&self.last_spoken);

        let join = tokio::spawn(async move {
            struct SeqGuard {
                generation: Arc<AtomicU64>,
                ordered: Arc<StdMutex<OrderedTtsQueue>>,
                out_tx: mpsc::Sender<pb::CallServerMsg>,
                turn_gen: u64,
                seq: u64,
                armed: bool,
            }
            impl Drop for SeqGuard {
                fn drop(&mut self) {
                    if !self.armed || self.generation.load(Ordering::Acquire) != self.turn_gen {
                        return;
                    }
                    let mut drained = Vec::new();
                    if let Ok(mut ord) = self.ordered.lock()
                        && ord.turn_gen == self.turn_gen
                    {
                        ord.ready.insert(self.seq, None);
                        loop {
                            let key = ord.play_seq;
                            let Some(item) = ord.ready.remove(&key) else {
                                break;
                            };
                            ord.play_seq += 1;
                            if let Some(chunk) = item {
                                drained.push(chunk);
                            }
                        }
                    }
                    for chunk in drained {
                        let _ = self.out_tx.try_send(pb::CallServerMsg {
                            msg: Some(pb::call_server_msg::Msg::TtsAudio(chunk)),
                        });
                    }
                }
            }

            let mut guard = SeqGuard {
                generation: Arc::clone(&generation),
                ordered: Arc::clone(&ordered),
                out_tx: out_tx.clone(),
                turn_gen,
                seq,
                armed: true,
            };

            let text_chars = u32::try_from(trimmed.chars().count()).unwrap_or(u32::MAX);
            let synth = tts_pool
                .synthesize(&trimmed, voice.as_deref(), lang.as_deref(), None, None)
                .await;

            if generation.load(Ordering::Acquire) != turn_gen {
                guard.armed = false;
                return;
            }

            let chunk = match synth {
                Ok(audio) => Some(pb::TtsAudioChunk {
                    turn_id: turn_gen,
                    seq,
                    sample_rate: audio.sample_rate,
                    pcm_s16le: audio.pcm_s16le,
                    duration_ms: audio.duration_ms,
                    text_chars,
                    tag,
                    is_final,
                }),
                Err(err) => {
                    tracing::warn!(seq, "daemon TTS synthesis failed: {err}");
                    None
                }
            };

            if let Ok(mut spoken) = last_spoken.lock() {
                spoken.1 = Some(Instant::now());
            }

            guard.armed = false;
            let mut to_send = Vec::new();
            if let Ok(mut ord) = ordered.lock() {
                if ord.turn_gen != turn_gen {
                    return;
                }
                ord.ready.insert(seq, chunk);
                loop {
                    let key = ord.play_seq;
                    let Some(item) = ord.ready.remove(&key) else {
                        break;
                    };
                    ord.play_seq += 1;
                    if let Some(c) = item {
                        to_send.push(c);
                    }
                }
            }
            for c in to_send {
                let _ = out_tx
                    .send(pb::CallServerMsg {
                        msg: Some(pb::call_server_msg::Msg::TtsAudio(c)),
                    })
                    .await;
            }
        });

        let abort = join.abort_handle();
        if let Ok(mut ord) = self.ordered.lock() {
            if ord.turn_gen == turn_gen {
                ord.in_flight.push(abort);
            } else {
                abort.abort();
            }
        }
    }

    /// Speaks an arbitrary full message (e.g., manual replay button), splitting and sanitizing it cleanly.
    pub(crate) fn speak_text(
        &self,
        text: &str,
        voice: Option<String>,
        lang: Option<String>,
        tag: Option<u64>,
    ) {
        let turn_gen = self.begin_turn(true);
        let hints = TtsHints::for_locale(lang.as_deref());
        let mut offset = 0usize;
        let mut chunks = extract_ready_sentences_with_hints(text, &mut offset, hints);
        chunks.extend(finalize_tts_tail_with_hints(text, &mut offset, hints));

        if chunks.is_empty() {
            let _ = self.out_tx.try_send(pb::CallServerMsg {
                msg: Some(pb::call_server_msg::Msg::State(pb::CallState::Idle as i32)),
            });
            return;
        }

        let _ = self.out_tx.try_send(pb::CallServerMsg {
            msg: Some(pb::call_server_msg::Msg::State(
                pb::CallState::Speaking as i32,
            )),
        });

        let last_idx = chunks.len() - 1;
        for (idx, chunk) in chunks.into_iter().enumerate() {
            self.enqueue_sentence(
                chunk,
                voice.clone(),
                lang.clone(),
                turn_gen,
                tag,
                idx == last_idx,
            );
        }
    }

    /// Returns `true` if the recognized transcript matches speech recently synthesized on this call.
    pub(crate) fn is_recent_echo(&self, phrase: &str) -> bool {
        let Ok(guard) = self.last_spoken.lock() else {
            return false;
        };
        let Some(at) = guard.1 else {
            return false;
        };
        if at.elapsed() > ECHO_MEMORY_WINDOW {
            return false;
        }
        is_echo_of_spoken(phrase, &[guard.0.as_str()])
    }
}

/// Bridges `SessionEntry` LLM streaming + tool events directly into `CallTtsPipeline` over `/v1/call/ws`.
pub(crate) fn spawn_session_tts_bridge(
    entry: Arc<SessionEntry>,
    pipeline: Arc<CallTtsPipeline>,
    out_tx: mpsc::Sender<pb::CallServerMsg>,
    voice: Option<String>,
    lang: Option<String>,
) -> JoinHandle<()> {
    let mut rx = entry.events.subscribe();
    tokio::spawn(async move {
        let hints = TtsHints::for_locale(lang.as_deref());
        let mut streamed_body = String::new();
        let mut streamed_offset = 0usize;
        let mut turn_gen: Option<u64> = None;
        let mut announced_tool = false;
        let mut tool_timer: Option<JoinHandle<()>> = None;

        while let Ok(server_msg) = rx.recv().await {
            let Some(msg) = server_msg.msg else {
                continue;
            };
            match msg {
                pb::server_msg::Msg::Started(_) => {
                    if let Some(t) = tool_timer.take() {
                        t.abort();
                    }
                    streamed_body.clear();
                    streamed_offset = 0;
                    announced_tool = false;
                    turn_gen = Some(pipeline.begin_turn(true));
                    let _ = out_tx
                        .send(pb::CallServerMsg {
                            msg: Some(pb::call_server_msg::Msg::State(
                                pb::CallState::Thinking as i32,
                            )),
                        })
                        .await;
                }
                pb::server_msg::Msg::Delta(delta_ev) => {
                    let Some(pb::stream_event::Event::TextDelta(td)) =
                        delta_ev.event.and_then(|e| e.event)
                    else {
                        continue;
                    };
                    if td.delta.is_empty() {
                        continue;
                    }
                    streamed_body.push_str(&td.delta);
                    let ready = extract_ready_sentences_with_hints(
                        &streamed_body,
                        &mut streamed_offset,
                        hints,
                    );
                    if !ready.is_empty() {
                        if let Some(t) = tool_timer.take() {
                            t.abort();
                        }
                        let cur_gen = *turn_gen.get_or_insert_with(|| pipeline.begin_turn(false));
                        let _ = out_tx
                            .send(pb::CallServerMsg {
                                msg: Some(pb::call_server_msg::Msg::State(
                                    pb::CallState::Speaking as i32,
                                )),
                            })
                            .await;
                        for chunk in ready {
                            pipeline.enqueue_sentence(
                                chunk,
                                voice.clone(),
                                lang.clone(),
                                cur_gen,
                                None,
                                false,
                            );
                        }
                    }
                }
                pb::server_msg::Msg::ToolStarted(ts) => {
                    if !streamed_body.is_empty() && streamed_body.len() > streamed_offset {
                        let tail = finalize_tts_tail_with_hints(
                            &streamed_body,
                            &mut streamed_offset,
                            hints,
                        );
                        if !tail.is_empty() {
                            if let Some(t) = tool_timer.take() {
                                t.abort();
                            }
                            let cur_gen =
                                *turn_gen.get_or_insert_with(|| pipeline.begin_turn(false));
                            let _ = out_tx
                                .send(pb::CallServerMsg {
                                    msg: Some(pb::call_server_msg::Msg::State(
                                        pb::CallState::Speaking as i32,
                                    )),
                                })
                                .await;
                            for piece in tail {
                                pipeline.enqueue_sentence(
                                    piece,
                                    voice.clone(),
                                    lang.clone(),
                                    cur_gen,
                                    None,
                                    false,
                                );
                            }
                        }
                    }
                    if streamed_offset == 0 && !announced_tool && tool_timer.is_none() {
                        let cur_gen = *turn_gen.get_or_insert_with(|| pipeline.begin_turn(false));
                        let phrase = tts_tool_announcement(&ts.name, lang.as_deref()).to_owned();
                        let p = Arc::clone(&pipeline);
                        let tx = out_tx.clone();
                        let v = voice.clone();
                        let l = lang.clone();
                        announced_tool = true;
                        tool_timer = Some(tokio::spawn(async move {
                            tokio::time::sleep(TOOL_ANNOUNCE_DELAY).await;
                            if p.generation() == cur_gen {
                                let _ = tx
                                    .send(pb::CallServerMsg {
                                        msg: Some(pb::call_server_msg::Msg::State(
                                            pb::CallState::Speaking as i32,
                                        )),
                                    })
                                    .await;
                                p.enqueue_sentence(phrase, v, l, cur_gen, None, false);
                            }
                        }));
                    }
                }
                pb::server_msg::Msg::Completed(_) => {
                    if let Some(t) = tool_timer.take() {
                        t.abort();
                    }
                    let cur_gen = turn_gen
                        .take()
                        .unwrap_or_else(|| pipeline.begin_turn(false));
                    let tail =
                        finalize_tts_tail_with_hints(&streamed_body, &mut streamed_offset, hints);
                    if !tail.is_empty() {
                        let _ = out_tx
                            .send(pb::CallServerMsg {
                                msg: Some(pb::call_server_msg::Msg::State(
                                    pb::CallState::Speaking as i32,
                                )),
                            })
                            .await;
                        let last_idx = tail.len() - 1;
                        for (idx, piece) in tail.into_iter().enumerate() {
                            pipeline.enqueue_sentence(
                                piece,
                                voice.clone(),
                                lang.clone(),
                                cur_gen,
                                None,
                                idx == last_idx,
                            );
                        }
                    } else {
                        let _ = out_tx
                            .send(pb::CallServerMsg {
                                msg: Some(pb::call_server_msg::Msg::State(
                                    pb::CallState::Listening as i32,
                                )),
                            })
                            .await;
                    }
                    streamed_body.clear();
                    streamed_offset = 0;
                    announced_tool = false;
                }
                pb::server_msg::Msg::Error(_) => {
                    if let Some(t) = tool_timer.take() {
                        t.abort();
                    }
                    streamed_body.clear();
                    streamed_offset = 0;
                    announced_tool = false;
                    turn_gen = None;
                    let _ = out_tx
                        .send(pb::CallServerMsg {
                            msg: Some(pb::call_server_msg::Msg::State(
                                pb::CallState::Listening as i32,
                            )),
                        })
                        .await;
                }
                _ => {}
            }
        }
    })
}
