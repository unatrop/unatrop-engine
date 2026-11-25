use std::path::Path;

use async_trait::async_trait;
use ignore::{DirEntry, WalkBuilder};
use regex::RegexBuilder;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use uno_tools_core::{Approval, ToolError, ToolOutput, TypedTool};

use crate::workspace::Workspace;

/// Максимальная длина поискового запроса в байтах
pub const MAX_QUERY_BYTES: usize = 4096;
/// Максимальное число совпадений в одном ответе
pub const MAX_MATCHES: usize = 100;
/// Максимальная длина строки совпадения в символах
pub const MAX_MATCH_LINE_CHARS: usize = 500;
const MAX_FILE_BYTES: usize = 1024 * 1024;
const MAX_TOTAL_BYTES: usize = 32 * 1024 * 1024;
const MAX_WALK_ENTRIES: usize = 10_000;
const MAX_ISSUE_DETAILS: usize = 20;

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SearchTextArgs {
    #[schemars(description = "Text or regex to search for.")]
    pub query: String,
    #[schemars(description = "Optional workspace-relative subdir or file.")]
    pub path: Option<String>,
    #[schemars(description = "Treat query as regex.")]
    pub regex: Option<bool>,
    #[schemars(description = "Case-sensitive match.")]
    pub case_sensitive: Option<bool>,
    #[schemars(description = "Include hidden files.")]
    pub include_hidden: Option<bool>,
    #[schemars(description = "Max matches (up to 100).")]
    pub max_results: Option<usize>,
}

/// Инструмент поиска текста по файлам рабочей области
pub struct SearchText {
    workspace: Workspace,
}

impl SearchText {
    /// Создаёт инструмент для указанного корня рабочей области
    pub fn new(workspace: impl AsRef<Path>) -> Result<Self, ToolError> {
        Ok(Self {
            workspace: Workspace::new(workspace)?,
        })
    }
}

#[derive(Serialize)]
struct SearchResult {
    query: String,
    regex: bool,
    scope: String,
    files_scanned: usize,
    bytes_scanned: usize,
    matches: Vec<SearchMatch>,
    truncated: bool,
    skipped_large_files: usize,
    skipped_binary_files: usize,
    skipped_non_utf8_files: usize,
    errors: Vec<SearchIssue>,
    omitted_errors: usize,
}

#[derive(Serialize)]
struct SearchMatch {
    path: String,
    line: usize,
    text: String,
    line_truncated: bool,
}

#[derive(Serialize)]
struct SearchIssue {
    path: Option<String>,
    message: String,
}

#[async_trait]
impl TypedTool for SearchText {
    type Args = SearchTextArgs;

    fn name(&self) -> &str {
        "search_text"
    }

    fn description(&self) -> &str {
        "Search workspace text and return bounded JSON line matches. Respects ignore files and does not follow symlinks."
    }

    fn approval(&self, _args: &Self::Args) -> Approval {
        Approval::Never
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        if args.query.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "query must not be empty".into(),
            ));
        }
        if args.query.len() > MAX_QUERY_BYTES {
            return Err(ToolError::InvalidArguments(format!(
                "query exceeds the {MAX_QUERY_BYTES}-byte limit"
            )));
        }

        let regex_mode = args.regex.unwrap_or(false);
        let pattern = if regex_mode {
            args.query.clone()
        } else {
            regex::escape(&args.query)
        };
        let regex = RegexBuilder::new(&pattern)
            .case_insensitive(!args.case_sensitive.unwrap_or(false))
            .size_limit(1024 * 1024)
            .build()
            .map_err(|error| ToolError::InvalidArguments(format!("invalid query: {error}")))?;

        let max_results = args.max_results.unwrap_or(MAX_MATCHES);
        if max_results == 0 {
            return Err(ToolError::InvalidArguments(
                "max_results must be greater than zero".into(),
            ));
        }
        let max_results = max_results.min(MAX_MATCHES);
        let scope = self.workspace.resolve_scope(args.path.as_deref())?;
        let scope_label = args
            .path
            .as_deref()
            .filter(|path| !path.is_empty())
            .unwrap_or(".");
        let mut result = SearchResult {
            query: args.query,
            regex: regex_mode,
            scope: scope_label.to_owned(),
            files_scanned: 0,
            bytes_scanned: 0,
            matches: Vec::new(),
            truncated: false,
            skipped_large_files: 0,
            skipped_binary_files: 0,
            skipped_non_utf8_files: 0,
            errors: Vec::new(),
            omitted_errors: 0,
        };

        let mut walker = WalkBuilder::new(&scope);
        walker
            .hidden(!args.include_hidden.unwrap_or(false))
            .follow_links(false)
            .require_git(false);

        let mut entries_seen = 0;
        'walk: for entry in walker.build() {
            entries_seen += 1;
            if entries_seen > MAX_WALK_ENTRIES {
                result.truncated = true;
                break;
            }
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    push_issue(&mut result, self.workspace.root(), None, error.to_string());
                    continue;
                }
            };
            if !is_searchable_file(&entry) {
                continue;
            }

            let path = entry.path();
            if !path.starts_with(self.workspace.root()) {
                push_issue(
                    &mut result,
                    self.workspace.root(),
                    Some(path),
                    "walker returned a path outside the workspace".into(),
                );
                continue;
            }

            let metadata = match tokio::fs::symlink_metadata(path).await {
                Ok(metadata) => metadata,
                Err(error) => {
                    push_issue(
                        &mut result,
                        self.workspace.root(),
                        Some(path),
                        error.to_string(),
                    );
                    continue;
                }
            };
            if metadata.file_type().is_symlink() {
                continue;
            }
            if !metadata.is_file() {
                continue;
            }
            if metadata.len() > MAX_FILE_BYTES as u64 {
                result.skipped_large_files += 1;
                continue;
            }

            let remaining_bytes = MAX_TOTAL_BYTES.saturating_sub(result.bytes_scanned);
            if remaining_bytes == 0 {
                result.truncated = true;
                break;
            }
            if metadata.len() > remaining_bytes as u64 {
                result.truncated = true;
                break;
            }

            result.files_scanned += 1;
            let bytes_to_read = remaining_bytes.min(MAX_FILE_BYTES);
            let mut reader = match tokio::fs::File::open(path).await {
                Ok(file) => file.take((bytes_to_read + 1) as u64),
                Err(error) => {
                    push_issue(
                        &mut result,
                        self.workspace.root(),
                        Some(path),
                        error.to_string(),
                    );
                    continue;
                }
            };
            let mut bytes = Vec::new();
            if let Err(error) = reader.read_to_end(&mut bytes).await {
                push_issue(
                    &mut result,
                    self.workspace.root(),
                    Some(path),
                    error.to_string(),
                );
                continue;
            }
            if bytes.len() > MAX_FILE_BYTES {
                result.skipped_large_files += 1;
                continue;
            }
            if bytes.len() > bytes_to_read {
                result.truncated = true;
                break;
            }
            result.bytes_scanned += bytes.len();

            if bytes.contains(&0) {
                result.skipped_binary_files += 1;
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                result.skipped_non_utf8_files += 1;
                continue;
            };

            let relative_path = path
                .strip_prefix(self.workspace.root())
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned();
            for (index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    if result.matches.len() == max_results {
                        result.truncated = true;
                        break 'walk;
                    }
                    let (display_text, line_truncated) = truncate_line(line);
                    result.matches.push(SearchMatch {
                        path: relative_path.clone(),
                        line: index + 1,
                        text: display_text,
                        line_truncated,
                    });
                }
            }
        }

        let json = serde_json::to_string(&result).map_err(|error| {
            ToolError::Execution(format!("cannot encode search result: {error}"))
        })?;
        Ok(ToolOutput::text(json))
    }
}

fn is_searchable_file(entry: &DirEntry) -> bool {
    entry
        .file_type()
        .is_some_and(|file_type| file_type.is_file())
}

fn truncate_line(line: &str) -> (String, bool) {
    let mut chars = line.chars();
    let text = chars.by_ref().take(MAX_MATCH_LINE_CHARS).collect();
    (text, chars.next().is_some())
}

fn push_issue(
    result: &mut SearchResult,
    workspace_root: &Path,
    path: Option<&Path>,
    message: String,
) {
    if result.errors.len() == MAX_ISSUE_DETAILS {
        result.omitted_errors += 1;
        return;
    }

    result.errors.push(SearchIssue {
        path: path.map(|path| {
            path.strip_prefix(workspace_root)
                .unwrap_or(path)
                .to_string_lossy()
                .into_owned()
        }),
        message,
    });
}
