use protocol::FinishReason;
use serde::Deserialize;

/// Чанк потокового ответа `/chat/completions`
#[derive(Debug, Deserialize)]
pub struct ChatChunk {
    /// Варианты ответа в чанке
    #[serde(default)]
    pub choices: Vec<ChunkChoice>,
    /// Статистика токенов
    pub usage: Option<ChunkUsage>,
}

/// Вариант ответа внутри чанка
#[derive(Debug, Deserialize)]
pub struct ChunkChoice {
    /// Дельта содержимого
    pub delta: ChunkDelta,
    /// Причина остановки генерации
    pub finish_reason: Option<FinishReason>,
}

/// Дельта полей сообщения в чанке
#[derive(Debug, Default, Deserialize)]
pub struct ChunkDelta {
    /// Дельта основного текста
    pub content: Option<String>,
    /// Поле рассуждений (DeepSeek-R1, vLLM, Ollama)
    pub reasoning_content: Option<String>,
    /// Альтернативное поле рассуждений
    pub reasoning: Option<String>,
    /// Дельты вызовов инструментов
    #[serde(default)]
    pub tool_calls: Vec<ChunkToolCallDelta>,
}

/// Дельта вызова инструмента в чанке
#[derive(Debug, Deserialize)]
pub struct ChunkToolCallDelta {
    /// Индекс вызова в ответе
    pub index: usize,
    /// Идентификатор вызова
    pub id: Option<String>,
    /// Дельта имени и аргументов функции
    pub function: Option<ChunkFunctionDelta>,
}

/// Дельта функции внутри вызова инструмента
#[derive(Debug, Deserialize)]
pub struct ChunkFunctionDelta {
    /// Имя вызываемой функции
    pub name: Option<String>,
    /// Фрагмент JSON-аргументов
    pub arguments: Option<String>,
}

/// Статистика использования токенов из чанка
#[derive(Debug, Deserialize)]
pub struct ChunkUsage {
    /// Токены входного промпта
    pub prompt_tokens: u32,
    /// Токены сгенерированного ответа
    pub completion_tokens: u32,
    /// Попадания в кэш промпта у DeepSeek
    pub prompt_cache_hit_tokens: Option<u32>,
    /// Токены, прочитанные из кэша у Anthropic / Polza.ai
    pub cache_read_input_tokens: Option<u32>,
    /// Токены, записанные в кэш у Anthropic / Polza.ai
    pub cache_creation_input_tokens: Option<u32>,
    /// Детализация токенов промпта у OpenAI и совместимых серверов
    pub prompt_tokens_details: Option<PromptTokensDetails>,
}

/// Детализация входных токенов
#[derive(Debug, Deserialize)]
pub struct PromptTokensDetails {
    /// Токены, прочитанные из кэша
    pub cached_tokens: Option<u32>,
}

impl ChunkUsage {
    /// Количество токенов из кэша: поле DeepSeek, Anthropic/Polza или OpenAI
    pub fn cached_tokens(&self) -> Option<u32> {
        self.prompt_cache_hit_tokens
            .or(self.cache_read_input_tokens)
            .or_else(|| {
                self.prompt_tokens_details
                    .as_ref()
                    .and_then(|details| details.cached_tokens)
            })
    }

    /// Полное число входных токенов с учётом отдельно репортимых Anthropic-полей
    pub fn total_input_tokens(&self) -> u32 {
        self.prompt_tokens
            .saturating_add(self.cache_read_input_tokens.unwrap_or(0))
            .saturating_add(self.cache_creation_input_tokens.unwrap_or(0))
    }
}
