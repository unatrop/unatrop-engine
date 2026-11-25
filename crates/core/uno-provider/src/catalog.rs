use serde::{Deserialize, Serialize};

/// Унифицированное описание возможностей и лимитов модели в каталоге
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Идентификатор модели для `CompletionRequest::model`
    pub id: String,
    /// Человекочитаемое имя модели
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub display_name: Option<String>,
    /// Имя провайдера (`openai`, `anthropic`, `copilot`, `antigravity` и т.д.)
    pub provider: String,
    /// Лимит входного контекста в токенах
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,
    /// Лимит выходных токенов ответа
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,
    /// Поддерживает ли модель управляемые рассуждения (`ThinkingLevel`)
    #[serde(default)]
    pub supports_thinking: bool,
    /// Поддерживает ли модель вызов инструментов (`tools`)
    #[serde(default)]
    pub supports_tools: bool,
    /// Поддерживает ли модель изображения во входных сообщениях
    #[serde(default)]
    pub supports_vision: bool,
    /// Рекомендована ли модель по умолчанию
    #[serde(default)]
    pub recommended: bool,
}

/// Профиль контекста модели: бюджет в токенах, не зависящий от провайдера
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ContextProfile {
    /// Лимит входного контекста в токенах
    pub max_input_tokens: u64,
    /// Лимит выходных токенов одного ответа
    pub max_output_tokens: u64,
    /// Порог, после которого запускается компактификация (обычно 80% входа)
    pub compact_at_tokens: u64,
}

impl ContextProfile {
    /// Создаёт профиль и вычисляет порог компактификации как 80% входного лимита
    pub const fn new(max_input_tokens: u64, max_output_tokens: u64) -> Self {
        let max_input_tokens = if max_input_tokens < 2_048 {
            2_048
        } else {
            max_input_tokens
        };
        let mut compact_at_tokens = max_input_tokens * 80 / 100;
        if compact_at_tokens < 1_024 {
            compact_at_tokens = 1_024;
        }
        Self {
            max_input_tokens,
            max_output_tokens: if max_output_tokens < 256 {
                256
            } else {
                max_output_tokens
            },
            compact_at_tokens,
        }
    }

    /// Консервативный фолбэк для моделей без явных лимитов в каталоге.
    ///
    /// Не пытаемся угадывать конкретную модель по имени: лимиты должен сообщать
    /// провайдер (`ModelInfo::max_*`) или хост через конфигурацию.
    pub const DEFAULT: Self = Self::new(40_960, 8_192);
}

impl ModelInfo {
    /// Возвращает профиль контекста по лимитам из каталога провайдера.
    ///
    /// `None`, если провайдер не сообщил ни входной, ни выходной лимит —
    /// в этом случае хост должен подставить свой `ContextProfile`.
    pub fn context_profile(&self) -> Option<ContextProfile> {
        match (self.max_input_tokens, self.max_output_tokens) {
            (None, None) => None,
            (input, output) => Some(ContextProfile::new(
                input.unwrap_or(ContextProfile::DEFAULT.max_input_tokens),
                output.unwrap_or(ContextProfile::DEFAULT.max_output_tokens),
            )),
        }
    }

    /// Создаёт базовое описание модели для указанного провайдера и идентификатора
    pub fn new(provider: impl Into<String>, id: impl Into<String>) -> Self {
        let id = id.into();
        let lower = id.to_ascii_lowercase();
        let supports_thinking = lower.contains("claude-3-7")
            || lower.contains("claude-sonnet-4")
            || lower.contains("claude-opus-4")
            || lower.contains("claude-haiku-4")
            || lower.starts_with("o1")
            || lower.starts_with("o3")
            || lower.starts_with("o4")
            || lower.starts_with("gpt-5")
            || lower.starts_with("gpt-6")
            || lower.contains("gemini-2.5")
            || lower.contains("gemini-3")
            || lower.contains("gemini-4")
            || lower.contains("reasoner")
            || lower.contains("-r1");
        Self {
            id,
            display_name: None,
            provider: provider.into(),
            max_input_tokens: None,
            max_output_tokens: None,
            supports_thinking,
            supports_tools: true,
            supports_vision: false,
            recommended: false,
        }
    }
}

/// Унифицированный каталог моделей одного или нескольких провайдеров
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ModelCatalog {
    models: Vec<ModelInfo>,
}

impl ModelCatalog {
    /// Создаёт пустой каталог моделей
    pub fn new() -> Self {
        Self { models: Vec::new() }
    }

    /// Создаёт каталог из списка моделей
    pub fn from_models(models: Vec<ModelInfo>) -> Self {
        Self { models }
    }

    /// Возвращает все модели каталога
    pub fn models(&self) -> &[ModelInfo] {
        &self.models
    }

    /// Преобразует каталог во владеющий вектор моделей
    pub fn into_models(self) -> Vec<ModelInfo> {
        self.models
    }

    /// Добавляет или обновляет модель в каталоге по паре `(provider, id)`
    pub fn upsert(&mut self, info: ModelInfo) {
        if let Some(existing) = self
            .models
            .iter_mut()
            .find(|m| m.provider == info.provider && m.id == info.id)
        {
            *existing = info;
        } else {
            self.models.push(info);
        }
    }

    /// Ищет модель по идентификатору
    pub fn get(&self, id: &str) -> Option<&ModelInfo> {
        self.models.iter().find(|m| m.id == id)
    }

    /// Возвращает профиль контекста для модели по её лимитам из каталога
    pub fn context_profile_for(&self, id: &str) -> Option<ContextProfile> {
        self.get(id).and_then(ModelInfo::context_profile)
    }

    /// Возвращает модели, поддерживающие `thinking_level`
    pub fn thinking_models(&self) -> impl Iterator<Item = &ModelInfo> {
        self.models.iter().filter(|m| m.supports_thinking)
    }

    /// Возвращает рекомендованные модели
    pub fn recommended_models(&self) -> impl Iterator<Item = &ModelInfo> {
        self.models.iter().filter(|m| m.recommended)
    }
}
