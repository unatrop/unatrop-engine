//! Session registry, persistence lookup, and lifecycle operations.

use std::{
    collections::{HashMap, HashSet},
    path::{Path, PathBuf},
    sync::{
        Arc, RwLock,
        atomic::{AtomicU64, Ordering},
    },
};

use uno_daemon_protocol::pb;

use super::{builder::build_session_entry, runtime::entry::SessionEntry};
use crate::{
    app::DaemonError,
    modules::accounts::AccountService,
    shared::sqlite::{SqliteSessionStore, derive_session_title},
};

pub(crate) struct SessionService {
    entries: RwLock<HashMap<String, Arc<SessionEntry>>>,
    sessions_dir: PathBuf,
    next_session_id: AtomicU64,
    store: Option<Arc<SqliteSessionStore>>,
    accounts: Arc<AccountService>,
}

impl SessionService {
    pub(crate) fn new(sessions_dir: PathBuf, accounts: Arc<AccountService>) -> Self {
        let _ = std::fs::create_dir_all(&sessions_dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o700));
        }
        let store = SqliteSessionStore::open(&sessions_dir).map(Arc::new);
        Self {
            entries: RwLock::new(HashMap::new()),
            sessions_dir,
            next_session_id: AtomicU64::new(1),
            store,
            accounts,
        }
    }

    pub(super) fn sessions_dir(&self) -> &Path {
        &self.sessions_dir
    }

    pub(super) fn store(&self) -> Option<&Arc<SqliteSessionStore>> {
        self.store.as_ref()
    }

    pub(super) fn accounts(&self) -> &Arc<AccountService> {
        &self.accounts
    }

    pub(crate) fn get(&self, id: &str) -> Result<Arc<SessionEntry>, DaemonError> {
        self.entries
            .read()
            .ok()
            .and_then(|map| map.get(id).cloned())
            .ok_or_else(|| DaemonError::not_found("session not found"))
    }

    pub(crate) fn get_validated(&self, raw_id: &str) -> Result<Arc<SessionEntry>, DaemonError> {
        let id = validate_session_id(raw_id)?;
        self.get(&id)
    }

    #[cfg(feature = "host-tools")]
    pub(crate) fn workspace_of(&self, raw_id: &str) -> Result<PathBuf, DaemonError> {
        Ok(self.get_validated(raw_id)?.workspace.clone())
    }

    pub(super) fn allocate_session_id(&self) -> String {
        loop {
            let candidate = format!("s-{}", self.next_session_id.fetch_add(1, Ordering::Relaxed));
            let exists_in_mem = self
                .entries
                .read()
                .ok()
                .is_some_and(|m| m.contains_key(&candidate));
            let exists_on_disk = self.sessions_dir.join(format!("{candidate}.json")).exists();
            if !exists_in_mem && !exists_on_disk {
                return candidate;
            }
        }
    }

    pub(super) fn list(&self) -> pb::ListSessionsResponse {
        let entries: Vec<Arc<SessionEntry>> = self
            .entries
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default();
        let mut seen = HashSet::with_capacity(entries.len());
        let mut out = Vec::with_capacity(entries.len());

        for entry in entries {
            seen.insert(entry.id.clone());
            let history = entry.client.history().unwrap_or_default();
            let is_running = entry
                .active_task
                .lock()
                .map(|g| g.is_some())
                .unwrap_or(false);
            out.push(pb::SessionSummary {
                session_id: entry.id.clone(),
                title: derive_session_title(&history),
                model: entry.model.clone(),
                workspace: entry.workspace.display().to_string(),
                message_count: history.len() as u64,
                is_running,
                persisted: entry.session_file.is_file() || entry.store.is_some(),
                updated_at: entry.updated_at.load(Ordering::Relaxed),
            });
        }

        if let Some(store) = &self.store {
            for meta in store.list_sessions() {
                if seen.insert(meta.id.clone()) {
                    out.push(pb::SessionSummary {
                        session_id: meta.id,
                        title: meta.title,
                        model: meta.model,
                        workspace: meta.workspace,
                        message_count: meta.message_count as u64,
                        is_running: false,
                        persisted: true,
                        updated_at: meta.updated_at,
                    });
                }
            }
        }

        out.sort_by(|a, b| {
            b.updated_at
                .cmp(&a.updated_at)
                .then_with(|| a.session_id.cmp(&b.session_id))
        });
        pb::ListSessionsResponse { sessions: out }
    }

    pub(super) async fn create(
        &self,
        req: pb::CreateSessionRequest,
    ) -> Result<pb::CreateSessionResponse, DaemonError> {
        let (entry, context_profile) = build_session_entry(self, req).await?;
        let id = entry.id.clone();
        if let Ok(mut map) = self.entries.write() {
            map.insert(id.clone(), entry);
        }
        Ok(pb::CreateSessionResponse {
            session_id: id,
            context_profile: Some(context_profile.into()),
        })
    }

    pub(super) fn delete(&self, raw_id: &str) -> Result<bool, DaemonError> {
        let id = validate_session_id(raw_id)?;
        let removed_mem = self
            .entries
            .write()
            .ok()
            .and_then(|mut map| map.remove(&id));
        if let Some(ref entry) = removed_mem {
            entry.cancel_active_turn();
            let path = entry.session_file.clone();
            tokio::task::spawn_blocking(move || {
                let _ = std::fs::remove_file(path);
            });
        }
        let removed_db = self
            .store
            .as_ref()
            .is_some_and(|store| store.delete_sync(&id));
        Ok(removed_mem.is_some() || removed_db)
    }

    pub(super) fn history(&self, raw_id: &str) -> Result<pb::HistoryResponse, DaemonError> {
        let id = validate_session_id(raw_id)?;
        if let Ok(entry) = self.get(&id) {
            let history = entry.client.history()?;
            return Ok(pb::HistoryResponse {
                messages: history.iter().map(pb::Message::from).collect(),
            });
        }
        if let Some(store) = &self.store
            && let Some(history) = store.load_history(&id)
        {
            return Ok(pb::HistoryResponse {
                messages: history.iter().map(pb::Message::from).collect(),
            });
        }
        Err(DaemonError::not_found("session not found"))
    }

    pub(super) fn metrics(&self, raw_id: &str) -> Result<pb::SessionMetricsResponse, DaemonError> {
        let entry = self.get_validated(raw_id)?;
        entry
            .metrics
            .lock()
            .map(|m| m.to_response())
            .map_err(|_| DaemonError::internal("session metrics poisoned"))
    }
}

pub(crate) fn validate_session_id(raw: &str) -> Result<String, DaemonError> {
    let trimmed = raw.trim();
    let valid = !trimmed.is_empty()
        && trimmed.len() <= 96
        && trimmed
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-');
    if valid {
        Ok(trimmed.to_owned())
    } else {
        Err(DaemonError::bad_request("invalid session_id"))
    }
}
