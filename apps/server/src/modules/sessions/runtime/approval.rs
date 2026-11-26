use std::{
    collections::{BTreeSet, HashMap},
    sync::{
        Arc, Mutex,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use tokio::sync::{broadcast, oneshot};
use uno_tools_core::{
    ApprovalHandler, ApprovalRequest, extract_command_prefix, matches_safe_command_prefix,
};

use crate::protocol::{pb, stringify_json_args};

pub(crate) struct DaemonApprovalHandler {
    events: broadcast::Sender<pb::ServerMsg>,
    pending: Arc<Mutex<HashMap<String, oneshot::Sender<pb::ApprovalDecision>>>>,
    allowed_tools: Mutex<BTreeSet<String>>,
    allowed_prefixes: Mutex<Vec<String>>,
    next_id: AtomicU64,
}

impl DaemonApprovalHandler {
    pub(crate) fn new(events: broadcast::Sender<pb::ServerMsg>) -> Self {
        Self {
            events,
            pending: Arc::new(Mutex::new(HashMap::new())),
            allowed_tools: Mutex::new(BTreeSet::new()),
            allowed_prefixes: Mutex::new(Vec::new()),
            next_id: AtomicU64::new(1),
        }
    }

    pub(crate) fn resolve(&self, request_id: &str, decision: pb::ApprovalDecision) -> bool {
        if let Ok(mut map) = self.pending.lock()
            && let Some(tx) = map.remove(request_id)
        {
            let _ = tx.send(decision);
            return true;
        }
        false
    }

    fn always_allow_target(request: &ApprovalRequest) -> String {
        if matches!(request.tool_name.as_str(), "exec_command" | "exec_shell")
            && let Some(prefix) = extract_command_prefix(&request.arguments)
        {
            return prefix;
        }
        request.tool_name.clone()
    }

    fn is_auto_allowed(&self, request: &ApprovalRequest) -> bool {
        if let Ok(tools) = self.allowed_tools.lock()
            && tools.contains(&request.tool_name)
        {
            return true;
        }
        if matches!(request.tool_name.as_str(), "exec_command" | "exec_shell")
            && let Some(cmd) = request
                .arguments
                .get("cmd")
                .or_else(|| request.arguments.get("command"))
                .and_then(|v| v.as_str())
            && let Ok(prefixes) = self.allowed_prefixes.lock()
        {
            return prefixes
                .iter()
                .any(|prefix| matches_safe_command_prefix(cmd, prefix));
        }
        false
    }

    fn remember_always_allow(&self, request: &ApprovalRequest) {
        if matches!(request.tool_name.as_str(), "exec_command" | "exec_shell")
            && let Some(prefix) = extract_command_prefix(&request.arguments)
        {
            if let Ok(mut prefixes) = self.allowed_prefixes.lock()
                && !prefixes.contains(&prefix)
            {
                prefixes.push(prefix);
            }
            return;
        }
        if let Ok(mut tools) = self.allowed_tools.lock() {
            tools.insert(request.tool_name.clone());
        }
    }
}

#[async_trait]
impl ApprovalHandler for DaemonApprovalHandler {
    async fn approve(&self, request: ApprovalRequest) -> Result<bool, String> {
        if self.is_auto_allowed(&request) {
            return Ok(true);
        }

        let id = format!("req-{}", self.next_id.fetch_add(1, Ordering::Relaxed));
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self
                .pending
                .lock()
                .map_err(|_| "approval map poisoned".to_owned())?;
            map.insert(id.clone(), tx);
        }

        let target = Self::always_allow_target(&request);
        let _ = self.events.send(pb::ServerMsg {
            msg: Some(pb::server_msg::Msg::ApprovalRequired(
                pb::ApprovalRequiredEvent {
                    request_id: id.clone(),
                    tool_name: request.tool_name.clone(),
                    arguments_json: stringify_json_args(&request.arguments),
                    reason: request.reason.clone(),
                    always_allow_target: target,
                },
            )),
        });

        let Ok(decision) = rx.await else {
            if let Ok(mut map) = self.pending.lock() {
                map.remove(&id);
            }
            return Err("approval cancelled".to_owned());
        };

        match decision {
            pb::ApprovalDecision::Deny | pb::ApprovalDecision::Unspecified => Ok(false),
            pb::ApprovalDecision::AllowOnce => Ok(true),
            pb::ApprovalDecision::AlwaysAllow => {
                self.remember_always_allow(&request);
                Ok(true)
            }
        }
    }
}
