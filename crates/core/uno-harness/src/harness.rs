use std::sync::Arc;

use async_stream::try_stream;
use futures_util::StreamExt;
use protocol::{CompletionRequest, Message, MessageAccumulator, StreamEvent, Usage};
use provider::{ContextProfile, Provider};
use tool_core::{ApprovalHandler, SpillStore, ToolOutput, ToolRegistry};

pub use crate::helpers::{MIN_DEDUPE_TOOL_CHARS, dedupe_consecutive_tool_result};
use crate::{
    AgentError, AgentInbox, RunEvent, RunState, RunStream, SessionCheckpoint,
    compaction::{
        DEFAULT_CONTEXT_CAP_TOKENS, compact_batch, estimate_messages_tokens, estimate_tools_tokens,
        mask_stale_tool_results, should_compact, trim_tool_call_arguments,
    },
    helpers::{
        format_tool_output_with_spill, insert_compaction_summary, start_provider_stream,
        summarize_dropped, validate_tool_calls,
    },
    xml_recovery::{has_truncated_tool_call, recover_xml_tool_calls},
};

pub(crate) use crate::compaction::tool_result_content;

pub(crate) const MAX_PROVIDER_START_ATTEMPTS: usize = 2;
const _: () = assert!(MAX_PROVIDER_START_ATTEMPTS > 0);
const MAX_MALFORMED_TOOL_RETRIES: usize = 2;
const REPEAT_TOOL_THRESHOLD: usize = 3;
/// Жёсткий лимит стоимости запуска, если хост не задал свой: сумма `input + output`
/// по **всем** ходам провайдера (input каждого хода включает всю переотправленную историю,
/// поэтому это бюджет расхода, а не размер окна контекста).
const DEFAULT_MAX_TOTAL_TOKENS: u64 = 1_000_000;
/// Максимум одновременно выполняемых read-only инструментов
const DEFAULT_MAX_PARALLEL_READ_TOOLS: usize = 8;
/// Бюджет ответа модели-суммаризатора при LLM-компактификации
pub(crate) const COMPACTION_SUMMARY_MAX_TOKENS: u32 = 512;

/// Исполнитель цикла агента: провайдер, реестр инструментов и политика подтверждений
pub struct Harness {
    provider: Arc<dyn Provider>,
    tools: Arc<ToolRegistry>,
    approval_handler: Arc<dyn ApprovalHandler>,
    max_provider_turns: usize,
    max_total_tokens: Option<u64>,
    context_cap_tokens: u64,
    max_output_tokens: Option<u32>,
    max_parallel_tools: usize,
    llm_compaction: bool,
    inbox: Option<Arc<AgentInbox>>,
    checkpoint: Option<Arc<dyn SessionCheckpoint>>,
    spill: Option<(Arc<dyn SpillStore>, String)>,
    tool_order: Option<Vec<String>>,
}

impl Harness {
    pub fn new(
        provider: Arc<dyn Provider>,
        tools: ToolRegistry,
        approval_handler: Arc<dyn ApprovalHandler>,
        max_provider_turns: usize,
    ) -> Result<Self, AgentError> {
        if max_provider_turns == 0 {
            return Err(AgentError::InvalidMaxProviderTurns);
        }

        Ok(Self {
            provider,
            tools: Arc::new(tools),
            approval_handler,
            max_provider_turns,
            max_total_tokens: Some(DEFAULT_MAX_TOTAL_TOKENS),
            context_cap_tokens: DEFAULT_CONTEXT_CAP_TOKENS,
            max_output_tokens: None,
            max_parallel_tools: DEFAULT_MAX_PARALLEL_READ_TOOLS,
            llm_compaction: false,
            inbox: None,
            checkpoint: None,
            spill: None,
            tool_order: None,
        })
    }

    /// Задаёт жёсткий бюджет суммарных токенов (`input + output`) на запуск агента
    pub fn with_max_total_tokens(mut self, max_tokens: u64) -> Self {
        self.max_total_tokens = Some(max_tokens.max(1));
        self
    }

    /// Задаёт порог пакетной компактификации контекста в токенах
    pub fn with_context_tokens(mut self, context_cap_tokens: u64) -> Self {
        self.context_cap_tokens = context_cap_tokens.max(1_024);
        self
    }

    /// Задаёт порог компактификации в символах (legacy-шорткат, ≈ 4 символа на токен)
    pub fn with_context_cap(mut self, context_cap_chars: usize) -> Self {
        self.context_cap_tokens = (context_cap_chars as u64 / 4).max(1_024);
        self
    }

    /// Применяет профиль контекста модели (лимиты из каталога провайдера или конфигурации)
    pub fn with_context_profile(mut self, profile: ContextProfile) -> Self {
        self.context_cap_tokens = profile.compact_at_tokens.max(1_024);
        self.max_output_tokens = Some(profile.max_output_tokens.min(u64::from(u32::MAX)) as u32);
        self
    }

    /// Задаёт лимит выходных токенов одного ответа, если он не задан в запросе
    pub fn with_max_output_tokens(mut self, max_output_tokens: u32) -> Self {
        self.max_output_tokens = Some(max_output_tokens.max(1));
        self
    }

    /// Ограничивает число одновременно выполняемых read-only инструментов
    pub fn with_max_parallel_tools(mut self, max_parallel_tools: usize) -> Self {
        self.max_parallel_tools = max_parallel_tools.max(1);
        self
    }

    /// Включает LLM-суммаризацию вытесняемых промежуточных шагов вместо молчаливого удаления
    pub fn with_llm_compaction(mut self, enabled: bool) -> Self {
        self.llm_compaction = enabled;
        self
    }

    /// Подключает двухступенчатый инбокс для вброса сообщений на лету (`next_step` / `next_turn`)
    pub fn with_inbox(mut self, inbox: Arc<AgentInbox>) -> Self {
        self.inbox = Some(inbox);
        self
    }

    /// Подключает fail-closed чекпоинт перед запросом к модели и перед вызовом инструментов
    pub fn with_checkpoint(mut self, checkpoint: Arc<dyn SessionCheckpoint>) -> Self {
        self.checkpoint = Some(checkpoint);
        self
    }

    /// Подключает spill-хранилище для выгрузки длинных результатов инструментов на диск/в стор
    pub fn with_spill_store(
        mut self,
        store: Arc<dyn SpillStore>,
        session_id: impl Into<String>,
    ) -> Self {
        self.spill = Some((store, session_id.into()));
        self
    }

    /// Задаёт явный порядок определений инструментов (с поддержкой `<unlisted-tools>`) для сохранения KV-кэша
    pub fn with_tool_order(mut self, order: Vec<String>) -> Self {
        self.tool_order = Some(order);
        self
    }

    /// Запускает ходы провайдера и инструменты до завершения ответа модели
    ///
    /// Поле `request.tools` заменяется определениями из реестра этого `Harness`
    /// Повторяемая ошибка при старте потока переигрывается один раз, после первых дельт повторов нет
    /// Сброс потока отменяет активный запрос к провайдеру или инструменту (требуется cancel-safety от `tool_core::Tool`)
    pub fn run<'a>(&'a self, request: CompletionRequest) -> RunStream<'a> {
        Self::run_with_parts(
            Arc::clone(&self.provider),
            Arc::clone(&self.tools),
            Arc::clone(&self.approval_handler),
            self.max_provider_turns,
            self.max_total_tokens,
            self.context_cap_tokens,
            self.max_output_tokens,
            self.max_parallel_tools,
            self.llm_compaction,
            self.inbox.clone(),
            self.checkpoint.clone(),
            self.spill.clone(),
            self.tool_order.clone(),
            request,
        )
    }

    /// То же, что [`Self::run`], но без заимствования `self` — удобно для фоновых задач
    pub fn run_owned(self: Arc<Self>, request: CompletionRequest) -> RunStream<'static> {
        Self::run_with_parts(
            Arc::clone(&self.provider),
            Arc::clone(&self.tools),
            Arc::clone(&self.approval_handler),
            self.max_provider_turns,
            self.max_total_tokens,
            self.context_cap_tokens,
            self.max_output_tokens,
            self.max_parallel_tools,
            self.llm_compaction,
            self.inbox.clone(),
            self.checkpoint.clone(),
            self.spill.clone(),
            self.tool_order.clone(),
            request,
        )
    }

    #[allow(clippy::too_many_arguments)]
    fn run_with_parts(
        provider: Arc<dyn Provider>,
        tools: Arc<ToolRegistry>,
        approval_handler: Arc<dyn ApprovalHandler>,
        max_provider_turns: usize,
        max_total_tokens: Option<u64>,
        context_cap_tokens: u64,
        max_output_tokens: Option<u32>,
        max_parallel_tools: usize,
        llm_compaction: bool,
        inbox: Option<Arc<AgentInbox>>,
        checkpoint: Option<Arc<dyn SessionCheckpoint>>,
        spill: Option<(Arc<dyn SpillStore>, String)>,
        tool_order: Option<Vec<String>>,
        mut request: CompletionRequest,
    ) -> RunStream<'static> {
        Box::pin(try_stream! {
            request.tools = match &tool_order {
                Some(order) => {
                    let refs: Vec<&str> = order.iter().map(String::as_str).collect();
                    tools.definitions_ordered(&refs)
                }
                None => tools.definitions().to_vec(),
            };
            let tools_tokens = estimate_tools_tokens(&request.tools) as u64;
            let effective_msg_cap = context_cap_tokens.saturating_sub(tools_tokens).max(1_024);
            if request.max_tokens.is_none() {
                request.max_tokens = max_output_tokens;
            }
            yield RunEvent::Started;
            let mut total_usage = Usage::default();
            let mut has_usage = false;
            let mut last_input_tokens: Option<u32> = None;
            let mut malformed_tool_retries = 0usize;
            let mut repeat_detector = RepeatToolDetector::default();

            for turn in 0..max_provider_turns {
                if let Some(cp) = &checkpoint {
                    cp.flush().await.map_err(AgentError::CheckpointFailed)?;
                }
                let mut accumulator = MessageAccumulator::new();
                let mut finished = false;
                let mut streamed_content = false;
                yield RunEvent::StateChanged(RunState::WaitingForModel);
                mask_stale_tool_results(
                    &mut request.messages,
                    &request.tools,
                    last_input_tokens,
                    context_cap_tokens,
                );
                if should_compact(
                    &request.messages,
                    &request.tools,
                    last_input_tokens,
                    context_cap_tokens,
                ) {
                    let dropped = compact_batch(&mut request.messages, effective_msg_cap);
                    let dropped_messages = dropped.len();
                    if llm_compaction
                        && !dropped.is_empty()
                        && let Some(summary) = summarize_dropped(
                            provider.as_ref(),
                            &request.model,
                            request.session_id.as_deref(),
                            &dropped,
                        )
                        .await
                    {
                        insert_compaction_summary(&mut request.messages, summary);
                    }
                    last_input_tokens = None;
                    yield RunEvent::ContextCompacted {
                        dropped_messages,
                        estimated_tokens_after: estimate_messages_tokens(&request.messages) as u64,
                    };
                }
                let mut provider_stream =
                    start_provider_stream(provider.as_ref(), &request).await?;
                yield RunEvent::StateChanged(RunState::StreamingResponse);

                while let Some(item) = provider_stream.next().await {
                    if finished {
                        Err(AgentError::EventAfterFinish)?;
                    }

                    let event = match item {
                        Ok(event) => event,
                        Err(error) => Err(AgentError::ProviderStream {
                            message: error.to_string(),
                            retryable: error.is_retryable() && !streamed_content,
                        })?,
                    };
                    if let StreamEvent::Error { message, retryable } = &event {
                        yield RunEvent::ProviderEvent(event.clone());
                        Err(AgentError::ProviderStream {
                            message: message.clone(),
                            retryable: *retryable && !streamed_content,
                        })?;
                    }

                    accumulator.process_event(&event)?;
                    finished = matches!(event, StreamEvent::Finished { .. });
                    streamed_content |= matches!(
                        event,
                        StreamEvent::TextDelta { .. }
                            | StreamEvent::ReasoningDelta { .. }
                            | StreamEvent::ToolCallDelta { .. }
                    );
                    yield RunEvent::ProviderEvent(event);
                }

                if !finished {
                    Err(AgentError::IncompleteProviderStream {
                        retryable: !streamed_content,
                    })?;
                }

                let accumulated = accumulator.finish()?;
                let mut assistant = accumulated.message;
                if assistant.tool_calls.is_empty() {
                    recover_xml_tool_calls(&mut assistant, turn);
                }
                if let Some(usage) = accumulated.usage {
                    has_usage = true;
                    last_input_tokens = Some(usage.input_tokens);
                    total_usage.input_tokens =
                        total_usage.input_tokens.saturating_add(usage.input_tokens);
                    total_usage.output_tokens =
                        total_usage.output_tokens.saturating_add(usage.output_tokens);
                    if let Some(cached_tokens) = usage.cached_tokens {
                        total_usage.cached_tokens = Some(
                            total_usage
                                .cached_tokens
                                .unwrap_or_default()
                                .saturating_add(cached_tokens),
                        );
                    }
                }
                if let Some(max_tokens) = max_total_tokens {
                    let used_tokens = u64::from(total_usage.input_tokens)
                        .saturating_add(u64::from(total_usage.output_tokens));
                    if used_tokens > max_tokens {
                        if !assistant.tool_calls.is_empty() {
                            let mut text_only = assistant.clone();
                            text_only.tool_calls.clear();
                            if !text_only.content.is_empty() {
                                request.messages.push(text_only.clone());
                                yield RunEvent::MessageAppended(text_only);
                            }
                        }
                        Err(AgentError::TokenBudgetExceeded {
                            used_tokens,
                            max_tokens,
                        })?;
                    }
                }
                if !assistant.tool_calls.is_empty() {
                    validate_tool_calls(&assistant.tool_calls)?;

                    // Не оставляем в истории вызов инструментов без ответа, если лимит ходов исчерпан
                    if turn + 1 == max_provider_turns {
                        let mut text_only = assistant.clone();
                        text_only.tool_calls.clear();
                        if !text_only.content.is_empty() {
                            request.messages.push(text_only.clone());
                            yield RunEvent::MessageAppended(text_only);
                        }
                        yield RunEvent::TurnLimitReached {
                            max_turns: max_provider_turns,
                        };
                        Err(AgentError::MaxTurnsExceeded {
                            max_turns: max_provider_turns,
                        })?;
                    }
                }

                let mut history_message = assistant.clone();
                trim_tool_call_arguments(&mut history_message);
                request.messages.push(history_message.clone());
                yield RunEvent::MessageAppended(history_message);

                if assistant.tool_calls.is_empty() {
                    if has_truncated_tool_call(&assistant)
                        && malformed_tool_retries < MAX_MALFORMED_TOOL_RETRIES
                        && turn + 1 < max_provider_turns
                    {
                        malformed_tool_retries += 1;
                        let nudge = Message::user(
                            "Your previous response contained a malformed or truncated tool call tag instead of a valid tool call. Re-emit the tool call using the structured tool calling API.",
                        );
                        request.messages.push(nudge.clone());
                        yield RunEvent::MessageAppended(nudge);
                        continue;
                    }

                    if let Some(next_turn_msg) = inbox.as_ref().and_then(|ib| ib.pop_next_turn())
                        && turn + 1 < max_provider_turns
                    {
                        request.messages.push(next_turn_msg.clone());
                        yield RunEvent::MessageAppended(next_turn_msg);
                        continue;
                    }

                    yield RunEvent::StateChanged(RunState::Completed);
                    yield RunEvent::Completed {
                        messages: request.messages.clone(),
                        finish_reason: accumulated.finish_reason,
                        usage: has_usage.then_some(total_usage),
                    };
                    break;
                }
                malformed_tool_retries = 0;

                yield RunEvent::StateChanged(RunState::HandlingToolCalls);
                let calls = assistant.tool_calls;
                let mut idx = 0;
                while idx < calls.len() {
                    let is_ro = tools.is_read_only(&calls[idx].name);
                    if is_ro {
                        let mut end = idx + 1;
                        while end < calls.len() && tools.is_read_only(&calls[end].name) {
                            end += 1;
                        }
                        if end - idx > 1 {
                            if let Some(cp) = &checkpoint {
                                cp.flush().await.map_err(AgentError::CheckpointFailed)?;
                            }
                            let batch = &calls[idx..end];
                            for call in batch {
                                yield RunEvent::ToolCallStarted {
                                    id: call.id.clone(),
                                    name: call.name.clone(),
                                    arguments: call.arguments.clone(),
                                };
                            }
                            let jobs: Vec<(usize, String, serde_json::Value)> = batch
                                .iter()
                                .enumerate()
                                .map(|(offset, call)| {
                                    (offset, call.name.clone(), call.arguments.clone())
                                })
                                .collect();
                            let mut outputs: Vec<(usize, ToolOutput)> =
                                futures_util::stream::iter(jobs)
                                    .map(|(offset, name, args)| {
                                        let tools = Arc::clone(&tools);
                                        let approval_handler = Arc::clone(&approval_handler);
                                        async move {
                                            let output = tools
                                                .call(&name, args, approval_handler.as_ref())
                                                .await;
                                            (offset, output)
                                        }
                                    })
                                    .buffer_unordered(max_parallel_tools)
                                    .collect()
                                    .await;
                            outputs.sort_by_key(|(offset, _)| *offset);

                            for (call, (_, output)) in batch.iter().zip(outputs) {
                                let call_signature = format!(
                                    "{}:{}",
                                    call.name,
                                    serde_json::to_string(&call.arguments).unwrap_or_default()
                                );
                                let formatted_content = format_tool_output_with_spill(
                                    &call.name,
                                    &output,
                                    spill.as_ref(),
                                )
                                .await;
                                let tool_message =
                                    Message::tool_result(
                                    call.id.clone(),
                                    dedupe_consecutive_tool_result(
                                        &request.messages,
                                        formatted_content,
                                    ),
                                );
                                request.messages.push(tool_message.clone());
                                yield RunEvent::MessageAppended(tool_message);
                                yield RunEvent::ToolCallFinished {
                                    id: call.id.clone(),
                                    name: call.name.clone(),
                                    output,
                                };
                                if repeat_detector.record_and_should_nudge(&call_signature) {
                                    let reminder = Message::user(
                                        "Notice: you have called the exact same tool with identical arguments several times in a row. Re-evaluate your approach or inspect a different part of the problem.",
                                    );
                                    request.messages.push(reminder.clone());
                                    yield RunEvent::MessageAppended(reminder);
                                }
                            }
                            idx = end;
                            continue;
                        }
                    }

                    let call = &calls[idx];
                    if let Some(cp) = &checkpoint {
                        cp.flush().await.map_err(AgentError::CheckpointFailed)?;
                    }
                    yield RunEvent::ToolCallStarted {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        arguments: call.arguments.clone(),
                    };

                    let call_signature = format!(
                        "{}:{}",
                        call.name,
                        serde_json::to_string(&call.arguments).unwrap_or_default()
                    );
                    let (delta_tx, mut delta_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
                    let output = {
                        let call_fut = tools.call_streaming(
                            &call.name,
                            call.arguments.clone(),
                            approval_handler.as_ref(),
                            Some(delta_tx),
                        );
                        tokio::pin!(call_fut);
                        loop {
                            tokio::select! {
                                output = &mut call_fut => break output,
                                maybe_chunk = delta_rx.recv() => {
                                    if let Some(chunk) = maybe_chunk {
                                        yield RunEvent::ToolOutputDelta {
                                            id: call.id.clone(),
                                            name: call.name.clone(),
                                            chunk,
                                        };
                                    }
                                }
                            }
                        }
                    };
                    while let Ok(chunk) = delta_rx.try_recv() {
                        yield RunEvent::ToolOutputDelta {
                            id: call.id.clone(),
                            name: call.name.clone(),
                            chunk,
                        };
                    }

                    let formatted_content =
                        format_tool_output_with_spill(&call.name, &output, spill.as_ref()).await;
                    let tool_message =
                        Message::tool_result(
                                    call.id.clone(),
                                    dedupe_consecutive_tool_result(
                                        &request.messages,
                                        formatted_content,
                                    ),
                                );
                    request.messages.push(tool_message.clone());
                    yield RunEvent::MessageAppended(tool_message);
                    yield RunEvent::ToolCallFinished {
                        id: call.id.clone(),
                        name: call.name.clone(),
                        output,
                    };

                    if repeat_detector.record_and_should_nudge(&call_signature) {
                        let reminder = Message::user(
                            "Notice: you have called the exact same tool with identical arguments several times in a row. Re-evaluate your approach or inspect a different part of the problem.",
                        );
                        request.messages.push(reminder.clone());
                        yield RunEvent::MessageAppended(reminder);
                    }
                    idx += 1;
                }

                if let Some(ib) = &inbox {
                    for msg in ib.drain_next_step() {
                        request.messages.push(msg.clone());
                        yield RunEvent::MessageAppended(msg);
                    }
                }
            }
        })
    }
}

#[derive(Default)]
struct RepeatToolDetector {
    last_signature: Option<String>,
    streak: usize,
}

impl RepeatToolDetector {
    fn record_and_should_nudge(&mut self, signature: &str) -> bool {
        if self.last_signature.as_deref() == Some(signature) {
            self.streak += 1;
            if self.streak >= REPEAT_TOOL_THRESHOLD {
                self.streak = 0;
                return true;
            }
        } else {
            self.last_signature = Some(signature.to_owned());
            self.streak = 1;
        }
        false
    }
}
