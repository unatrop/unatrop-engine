//! High-level session client used by `uno-daemon` to configure and drive `Harness` + `Session`.

use std::{
    path::{Path, PathBuf},
    sync::Arc,
};

use uno_harness::{AgentError, Harness, RunStream};
use uno_protocol::{CompletionRequest, Message, Role, ThinkingLevel};
use uno_provider::{ContextProfile, Provider};
use uno_session::{Session, SessionError};
use uno_session_jsonl::{load_session_json, save_session_json};
use uno_tools_core::{ApprovalHandler, ToolRegistry};

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct ClientConfig {
    pub model: String,
    pub max_provider_turns: usize,
    pub context_cap_chars: Option<usize>,
    pub context_profile: Option<ContextProfile>,
    pub max_total_tokens: Option<u64>,
    pub llm_compaction: bool,
    pub max_parallel_tools: Option<usize>,
    pub prompt_cache: Option<bool>,
    pub temperature: Option<f32>,
    pub max_tokens: Option<u32>,
    pub thinking_level: Option<ThinkingLevel>,
}

impl ClientConfig {
    pub(crate) fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            max_provider_turns: 10,
            context_cap_chars: None,
            context_profile: None,
            max_total_tokens: None,
            llm_compaction: false,
            max_parallel_tools: None,
            prompt_cache: None,
            temperature: None,
            max_tokens: None,
            thinking_level: None,
        }
    }
}

pub(crate) struct ClientBuilder {
    config: ClientConfig,
    provider: Option<Arc<dyn Provider>>,
    tools: ToolRegistry,
    initial_messages: Vec<Message>,
    session_path: Option<PathBuf>,
    approval_handler: Arc<dyn ApprovalHandler>,
}

impl ClientBuilder {
    pub(crate) fn new(config: ClientConfig, approval_handler: Arc<dyn ApprovalHandler>) -> Self {
        Self {
            config,
            provider: None,
            tools: ToolRegistry::new(),
            initial_messages: Vec::new(),
            session_path: None,
            approval_handler,
        }
    }

    pub(crate) fn provider(mut self, provider: Arc<dyn Provider>) -> Self {
        self.provider = Some(provider);
        self
    }

    pub(crate) fn tools(mut self, tools: ToolRegistry) -> Self {
        self.tools = tools;
        self
    }

    pub(crate) fn initial_messages(mut self, messages: Vec<Message>) -> Self {
        self.initial_messages = messages;
        self
    }

    pub(crate) fn session_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.session_path = Some(path.into());
        self
    }

    pub(crate) fn build(self) -> Result<Client, ClientBuildError> {
        if self.config.model.trim().is_empty() {
            return Err(ClientBuildError::EmptyModel);
        }
        let provider = self.provider.ok_or(ClientBuildError::MissingProvider)?;
        let mut harness = Harness::new(
            provider,
            self.tools,
            self.approval_handler,
            self.config.max_provider_turns,
        )?;
        if let Some(profile) = self.config.context_profile {
            harness = harness.with_context_profile(profile);
        }
        if let Some(cap) = self.config.context_cap_chars {
            harness = harness.with_context_cap(cap);
        }
        if let Some(max_total) = self.config.max_total_tokens {
            harness = harness.with_max_total_tokens(max_total);
        }
        if self.config.llm_compaction {
            harness = harness.with_llm_compaction(true);
        }
        if let Some(max_parallel) = self.config.max_parallel_tools {
            harness = harness.with_max_parallel_tools(max_parallel);
        }
        let harness = Arc::new(harness);
        let initial_messages = self.initial_messages;
        let session = match self.session_path {
            Some(path) if path.exists() => load_session_json(harness, path)?,
            _ => Session::new(harness, initial_messages.clone()),
        };
        Ok(Client {
            config: self.config,
            initial_messages,
            session,
        })
    }
}

pub(crate) struct Client {
    config: ClientConfig,
    initial_messages: Vec<Message>,
    session: Session,
}

impl Client {
    pub(crate) fn history(&self) -> Result<Vec<Message>, SessionError> {
        self.session.history()
    }

    pub(crate) fn reset_session(&self) -> Result<(), SessionError> {
        self.session.set_history(self.initial_messages.clone())
    }

    pub(crate) fn set_history(&self, mut messages: Vec<Message>) -> Result<(), SessionError> {
        let has_system_prefix = messages
            .first()
            .is_some_and(|m| matches!(m.role, Role::System | Role::Developer));
        if !has_system_prefix && !self.initial_messages.is_empty() {
            let mut full = self.initial_messages.clone();
            full.append(&mut messages);
            messages = full;
        }
        self.session.set_history(messages)
    }

    pub(crate) fn send_with_voice(
        &self,
        message: impl Into<String>,
        voice_mode: bool,
        voice_locale: Option<&str>,
    ) -> Result<RunStream<'static>, SessionError> {
        let (thinking_level, system_hint) = if voice_mode {
            (
                Some(ThinkingLevel::Off),
                Some(uno_voice::voice_mode_instruction(voice_locale).to_owned()),
            )
        } else {
            (self.config.thinking_level, None)
        };
        self.session.run_with_system_hint(
            CompletionRequest {
                model: self.config.model.clone(),
                messages: vec![Message::user(message)],
                tools: Vec::new(),
                temperature: self.config.temperature,
                max_tokens: self.config.max_tokens,
                thinking_level,
                ..CompletionRequest::default()
            },
            system_hint,
        )
    }

    pub(crate) fn save_session(&self, path: impl AsRef<Path>) -> Result<(), SessionError> {
        save_session_json(&self.session, path)
    }
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ClientBuildError {
    #[error("model must not be empty")]
    EmptyModel,
    #[error("provider must be supplied by the daemon account registry")]
    MissingProvider,
    #[error(transparent)]
    Agent(#[from] AgentError),
    #[error(transparent)]
    Session(#[from] SessionError),
}
