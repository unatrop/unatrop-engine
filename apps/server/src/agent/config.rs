//! Default config, skills, and MCP discovery for `unatropd`.

use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
};

use serde::Deserialize;

#[derive(Clone, Debug, Default, Deserialize)]
pub struct DaemonFileConfig {
    #[serde(default)]
    pub allow_tools: Vec<String>,
    #[serde(default)]
    pub allow_commands: Vec<String>,
}

pub fn resolve_unatrop_home() -> Option<PathBuf> {
    if let Some(explicit) = std::env::var_os("UNATROP_HOME") {
        let p = PathBuf::from(explicit);
        if !p.as_os_str().is_empty() {
            return Some(p);
        }
    }
    if let Some(home) = std::env::var_os("USERPROFILE").or_else(|| std::env::var_os("HOME")) {
        return Some(PathBuf::from(home).join(".unatrop"));
    }
    None
}

fn env_truthy(name: &str) -> bool {
    std::env::var(name)
        .ok()
        .is_some_and(|v| matches!(v.trim().to_ascii_lowercase().as_str(), "1" | "true" | "yes"))
}

fn trust_workspace_config() -> bool {
    env_truthy("UNATROP_TRUST_WORKSPACE_CONFIG")
}

#[cfg(feature = "host-tools")]
fn trust_workspace_mcp() -> bool {
    trust_workspace_config() || env_truthy("UNATROP_TRUST_WORKSPACE_MCP")
}

/// Loads or creates a per-user local authentication token at `$UNATROP_HOME/config/daemon.token`.
pub fn load_or_create_local_daemon_token() -> Option<String> {
    let home = resolve_unatrop_home()?;
    let config_dir = home.join("config");
    if std::fs::create_dir_all(&config_dir).is_err() {
        return None;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&config_dir, std::fs::Permissions::from_mode(0o700));
    }

    let token_path = config_dir.join("daemon.token");
    if let Ok(existing) = std::fs::read_to_string(&token_path) {
        let trimmed = existing.trim();
        if !trimmed.is_empty() {
            return Some(trimmed.to_owned());
        }
    }

    let generated = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        uuid::Uuid::new_v4().simple()
    );
    match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&token_path)
    {
        Ok(mut file) => {
            use std::io::Write as _;
            let _ = file.write_all(generated.as_bytes());
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ =
                    std::fs::set_permissions(&token_path, std::fs::Permissions::from_mode(0o600));
            }
            Some(generated)
        }
        Err(_) => std::fs::read_to_string(&token_path)
            .ok()
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty()),
    }
}

pub fn load_daemon_file_config(workspace: &Path) -> DaemonFileConfig {
    let mut candidates = Vec::new();
    if let Some(home) = resolve_unatrop_home() {
        candidates.push(home.join("config").join("daemon.json"));
    }
    if trust_workspace_config() {
        candidates.push(workspace.join(".unatrop").join("daemon.json"));
    } else if workspace.join(".unatrop").join("daemon.json").is_file() {
        tracing::warn!(
            workspace = %workspace.display(),
            "ignoring untrusted workspace .unatrop/daemon.json (set UNATROP_TRUST_WORKSPACE_CONFIG=1 to opt in)"
        );
    }

    let mut merged = DaemonFileConfig::default();
    for path in candidates {
        if let Ok(raw) = std::fs::read_to_string(&path)
            && let Ok(parsed) = serde_json::from_str::<DaemonFileConfig>(&raw)
        {
            merged.allow_tools.extend(parsed.allow_tools);
            merged.allow_commands.extend(parsed.allow_commands);
        }
    }
    merged
}

pub fn merge_skill_dirs(explicit: Vec<String>, workspace: &Path) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |p: PathBuf| {
        if seen.insert(p.clone()) {
            out.push(p);
        }
    };

    for raw in explicit {
        if !raw.trim().is_empty() {
            push(PathBuf::from(raw));
        }
    }
    if let Some(home) = resolve_unatrop_home() {
        push(home.join("skills"));
    }
    push(workspace.join(".unatrop").join("skills"));
    push(workspace.join(".agents").join("skills"));
    out
}

#[cfg(feature = "host-tools")]
pub fn merge_mcp_configs(explicit: Vec<String>, workspace: &Path) -> Vec<PathBuf> {
    let mut seen = BTreeSet::new();
    let mut out = Vec::new();
    let mut push = |p: PathBuf| {
        if seen.insert(p.clone()) {
            out.push(p);
        }
    };

    for raw in explicit {
        if !raw.trim().is_empty() {
            push(PathBuf::from(raw));
        }
    }
    if let Some(home) = resolve_unatrop_home() {
        push(home.join("config").join("mcp.json"));
    }
    if trust_workspace_mcp() {
        push(workspace.join(".unatrop").join("mcp.json"));
        push(workspace.join(".mcp.json"));
    } else if workspace.join(".unatrop").join("mcp.json").is_file()
        || workspace.join(".mcp.json").is_file()
    {
        tracing::warn!(
            workspace = %workspace.display(),
            "ignoring untrusted workspace MCP config (set UNATROP_TRUST_WORKSPACE_MCP=1 to opt in)"
        );
    }
    out
}
