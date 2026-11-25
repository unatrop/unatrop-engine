//! Shared daemon state: session registry, SQLite store, and HTTP transport pool.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        Arc, OnceLock, RwLock,
        atomic::{AtomicU64, AtomicUsize, Ordering},
    },
};

use uno_reqwest_transport::ReqwestTransport;
use uno_voice::OnnxStt;

use super::error::DaemonError;
use crate::{session::SessionEntry, storage::SqliteSessionStore, tts::AliceTtsPool};

#[derive(Clone)]
pub struct DaemonState {
    pub(crate) sessions: Arc<RwLock<HashMap<String, Arc<SessionEntry>>>>,
    pub(crate) sessions_dir: PathBuf,
    pub(crate) next_session_id: Arc<AtomicU64>,
    pub(crate) auth_token: Option<String>,
    /// Single shared HTTP/2 + TLS connection pool reused across all sessions for low-latency TTFB.
    pub(crate) transport: Arc<ReqwestTransport>,
    pub(crate) store: Option<Arc<SqliteSessionStore>>,
    /// Pooled pure-Rust `uni.ws` TTS client + Ogg/Opus decoder.
    pub(crate) tts_pool: Arc<AliceTtsPool>,
    /// Lazily initialized local GigaAM ONNX STT engine.
    pub(crate) stt: Arc<OnceLock<Arc<OnnxStt>>>,
    pub(crate) active_calls: Arc<AtomicUsize>,
    pub(crate) call_epoch: Arc<AtomicU64>,
}

fn default_sessions_dir() -> PathBuf {
    if let Some(explicit) = std::env::var_os("UNATROP_DAEMON_SESSIONS_DIR") {
        let p = PathBuf::from(explicit);
        if !p.as_os_str().is_empty() {
            return p;
        }
    }
    if let Some(local) = std::env::var_os("LOCALAPPDATA") {
        return PathBuf::from(local).join("unatrop").join("daemon-sessions");
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        return PathBuf::from(home).join(".unatrop").join("daemon-sessions");
    }
    let user = std::env::var("USERNAME")
        .or_else(|_| std::env::var("USER"))
        .unwrap_or_else(|_| "user".to_owned())
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
        .collect::<String>();
    std::env::temp_dir().join(format!("unatrop-daemon-sessions-{user}"))
}

impl DaemonState {
    pub fn new(auth_token: Option<String>) -> Self {
        Self::with_sessions_dir(auth_token, default_sessions_dir())
    }

    pub fn with_sessions_dir(auth_token: Option<String>, sessions_dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&sessions_dir);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&sessions_dir, std::fs::Permissions::from_mode(0o700));
        }
        let store = SqliteSessionStore::open(&sessions_dir).map(Arc::new);
        let transport = Arc::new(
            ReqwestTransport::new().expect("failed to initialize shared ReqwestTransport"),
        );
        Self {
            sessions: Arc::new(RwLock::new(HashMap::new())),
            sessions_dir,
            next_session_id: Arc::new(AtomicU64::new(1)),
            auth_token: auth_token.filter(|t| !t.trim().is_empty()),
            transport,
            store,
            tts_pool: Arc::new(AliceTtsPool::new()),
            stt: Arc::new(OnceLock::new()),
            active_calls: Arc::new(AtomicUsize::new(0)),
            call_epoch: Arc::new(AtomicU64::new(0)),
        }
    }

    pub(crate) fn shared_stt(&self) -> Arc<OnnxStt> {
        Arc::clone(self.stt.get_or_init(|| {
            let models_dir = std::env::var_os("UNATROP_MODELS_DIR")
                .map(PathBuf::from)
                .filter(|p| !p.as_os_str().is_empty())
                .or_else(|| {
                    crate::agent::config::resolve_unatrop_home().map(|home| home.join("models"))
                })
                .unwrap_or_else(|| {
                    self.sessions_dir
                        .parent()
                        .unwrap_or(&self.sessions_dir)
                        .join("models")
                });
            Arc::new(OnnxStt::new(models_dir))
        }))
    }

    pub(crate) fn get_session(&self, id: &str) -> Result<Arc<SessionEntry>, DaemonError> {
        self.sessions
            .read()
            .ok()
            .and_then(|map| map.get(id).cloned())
            .ok_or_else(|| DaemonError::not_found("session not found"))
    }

    pub(crate) fn list_entries(&self) -> Vec<Arc<SessionEntry>> {
        self.sessions
            .read()
            .map(|m| m.values().cloned().collect())
            .unwrap_or_default()
    }

    pub(crate) fn insert_session(&self, id: String, entry: Arc<SessionEntry>) {
        if let Ok(mut map) = self.sessions.write() {
            map.insert(id, entry);
        }
    }

    pub(crate) fn remove_session(&self, id: &str) -> Option<Arc<SessionEntry>> {
        self.sessions
            .write()
            .ok()
            .and_then(|mut map| map.remove(id))
    }

    pub(crate) fn allocate_session_id(&self) -> String {
        loop {
            let candidate = format!("s-{}", self.next_session_id.fetch_add(1, Ordering::Relaxed));
            let exists_in_mem = self
                .sessions
                .read()
                .ok()
                .is_some_and(|m| m.contains_key(&candidate));
            let exists_on_disk = self.sessions_dir.join(format!("{candidate}.json")).exists();
            if !exists_in_mem && !exists_on_disk {
                return candidate;
            }
        }
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
