use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use uno_tools_core::{Approval, ToolError, ToolOutput, TypedTool};

/// Host-provided backend for an interactive browser session.
#[async_trait]
pub trait BrowserBackend: Send + Sync {
    async fn navigate(&self, url: &str) -> Result<String, String>;
    async fn snapshot(&self, include_images: bool) -> Result<String, String>;
    async fn network_log(&self, clear: bool, include_preview: bool) -> Result<String, String>;
    async fn execute_script(&self, script: &str) -> Result<String, String>;
    async fn cookies(&self, url: Option<&str>, include_values: bool) -> Result<String, String>;
    async fn click(&self, ref_id: &str) -> Result<String, String>;
    async fn type_text(&self, ref_id: &str, text: &str, submit: bool) -> Result<String, String>;
    async fn scroll(&self, delta_y: i32) -> Result<String, String>;
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserNavigateArgs {
    #[schemars(
        description = "Target http:// or https:// URL to open in the host-provided browser."
    )]
    pub url: String,
}

pub struct BrowserNavigate {
    backend: Arc<dyn BrowserBackend>,
}

impl BrowserNavigate {
    pub fn new(backend: Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl TypedTool for BrowserNavigate {
    type Args = BrowserNavigateArgs;

    fn name(&self) -> &str {
        "browser_navigate"
    }

    fn description(&self) -> &str {
        "Open an http/https URL in the embedded browser and return a compact DOM snapshot."
    }

    fn approval(&self, args: &Self::Args) -> Approval {
        Approval::Ask {
            reason: format!("Navigate browser to {}", args.url),
        }
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let result = self
            .backend
            .navigate(&args.url)
            .await
            .map_err(ToolError::Execution)?;
        Ok(ToolOutput::text(result))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserSnapshotArgs {
    #[schemars(
        description = "Include visible image URLs/sizes in the snapshot (defaults to false to save tokens)."
    )]
    pub include_images: Option<bool>,
}

pub struct BrowserSnapshot {
    backend: Arc<dyn BrowserBackend>,
}

impl BrowserSnapshot {
    pub fn new(backend: Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl TypedTool for BrowserSnapshot {
    type Args = BrowserSnapshotArgs;

    fn name(&self) -> &str {
        "browser_snapshot"
    }

    fn description(&self) -> &str {
        "Capture a compact DOM snapshot (URL, title, element refs e1..eN, text) of the current page."
    }

    fn approval(&self, _args: &Self::Args) -> Approval {
        Approval::Never
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let json = self
            .backend
            .snapshot(args.include_images.unwrap_or(false))
            .await
            .map_err(ToolError::Execution)?;
        Ok(ToolOutput::text(json))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserNetworkArgs {
    #[schemars(
        description = "If true, clear captured network/console entries after reading them."
    )]
    pub clear: Option<bool>,
    #[schemars(
        description = "Include bounded request/response body previews and diagnostic headers (defaults to false; may contain sensitive data and requires approval)."
    )]
    pub include_preview: Option<bool>,
}

pub struct BrowserNetwork {
    backend: Arc<dyn BrowserBackend>,
}

impl BrowserNetwork {
    pub fn new(backend: Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl TypedTool for BrowserNetwork {
    type Args = BrowserNetworkArgs;

    fn name(&self) -> &str {
        "browser_network"
    }

    fn description(&self) -> &str {
        "Inspect captured HTTP/SSE/WebSocket traffic and console errors from the browser."
    }

    fn approval(&self, args: &Self::Args) -> Approval {
        if args.include_preview.unwrap_or(false) {
            Approval::Ask {
                reason:
                    "Read request/response previews and diagnostic headers from browser network log"
                        .into(),
            }
        } else {
            Approval::Never
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let json = self
            .backend
            .network_log(
                args.clear.unwrap_or(false),
                args.include_preview.unwrap_or(false),
            )
            .await
            .map_err(ToolError::Execution)?;
        Ok(ToolOutput::text(json))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserExecuteScriptArgs {
    #[schemars(
        description = "JavaScript statements to run in the current browser page. Return a JSON-serializable value to get a result; maximum 16 KiB."
    )]
    pub script: String,
}

pub struct BrowserExecuteScript {
    backend: Arc<dyn BrowserBackend>,
}

impl BrowserExecuteScript {
    pub fn new(backend: Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl TypedTool for BrowserExecuteScript {
    type Args = BrowserExecuteScriptArgs;

    fn name(&self) -> &str {
        "browser_execute_script"
    }

    fn description(&self) -> &str {
        "Run JavaScript in the current browser page and return a bounded JSON result."
    }

    fn approval(&self, _args: &Self::Args) -> Approval {
        Approval::Ask {
            reason: "Execute JavaScript in the browser page; it can inspect or modify page state"
                .into(),
        }
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        if args.script.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "script must not be empty".into(),
            ));
        }
        if args.script.len() > 16 * 1024 {
            return Err(ToolError::InvalidArguments(
                "script exceeds the 16 KiB limit".into(),
            ));
        }
        let result = self
            .backend
            .execute_script(&args.script)
            .await
            .map_err(ToolError::Execution)?;
        Ok(ToolOutput::text(format!(
            "[UNTRUSTED PAGE SCRIPT RESULT]\n{result}"
        )))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserCookiesArgs {
    #[schemars(
        description = "Optional http/https URL to filter cookies for; if omitted, returns cookies across the browser profile."
    )]
    pub url: Option<String>,
    #[schemars(
        description = "Include cookie values (defaults to false; returns only cookie names and attributes unless explicitly requested)."
    )]
    pub include_values: Option<bool>,
}

pub struct BrowserCookies {
    backend: Arc<dyn BrowserBackend>,
}

impl BrowserCookies {
    pub fn new(backend: Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl TypedTool for BrowserCookies {
    type Args = BrowserCookiesArgs;

    fn name(&self) -> &str {
        "browser_cookies"
    }

    fn description(&self) -> &str {
        "Inspect browser cookies (names/attributes; values only when include_values=true)."
    }

    fn approval(&self, args: &Self::Args) -> Approval {
        if args.include_values.unwrap_or(false) {
            Approval::Ask {
                reason: "Read raw cookie values (including HttpOnly) from browser".into(),
            }
        } else {
            Approval::Never
        }
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let json = self
            .backend
            .cookies(args.url.as_deref(), args.include_values.unwrap_or(false))
            .await
            .map_err(ToolError::Execution)?;
        Ok(ToolOutput::text(json))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum BrowserActionKind {
    Click,
    Type,
    Scroll,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BrowserActionArgs {
    #[schemars(description = "Action to perform: 'click', 'type', or 'scroll'.")]
    pub action: BrowserActionKind,
    #[schemars(description = "Element reference id from snapshot (e.g. 'e1') for click/type.")]
    pub ref_id: Option<String>,
    #[schemars(description = "Text to type for 'type' action.")]
    pub text: Option<String>,
    #[schemars(
        description = "For 'type': press Enter / submit the form immediately (saves an extra click step)."
    )]
    pub submit: Option<bool>,
    #[schemars(
        description = "Return an updated compact DOM snapshot after the action (defaults to true for click/type)."
    )]
    pub return_snapshot: Option<bool>,
    #[schemars(
        description = "Vertical scroll delta in pixels for 'scroll' action (positive = down, negative = up)."
    )]
    pub delta_y: Option<i32>,
}

pub struct BrowserAction {
    backend: Arc<dyn BrowserBackend>,
}

impl BrowserAction {
    pub fn new(backend: Arc<dyn BrowserBackend>) -> Self {
        Self { backend }
    }
}

#[async_trait]
impl TypedTool for BrowserAction {
    type Args = BrowserActionArgs;

    fn name(&self) -> &str {
        "browser_action"
    }

    fn description(&self) -> &str {
        "Interact with the browser page: click/type by ref_id, or scroll vertically."
    }

    fn approval(&self, _args: &Self::Args) -> Approval {
        Approval::Ask {
            reason: "Perform interaction inside browser page".into(),
        }
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn run(&self, args: Self::Args) -> Result<ToolOutput, ToolError> {
        let (msg, default_snap, settle_ms) = match args.action {
            BrowserActionKind::Click => {
                let ref_id = args.ref_id.ok_or_else(|| {
                    ToolError::InvalidArguments("ref_id is required for click".into())
                })?;
                let res = self
                    .backend
                    .click(&ref_id)
                    .await
                    .map_err(ToolError::Execution)?;
                (res, true, 220)
            }
            BrowserActionKind::Type => {
                let ref_id = args.ref_id.ok_or_else(|| {
                    ToolError::InvalidArguments("ref_id is required for type".into())
                })?;
                let text = args.text.ok_or_else(|| {
                    ToolError::InvalidArguments("text is required for type".into())
                })?;
                let submit = args.submit.unwrap_or(false);
                let res = self
                    .backend
                    .type_text(&ref_id, &text, submit)
                    .await
                    .map_err(ToolError::Execution)?;
                (res, submit, if submit { 450 } else { 120 })
            }
            BrowserActionKind::Scroll => {
                let delta_y = args.delta_y.unwrap_or(400);
                let res = self
                    .backend
                    .scroll(delta_y)
                    .await
                    .map_err(ToolError::Execution)?;
                (res, false, 200)
            }
        };

        if args.return_snapshot.unwrap_or(default_snap) {
            tokio::time::sleep(std::time::Duration::from_millis(settle_ms)).await;
            if let Ok(snap) = self.backend.snapshot(false).await {
                return Ok(ToolOutput::text(format!("{msg}\n\n{snap}")));
            }
        }
        Ok(ToolOutput::text(msg))
    }
}
