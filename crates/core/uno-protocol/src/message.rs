use std::collections::BTreeMap;

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use serde_json::Value;

/// Роль участника диалога
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    Developer,
    User,
    Assistant,
    Tool,
}

/// Фрагмент содержимого сообщения
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    /// Обычный текст
    Text { text: String },
    /// Блок рассуждений модели
    Reasoning { text: String },
    /// Ссылка или data-URI на изображение для мультимодальных моделей
    ImageUrl {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

impl ContentPart {
    /// Создаёт текстовый фрагмент
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    /// Создаёт фрагмент рассуждений
    pub fn reasoning(text: impl Into<String>) -> Self {
        Self::Reasoning { text: text.into() }
    }

    /// Создаёт фрагмент с изображением (URL или `data:image/...;base64,...`)
    pub fn image_url(url: impl Into<String>) -> Self {
        Self::ImageUrl {
            url: url.into(),
            detail: None,
        }
    }
}

/// Список фрагментов содержимого; при десериализации принимает также строку и `null`
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Content(pub Vec<ContentPart>);

impl Content {
    /// Создаёт содержимое из одного текстового фрагмента
    pub fn text(text: impl Into<String>) -> Self {
        Self(vec![ContentPart::text(text)])
    }

    /// Возвращает `true`, если фрагментов нет
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
}

impl Serialize for Content {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        self.0.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for Content {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum ContentHelper {
            Null(()),
            Single(String),
            Multiple(Vec<ContentPart>),
        }

        match ContentHelper::deserialize(deserializer)? {
            ContentHelper::Null(()) => Ok(Content::default()),
            ContentHelper::Single(text) => Ok(Content(vec![ContentPart::text(text)])),
            ContentHelper::Multiple(parts) => Ok(Content(parts)),
        }
    }
}

/// Сообщение в истории диалога
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Message {
    /// Роль отправителя
    pub role: Role,
    /// Содержимое сообщения
    #[serde(default, skip_serializing_if = "Content::is_empty")]
    pub content: Content,
    /// Вызовы инструментов, запрошенные ассистентом
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tool_calls: Vec<ToolCall>,
    /// Идентификатор вызова для ответа роли `Tool`
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

impl Message {
    /// Создаёт системное сообщение
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: Role::System,
            content: Content::text(text),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Создаёт сообщение пользователя
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: Role::User,
            content: Content::text(text),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Создаёт сообщение ассистента
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: Role::Assistant,
            content: Content::text(text),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }

    /// Создаёт сообщение с результатом выполнения инструмента
    pub fn tool_result(tool_call_id: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            role: Role::Tool,
            content: Content::text(content),
            tool_calls: Vec::new(),
            tool_call_id: Some(tool_call_id.into()),
        }
    }
}

/// Вызов инструмента моделью
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCall {
    /// Уникальный идентификатор вызова
    pub id: String,
    /// Имя инструмента
    pub name: String,
    /// Аргументы вызова в виде JSON
    pub arguments: Value,
    /// Непрозрачная сигнатура рассуждений провайдера (например, Gemini thought signature)
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
}

/// Описание инструмента для модели
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolDefinition {
    /// Имя инструмента
    pub name: String,
    /// Описание назначения инструмента
    pub description: String,
    /// JSON Schema входных параметров
    pub input_schema: Value,
}

/// Уровень глубины рассуждений модели
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThinkingLevel {
    /// Рассуждения отключены
    Off,
    /// Минимальный уровень рассуждений
    Minimal,
    /// Низкий уровень рассуждений
    Low,
    /// Средний уровень рассуждений
    Medium,
    /// Высокий уровень рассуждений
    High,
    /// Максимальный уровень рассуждений (если поддерживается моделью)
    XHigh,
}

/// Запрос на генерацию ответа моделью
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CompletionRequest {
    /// Идентификатор модели
    pub model: String,
    /// История сообщений
    pub messages: Vec<Message>,
    /// Доступные модели инструменты
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tools: Vec<ToolDefinition>,
    /// Температура семплирования
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,
    /// Лимит токенов ответа
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Желаемый уровень глубины рассуждений
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub thinking_level: Option<ThinkingLevel>,
    /// Идентификатор сессии для трассировки и кэширования провайдером
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    /// Произвольные строковые метаданные запроса (`user_id`, метки и т.д.)
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub metadata: BTreeMap<String, String>,
}

impl CompletionRequest {
    /// Создаёт базовый запрос с указанной моделью и сообщениями
    pub fn new(model: impl Into<String>, messages: Vec<Message>) -> Self {
        Self {
            model: model.into(),
            messages,
            ..Self::default()
        }
    }
}
