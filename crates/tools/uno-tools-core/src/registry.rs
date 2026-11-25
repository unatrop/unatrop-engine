use std::{collections::BTreeMap, sync::Arc};

use protocol::ToolDefinition;
use serde_json::Value;

use crate::tool::MAX_NAME_LEN;
use crate::{
    Approval, ApprovalHandler, ApprovalPolicy, PolicyDecision, Tool, ToolError, ToolOutput,
};

#[derive(Clone)]
struct RegisteredTool {
    definition: ToolDefinition,
    implementation: Arc<dyn Tool>,
}

/// Маркер места вставки остальных инструментов при явной сортировке (`definitions_ordered`)
pub const UNLISTED_TOOLS_MARKER: &str = "<unlisted-tools>";

/// Реестр инструментов с кэшем определений
#[derive(Default)]
pub struct ToolRegistry {
    tools: BTreeMap<String, RegisteredTool>,
    /// Кэш определений в порядке имён: стабильный порядок сохраняет префикс запроса для кэша провайдера
    definitions: Vec<ToolDefinition>,
    policy: ApprovalPolicy,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Создаёт реестр с заданной политикой подтверждения
    pub fn with_policy(policy: ApprovalPolicy) -> Self {
        Self {
            policy,
            ..Self::default()
        }
    }

    /// Обновляет политику подтверждения инструментов
    pub fn set_policy(&mut self, policy: ApprovalPolicy) {
        self.policy = policy;
    }

    /// Возвращает текущую политику подтверждения
    pub fn policy(&self) -> &ApprovalPolicy {
        &self.policy
    }

    /// Возвращает подмножество только из `read_only` инструментов с политикой `ReadOnly`
    pub fn read_only_subset(&self, exclude: &[&str]) -> Self {
        let mut sub = Self::with_policy(
            ApprovalPolicy::with_default_safe_commands().with_mode(crate::ApprovalMode::ReadOnly),
        );
        for (name, registered) in &self.tools {
            if exclude.contains(&name.as_str()) || !registered.implementation.read_only() {
                continue;
            }
            sub.tools.insert(name.clone(), registered.clone());
            sub.definitions.push(registered.definition.clone());
        }
        sub
    }

    pub fn register(&mut self, tool: impl Tool + 'static) -> Result<(), ToolError> {
        let definition = tool.definition();
        let name = definition.name.clone();

        validate_name(&name)?;
        if self.tools.contains_key(&name) {
            return Err(ToolError::DuplicateTool(name));
        }

        self.tools.insert(
            name,
            RegisteredTool {
                definition,
                implementation: Arc::new(tool),
            },
        );
        self.definitions = self
            .tools
            .values()
            .map(|tool| tool.definition.clone())
            .collect();

        Ok(())
    }

    pub fn len(&self) -> usize {
        self.tools.len()
    }

    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Определения инструментов в порядке имён для передачи в `CompletionRequest`
    pub fn definitions(&self) -> &[ToolDefinition] {
        &self.definitions
    }

    /// Определения инструментов в заданном порядке (с поддержкой [`UNLISTED_TOOLS_MARKER`]).
    /// Стабильный порядок инструментов сохраняет префикс запроса для KV-кэша провайдера.
    pub fn definitions_ordered(&self, order: &[&str]) -> Vec<ToolDefinition> {
        let mut listed = std::collections::HashSet::with_capacity(order.len());
        let mut result = Vec::with_capacity(self.definitions.len());
        let mut inserted_rest = false;

        for &name in order {
            if name == UNLISTED_TOOLS_MARKER {
                if !inserted_rest {
                    inserted_rest = true;
                    for (tool_name, registered) in &self.tools {
                        if !order.contains(&tool_name.as_str()) {
                            result.push(registered.definition.clone());
                        }
                    }
                }
                continue;
            }
            if listed.insert(name)
                && let Some(registered) = self.tools.get(name)
            {
                result.push(registered.definition.clone());
            }
        }

        if !inserted_rest {
            for (tool_name, registered) in &self.tools {
                if !listed.contains(tool_name.as_str()) {
                    result.push(registered.definition.clone());
                }
            }
        }

        result
    }

    /// Политика подтверждения для указанного вызова
    pub fn approval(&self, name: &str, arguments: &Value) -> Result<Approval, ToolError> {
        Ok(self.tool(name)?.approval(arguments))
    }

    /// Возвращает `false` для неизвестных инструментов
    pub fn is_read_only(&self, name: &str) -> bool {
        self.tool(name).is_ok_and(|tool| tool.read_only())
    }

    /// Запускает инструмент после прохождения политики подтверждения
    pub async fn execute(
        &self,
        name: &str,
        arguments: Value,
        approval_handler: &dyn ApprovalHandler,
    ) -> Result<ToolOutput, ToolError> {
        self.execute_streaming(name, arguments, approval_handler, None)
            .await
    }

    /// Запускает инструмент со стримингом инкрементального вывода в `delta_tx`
    pub async fn execute_streaming(
        &self,
        name: &str,
        arguments: Value,
        approval_handler: &dyn ApprovalHandler,
        delta_tx: Option<crate::OutputDeltaSender>,
    ) -> Result<ToolOutput, ToolError> {
        let tool = self.tool(name)?;

        match self.policy.evaluate(
            name,
            tool.read_only(),
            tool.approval(&arguments),
            &arguments,
        ) {
            PolicyDecision::Allow => {}
            PolicyDecision::Deny { reason } => return Err(ToolError::ApprovalDenied(reason)),
            PolicyDecision::Ask { reason } => {
                let approved = approval_handler
                    .approve(crate::ApprovalRequest {
                        tool_name: name.to_owned(),
                        arguments: arguments.clone(),
                        reason,
                    })
                    .await
                    .map_err(ToolError::ApprovalFailed)?;

                if !approved {
                    return Err(ToolError::ApprovalDenied(name.to_owned()));
                }
            }
        }

        tool.execute_streaming(arguments, delta_tx).await
    }

    /// Запускает инструмент в цикле агента: ошибки преобразуются в вывод для модели, чтобы она могла исправиться
    pub async fn call(
        &self,
        name: &str,
        arguments: Value,
        approval_handler: &dyn ApprovalHandler,
    ) -> ToolOutput {
        self.call_streaming(name, arguments, approval_handler, None)
            .await
    }

    /// Запускает инструмент в цикле агента со стримингом инкрементального вывода
    pub async fn call_streaming(
        &self,
        name: &str,
        arguments: Value,
        approval_handler: &dyn ApprovalHandler,
        delta_tx: Option<crate::OutputDeltaSender>,
    ) -> ToolOutput {
        match self
            .execute_streaming(name, arguments, approval_handler, delta_tx)
            .await
        {
            Ok(output) => output,
            Err(err) => ToolOutput::error(err.to_string()),
        }
    }

    fn tool(&self, name: &str) -> Result<&Arc<dyn Tool>, ToolError> {
        self.tools
            .get(name)
            .map(|tool| &tool.implementation)
            .ok_or_else(|| ToolError::UnknownTool(name.to_owned()))
    }
}

fn validate_name(name: &str) -> Result<(), ToolError> {
    if name.trim().is_empty() {
        return Err(ToolError::EmptyName);
    }

    let valid = name.len() <= MAX_NAME_LEN
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-');

    if valid {
        Ok(())
    } else {
        Err(ToolError::InvalidName(name.to_owned()))
    }
}
