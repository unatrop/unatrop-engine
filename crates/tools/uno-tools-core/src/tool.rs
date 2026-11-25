use async_trait::async_trait;
use protocol::ToolDefinition;
use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde_json::Value;

use crate::ToolError;

/// Канал инкрементального стриминга вывода инструмента (stdout/stderr чанки).
pub type OutputDeltaSender = tokio::sync::mpsc::UnboundedSender<String>;

/// Максимальная длина имени инструмента для OpenAI-совместимых серверов
pub(crate) const MAX_NAME_LEN: usize = 64;

/// Результат выполнения инструмента для модели
///
/// `is_error` отмечает ошибку уровня выполнения (неверный путь, код возврата),
/// на которую модель может отреагировать, в отличие от [`ToolError`]
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn text(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
        }
    }
}

/// Политика подтверждения вызова
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Approval {
    Never,
    Ask { reason: String },
}

/// Запрос на подтверждение вызова инструмента
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRequest {
    pub tool_name: String,
    pub arguments: Value,
    pub reason: String,
}

/// Обработчик запросов на подтверждение от пользователя
///
/// Возвращаемый future отменяется при сбросе запуска агента:
/// реализация должна закрыть или удалить зависший UI-запрос при drop
#[async_trait]
pub trait ApprovalHandler: Send + Sync {
    async fn approve(&self, request: ApprovalRequest) -> Result<bool, String>;
}

/// Инструмент, доступный агенту
///
/// Контракт:
/// - `execute` должен быть cancel-safe (дочерние процессы нужно завершать при drop)
/// - `approval` вызывается до `execute` и может учитывать аргументы
#[async_trait]
pub trait Tool: Send + Sync {
    fn definition(&self) -> ToolDefinition;

    fn approval(&self, _arguments: &Value) -> Approval {
        if self.read_only() {
            Approval::Never
        } else {
            Approval::Ask {
                reason: "tool has no automatic-approval policy".into(),
            }
        }
    }

    /// Разрешает параллельный запуск с другими read-only инструментами
    fn read_only(&self) -> bool {
        false
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError>;

    async fn execute_streaming(
        &self,
        arguments: Value,
        _delta_tx: Option<OutputDeltaSender>,
    ) -> Result<ToolOutput, ToolError> {
        self.execute(arguments).await
    }
}

/// Инструмент со строгой типизацией аргументов через Rust-тип
///
/// JSON-схема и разбор аргументов строятся из `Args`, поэтому не расходятся
/// Регистрируется через `registry.register(Typed(MyTool))`
#[async_trait]
pub trait TypedTool: Send + Sync {
    type Args: DeserializeOwned + JsonSchema + Send;

    fn name(&self) -> &str;

    fn description(&self) -> &str;

    fn approval(&self, _args: &Self::Args) -> Approval {
        if self.read_only() {
            Approval::Never
        } else {
            Approval::Ask {
                reason: "tool has no automatic-approval policy".into(),
            }
        }
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError>;

    async fn run_streaming(
        &self,
        args: Self::Args,
        _delta_tx: Option<OutputDeltaSender>,
    ) -> Result<ToolOutput, ToolError> {
        self.run(args).await
    }
}

/// Адаптер [`TypedTool`] к трейту [`Tool`]
pub struct Typed<T>(pub T);

#[async_trait]
impl<T: TypedTool> Tool for Typed<T> {
    fn definition(&self) -> ToolDefinition {
        let mut schema = serde_json::to_value(schemars::schema_for!(T::Args))
            .unwrap_or_else(|_| serde_json::json!({ "type": "object" }));
        compact_schema(&mut schema, true);

        ToolDefinition {
            name: self.0.name().to_owned(),
            description: self.0.description().to_owned(),
            input_schema: schema,
        }
    }

    fn approval(&self, arguments: &Value) -> Approval {
        // При невалидных аргументах политика не может их проверить — требуем подтверждение
        match serde_json::from_value::<T::Args>(arguments.clone()) {
            Ok(args) => self.0.approval(&args),
            Err(_) => Approval::Ask {
                reason: "tool arguments are invalid; confirmation is required".into(),
            },
        }
    }

    fn read_only(&self) -> bool {
        self.0.read_only()
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let args = serde_json::from_value::<T::Args>(arguments)
            .map_err(|err| ToolError::InvalidArguments(err.to_string()))?;
        self.0.run(args).await
    }

    async fn execute_streaming(
        &self,
        arguments: Value,
        delta_tx: Option<OutputDeltaSender>,
    ) -> Result<ToolOutput, ToolError> {
        let args = serde_json::from_value::<T::Args>(arguments)
            .map_err(|err| ToolError::InvalidArguments(err.to_string()))?;
        self.0.run_streaming(args, delta_tx).await
    }
}

/// Убирает из JSON-схемы служебные и дублирующие поля (`$schema`, `title`, `format`, `minimum: 0`),
/// чтобы экономить токены в описаниях инструментов.
fn compact_schema(value: &mut Value, is_root: bool) {
    match value {
        Value::Object(map) => {
            if is_root {
                map.remove("$schema");
                map.remove("title");
                map.remove("description");
            }
            if map.get("type").and_then(Value::as_str) == Some("integer") {
                map.remove("format");
                if map.get("minimum").and_then(Value::as_u64) == Some(0) {
                    map.remove("minimum");
                }
            }
            for child in map.values_mut() {
                compact_schema(child, false);
            }
        }
        Value::Array(items) => {
            for item in items {
                compact_schema(item, false);
            }
        }
        _ => {}
    }
}
