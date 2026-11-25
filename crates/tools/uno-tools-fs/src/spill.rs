//! Локальное файловое spill-хранилище с приватными правами доступа (`0700` / `0600`)

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use async_trait::async_trait;
use tokio::io::AsyncWriteExt;
use uno_tools_core::{SpillRef, SpillStore, ToolError, encode_safe_segment};

/// Локальное файловое хранилище выгрузок больших результатов инструментов
pub struct LocalSpillStore {
    root: PathBuf,
    counter: AtomicU64,
}

impl LocalSpillStore {
    pub fn new(root: impl AsRef<Path>) -> Self {
        Self {
            root: root.as_ref().to_path_buf(),
            counter: AtomicU64::new(0),
        }
    }

    pub fn default_temp() -> Self {
        Self::new(std::env::temp_dir().join("uno-spill"))
    }
}

#[async_trait]
impl SpillStore for LocalSpillStore {
    async fn save(
        &self,
        session_id: &str,
        suggested_name: &str,
        content: &str,
    ) -> Result<SpillRef, ToolError> {
        let session_dir = self
            .root
            .join(format!("session-{}", encode_safe_segment(session_id)));
        tokio::fs::create_dir_all(&session_dir)
            .await
            .map_err(|err| ToolError::Execution(format!("cannot create spill dir: {err}")))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ =
                tokio::fs::set_permissions(&session_dir, std::fs::Permissions::from_mode(0o700))
                    .await;
        }

        let safe_name = encode_safe_segment(suggested_name);
        let pid = std::process::id();

        for _ in 0..8 {
            let seq = self.counter.fetch_add(1, Ordering::Relaxed);
            let file_path = session_dir.join(format!("{pid}-{seq}-{safe_name}"));
            let mut opts = tokio::fs::OpenOptions::new();
            opts.write(true).create_new(true);
            #[cfg(unix)]
            opts.mode(0o600);

            match opts.open(&file_path).await {
                Ok(mut file) => {
                    file.write_all(content.as_bytes()).await.map_err(|err| {
                        ToolError::Execution(format!("spill write failed: {err}"))
                    })?;
                    file.flush().await.map_err(|err| {
                        ToolError::Execution(format!("spill flush failed: {err}"))
                    })?;
                    return Ok(SpillRef {
                        location: file_path.to_string_lossy().into_owned(),
                        bytes: content.len(),
                    });
                }
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => continue,
                Err(err) => {
                    return Err(ToolError::Execution(format!(
                        "cannot open spill file: {err}"
                    )));
                }
            }
        }

        Err(ToolError::Execution(
            "failed to allocate unique spill file".into(),
        ))
    }
}
