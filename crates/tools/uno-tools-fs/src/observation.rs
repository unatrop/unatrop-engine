//! Optimistic Locking (CAS) для файловых инструментов на уровне сессии.
//! Защищает от редактирования непрочитанных файлов и перезаписи внешних изменений.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::UNIX_EPOCH;

use uno_tools_core::ToolError;

/// Зафиксированное состояние файла на момент последнего наблюдения
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileSnapshot {
    pub len: u64,
    pub modified_nanos: Option<u128>,
}

/// Трекер наблюдений файлов для одной сессии/воркспейса
#[derive(Default)]
pub struct FileObservationTracker {
    observed: Mutex<HashMap<PathBuf, FileSnapshot>>,
}

impl FileObservationTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Фиксирует текущую версию файла после чтения или записи
    pub async fn record_present(&self, path: &Path) -> Result<(), ToolError> {
        let snap = read_snapshot(path).await?;
        let mut guard = self
            .observed
            .lock()
            .map_err(|_| ToolError::Execution("observation tracker lock poisoned".into()))?;
        guard.insert(path.to_path_buf(), snap);
        Ok(())
    }

    /// Проверяет перед `apply_patch`, что файл был прочитан и не изменился на диске
    pub async fn verify_fresh_for_edit(&self, path: &Path) -> Result<(), ToolError> {
        let prior = {
            let guard = self
                .observed
                .lock()
                .map_err(|_| ToolError::Execution("observation tracker lock poisoned".into()))?;
            guard.get(path).copied()
        };
        let Some(prior) = prior else {
            return Err(ToolError::InvalidArguments(format!(
                "edit requires reading `{}` first",
                path.display()
            )));
        };

        let current = read_snapshot(path).await?;
        if current != prior {
            return Err(ToolError::Execution(format!(
                "file `{}` changed on disk since last read; re-read before editing",
                path.display()
            )));
        }
        Ok(())
    }

    /// Проверяет перед `write_file`: новый файл можно создавать, а существующий должен быть прочитан и свеж
    pub async fn verify_fresh_for_write(&self, path: &Path) -> Result<(), ToolError> {
        match tokio::fs::metadata(path).await {
            Ok(meta) => {
                let current = snapshot_from_meta(&meta);
                let prior = {
                    let guard = self.observed.lock().map_err(|_| {
                        ToolError::Execution("observation tracker lock poisoned".into())
                    })?;
                    guard.get(path).copied()
                };
                let Some(prior) = prior else {
                    return Err(ToolError::InvalidArguments(format!(
                        "overwriting existing file `{}` requires reading it first",
                        path.display()
                    )));
                };
                if current != prior {
                    return Err(ToolError::Execution(format!(
                        "file `{}` changed on disk since last read; re-read before overwriting",
                        path.display()
                    )));
                }
                Ok(())
            }
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(ToolError::Execution(format!(
                "cannot stat file `{}`: {err}",
                path.display()
            ))),
        }
    }
}

async fn read_snapshot(path: &Path) -> Result<FileSnapshot, ToolError> {
    let meta = tokio::fs::metadata(path)
        .await
        .map_err(|err| ToolError::Execution(format!("cannot stat `{}`: {err}", path.display())))?;
    Ok(snapshot_from_meta(&meta))
}

fn snapshot_from_meta(meta: &std::fs::Metadata) -> FileSnapshot {
    let modified_nanos = meta
        .modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_nanos());
    FileSnapshot {
        len: meta.len(),
        modified_nanos,
    }
}
