//! Active session runtime: turn execution, history manipulation, and persistence.

use std::{
    path::PathBuf,
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use futures_util::StreamExt;
use tokio::sync::broadcast;
use uno_protocol::{Message, Role};

use super::{events::map_run_event, metrics::SessionMetrics};
use crate::{
    agent::{Client, DaemonApprovalHandler},
    protocol::pb,
    storage::{SqliteSessionStore, StoredDaemonSession, derive_session_title, unix_timestamp_secs},
};

pub(crate) const SESSION_EVENT_CHANNEL_CAPACITY: usize = 512;

pub(crate) struct SessionEntry {
    pub id: String,
    pub model: String,
    pub workspace: PathBuf,
    pub session_file: PathBuf,
    pub client: Arc<Client>,
    pub approval: Arc<DaemonApprovalHandler>,
    pub events: broadcast::Sender<pb::ServerMsg>,
    pub active_task: Mutex<Option<tokio::task::JoinHandle<()>>>,
    pub metrics: Mutex<SessionMetrics>,
    pub store: Option<Arc<SqliteSessionStore>>,
    pub updated_at: AtomicU64,
    pub turn_started_at: Mutex<Option<Instant>>,
    pub first_token_logged: AtomicBool,
}

impl SessionEntry {
    pub(super) fn mark_first_token_ms(&self) -> Option<u64> {
        if self
            .first_token_logged
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_ok()
        {
            return self.turn_elapsed_ms();
        }
        None
    }

    pub(super) fn turn_elapsed_ms(&self) -> Option<u64> {
        self.turn_started_at
            .lock()
            .ok()
            .and_then(|g| g.map(|t| u64::try_from(t.elapsed().as_millis()).unwrap_or(u64::MAX)))
    }
    fn to_stored(&self) -> Option<StoredDaemonSession> {
        let messages = self.client.history().ok()?;
        let now = unix_timestamp_secs();
        self.updated_at.store(now, Ordering::Relaxed);
        Some(StoredDaemonSession {
            id: self.id.clone(),
            title: derive_session_title(&messages),
            model: self.model.clone(),
            workspace: self.workspace.display().to_string(),
            messages,
            updated_at: now,
        })
    }

    pub(crate) fn persist_sync(&self) {
        if let Some(store) = &self.store
            && let Some(stored) = self.to_stored()
        {
            store.save_sync(&stored);
        }
        let _ = self.client.save_session(&self.session_file);
    }

    pub(crate) fn persist_async(&self) {
        if let Some(store) = &self.store
            && let Some(stored) = self.to_stored()
        {
            store.save_async(stored);
        }
        let client = Arc::clone(&self.client);
        let path = self.session_file.clone();
        tokio::task::spawn_blocking(move || {
            let _ = client.save_session(path);
        });
    }

    fn take_and_abort_active_turn(&self) -> Option<tokio::task::JoinHandle<()>> {
        let mut guard = self.active_task.lock().ok()?;
        let handle = guard.take()?;
        handle.abort();
        Some(handle)
    }

    pub(crate) fn cancel_active_turn(&self) {
        let _ = self.take_and_abort_active_turn();
    }

    pub(crate) fn truncate_last_user_turn(&self) {
        match self.client.history() {
            Ok(mut history) => {
                if let Some(idx) = history.iter().rposition(|m| m.role == Role::User) {
                    history.truncate(idx);
                    self.set_history(history);
                }
            }
            Err(err) => self.emit_error(err.to_string()),
        }
    }

    pub(crate) fn set_history(&self, messages: Vec<Message>) {
        if let Err(err) = self.client.set_history(messages) {
            self.emit_error(err.to_string());
        } else {
            self.persist_async();
        }
    }

    pub(crate) fn reset_session(&self) {
        if let Err(err) = self.client.reset_session() {
            self.emit_error(err.to_string());
        } else {
            self.persist_async();
        }
    }

    pub(crate) fn spawn_turn_with_options(
        self: &Arc<Self>,
        text: String,
        voice_mode: bool,
        voice_locale: Option<String>,
    ) {
        let prev_handle = self.take_and_abort_active_turn();
        if let Ok(mut started) = self.turn_started_at.lock() {
            *started = Some(Instant::now());
        }
        self.first_token_logged.store(false, Ordering::Release);

        let entry = Arc::clone(self);
        let handle = tokio::spawn(async move {
            if let Some(prev) = prev_handle {
                let _ = prev.await;
            }

            let mut attempt = 0u8;
            let stream_res = loop {
                match entry.client.send_with_voice(
                    text.clone(),
                    voice_mode,
                    voice_locale.as_deref(),
                ) {
                    Ok(stream) => break Ok(stream),
                    Err(err) if attempt < 3 && err.to_string().contains("already active") => {
                        attempt += 1;
                        tokio::time::sleep(Duration::from_millis(25)).await;
                    }
                    Err(err) => break Err(err),
                }
            };

            match stream_res {
                Ok(mut stream) => {
                    while let Some(item) = stream.next().await {
                        let msg = map_run_event(&entry, item);
                        let _ = entry.events.send(msg);
                    }
                }
                Err(err) => entry.emit_error(err.to_string()),
            }
            if let Ok(mut guard) = entry.active_task.lock() {
                *guard = None;
            }
        });

        if let Ok(mut guard) = self.active_task.lock() {
            *guard = Some(handle);
        }
    }

    fn emit_error(&self, message: String) {
        let _ = self.events.send(pb::ServerMsg {
            msg: Some(pb::server_msg::Msg::Error(pb::ErrorEvent { message })),
        });
    }
}
