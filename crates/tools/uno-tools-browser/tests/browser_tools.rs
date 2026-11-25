use std::sync::Arc;

use async_trait::async_trait;
use uno_tools_browser::{
    BrowserAction, BrowserActionArgs, BrowserActionKind, BrowserBackend, BrowserCookies,
    BrowserCookiesArgs, BrowserExecuteScript, BrowserExecuteScriptArgs, BrowserNavigate,
    BrowserNavigateArgs, BrowserNetwork, BrowserNetworkArgs, BrowserSnapshot, BrowserSnapshotArgs,
};
use uno_tools_core::{Approval, ToolError, TypedTool};

struct MockBrowser;

#[async_trait]
impl BrowserBackend for MockBrowser {
    async fn navigate(&self, url: &str) -> Result<String, String> {
        Ok(format!("navigated:{url}"))
    }
    async fn snapshot(&self, _include_images: bool) -> Result<String, String> {
        Ok("URL: https://example.com\nTitle: Ex\nText:\nHi".into())
    }
    async fn network_log(&self, clear: bool, _include_preview: bool) -> Result<String, String> {
        Ok(format!(
            r#"{{"url":"https://example.com","requests":[],"clear":{clear}}}"#
        ))
    }
    async fn execute_script(&self, script: &str) -> Result<String, String> {
        Ok(format!("executed:{script}"))
    }
    async fn cookies(&self, url: Option<&str>, _include_values: bool) -> Result<String, String> {
        Ok(format!(r#"{{"count":0,"cookies":[],"url":{url:?}}}"#))
    }
    async fn click(&self, ref_id: &str) -> Result<String, String> {
        Ok(format!("clicked:{ref_id}"))
    }
    async fn type_text(&self, ref_id: &str, text: &str, submit: bool) -> Result<String, String> {
        Ok(format!("typed:{ref_id}:{text}:{submit}"))
    }
    async fn scroll(&self, delta_y: i32) -> Result<String, String> {
        Ok(format!("scrolled:{delta_y}"))
    }
}

#[tokio::test]
async fn browser_tools_dispatch_to_backend() {
    let backend: Arc<dyn BrowserBackend> = Arc::new(MockBrowser);
    let nav = BrowserNavigate::new(Arc::clone(&backend));
    let snap = BrowserSnapshot::new(Arc::clone(&backend));
    let act = BrowserAction::new(Arc::clone(&backend));

    let out = nav
        .run(BrowserNavigateArgs {
            url: "https://example.com".into(),
        })
        .await
        .unwrap();
    assert!(out.content.contains("navigated:https://example.com"));

    let snap_out = snap
        .run(BrowserSnapshotArgs {
            include_images: Some(false),
        })
        .await
        .unwrap();
    assert!(snap_out.content.contains("https://example.com"));

    let net = BrowserNetwork::new(Arc::clone(&backend));
    assert!(matches!(
        net.approval(&BrowserNetworkArgs {
            clear: None,
            include_preview: Some(true)
        }),
        Approval::Ask { .. }
    ));
    let net_out = net
        .run(BrowserNetworkArgs {
            clear: Some(true),
            include_preview: Some(false),
        })
        .await
        .unwrap();
    assert!(net_out.content.contains("\"clear\":true"));

    let execute_script = BrowserExecuteScript::new(Arc::clone(&backend));
    let script_out = execute_script
        .run(BrowserExecuteScriptArgs {
            script: "return document.title".into(),
        })
        .await
        .unwrap();
    assert!(
        script_out
            .content
            .contains("executed:return document.title")
    );
    assert!(matches!(
        execute_script.approval(&BrowserExecuteScriptArgs {
            script: "return 1".into()
        }),
        Approval::Ask { .. }
    ));
    assert!(!execute_script.read_only());
    assert!(matches!(
        execute_script
            .run(BrowserExecuteScriptArgs { script: " ".into() })
            .await,
        Err(ToolError::InvalidArguments(_))
    ));
    assert!(matches!(
        execute_script
            .run(BrowserExecuteScriptArgs {
                script: "x".repeat(16 * 1024 + 1)
            })
            .await,
        Err(ToolError::InvalidArguments(_))
    ));

    let cookies = BrowserCookies::new(Arc::clone(&backend));
    let cookies_out = cookies
        .run(BrowserCookiesArgs {
            url: Some("https://example.com".into()),
            include_values: Some(false),
        })
        .await
        .unwrap();
    assert!(cookies_out.content.contains("https://example.com"));

    let click_out = act
        .run(BrowserActionArgs {
            action: BrowserActionKind::Click,
            ref_id: Some("e1".into()),
            text: None,
            submit: None,
            return_snapshot: Some(false),
            delta_y: None,
        })
        .await
        .unwrap();
    assert!(click_out.content.contains("clicked:e1"));

    let err = act
        .run(BrowserActionArgs {
            action: BrowserActionKind::Click,
            ref_id: None,
            text: None,
            submit: None,
            return_snapshot: Some(false),
            delta_y: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, ToolError::InvalidArguments(_)));
}
