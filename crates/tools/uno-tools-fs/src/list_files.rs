use std::{
    collections::VecDeque,
    ffi::OsStr,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use uno_tools_core::{Approval, ToolError, ToolOutput, TypedTool};

use crate::workspace::Workspace;

/// Лимит записей по умолчанию для одного вызова
pub const MAX_DIRECTORY_ENTRIES: usize = 200;
/// Максимально допустимое значение `limit`
pub const MAX_LIST_ENTRIES: usize = 1000;
/// Максимально допустимая глубина рекурсивного обхода
pub const MAX_LIST_DEPTH: usize = 8;
const DEFAULT_LIST_DEPTH: usize = 3;
const MAX_WALK_ENTRIES: usize = 10_000;

/// Аргументы инструмента `list_files`
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ListFilesArgs {
    /// Путь к директории относительно корня рабочей области
    #[schemars(description = "Workspace-relative directory path; defaults to the workspace root.")]
    pub path: Option<String>,
    /// Включает рекурсивный обход поддиректорий
    #[schemars(description = "Traverse subdirectories; defaults to false.")]
    pub recursive: Option<bool>,
    /// Глубина обхода (от 1 до 8)
    #[schemars(description = "Maximum directory depth (1-8); defaults to 3.")]
    pub depth: Option<usize>,
    /// Максимальное число записей (от 1 до 1000)
    #[schemars(description = "Maximum entries (1-1000); defaults to 200.")]
    pub limit: Option<usize>,
}

/// Инструмент просмотра содержимого директорий рабочей области
pub struct ListFiles {
    workspace: Workspace,
}

impl ListFiles {
    /// Создаёт инструмент для указанного корня рабочей области
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ToolError> {
        Ok(Self {
            workspace: Workspace::new(workspace)?,
        })
    }
}

#[async_trait]
impl TypedTool for ListFiles {
    type Args = ListFilesArgs;

    fn name(&self) -> &str {
        "list_files"
    }

    fn description(&self) -> &str {
        "List workspace entries, skipping common build/cache/VCS directories and never following symlinks. Recursive traversal is optional and bounded by depth and entry limits."
    }

    fn approval(&self, _args: &Self::Args) -> Approval {
        Approval::Never
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let recursive = args.recursive.unwrap_or(false);
        let depth = args.depth.unwrap_or(DEFAULT_LIST_DEPTH);
        if !(1..=MAX_LIST_DEPTH).contains(&depth) {
            return Err(ToolError::InvalidArguments(format!(
                "depth must be between 1 and {MAX_LIST_DEPTH}"
            )));
        }
        let limit = args.limit.unwrap_or(MAX_DIRECTORY_ENTRIES);
        if !(1..=MAX_LIST_ENTRIES).contains(&limit) {
            return Err(ToolError::InvalidArguments(format!(
                "limit must be between 1 and {MAX_LIST_ENTRIES}"
            )));
        }

        let directory = self.workspace.directory(args.path.as_deref()).await?;
        let mut pending = VecDeque::from([(directory, PathBuf::new(), 1)]);
        let mut entries = Vec::new();
        let mut truncated = false;
        let mut traversal_capped = false;
        let mut visited = 0;

        'walk: while let Some((directory, relative_directory, current_depth)) = pending.pop_front()
        {
            let mut reader = tokio::fs::read_dir(directory)
                .await
                .map_err(|error| ToolError::Execution(format!("cannot list directory: {error}")))?;
            while let Some(entry) = reader
                .next_entry()
                .await
                .map_err(|error| ToolError::Execution(format!("cannot list directory: {error}")))?
            {
                visited += 1;
                if visited > MAX_WALK_ENTRIES {
                    truncated = true;
                    traversal_capped = true;
                    break 'walk;
                }

                let file_type = entry.file_type().await.map_err(|error| {
                    ToolError::Execution(format!("cannot inspect entry: {error}"))
                })?;
                let name = entry.file_name();
                if file_type.is_dir() && is_ignored_directory(&name) {
                    continue;
                }

                let relative_path = relative_directory.join(&name);
                entries.push((
                    relative_path.clone(),
                    file_type.is_dir(),
                    file_type.is_symlink(),
                ));
                if entries.len() == limit {
                    truncated = true;
                    break 'walk;
                }

                if recursive && file_type.is_dir() && current_depth < depth {
                    pending.push_back((entry.path(), relative_path, current_depth + 1));
                }
            }
        }

        entries.sort_by(|left, right| left.0.cmp(&right.0));
        let path_label = args
            .path
            .as_deref()
            .filter(|path| !path.is_empty())
            .unwrap_or(".");
        let mut content = if recursive {
            format!("Entries in {path_label:?} (recursive, depth {depth}):")
        } else {
            format!("Entries in {path_label:?} (non-recursive):")
        };
        if entries.is_empty() {
            content.push_str("\n(empty)");
        } else {
            for (path, is_directory, is_symlink) in entries {
                let mut name = path.to_string_lossy().escape_debug().to_string();
                if is_directory {
                    name.push('/');
                } else if is_symlink {
                    name.push('@');
                }
                content.push('\n');
                content.push_str(&name);
            }
        }
        if truncated {
            if traversal_capped {
                content.push_str(&format!(
                    "\n... additional entries omitted (traversal cap {MAX_WALK_ENTRIES})"
                ));
            } else {
                content.push_str(&format!("\n... additional entries omitted (limit {limit})"));
            }
        }

        Ok(ToolOutput::text(content))
    }
}

fn is_ignored_directory(name: &OsStr) -> bool {
    matches!(
        name.to_str(),
        Some(
            ".git"
                | ".hg"
                | ".svn"
                | ".cache"
                | ".idea"
                | ".vscode"
                | ".gradle"
                | ".cxx"
                | "build"
                | "node_modules"
                | "__pycache__"
                | ".venv"
                | "venv"
                | "target"
                | "cmake-build-debug"
                | "cmake-build-release"
        )
    )
}
