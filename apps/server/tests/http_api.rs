use std::{net::SocketAddr, path::PathBuf};

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use uno_daemon::{DaemonState, build_router};
use uno_daemon_protocol::{CONTENT_TYPE_PROTOBUF, decode_proto, encode_proto, pb};
use uno_protocol::Message;

async fn spawn_test_server_in(token: Option<String>, sessions_dir: PathBuf) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0").await.expect("bind");
    let addr = listener.local_addr().expect("local_addr");
    let state = DaemonState::with_sessions_dir(token, sessions_dir);
    let app = build_router(state);
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    addr
}

async fn spawn_test_server(token: Option<String>) -> SocketAddr {
    let dir = std::env::temp_dir().join(format!("uno-daemon-test-{}", std::process::id()));
    spawn_test_server_in(token, dir).await
}

struct HttpRawResponse {
    status_line: String,
    body: Vec<u8>,
}

async fn send_http_bytes(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&[u8]>,
) -> HttpRawResponse {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let _ = stream.set_nodelay(true);
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    if let Some(tok) = token {
        head.push_str(&format!("Authorization: Bearer {tok}\r\n"));
    }
    if let Some(b) = body {
        head.push_str(&format!(
            "Content-Type: {CONTENT_TYPE_PROTOBUF}\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await.expect("write head");
    if let Some(b) = body {
        stream.write_all(b).await.expect("write body");
    }
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");

    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http header separator");
    let headers_str = String::from_utf8_lossy(&buf[..split]);
    let status_line = headers_str.lines().next().unwrap_or("").to_owned();
    let body = buf[split + 4..].to_vec();
    HttpRawResponse { status_line, body }
}

fn create_req_bytes(
    session_id: &str,
    fork_from: Option<&str>,
    account_id: &str,
    workspace: &str,
) -> Vec<u8> {
    encode_proto(&pb::CreateSessionRequest {
        session_id: Some(session_id.to_owned()),
        fork_from: fork_from.map(ToOwned::to_owned),
        account_id: Some(account_id.to_owned()),
        model: "gpt-5.4".to_owned(),
        workspace: Some(workspace.to_owned()),
        ..Default::default()
    })
}

async fn ensure_test_account(addr: SocketAddr, token: Option<&str>) {
    let body = r#"{"id":"test_openai","label":"Test OpenAI","kind":"openai","auth_kind":"api_key","secret":"sk-test-key","models":["gpt-5.4"]}"#;
    let resp = send_http_json(addr, "POST", "/v1/accounts", token, Some(body)).await;
    assert!(
        resp.status_line.starts_with("HTTP/1.1 200 OK"),
        "seed account={}",
        resp.status_line
    );
}

#[tokio::test]
async fn health_and_sessions_crud_validation() {
    let addr = spawn_test_server(Some("secret-token".to_string())).await;

    let resp = send_http_bytes(addr, "GET", "/health", None, None).await;
    assert!(
        resp.status_line.starts_with("HTTP/1.1 200 OK"),
        "status={}",
        resp.status_line
    );
    let health = String::from_utf8_lossy(&resp.body);
    assert!(health.contains("\"status\":\"ok\""), "health={health}");

    let unauth = send_http_bytes(addr, "GET", "/v1/sessions", None, None).await;
    assert!(
        unauth.status_line.starts_with("HTTP/1.1 401"),
        "unauth={}",
        unauth.status_line
    );

    let list = send_http_bytes(addr, "GET", "/v1/sessions", Some("secret-token"), None).await;
    assert!(
        list.status_line.starts_with("HTTP/1.1 200 OK"),
        "list={}",
        list.status_line
    );
    let parsed_list = decode_proto::<pb::ListSessionsResponse>(&list.body).expect("decode list");
    assert!(parsed_list.sessions.is_empty());

    let bad_body = create_req_bytes("bad-1", None, "   ", ".");
    let create_bad = send_http_bytes(
        addr,
        "POST",
        "/v1/sessions",
        Some("secret-token"),
        Some(&bad_body),
    )
    .await;
    assert!(
        create_bad.status_line.starts_with("HTTP/1.1 400"),
        "create_bad={}",
        create_bad.status_line
    );

    let del_missing = send_http_bytes(
        addr,
        "DELETE",
        "/v1/sessions/nonexistent",
        Some("secret-token"),
        None,
    )
    .await;
    assert!(
        del_missing.status_line.starts_with("HTTP/1.1 404"),
        "del_missing={}",
        del_missing.status_line
    );
}

#[tokio::test]
async fn persists_and_resumes_and_forks_session_history() {
    let dir = std::env::temp_dir().join(format!(
        "uno-daemon-persist-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));

    let addr1 = spawn_test_server_in(None, dir.clone()).await;
    ensure_test_account(addr1, None).await;
    let create_body = create_req_bytes("chat-42", None, "test_openai", ".");
    let create_resp =
        send_http_bytes(addr1, "POST", "/v1/sessions", None, Some(&create_body)).await;
    assert!(
        create_resp.status_line.starts_with("HTTP/1.1 200 OK"),
        "resp={}",
        create_resp.status_line
    );
    let created =
        decode_proto::<pb::CreateSessionResponse>(&create_resp.body).expect("decode create");
    assert_eq!(created.session_id, "chat-42");

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    let session_file = dir.join("chat-42.json");
    assert!(session_file.is_file(), "expected persisted session file");

    let saved_history = vec![
        Message::system("sys"),
        Message::user("hello daemon"),
        Message::assistant("hi user"),
    ];
    std::fs::write(
        &session_file,
        serde_json::to_vec_pretty(&saved_history).unwrap(),
    )
    .unwrap();

    let addr2 = spawn_test_server_in(None, dir.clone()).await;
    let resume_resp =
        send_http_bytes(addr2, "POST", "/v1/sessions", None, Some(&create_body)).await;
    assert!(
        resume_resp.status_line.starts_with("HTTP/1.1 200 OK"),
        "resume={}",
        resume_resp.status_line
    );

    let hist_resp = send_http_bytes(addr2, "GET", "/v1/sessions/chat-42/history", None, None).await;
    let hist = decode_proto::<pb::HistoryResponse>(&hist_resp.body).expect("decode history");
    let hist_texts: Vec<String> = hist
        .messages
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|p| match &p.kind {
            Some(pb::content_part::Kind::Text(t)) => Some(t.clone()),
            _ => None,
        })
        .collect();
    assert!(
        hist_texts.iter().any(|t| t == "hello daemon"),
        "hist={hist_texts:?}"
    );
    assert!(
        hist_texts.iter().any(|t| t == "hi user"),
        "hist={hist_texts:?}"
    );

    let fork_body = create_req_bytes("chat-42-fork", Some("chat-42"), "test_openai", ".");
    let fork_resp = send_http_bytes(addr2, "POST", "/v1/sessions", None, Some(&fork_body)).await;
    assert!(
        fork_resp.status_line.starts_with("HTTP/1.1 200 OK"),
        "fork={}",
        fork_resp.status_line
    );

    let fork_hist = send_http_bytes(
        addr2,
        "GET",
        "/v1/sessions/chat-42-fork/history",
        None,
        None,
    )
    .await;
    let fork_hist_pb =
        decode_proto::<pb::HistoryResponse>(&fork_hist.body).expect("decode fork history");
    assert_eq!(fork_hist_pb.messages.len(), 3);

    let _ = std::fs::remove_dir_all(&dir);
}

#[tokio::test]
async fn turn_diff_and_revert_work_in_git_workspace() {
    let base = std::env::temp_dir().join(format!(
        "uno-daemon-git-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let ws = base.join("repo");
    let sessions_dir = base.join("sessions");
    std::fs::create_dir_all(&ws).unwrap();

    let git = |args: &[&str]| {
        std::process::Command::new("git")
            .args(args)
            .current_dir(&ws)
            .output()
            .unwrap()
    };
    assert!(git(&["init"]).status.success());
    git(&["config", "user.email", "test@example.com"]);
    git(&["config", "user.name", "Test"]);
    let file = ws.join("hello.txt");
    std::fs::write(&file, "original\n").unwrap();
    assert!(git(&["add", "."]).status.success());
    assert!(git(&["commit", "-m", "init"]).status.success());

    let addr = spawn_test_server_in(None, sessions_dir).await;
    ensure_test_account(addr, None).await;
    let create_body = create_req_bytes("git-1", None, "test_openai", &ws.display().to_string());
    let resp = send_http_bytes(addr, "POST", "/v1/sessions", None, Some(&create_body)).await;
    assert!(
        resp.status_line.starts_with("HTTP/1.1 200 OK"),
        "resp={}",
        resp.status_line
    );

    std::fs::write(&file, "modified\n").unwrap();
    let diff_resp = send_http_bytes(addr, "GET", "/v1/sessions/git-1/diff", None, None).await;
    let diff = decode_proto::<pb::TurnDiffResponse>(&diff_resp.body).expect("decode diff");
    assert!(diff.is_git_repo);
    assert!(diff.changed_files.iter().any(|f| f == "hello.txt"));
    assert!(diff.diff.contains("+modified"), "diff={}", diff.diff);

    let rev_body = encode_proto(&pb::RevertRequest {
        files: vec!["hello.txt".to_owned()],
    });
    let rev_resp = send_http_bytes(
        addr,
        "POST",
        "/v1/sessions/git-1/revert",
        None,
        Some(&rev_body),
    )
    .await;
    assert!(
        rev_resp.status_line.starts_with("HTTP/1.1 200 OK"),
        "rev={}",
        rev_resp.status_line
    );
    assert_eq!(std::fs::read_to_string(&file).unwrap().trim(), "original");

    let _ = std::fs::remove_dir_all(&base);
}

async fn send_http_json(
    addr: SocketAddr,
    method: &str,
    path: &str,
    token: Option<&str>,
    json_body: Option<&str>,
) -> HttpRawResponse {
    let mut stream = TcpStream::connect(addr).await.expect("connect");
    let _ = stream.set_nodelay(true);
    let mut head = format!("{method} {path} HTTP/1.1\r\nHost: localhost\r\n");
    if let Some(tok) = token {
        head.push_str(&format!("Authorization: Bearer {tok}\r\n"));
    }
    if let Some(b) = json_body {
        head.push_str(&format!(
            "Content-Type: application/json\r\nContent-Length: {}\r\n",
            b.len()
        ));
    }
    head.push_str("Connection: close\r\n\r\n");
    stream.write_all(head.as_bytes()).await.expect("write head");
    if let Some(b) = json_body {
        stream.write_all(b.as_bytes()).await.expect("write body");
    }
    let mut buf = Vec::new();
    stream.read_to_end(&mut buf).await.expect("read");

    let split = buf
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .expect("http header separator");
    let headers_str = String::from_utf8_lossy(&buf[..split]);
    let status_line = headers_str.lines().next().unwrap_or("").to_owned();
    let body = buf[split + 4..].to_vec();
    HttpRawResponse { status_line, body }
}

#[tokio::test]
async fn accounts_crud_redacts_secrets_and_builds_session_by_account_id() {
    let base = std::env::temp_dir().join(format!("uno-daemon-accounts-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    let addr = spawn_test_server_in(Some("secret-token".to_string()), base.clone()).await;

    let providers_resp =
        send_http_json(addr, "GET", "/v1/providers", Some("secret-token"), None).await;
    assert!(
        providers_resp.status_line.starts_with("HTTP/1.1 200 OK"),
        "providers={}",
        providers_resp.status_line
    );
    let providers_text = String::from_utf8_lossy(&providers_resp.body);
    assert!(providers_text.contains("\"copilot_account\""));
    assert!(providers_text.contains("\"google_gemini\""));
    assert!(providers_text.contains("\"custom_openai\""));

    let create_ant = r#"{"label":"My Anthropic","kind":"anthropic","auth_kind":"api_key","secret":"sk-ant-super-secret-123","models":["claude-sonnet-4-5"]}"#;
    let resp = send_http_json(
        addr,
        "POST",
        "/v1/accounts",
        Some("secret-token"),
        Some(create_ant),
    )
    .await;
    assert!(
        resp.status_line.starts_with("HTTP/1.1 200 OK"),
        "create={}",
        resp.status_line
    );
    let body_text = String::from_utf8_lossy(&resp.body);
    assert!(
        body_text.contains("\"id\":\"my_anthropic\""),
        "body={body_text}"
    );
    assert!(
        !body_text.contains("sk-ant-super-secret-123"),
        "secret must never leak in API response"
    );

    let list = send_http_json(addr, "GET", "/v1/accounts", Some("secret-token"), None).await;
    assert!(list.status_line.starts_with("HTTP/1.1 200 OK"));
    let list_text = String::from_utf8_lossy(&list.body);
    assert!(list_text.contains("my_anthropic"));
    assert!(!list_text.contains("sk-ant-super-secret-123"));

    // Encrypted on disk inside accounts.json
    let disk_json =
        std::fs::read_to_string(base.join("config/accounts.json")).expect("accounts.json");
    assert!(disk_json.contains("enc:v1:"));
    assert!(!disk_json.contains("sk-ant-super-secret-123"));

    // Create Copilot API-token account + local no-auth OpenAI account separately
    let create_copilot_api = r#"{"id":"copilot_api","label":"Copilot API","kind":"copilot","auth_kind":"api_key","secret":"cop-api-secret","models":["gpt-5"]}"#;
    let cp = send_http_json(
        addr,
        "POST",
        "/v1/accounts",
        Some("secret-token"),
        Some(create_copilot_api),
    )
    .await;
    assert!(cp.status_line.starts_with("HTTP/1.1 200 OK"));

    let create_local = r#"{"id":"ollama","label":"Ollama Local","kind":"openai","base_url":"http://127.0.0.1:11434/v1","auth_kind":"none","models":["qwen3:32b"]}"#;
    let loc = send_http_json(
        addr,
        "POST",
        "/v1/accounts",
        Some("secret-token"),
        Some(create_local),
    )
    .await;
    assert!(loc.status_line.starts_with("HTTP/1.1 200 OK"));

    // Create daemon session solely via account_id (no credentials from client!)
    let sess_bytes = encode_proto(&pb::CreateSessionRequest {
        session_id: Some("acc-sess-1".to_owned()),
        account_id: Some("my_anthropic".to_owned()),
        model: "claude-sonnet-4-5".to_owned(),
        workspace: Some(".".to_owned()),
        ..Default::default()
    });
    let created = send_http_bytes(
        addr,
        "POST",
        "/v1/sessions",
        Some("secret-token"),
        Some(&sess_bytes),
    )
    .await;
    assert!(
        created.status_line.starts_with("HTTP/1.1 200 OK"),
        "session={}",
        created.status_line
    );

    let del = send_http_json(
        addr,
        "DELETE",
        "/v1/accounts/copilot_api",
        Some("secret-token"),
        None,
    )
    .await;
    assert!(del.status_line.starts_with("HTTP/1.1 200 OK"));
    assert!(String::from_utf8_lossy(&del.body).contains("true"));

    let _ = std::fs::remove_dir_all(&base);
}
