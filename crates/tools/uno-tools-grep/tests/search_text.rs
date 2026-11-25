use std::fs;

use uno_tools_core::{Approval, ToolError, ToolRegistry, Typed, TypedTool};
use uno_tools_grep::{
    MAX_MATCH_LINE_CHARS, MAX_MATCHES, MAX_QUERY_BYTES, SearchText, SearchTextArgs,
};

fn args(query: &str) -> SearchTextArgs {
    SearchTextArgs {
        query: query.into(),
        path: None,
        regex: None,
        case_sensitive: None,
        include_hidden: None,
        max_results: None,
    }
}

#[tokio::test]
async fn returns_structured_literal_matches_with_workspace_relative_paths() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::create_dir(workspace.path().join("src")).expect("create src directory");
    fs::write(
        workspace.path().join("src/lib.rs"),
        "fn main() {}\nlet value = 42;\n",
    )
    .expect("write source");
    let tool = SearchText::new(workspace.path()).expect("create search tool");

    let output = tool.run(args("VALUE")).await.expect("search source");
    let result: serde_json::Value =
        serde_json::from_str(&output.content).expect("parse JSON result");

    assert_eq!(result["query"], "VALUE");
    assert_eq!(result["regex"], false);
    assert_eq!(result["files_scanned"], 1);
    assert_eq!(
        result["matches"][0]["path"]
            .as_str()
            .unwrap()
            .replace('\\', "/"),
        "src/lib.rs"
    );
    assert_eq!(result["matches"][0]["line"], 2);
    assert_eq!(result["matches"][0]["text"], "let value = 42;");
    assert_eq!(result["truncated"], false);
}

#[tokio::test]
async fn supports_regex_and_case_sensitive_matching() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(workspace.path().join("words.txt"), "Hello\nhello\n").expect("write fixture");
    let tool = SearchText::new(workspace.path()).expect("create search tool");
    let mut search = args("^hello$");
    search.regex = Some(true);
    search.case_sensitive = Some(true);

    let output = tool.run(search).await.expect("search words");
    let result: serde_json::Value =
        serde_json::from_str(&output.content).expect("parse JSON result");

    assert_eq!(result["matches"].as_array().expect("matches").len(), 1);
    assert_eq!(result["matches"][0]["line"], 2);
}

#[tokio::test]
async fn respects_gitignore_and_hides_dotfiles_by_default() {
    let workspace = tempfile::tempdir().expect("create workspace");
    fs::write(workspace.path().join(".gitignore"), "ignored/\n").expect("write ignore rules");
    fs::create_dir(workspace.path().join("ignored")).expect("create ignored directory");
    fs::write(workspace.path().join("ignored/secret.txt"), "needle").expect("write ignored file");
    fs::write(workspace.path().join(".hidden.txt"), "needle").expect("write hidden file");
    fs::write(workspace.path().join("visible.txt"), "needle").expect("write visible file");
    let tool = SearchText::new(workspace.path()).expect("create search tool");

    let output = tool.run(args("needle")).await.expect("search workspace");
    let result: serde_json::Value =
        serde_json::from_str(&output.content).expect("parse JSON result");
    let paths = result["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|item| item["path"].as_str().expect("path"))
        .collect::<Vec<_>>();

    assert_eq!(paths, ["visible.txt"]);
}

#[cfg(unix)]
#[tokio::test]
async fn can_include_hidden_files_without_following_symlinks() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().expect("create parent");
    let workspace_path = parent.path().join("workspace");
    let outside = parent.path().join("outside");
    fs::create_dir(&workspace_path).expect("create workspace");
    fs::create_dir(&outside).expect("create outside directory");
    fs::write(outside.join("external.txt"), "needle").expect("write outside file");
    fs::write(workspace_path.join(".hidden.txt"), "needle").expect("write hidden file");
    symlink(&outside, workspace_path.join("linked")).expect("create symlink");
    let tool = SearchText::new(&workspace_path).expect("create search tool");
    let mut search = args("needle");
    search.include_hidden = Some(true);

    let output = tool.run(search).await.expect("search workspace");
    let result: serde_json::Value =
        serde_json::from_str(&output.content).expect("parse JSON result");
    let paths = result["matches"]
        .as_array()
        .expect("matches")
        .iter()
        .map(|item| item["path"].as_str().expect("path"))
        .collect::<Vec<_>>();

    assert_eq!(paths, [".hidden.txt"]);
}

#[tokio::test]
async fn bounds_match_count_and_line_length() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let matching_line = format!("needle{}", "x".repeat(MAX_MATCH_LINE_CHARS));
    let content = std::iter::repeat_n(matching_line, MAX_MATCHES + 1)
        .collect::<Vec<_>>()
        .join("\n");
    fs::write(workspace.path().join("large-lines.txt"), content).expect("write fixture");
    let tool = SearchText::new(workspace.path()).expect("create search tool");
    let mut search = args("needle");
    search.max_results = Some(MAX_MATCHES + 1);

    let output = tool.run(search).await.expect("search matching file");
    let result: serde_json::Value =
        serde_json::from_str(&output.content).expect("parse JSON result");
    let matches = result["matches"].as_array().expect("matches");

    assert_eq!(matches.len(), MAX_MATCHES);
    assert_eq!(
        matches[0]["text"].as_str().expect("line").chars().count(),
        MAX_MATCH_LINE_CHARS
    );
    assert_eq!(matches[0]["line_truncated"], true);
    assert_eq!(result["truncated"], true);
}

#[tokio::test]
async fn rejects_invalid_scope_query_and_result_limit() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let tool = SearchText::new(workspace.path()).expect("create search tool");

    let mut traversal = args("needle");
    traversal.path = Some("../outside".into());
    assert!(matches!(
        tool.run(traversal).await,
        Err(ToolError::InvalidArguments(_))
    ));

    let empty = args(" ");
    assert!(matches!(
        tool.run(empty).await,
        Err(ToolError::InvalidArguments(_))
    ));

    let mut bad_regex = args("[");
    bad_regex.regex = Some(true);
    assert!(matches!(
        tool.run(bad_regex).await,
        Err(ToolError::InvalidArguments(_))
    ));

    let mut no_results = args("needle");
    no_results.max_results = Some(0);
    assert!(matches!(
        tool.run(no_results).await,
        Err(ToolError::InvalidArguments(_))
    ));

    let large_query = args(&"q".repeat(MAX_QUERY_BYTES + 1));
    assert!(matches!(
        tool.run(large_query).await,
        Err(ToolError::InvalidArguments(_))
    ));

    let mut empty = args("needle");
    empty.path = Some(String::new());
    empty.include_hidden = Some(true);
    assert!(tool.run(empty).await.is_ok());
}

#[test]
fn registers_as_a_read_only_tool() {
    let workspace = tempfile::tempdir().expect("create workspace");
    let mut registry = ToolRegistry::new();
    registry
        .register(Typed(
            SearchText::new(workspace.path()).expect("create search tool"),
        ))
        .expect("register search tool");

    assert_eq!(registry.definitions()[0].name, "search_text");
    assert!(registry.is_read_only("search_text"));
    assert_eq!(
        registry
            .approval("search_text", &serde_json::json!({"query": "needle"}))
            .expect("approval policy"),
        Approval::Never
    );
}
