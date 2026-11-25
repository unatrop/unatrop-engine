use std::path::{Component, Path, PathBuf};

use tokio::io::AsyncReadExt;
use uno_tools_core::ToolError;

use crate::MAX_FILE_BYTES;

pub(crate) struct Workspace {
    root: PathBuf,
}

impl Workspace {
    pub(crate) fn new(root: impl AsRef<Path>) -> Result<Self, ToolError> {
        let root = std::fs::canonicalize(root)
            .map_err(|error| ToolError::Execution(format!("invalid workspace root: {error}")))?;
        if !root.is_dir() {
            return Err(ToolError::Execution(
                "workspace root is not a directory".into(),
            ));
        }

        Ok(Self { root })
    }

    pub(crate) async fn existing_file(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let relative = self.relative_path(requested)?;
        let path = tokio::fs::canonicalize(self.root.join(relative))
            .await
            .map_err(|error| ToolError::Execution(format!("cannot resolve file: {error}")))?;
        if !path.starts_with(&self.root) {
            return Err(ToolError::InvalidArguments(
                "path resolves outside the workspace".into(),
            ));
        }

        Ok(path)
    }

    pub(crate) async fn directory(&self, requested: Option<&str>) -> Result<PathBuf, ToolError> {
        let Some(requested) = requested.filter(|path| !path.is_empty()) else {
            return Ok(self.root.clone());
        };
        let relative = self.relative_path(requested)?;
        let path = tokio::fs::canonicalize(self.root.join(relative))
            .await
            .map_err(|error| ToolError::Execution(format!("cannot resolve directory: {error}")))?;
        if !path.starts_with(&self.root) {
            return Err(ToolError::InvalidArguments(
                "path resolves outside the workspace".into(),
            ));
        }
        if !path.is_dir() {
            return Err(ToolError::InvalidArguments(
                "path does not identify a directory".into(),
            ));
        }

        Ok(path)
    }

    pub(crate) async fn write_target(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let relative = self.relative_path(requested)?;
        let file_name = relative
            .file_name()
            .ok_or_else(|| ToolError::InvalidArguments("path must identify a file".into()))?;
        let parent = relative.parent().unwrap_or_else(|| Path::new(""));
        let parent = tokio::fs::canonicalize(self.root.join(parent))
            .await
            .map_err(|error| {
                ToolError::Execution(format!("cannot resolve parent directory: {error}"))
            })?;
        if !parent.starts_with(&self.root) {
            return Err(ToolError::InvalidArguments(
                "path resolves outside the workspace".into(),
            ));
        }

        let target = parent.join(file_name);
        match tokio::fs::symlink_metadata(&target).await {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                return Err(ToolError::InvalidArguments(
                    "target must not be a symbolic link".into(),
                ));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(ToolError::InvalidArguments(
                    "target must be a regular file".into(),
                ));
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(ToolError::Execution(format!(
                    "cannot inspect target file: {error}"
                )));
            }
        }

        Ok(target)
    }

    fn relative_path(&self, requested: &str) -> Result<PathBuf, ToolError> {
        let path = Path::new(requested);
        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => relative.push(part),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(ToolError::InvalidArguments(
                        "path must be workspace-relative and must not contain parent components"
                            .into(),
                    ));
                }
            }
        }

        if relative.as_os_str().is_empty() {
            return Err(ToolError::InvalidArguments(
                "path must identify a file".into(),
            ));
        }

        Ok(relative)
    }
}

pub(crate) async fn read_text_file(path: &Path) -> Result<String, ToolError> {
    let mut reader = tokio::fs::File::open(path)
        .await
        .map_err(|error| ToolError::Execution(format!("cannot open file: {error}")))?
        .take((MAX_FILE_BYTES + 1) as u64);
    let mut bytes = Vec::new();
    reader
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| ToolError::Execution(format!("cannot read file: {error}")))?;
    if bytes.len() > MAX_FILE_BYTES {
        return Err(ToolError::Execution(format!(
            "file exceeds the {MAX_FILE_BYTES}-byte limit"
        )));
    }

    String::from_utf8(bytes).map_err(|_| ToolError::Execution("file is not valid UTF-8".into()))
}
