//! Файловая персистентность сессий: append-only JSONL и JSON-снимки.
//!
//! Ввод-вывод с ОС изолирован в этом крейте, чтобы `uno-session` оставался
//! платформо-независимым.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, BufWriter, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use harness::{Harness, SessionCheckpoint};
use protocol::{Message, redact_message_secrets};
use session::{Session, SessionError, repair_history};

fn restrict_dir_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700));
    }
    #[cfg(not(unix))]
    let _ = path;
}

fn restrict_file_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    #[cfg(not(unix))]
    let _ = path;
}

/// Локальный append-only JSONL журнал сообщений сессии.
/// Реализует [`SessionCheckpoint`] с принудительным `flush` + `sync_data` на диск.
pub struct JsonlSessionStore {
    path: PathBuf,
    writer: Mutex<BufWriter<File>>,
}

impl JsonlSessionStore {
    /// Открывает (или создаёт) JSONL файл журнала сессии.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, SessionError> {
        let path = path.as_ref().to_path_buf();
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)?;
            restrict_dir_permissions(parent);
        }
        let file = OpenOptions::new().create(true).append(true).open(&path)?;
        restrict_file_permissions(&path);

        Ok(Self {
            path,
            writer: Mutex::new(BufWriter::new(file)),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Дописывает одно сообщение в конец JSONL-журнала
    pub fn append(&self, message: &Message) -> Result<(), SessionError> {
        let redacted = redact_message_secrets(std::slice::from_ref(message));
        let mut line = serde_json::to_string(&redacted[0])?;
        line.push('\n');

        let mut guard = self
            .writer
            .lock()
            .map_err(|_| SessionError::HistoryPoisoned)?;
        guard.write_all(line.as_bytes())?;
        Ok(())
    }

    /// Сбрасывает буфер на диск (`flush` + `sync_data`)
    pub fn flush(&self) -> Result<(), SessionError> {
        let mut guard = self
            .writer
            .lock()
            .map_err(|_| SessionError::HistoryPoisoned)?;
        guard.flush()?;
        guard.get_ref().sync_data()?;
        Ok(())
    }

    /// Читает все сообщения из JSONL-журнала и прогоняет через `repair_history`
    pub fn load_messages(path: impl AsRef<Path>) -> Result<Vec<Message>, SessionError> {
        let path = path.as_ref();
        if !path.exists() {
            return Ok(Vec::new());
        }
        let file = File::open(path)?;
        let reader = BufReader::new(file);
        let mut messages = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            let msg: Message = serde_json::from_str(&line)?;
            messages.push(msg);
        }
        repair_history(&mut messages);
        Ok(messages)
    }

    /// Загружает восстановленную историю сразу в [`Session`] с указанным [`Harness`]
    pub fn load_session(
        path: impl AsRef<Path>,
        harness: Arc<Harness>,
    ) -> Result<Session, SessionError> {
        let messages = Self::load_messages(path)?;
        Ok(Session::new(harness, messages))
    }
}

#[async_trait]
impl SessionCheckpoint for JsonlSessionStore {
    async fn flush(&self) -> Result<(), String> {
        JsonlSessionStore::flush(self).map_err(|err| err.to_string())
    }
}

/// Сохраняет историю сессии в JSON-файл (создавая родительские каталоги)
pub fn save_session_json(session: &Session, path: impl AsRef<Path>) -> Result<(), SessionError> {
    let history = redact_message_secrets(&session.history()?);
    let path = path.as_ref();
    if let Some(parent) = path.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
        restrict_dir_permissions(parent);
    }
    let encoded = serde_json::to_vec_pretty(&history)?;
    std::fs::write(path, encoded)?;
    restrict_file_permissions(path);
    Ok(())
}

/// Загружает сессию из JSON-файла и чинит незавершённые вызовы инструментов
pub fn load_session_json(
    harness: Arc<Harness>,
    path: impl AsRef<Path>,
) -> Result<Session, SessionError> {
    let bytes = std::fs::read(path.as_ref())?;
    let mut history: Vec<Message> = serde_json::from_slice(&bytes)?;
    repair_history(&mut history);
    Ok(Session::new(harness, history))
}
