//! Session construction: tools, skills, MCP discovery, prompts, provider wiring, and persistence.

#[cfg(feature = "host-tools")]
use std::path::Path;
use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64},
    },
};

use tokio::sync::broadcast;
use uno_harness::{
    plan::{PlanState, UpdatePlanTool},
    subagent::SpawnAgentTool,
};
#[cfg(feature = "host-tools")]
use uno_mcp as mcp;
use uno_protocol::{Message, ThinkingLevel};
use uno_provider::{ContextProfile, Provider};
use uno_provider_antigravity::{AntigravityAuth, AntigravityProvider};
use uno_provider_copilot::{CopilotAuth, CopilotProvider};
use uno_provider_openai::OpenAiCompatibleProvider;
use uno_provider_opencode::OpenCodeProvider;
use uno_skills as skills;
use uno_tools_core::{ApprovalMode, ApprovalPolicy, ToolRegistry};
#[cfg(feature = "host-tools")]
use uno_tools_core::{ToolError, Typed};
use uno_transport::HttpTransport;

use crate::{
    agent::{ClientBuilder, ClientConfig, DaemonApprovalHandler, config, prompts},
    protocol::pb,
    server::{DaemonError, DaemonState, validate_session_id},
    session::{SESSION_EVENT_CHANNEL_CAPACITY, SessionEntry, SessionMetrics},
    storage::unix_timestamp_secs,
};

#[cfg(feature = "host-tools")]
fn register_workspace_tools(
    registry: &mut ToolRegistry,
    workspace: &Path,
) -> Result<(), ToolError> {
    let session_manager = Arc::new(tools_exec::ExecSessionManager::new(workspace)?);
    registry.register(Typed(tools_fs::ListFiles::new(workspace)?))?;
    registry.register(Typed(tools_fs::ReadFile::new(workspace)?))?;
    registry.register(Typed(tools_grep::SearchText::new(workspace)?))?;
    registry.register(Typed(tools_fs::WriteFile::new(workspace)?))?;
    registry.register(Typed(tools_fs::ApplyPatch::new(workspace)?))?;
    registry.register(Typed(tools_exec::ExecCommand::new(Arc::clone(
        &session_manager,
    ))))?;
    registry.register(Typed(tools_exec::WriteStdin::new(session_manager)))?;
    Ok(())
}

pub(crate) async fn build_session_entry(
    state: &DaemonState,
    req: pb::CreateSessionRequest,
) -> Result<(Arc<SessionEntry>, ContextProfile), DaemonError> {
    let workspace = match req.workspace {
        Some(ref w) if !w.trim().is_empty() => PathBuf::from(w),
        _ => std::env::current_dir().map_err(|e| DaemonError::bad_request(e.to_string()))?,
    };

    let base_url = req
        .base_url
        .unwrap_or_else(|| crate::protocol::DEFAULT_OPENAI_BASE_URL.to_owned());
    let mut client_cfg = ClientConfig::new(base_url.clone(), req.model.clone());
    client_cfg.max_provider_turns = req
        .max_provider_turns
        .map(|n| n as usize)
        .unwrap_or(crate::protocol::DEFAULT_MAX_PROVIDER_TURNS);
    client_cfg.context_cap_chars = req.context_cap_chars.map(|n| n as usize);
    client_cfg.context_profile =
        if req.max_input_tokens.is_some() || req.max_output_tokens.is_some() {
            Some(ContextProfile::new(
                req.max_input_tokens
                    .unwrap_or(ContextProfile::DEFAULT.max_input_tokens),
                req.max_output_tokens
                    .unwrap_or(ContextProfile::DEFAULT.max_output_tokens),
            ))
        } else if let Some(chars) = req.context_cap_chars {
            Some(ContextProfile::new(
                chars / 4,
                ContextProfile::DEFAULT.max_output_tokens,
            ))
        } else {
            Some(ContextProfile::DEFAULT)
        };
    client_cfg.max_total_tokens = req.max_total_tokens;
    client_cfg.llm_compaction = req.llm_compaction.unwrap_or(false);
    client_cfg.max_parallel_tools = req.max_parallel_tools.map(|n| n as usize);
    client_cfg.prompt_cache = req.prompt_cache;
    client_cfg.temperature = req.temperature;
    client_cfg.max_tokens = req.max_tokens;
    client_cfg.thinking_level = req
        .thinking_level
        .and_then(|t| pb::ThinkingLevel::try_from(t).ok())
        .map(ThinkingLevel::from);

    let (events_tx, _) = broadcast::channel::<pb::ServerMsg>(SESSION_EVENT_CHANNEL_CAPACITY);
    let approval = Arc::new(DaemonApprovalHandler::new(events_tx.clone()));
    let transport = Arc::clone(&state.transport);

    let file_cfg = config::load_daemon_file_config(&workspace);
    let mode = req
        .approval_mode
        .and_then(|m| pb::ApprovalMode::try_from(m).ok())
        .map(ApprovalMode::from)
        .unwrap_or(ApprovalMode::Default);
    let mut policy = ApprovalPolicy::with_default_safe_commands().with_mode(mode);
    for tool_name in file_cfg.allow_tools {
        policy = policy.allow_tool(tool_name);
    }
    if !file_cfg.allow_commands.is_empty() {
        policy = policy.allow_command_prefixes(file_cfg.allow_commands);
    }
    let mut tools = ToolRegistry::with_policy(policy);
    let mut discovered_skills = Vec::new();
    if !req.disable_tools {
        #[cfg(feature = "host-tools")]
        register_workspace_tools(&mut tools, &workspace)
            .map_err(|e| DaemonError::bad_request(e.to_string()))?;

        let skill_dirs = config::merge_skill_dirs(req.skill_dirs, &workspace);
        discovered_skills = skills::discover_skills(&skill_dirs);
        if !discovered_skills.is_empty() {
            let _ = tools.register(skills::ReadSkillTool::new(discovered_skills.clone()));
        }

        #[cfg(feature = "host-tools")]
        {
            let mcp_paths = config::merge_mcp_configs(req.mcp_configs, &workspace);
            if !mcp_paths.is_empty() {
                let ws = workspace.clone();
                let mut reg = tools;
                tools = tokio::task::spawn_blocking(move || {
                    mcp::register_mcp_tools(&mcp_paths, &ws, &mut reg);
                    reg
                })
                .await
                .map_err(|e| DaemonError::internal(e.to_string()))?;
            }
        }
    }

    let system = match req.system_prompt.filter(|s| !s.trim().is_empty()) {
        Some(mut custom) => {
            if let Some(skills_sec) = skills::format_skills_section(&discovered_skills) {
                custom.push_str("\n\n");
                custom.push_str(&skills_sec);
            }
            custom
        }
        None => {
            let prompts_dir = config::resolve_unatrop_home().map(|h| h.join("prompts"));
            prompts::build_system_prompt(
                prompts_dir.as_deref(),
                &workspace,
                &req.model,
                &discovered_skills,
            )?
        }
    };

    if !req.disable_tools {
        let (plan_tx, mut plan_rx) = tokio::sync::mpsc::unbounded_channel::<PlanState>();
        let events_for_plan = events_tx.clone();
        tokio::spawn(async move {
            while let Some(plan) = plan_rx.recv().await {
                let _ = events_for_plan.send(pb::ServerMsg {
                    msg: Some(pb::server_msg::Msg::PlanUpdated(pb::PlanUpdatedEvent {
                        explanation: plan.explanation,
                        steps: plan.steps.iter().map(pb::PlanStep::from).collect(),
                    })),
                });
            }
        });
        let _ = tools.register(UpdatePlanTool::with_sender(plan_tx));
    }

    let transport_dyn: Arc<dyn HttpTransport> = transport.clone();
    let provider: Arc<dyn Provider> =
        match req.provider.as_deref() {
            Some("copilot") => {
                let auth = match req.api_key.filter(|k| !k.trim().is_empty()) {
                    Some(token) => CopilotAuth::from_github_token(token, transport_dyn),
                    None => CopilotAuth::from_env(transport_dyn)
                        .map_err(|e| DaemonError::bad_request(e.to_string()))?,
                };
                Arc::new(CopilotProvider::new(auth, transport.clone()))
            }
            Some("antigravity") => {
                let auth = match req.api_key.filter(|k| !k.trim().is_empty()) {
                    Some(raw) => {
                        let trimmed = raw.trim();
                        if trimmed.starts_with('{') {
                            AntigravityAuth::from_stored_token(trimmed, transport_dyn)
                                .map_err(|e| DaemonError::bad_request(e.to_string()))?
                        } else if trimmed.starts_with("1//") {
                            AntigravityAuth::from_refresh_token(trimmed, transport_dyn)
                        } else {
                            AntigravityAuth::from_access_token(trimmed, transport_dyn)
                        }
                    }
                    None => AntigravityAuth::from_env(transport_dyn)
                        .map_err(|e| DaemonError::bad_request(e.to_string()))?,
                };
                Arc::new(AntigravityProvider::new(auth, transport.clone()))
            }
            Some("opencode") => match req.api_key {
                Some(ref key) if !key.trim().is_empty() => Arc::new(
                    OpenCodeProvider::with_base_url(base_url, key.clone(), transport.clone()),
                ),
                _ => Arc::new(OpenCodeProvider::free(transport.clone())),
            },
            _ => {
                let Some(key) = req.api_key.filter(|k| !k.trim().is_empty()) else {
                    return Err(DaemonError::bad_request("api_key is required"));
                };
                Arc::new(
                    OpenAiCompatibleProvider::new("openai", base_url, key, transport.clone())
                        .prompt_cache(req.prompt_cache),
                )
            }
        };

    if !req.disable_tools {
        let _ = tools.register(SpawnAgentTool::new(
            Arc::clone(&provider),
            req.model.clone(),
            &tools,
            approval.clone(),
        ));
    }

    let mut context_profile = client_cfg
        .context_profile
        .unwrap_or(ContextProfile::DEFAULT);
    if let Some(max_tokens) = client_cfg.max_tokens {
        context_profile.max_output_tokens = u64::from(max_tokens);
    }

    let id = match req.session_id.as_deref() {
        Some(raw) => validate_session_id(raw)?,
        None => state.allocate_session_id(),
    };
    let session_file = state.sessions_dir.join(format!("{id}.json"));

    let mut initial_messages = vec![Message::system(system)];
    if let Some(fork_id) = req.fork_from.as_deref() {
        let fork_id = validate_session_id(fork_id)?;
        let fork_file = state.sessions_dir.join(format!("{fork_id}.json"));
        if let Ok(source) = state.get_session(&fork_id) {
            source.persist_sync();
        }
        if let Some(db_history) = state.store.as_ref().and_then(|s| s.load_history(&fork_id)) {
            initial_messages = db_history;
        } else if fork_file.is_file() {
            tokio::fs::copy(&fork_file, &session_file)
                .await
                .map_err(|e| DaemonError::internal(e.to_string()))?;
        } else {
            return Err(DaemonError::not_found(format!(
                "fork_from session {fork_id} not found"
            )));
        }
    } else if !session_file.is_file()
        && let Some(db_history) = state.store.as_ref().and_then(|s| s.load_history(&id))
    {
        initial_messages = db_history;
    }

    let client = ClientBuilder::new(client_cfg, transport, approval.clone())
        .provider(provider)
        .tools(tools)
        .initial_messages(initial_messages)
        .session_path(&session_file)
        .build()
        .map_err(|e| DaemonError::bad_request(e.to_string()))?;

    let entry = Arc::new(SessionEntry {
        id,
        model: req.model,
        workspace,
        session_file,
        client: Arc::new(client),
        approval,
        events: events_tx,
        active_task: Mutex::new(None),
        metrics: Mutex::new(SessionMetrics::default()),
        store: state.store.clone(),
        updated_at: AtomicU64::new(unix_timestamp_secs()),
        turn_started_at: Mutex::new(None),
        first_token_logged: AtomicBool::new(false),
    });
    entry.persist_async();

    Ok((entry, context_profile))
}
