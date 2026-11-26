//! Shared daemon state holding service handles and daemon auth token.

use std::{path::PathBuf, sync::Arc};

use uno_reqwest_transport::ReqwestTransport;

use crate::modules::{
    accounts::AccountService, call::CallService, sessions::SessionService, tts::TtsService,
};

#[derive(Clone)]
pub struct DaemonState {
    pub(crate) auth_token: Option<String>,
    pub(crate) accounts: Arc<AccountService>,
    pub(crate) sessions: Arc<SessionService>,
    pub(crate) tts: Arc<TtsService>,
    pub(crate) call: Arc<CallService>,
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

fn resolve_models_dir(sessions_dir: &std::path::Path) -> PathBuf {
    std::env::var_os("UNATROP_MODELS_DIR")
        .map(PathBuf::from)
        .filter(|p| !p.as_os_str().is_empty())
        .or_else(|| crate::shared::config::resolve_unatrop_home().map(|home| home.join("models")))
        .unwrap_or_else(|| sessions_dir.parent().unwrap_or(sessions_dir).join("models"))
}

impl DaemonState {
    pub fn new(auth_token: Option<String>) -> Self {
        let sessions_dir = default_sessions_dir();
        let config_dir = crate::shared::config::resolve_unatrop_home()
            .map(|h| h.join("config"))
            .unwrap_or_else(|| sessions_dir.join("config"));
        Self::build(auth_token, sessions_dir, config_dir)
    }

    pub fn with_sessions_dir(auth_token: Option<String>, sessions_dir: PathBuf) -> Self {
        let config_dir = sessions_dir.join("config");
        Self::build(auth_token, sessions_dir, config_dir)
    }

    fn build(auth_token: Option<String>, sessions_dir: PathBuf, config_dir: PathBuf) -> Self {
        let models_dir = resolve_models_dir(&sessions_dir);
        let transport = Arc::new(
            ReqwestTransport::new().expect("failed to initialize shared ReqwestTransport"),
        );
        let accounts = Arc::new(AccountService::new(config_dir, transport));
        let sessions = Arc::new(SessionService::new(sessions_dir, Arc::clone(&accounts)));
        let tts = Arc::new(TtsService::new());
        let call = Arc::new(CallService::new(
            models_dir,
            Arc::clone(&tts),
            Arc::clone(&sessions),
        ));

        Self {
            auth_token: auth_token.filter(|t| !t.trim().is_empty()),
            accounts,
            sessions,
            tts,
            call,
        }
    }
}
