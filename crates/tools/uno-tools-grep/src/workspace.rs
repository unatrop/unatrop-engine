use std::path::{Component, Path, PathBuf};

use uno_tools_core::ToolError;

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

    pub(crate) fn root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn resolve_scope(&self, requested: Option<&str>) -> Result<PathBuf, ToolError> {
        let Some(requested) = requested.filter(|path| !path.is_empty()) else {
            return Ok(self.root.clone());
        };
        let path = Path::new(requested);
        if path.is_absolute() {
            return Err(ToolError::InvalidArguments(
                "path must be workspace-relative".into(),
            ));
        }

        let mut relative = PathBuf::new();
        for component in path.components() {
            match component {
                Component::Normal(part) => relative.push(part),
                Component::CurDir => {}
                Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                    return Err(ToolError::InvalidArguments(
                        "path must not contain parent or root components".into(),
                    ));
                }
            }
        }
        if relative.as_os_str().is_empty() {
            return Ok(self.root.clone());
        }

        let path = std::fs::canonicalize(self.root.join(relative)).map_err(|error| {
            ToolError::Execution(format!("cannot resolve search path: {error}"))
        })?;
        if !path.starts_with(&self.root) {
            return Err(ToolError::InvalidArguments(
                "path resolves outside the workspace".into(),
            ));
        }
        if !path.is_dir() && !path.is_file() {
            return Err(ToolError::InvalidArguments(
                "path must identify a file or directory".into(),
            ));
        }

        Ok(path)
    }
}
