//! Кросс-платформенный контракт выгрузки больших результатов инструментов (spill storage)

use std::collections::HashMap;
use std::fmt::Write as _;
use std::sync::Mutex;

use async_trait::async_trait;

use crate::ToolError;

/// Ссылка на сохранённый полный вывод инструмента
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpillRef {
    /// Путь или URI сохранённого объекта (зависит от платформы/бэкенда)
    pub location: String,
    /// Размер сохранённых данных в байтах UTF-8
    pub bytes: usize,
}

/// Платформо-независимое хранилище для выгрузки больших выводов инструментов.
/// На Desktop пишется в приватную директорию сессии (`0600`), на Android/iOS/в тестах
/// может использоваться внутреннее хранилище приложения или `InMemorySpillStore`.
#[async_trait]
pub trait SpillStore: Send + Sync {
    async fn save(
        &self,
        session_id: &str,
        suggested_name: &str,
        content: &str,
    ) -> Result<SpillRef, ToolError>;
}

/// Безопасное инъективное кодирование произвольной строки в один сегмент имени файла.
/// Защищает от `../`, `/`, `\0`, `.` и `..` без коллизий между разными входными строками.
pub fn encode_safe_segment(raw: &str) -> String {
    if raw.is_empty() {
        return "~".to_owned();
    }
    if raw == "." {
        return "~002E".to_owned();
    }
    if raw == ".." {
        return "~002E~002E".to_owned();
    }
    let mut out = String::with_capacity(raw.len());
    for code in raw.encode_utf16() {
        let ch = char::from_u32(u32::from(code));
        let is_safe = matches!(
            ch,
            Some('A'..='Z' | 'a'..='z' | '0'..='9' | '.' | '_' | '-')
        );
        if is_safe && let Some(c) = ch {
            out.push(c);
        } else {
            let _ = write!(out, "~{code:04X}");
        }
    }
    out
}

/// Потокобезопасное in-memory хранилище для тестов, мобильных клиентов без FS и Wasm
#[derive(Default)]
pub struct InMemorySpillStore {
    entries: Mutex<HashMap<String, String>>,
    counter: std::sync::atomic::AtomicU64,
}

impl InMemorySpillStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self, location: &str) -> Option<String> {
        self.entries
            .lock()
            .ok()
            .and_then(|guard| guard.get(location).cloned())
    }
}

#[async_trait]
impl SpillStore for InMemorySpillStore {
    async fn save(
        &self,
        session_id: &str,
        suggested_name: &str,
        content: &str,
    ) -> Result<SpillRef, ToolError> {
        let seq = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let location = format!(
            "mem://{}/{seq}-{}",
            encode_safe_segment(session_id),
            encode_safe_segment(suggested_name)
        );
        let bytes = content.len();
        let mut guard = self
            .entries
            .lock()
            .map_err(|_| ToolError::Execution("spill store lock poisoned".into()))?;
        guard.insert(location.clone(), content.to_owned());
        Ok(SpillRef { location, bytes })
    }
}
