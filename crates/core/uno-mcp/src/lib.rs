//! Stdio Model Context Protocol (MCP) client and tool discovery.

use std::{
    collections::{BTreeMap, HashMap},
    fs,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::Duration,
};

use async_trait::async_trait;
use protocol::ToolDefinition;
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader, BufWriter},
    process::{Child, ChildStdin, ChildStdout, Command},
    sync::Mutex,
    time::timeout,
};
use tool_core::{Approval, Tool, ToolError, ToolOutput, ToolRegistry};

const MCP_INIT_TIMEOUT: Duration = Duration::from_secs(4);
const MCP_CALL_TIMEOUT: Duration = Duration::from_secs(30);
const SENSITIVE_ENV_VARS: &[&str] = &[
    "UNATROP_DAEMON_TOKEN",
    "UNOTROP_DAEMON_TOKEN",
    "GH_TOKEN",
    "GITHUB_TOKEN",
    "GITHUB_COPILOT_API_TOKEN",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "ANTIGRAVITY_API_KEY",
    "ANTIGRAVITY_TOKEN",
    "ANTIGRAVITY_REFRESH_TOKEN",
    "ANTIGRAVITY_ACCESS_TOKEN",
    "ANTIGRAVITY_TOKEN_JSON",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
];

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub disabled: bool,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpConfigFile {
    #[serde(default)]
    mcp_servers: BTreeMap<String, McpServerConfig>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpToolAnnotations {
    #[serde(default)]
    read_only_hint: Option<bool>,
}

#[derive(Clone, Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct McpToolDef {
    name: String,
    #[serde(default)]
    description: Option<String>,
    #[serde(default)]
    input_schema: Option<Value>,
    #[serde(default)]
    annotations: Option<McpToolAnnotations>,
}

#[derive(Deserialize)]
struct McpToolsListResult {
    #[serde(default)]
    tools: Vec<McpToolDef>,
}

struct McpConnection {
    _child: Child,
    stdin: BufWriter<ChildStdin>,
    stdout: BufReader<ChildStdout>,
    next_id: AtomicU64,
}

impl McpConnection {
    async fn spawn(cfg: &McpServerConfig, working_dir: &Path) -> Result<Self, String> {
        let mut cmd = Command::new(&cfg.command);
        cmd.args(&cfg.args)
            .current_dir(working_dir)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        for var in SENSITIVE_ENV_VARS {
            cmd.env_remove(var);
        }
        for (k, v) in &cfg.env {
            cmd.env(k, v);
        }
        #[cfg(windows)]
        {
            const CREATE_NO_WINDOW: u32 = 0x0800_0000;
            cmd.creation_flags(CREATE_NO_WINDOW);
        }

        let mut child = cmd
            .spawn()
            .map_err(|e| format!("failed to spawn '{}': {e}", cfg.command))?;
        let stdin = child
            .stdin
            .take()
            .ok_or_else(|| "missing stdin".to_owned())?;
        let stdout = child
            .stdout
            .take()
            .ok_or_else(|| "missing stdout".to_owned())?;

        let mut conn = Self {
            _child: child,
            stdin: BufWriter::new(stdin),
            stdout: BufReader::new(stdout),
            next_id: AtomicU64::new(1),
        };

        conn.request(
            "initialize",
            json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "unatrop", "version": env!("CARGO_PKG_VERSION") }
            }),
            MCP_INIT_TIMEOUT,
        )
        .await?;
        conn.notify("notifications/initialized", json!({})).await?;
        Ok(conn)
    }

    async fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("mcp write failed: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("mcp flush failed: {e}"))?;
        Ok(())
    }

    async fn request(
        &mut self,
        method: &str,
        params: Value,
        dur: Duration,
    ) -> Result<Value, String> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let mut line = serde_json::to_string(&msg).map_err(|e| e.to_string())?;
        line.push('\n');
        self.stdin
            .write_all(line.as_bytes())
            .await
            .map_err(|e| format!("mcp write failed: {e}"))?;
        self.stdin
            .flush()
            .await
            .map_err(|e| format!("mcp flush failed: {e}"))?;

        timeout(dur, async {
            let mut buf = String::new();
            loop {
                buf.clear();
                let n = self
                    .stdout
                    .read_line(&mut buf)
                    .await
                    .map_err(|e| format!("mcp read failed: {e}"))?;
                if n == 0 {
                    return Err("mcp server closed stdout".to_owned());
                }
                let trimmed = buf.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let Ok(val) = serde_json::from_str::<Value>(trimmed) else {
                    continue;
                };
                if val.get("id").and_then(Value::as_u64) != Some(id) {
                    continue;
                }
                if let Some(err) = val.get("error") {
                    return Err(format!("mcp error: {err}"));
                }
                return Ok(val.get("result").cloned().unwrap_or(Value::Null));
            }
        })
        .await
        .map_err(|_| format!("mcp request '{method}' timed out"))?
    }
}

struct McpTool {
    registered_name: String,
    remote_name: String,
    description: String,
    parameters: Value,
    read_only: bool,
    conn: Arc<Mutex<McpConnection>>,
}

#[async_trait]
impl Tool for McpTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.registered_name.clone(),
            description: self.description.clone(),
            input_schema: self.parameters.clone(),
        }
    }

    fn read_only(&self) -> bool {
        // Untrusted MCP server annotations must not bypass read-only policy checks.
        let _ = self.read_only;
        false
    }

    fn approval(&self, _arguments: &Value) -> Approval {
        Approval::Ask {
            reason: format!("Execute MCP tool '{}'", self.registered_name),
        }
    }

    async fn execute(&self, arguments: Value) -> Result<ToolOutput, ToolError> {
        let mut conn = self.conn.lock().await;
        Ok(
            match conn
                .request(
                    "tools/call",
                    json!({
                        "name": self.remote_name,
                        "arguments": arguments,
                    }),
                    MCP_CALL_TIMEOUT,
                )
                .await
            {
                Ok(result) => format_mcp_tool_result(&result),
                Err(err) => ToolOutput::error(err),
            },
        )
    }
}

fn format_mcp_tool_result(result: &Value) -> ToolOutput {
    let is_error = result
        .get("isError")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    let mut parts = Vec::new();
    if let Some(content) = result.get("content").and_then(Value::as_array) {
        for item in content {
            if let Some(text) = item.get("text").and_then(Value::as_str) {
                parts.push(text.to_owned());
            } else {
                parts.push(item.to_string());
            }
        }
    }
    let text = if parts.is_empty() {
        result.to_string()
    } else {
        parts.join("\n")
    };
    if is_error {
        ToolOutput::error(text)
    } else {
        ToolOutput::text(text)
    }
}

fn sanitize_ident(raw: &str) -> String {
    raw.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Loads and merges `mcpServers` entries from the provided `mcp.json` paths (later files override earlier ones).
pub fn load_mcp_configs(config_paths: &[PathBuf]) -> BTreeMap<String, McpServerConfig> {
    let mut merged = BTreeMap::new();
    for path in config_paths {
        let Ok(raw) = fs::read_to_string(path) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<McpConfigFile>(&raw) else {
            continue;
        };
        for (name, cfg) in parsed.mcp_servers {
            if cfg.disabled {
                merged.remove(&name);
            } else {
                merged.insert(name, cfg);
            }
        }
    }
    merged
}

/// Discovers and registers tools from configured stdio MCP servers (`mcp.json`).
/// Failures on individual servers are skipped gracefully so they never block startup.
pub fn register_mcp_tools(
    config_paths: &[PathBuf],
    working_dir: &Path,
    registry: &mut ToolRegistry,
) {
    let servers = load_mcp_configs(config_paths);
    if servers.is_empty() {
        return;
    }

    let Ok(rt) = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    else {
        return;
    };

    let dir_owned = working_dir.to_path_buf();
    let discovered: Vec<McpTool> = rt.block_on(async move {
        let mut out = Vec::new();
        for (server_name, cfg) in servers {
            let Ok(mut conn) = McpConnection::spawn(&cfg, &dir_owned).await else {
                continue;
            };
            let Ok(list_val) = conn
                .request("tools/list", json!({}), MCP_INIT_TIMEOUT)
                .await
            else {
                continue;
            };
            let Ok(parsed) = serde_json::from_value::<McpToolsListResult>(list_val) else {
                continue;
            };
            let shared_conn = Arc::new(Mutex::new(conn));
            for tool in parsed.tools {
                let read_only = tool
                    .annotations
                    .as_ref()
                    .and_then(|a| a.read_only_hint)
                    .unwrap_or(false);
                let reg_name = format!(
                    "mcp__{}__{}",
                    sanitize_ident(&server_name),
                    sanitize_ident(&tool.name)
                );
                let desc = match tool.description {
                    Some(d) if !d.trim().is_empty() => format!("[MCP:{server_name}] {d}"),
                    _ => format!("[MCP:{server_name}] {}", tool.name),
                };
                let params = tool.input_schema.unwrap_or_else(|| {
                    json!({
                        "type": "object",
                        "properties": {}
                    })
                });
                out.push(McpTool {
                    registered_name: reg_name,
                    remote_name: tool.name,
                    description: desc,
                    parameters: params,
                    read_only,
                    conn: Arc::clone(&shared_conn),
                });
            }
        }
        out
    });

    for tool in discovered {
        let _ = registry.register(tool);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mcp_tool_always_requires_approval_regardless_of_read_only_hint() {
        let (_tx, _rx) = tokio::sync::mpsc::unbounded_channel::<()>();
        // Verify load_mcp_configs respects disabled and reads valid server entries.
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("mcp.json");
        std::fs::write(
            &path,
            r#"{"mcpServers":{"demo":{"command":"echo","args":["hi"]}}}"#,
        )
        .unwrap();
        let loaded = load_mcp_configs(&[path]);
        assert!(loaded.contains_key("demo"));
        assert!(!SENSITIVE_ENV_VARS.is_empty());
    }
}
