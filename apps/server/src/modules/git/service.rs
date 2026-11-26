//! Git diff inspection and validated file revert service for session workspaces.

use std::path::{Component, Path};

use uno_daemon_protocol::pb;

use crate::app::DaemonError;

pub(super) fn read_turn_diff(ws: &Path) -> Result<pb::TurnDiffResponse, DaemonError> {
    let Ok(probe) = std::process::Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(ws)
        .output()
    else {
        return Ok(pb::TurnDiffResponse {
            is_git_repo: false,
            diff: String::new(),
            changed_files: Vec::new(),
        });
    };
    if !probe.status.success() {
        return Ok(pb::TurnDiffResponse {
            is_git_repo: false,
            diff: String::new(),
            changed_files: Vec::new(),
        });
    }

    let status_out = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(ws)
        .output()
        .map_err(|e| DaemonError::internal(e.to_string()))?;
    let changed_files = String::from_utf8_lossy(&status_out.stdout)
        .lines()
        .filter_map(|line| line.get(3..).map(str::trim))
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned)
        .collect();

    let diff_out = std::process::Command::new("git")
        .args(["diff", "HEAD"])
        .current_dir(ws)
        .output()
        .map_err(|e| DaemonError::internal(e.to_string()))?;

    Ok(pb::TurnDiffResponse {
        is_git_repo: true,
        diff: String::from_utf8_lossy(&diff_out.stdout).into_owned(),
        changed_files,
    })
}

fn validate_revert_path(raw: &str) -> Result<(), DaemonError> {
    if raw.is_empty()
        || raw.chars().any(char::is_control)
        || raw.starts_with('-')
        || raw.starts_with(':')
    {
        return Err(DaemonError::bad_request(format!(
            "invalid revert path: {raw}"
        )));
    }
    let path = Path::new(raw);
    if path.is_absolute() {
        return Err(DaemonError::bad_request(format!(
            "revert path must be relative: {raw}"
        )));
    }
    for comp in path.components() {
        match comp {
            Component::Normal(_) | Component::CurDir => {}
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(DaemonError::bad_request(format!(
                    "revert path escapes workspace: {raw}"
                )));
            }
        }
    }
    Ok(())
}

pub(super) fn revert_files(
    ws: &Path,
    files: Vec<String>,
) -> Result<pb::RevertResponse, DaemonError> {
    if files.is_empty() {
        return Err(DaemonError::bad_request(
            "files must not be empty; specify explicit relative paths to revert",
        ));
    }
    for f in &files {
        validate_revert_path(f)?;
    }
    let mut cmd = std::process::Command::new("git");
    cmd.arg("--literal-pathspecs").arg("checkout").arg("--");
    for f in &files {
        cmd.arg(f);
    }
    let out = cmd
        .current_dir(ws)
        .output()
        .map_err(|e| DaemonError::internal(e.to_string()))?;
    if !out.status.success() {
        return Err(DaemonError::bad_request(
            String::from_utf8_lossy(&out.stderr).into_owned(),
        ));
    }
    Ok(pb::RevertResponse {
        reverted_files: files,
    })
}
