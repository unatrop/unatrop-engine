use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::Bytes;
use futures_util::stream;
use http::{Request, Response, StatusCode};
use serde_json::Value;
use transport::{ByteStream, HttpTransport, TransportError};
use uno_tools_core::{Tool, Typed};
use uno_tools_web::{
    SearchBackend, WebFetchTool, WebSearchTool, is_public_ip, validate_public_url,
    validate_resolved_ips,
};

struct MockTransport {
    requests: Mutex<Vec<Request<Bytes>>>,
    responses: Mutex<Vec<Response<Bytes>>>,
}

impl MockTransport {
    fn new(responses: Vec<Response<Bytes>>) -> Self {
        Self {
            requests: Mutex::new(Vec::new()),
            responses: Mutex::new(responses),
        }
    }
}

#[async_trait]
impl HttpTransport for MockTransport {
    async fn send(&self, request: Request<Bytes>) -> Result<Response<ByteStream>, TransportError> {
        self.requests.lock().expect("lock").push(request);
        let resp = self.responses.lock().expect("lock").remove(0);
        let (parts, body) = resp.into_parts();
        Ok(Response::from_parts(
            parts,
            Box::pin(stream::iter([Ok(body)])),
        ))
    }
}

#[tokio::test]
async fn web_fetch_blocks_private_and_local_urls() {
    let transport = Arc::new(MockTransport::new(Vec::new()));
    let tool = Typed(WebFetchTool::new(transport.clone()));

    for url in [
        "http://127.0.0.1:8080/secret",
        "http://169.254.169.254/latest/meta-data/",
        "http://localhost:3000",
        "file:///etc/passwd",
    ] {
        let out = tool
            .execute(serde_json::json!({ "url": url }))
            .await
            .expect("tool returns error output");
        assert!(out.is_error, "expected SSRF error for {url}");
    }
    assert!(transport.requests.lock().expect("lock").is_empty());
}

#[tokio::test]
async fn web_fetch_extracts_title_and_markdown_without_scripts() {
    let html = concat!(
        "<html><head><title>Rust &amp; Docs</title>",
        "<style>body { color: red; }</style></head>",
        "<body><h1>Hello</h1><script>alert('xss')</script>",
        "<p>First paragraph.</p><ul><li>Item A</li><li>Item B</li></ul></body></html>"
    );
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "text/html; charset=utf-8")
        .body(Bytes::from_static(html.as_bytes()))
        .expect("resp");
    let transport = Arc::new(MockTransport::new(vec![resp]));
    let tool = Typed(WebFetchTool::new(transport));

    let out = tool
        .execute(serde_json::json!({ "url": "https://example.com/docs" }))
        .await
        .expect("execute ok");
    assert!(!out.is_error);

    let parsed: Value = serde_json::from_str(&out.content).expect("json");
    assert_eq!(parsed["title"], "Rust & Docs");
    let content = parsed["content"].as_str().expect("content");
    assert!(content.contains("# Hello"));
    assert!(content.contains("First paragraph."));
    assert!(content.contains("- Item A"));
    assert!(!content.contains("alert('xss')"));
    assert!(!content.contains("color: red"));
}

#[tokio::test]
async fn web_fetch_rejects_binary_content_type() {
    let resp = Response::builder()
        .status(StatusCode::OK)
        .header("content-type", "image/png")
        .body(Bytes::from_static(b"\x89PNG"))
        .expect("resp");
    let transport = Arc::new(MockTransport::new(vec![resp]));
    let tool = Typed(WebFetchTool::new(transport));

    let out = tool
        .execute(serde_json::json!({ "url": "https://example.com/pic.png" }))
        .await
        .expect("execute ok");
    assert!(out.is_error);
    assert!(out.content.contains("binary"));
}

#[tokio::test]
async fn web_search_parses_searxng_json() {
    let searxng_json =
        r#"{"results":[{"title":"Uno","url":"https://uno.dev","content":"Fast SDK"}]}"#;
    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from_static(searxng_json.as_bytes()))
            .expect("resp"),
    ]));

    let searxng = Typed(WebSearchTool::new(
        transport,
        SearchBackend::Searxng {
            base_url: "https://search.example.test".into(),
        },
    ));
    let out = searxng
        .execute(serde_json::json!({ "query": "uno sdk" }))
        .await
        .expect("ok");
    let v: Value = serde_json::from_str(&out.content).expect("json");
    assert_eq!(v["backend"], "searxng");
    assert_eq!(v["results"][0]["url"], "https://uno.dev");
}

#[tokio::test]
async fn web_search_searxng_pool_falls_back_on_http_error_and_empty_results() {
    let ok_json =
        r#"{"results":[{"title":"Third","url":"https://third.example","content":"Hit from 3rd"}]}"#;
    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(Bytes::from_static(b"overloaded"))
            .expect("resp1"),
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from_static(br#"{"results":[]}"#))
            .expect("resp2"),
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from_static(ok_json.as_bytes()))
            .expect("resp3"),
    ]));

    let tool = Typed(WebSearchTool::searxng_pool(
        transport.clone(),
        [
            "https://s1.example",
            "https://s2.example",
            "https://s3.example",
        ],
    ));

    let out = tool
        .execute(serde_json::json!({ "query": "rust async" }))
        .await
        .expect("ok");
    assert!(!out.is_error);

    let v: Value = serde_json::from_str(&out.content).expect("json");
    assert_eq!(v["backend"], "searxng");
    assert_eq!(v["results"][0]["url"], "https://third.example");

    let reqs = transport.requests.lock().expect("lock");
    assert_eq!(reqs.len(), 3);
    assert!(
        reqs[0]
            .uri()
            .to_string()
            .starts_with("https://s1.example/search")
    );
    assert!(
        reqs[1]
            .uri()
            .to_string()
            .starts_with("https://s2.example/search")
    );
    assert!(
        reqs[2]
            .uri()
            .to_string()
            .starts_with("https://s3.example/search")
    );
}

#[tokio::test]
async fn web_search_with_fallback_switches_from_searxng_to_brave() {
    let brave_json = r#"{"web":{"results":[{"title":"Brave Hit","url":"https://brave.example","description":"From Brave"}]}}"#;
    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::TOO_MANY_REQUESTS)
            .body(Bytes::from_static(b"rate limit"))
            .expect("resp1"),
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from_static(brave_json.as_bytes()))
            .expect("resp2"),
    ]));

    let tool = Typed(
        WebSearchTool::new(
            transport,
            SearchBackend::Searxng {
                base_url: "https://s1.example".into(),
            },
        )
        .with_fallback(SearchBackend::Brave {
            api_key: "test-key".into(),
        }),
    );

    let out = tool
        .execute(serde_json::json!({ "query": "uno" }))
        .await
        .expect("ok");
    assert!(!out.is_error);
    let v: Value = serde_json::from_str(&out.content).expect("json");
    assert_eq!(v["backend"], "brave");
    assert_eq!(v["results"][0]["url"], "https://brave.example");
}

#[tokio::test]
async fn web_search_returns_error_when_all_backends_fail() {
    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::BAD_GATEWAY)
            .body(Bytes::from_static(b"bad gateway"))
            .expect("resp1"),
        Response::builder()
            .status(StatusCode::OK)
            .body(Bytes::from_static(b"not-json"))
            .expect("resp2"),
    ]));

    let tool = Typed(WebSearchTool::searxng_pool(
        transport,
        ["https://s1.example", "https://s2.example"],
    ));

    let out = tool
        .execute(serde_json::json!({ "query": "test" }))
        .await
        .expect("ok");
    assert!(out.is_error);
    assert!(out.content.contains("all search backends failed"));
    assert!(out.content.contains("searxng(https://s1.example)"));
    assert!(out.content.contains("searxng(https://s2.example)"));
}

#[tokio::test]
async fn web_fetch_blocks_ssrf_redirect_to_private_ip() {
    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::FOUND)
            .header("location", "http://127.0.0.1:8080/admin")
            .body(Bytes::new())
            .expect("resp"),
    ]));
    let tool = Typed(WebFetchTool::new(transport.clone()));

    let out = tool
        .execute(serde_json::json!({ "url": "https://example.com/start" }))
        .await
        .expect("ok");
    assert!(out.is_error);
    assert!(out.content.contains("blocked redirect target"));
    assert_eq!(transport.requests.lock().expect("lock").len(), 1);
}

#[tokio::test]
async fn web_fetch_strips_nav_footer_aside_and_errors_on_403() {
    let html = concat!(
        "<html><body>",
        "<nav>Menu Item 1 | Menu Item 2</nav>",
        "<main><p>Real article text.</p></main>",
        "<aside>Sidebar Ad</aside>",
        "<footer>Copyright 2026</footer>",
        "</body></html>"
    );
    let transport = Arc::new(MockTransport::new(vec![
        Response::builder()
            .status(StatusCode::OK)
            .header("content-type", "text/html")
            .body(Bytes::from_static(html.as_bytes()))
            .expect("resp1"),
        Response::builder()
            .status(StatusCode::FORBIDDEN)
            .body(Bytes::from_static(b"Forbidden"))
            .expect("resp2"),
    ]));
    let tool = Typed(WebFetchTool::new(transport));

    let out = tool
        .execute(serde_json::json!({ "url": "https://example.com/article" }))
        .await
        .expect("ok");
    assert!(!out.is_error);
    let v: Value = serde_json::from_str(&out.content).expect("json");
    assert_eq!(v["untrusted"], true);
    let content = v["content"].as_str().expect("content");
    assert!(content.contains("Real article text."));
    assert!(!content.contains("Menu Item"));
    assert!(!content.contains("Sidebar Ad"));
    assert!(!content.contains("Copyright 2026"));

    let err_out = tool
        .execute(serde_json::json!({ "url": "https://example.com/blocked" }))
        .await
        .expect("ok");
    assert!(err_out.is_error);
    assert!(err_out.content.contains("HTTP 403"));
}

#[test]
fn ssrf_blocks_dns_rebinding_domains_and_resolved_private_ips() {
    for rebinding_url in [
        "http://127.0.0.1.nip.io/admin",
        "https://app.sslip.io/internal",
        "http://sub.localtest.me:8080",
        "http://api.lvh.me/v1",
    ] {
        assert!(
            validate_public_url(rebinding_url, false).is_err(),
            "expected {rebinding_url} to be blocked"
        );
    }

    let loopback: std::net::IpAddr = "127.0.0.1".parse().unwrap();
    let metadata: std::net::IpAddr = "169.254.169.254".parse().unwrap();
    let public_ip: std::net::IpAddr = "93.184.216.34".parse().unwrap();

    assert!(!is_public_ip(loopback));
    assert!(!is_public_ip(metadata));
    assert!(is_public_ip(public_ip));

    assert!(validate_resolved_ips("evil.example", [public_ip, metadata], false).is_err());
    assert!(validate_resolved_ips("ok.example", [public_ip], false).is_ok());
}
