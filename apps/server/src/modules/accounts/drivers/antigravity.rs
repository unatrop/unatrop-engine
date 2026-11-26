//! Google Antigravity provider driver (PKCE loopback OAuth, account info, and model/quota filtering).

use std::{collections::HashMap, sync::Arc, time::Duration};

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
use bytes::Bytes;
use futures_util::StreamExt;
use http::{
    Request,
    header::{ACCEPT, CONTENT_TYPE},
};
use ring::{digest, rand::SecureRandom as _};
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use uno_provider::{ModelInfo, Provider};
use uno_provider_antigravity::{
    AntigravityAuth, AntigravityModel, AntigravityProvider, resolve_oauth_client_id,
    resolve_oauth_client_secret,
};
use uno_transport::HttpTransport;

use super::{AccountCache, OAuthFlowRegistry, OAuthStartContext, ProviderDriver};
use crate::{
    app::DaemonError,
    modules::accounts::dto::{
        AccountAuth, AccountModelView, AccountRecord, OAuthStartResponse, ProviderPresetView,
    },
};

const GOOGLE_AUTH_URL: &str = "https://accounts.google.com/o/oauth2/v2/auth";
const GOOGLE_TOKEN_URL: &str = "https://oauth2.googleapis.com/token";
const GOOGLE_SCOPE: &str =
    "https://www.googleapis.com/auth/cloud-platform https://www.googleapis.com/auth/userinfo.email";
const OAUTH_TIMEOUT: Duration = Duration::from_secs(300);

pub(super) struct AntigravityDriver;

#[async_trait]
impl ProviderDriver for AntigravityDriver {
    fn kind(&self) -> &'static str {
        "antigravity"
    }

    fn icon(&self) -> &'static str {
        "gemini"
    }

    fn default_label(&self) -> &'static str {
        "Google Antigravity"
    }

    fn default_base_url(&self) -> &'static str {
        "https://daily-cloudcode-pa.googleapis.com"
    }

    fn presets(&self) -> Vec<ProviderPresetView> {
        vec![ProviderPresetView {
            id: "antigravity_account".to_owned(),
            title: "Google Antigravity (Account)".to_owned(),
            kind: self.kind().to_owned(),
            icon: self.icon().to_owned(),
            default_label: self.default_label().to_owned(),
            default_base_url: self.default_base_url().to_owned(),
            default_model: "gemini-2.5-pro".to_owned(),
            auth_mode: "oauth_browser".to_owned(),
            allows_empty_key: false,
            oauth_start_path: Some("/v1/accounts/oauth/antigravity/start".to_owned()),
            manual_token_auth_kind: Some("google_token".to_owned()),
        }]
    }

    fn build_provider(
        &self,
        record: &AccountRecord,
        transport: Arc<dyn HttpTransport>,
        _prompt_cache: Option<bool>,
    ) -> Result<Arc<dyn Provider>, DaemonError> {
        let AccountAuth::GoogleOAuth { token_json } = &record.auth else {
            return Err(DaemonError::bad_request(
                "incompatible auth method for Antigravity account",
            ));
        };
        let auth = build_antigravity_auth(token_json, Arc::clone(&transport))?;
        Ok(Arc::new(AntigravityProvider::new(auth, transport)))
    }

    async fn refresh(
        &self,
        record: &AccountRecord,
        transport: Arc<dyn HttpTransport>,
    ) -> AccountCache {
        let AccountAuth::GoogleOAuth { token_json } = &record.auth else {
            return AccountCache::default();
        };
        let Ok(auth) = build_antigravity_auth(token_json, Arc::clone(&transport)) else {
            return AccountCache {
                error: Some("invalid Antigravity OAuth credentials".to_owned()),
                ..Default::default()
            };
        };
        let provider = AntigravityProvider::new(auth, transport);
        let mut error = None;
        let account_label = match provider.fetch_account().await {
            Ok(acc) => {
                let parts: Vec<String> = [acc.email, acc.plan].into_iter().flatten().collect();
                (!parts.is_empty()).then(|| parts.join(" · "))
            }
            Err(err) => {
                error = Some(err.to_string());
                None
            }
        };
        let models = match provider.list_models().await {
            Ok(list) => filter_antigravity_models(list),
            Err(err) => {
                error = Some(error.unwrap_or_else(|| err.to_string()));
                Vec::new()
            }
        };
        AccountCache {
            account_label,
            quota: None,
            models,
            error,
        }
    }

    async fn start_oauth(&self, ctx: OAuthStartContext) -> Result<OAuthStartResponse, DaemonError> {
        let client_id = resolve_oauth_client_id().ok_or_else(|| {
            DaemonError::bad_request(
                "Set ANTIGRAVITY_OAUTH_CLIENT_ID and ANTIGRAVITY_OAUTH_CLIENT_SECRET before starting Antigravity OAuth",
            )
        })?;
        let client_secret = resolve_oauth_client_secret().ok_or_else(|| {
            DaemonError::bad_request(
                "Set ANTIGRAVITY_OAUTH_CLIENT_ID and ANTIGRAVITY_OAUTH_CLIENT_SECRET before starting Antigravity OAuth",
            )
        })?;

        let listener = TcpListener::bind("127.0.0.1:0").await.map_err(|e| {
            DaemonError::internal(format!("failed to bind OAuth callback port: {e}"))
        })?;
        let port = listener
            .local_addr()
            .map_err(|e| DaemonError::internal(format!("failed to read OAuth port: {e}")))?
            .port();
        let redirect_uri = format!("http://127.0.0.1:{port}/oauth-callback");
        let (expected_state, code_verifier, code_challenge) =
            generate_pkce_and_state().map_err(DaemonError::internal)?;

        let auth_url = format!(
            "{GOOGLE_AUTH_URL}?client_id={}&redirect_uri={}&response_type=code&scope={}&access_type=offline&prompt=consent&state={}&code_challenge={}&code_challenge_method=S256",
            form_encode(&client_id),
            form_encode(&redirect_uri),
            form_encode(GOOGLE_SCOPE),
            form_encode(&expected_state),
            form_encode(&code_challenge),
        );

        let flow_id = OAuthFlowRegistry::next_flow_id("antigravity");
        let response = OAuthStartResponse {
            flow_id: flow_id.clone(),
            user_code: None,
            verification_uri: None,
            auth_url: Some(auth_url),
        };

        ctx.flows.insert_pending(&flow_id, &ctx.account_id).await;
        let flows = Arc::clone(&ctx.flows);
        let params = PkceExchangeParams {
            ctx,
            listener,
            expected_state,
            code_verifier,
            redirect_uri,
            client_id,
            client_secret,
        };
        tokio::spawn(async move {
            let outcome = run_pkce_exchange(params).await;
            flows.finish(&flow_id, outcome).await;
        });

        Ok(response)
    }
}

struct PkceExchangeParams {
    ctx: OAuthStartContext,
    listener: TcpListener,
    expected_state: String,
    code_verifier: String,
    redirect_uri: String,
    client_id: String,
    client_secret: String,
}

async fn run_pkce_exchange(params: PkceExchangeParams) -> Result<String, String> {
    let code = tokio::time::timeout(
        OAUTH_TIMEOUT,
        accept_oauth_code(params.listener, &params.expected_state),
    )
    .await
    .map_err(|_| "Antigravity OAuth login timed out".to_owned())??;

    let body = format!(
        "client_id={}&client_secret={}&code={}&code_verifier={}&grant_type=authorization_code&redirect_uri={}",
        form_encode(&params.client_id),
        form_encode(&params.client_secret),
        form_encode(&code),
        form_encode(&params.code_verifier),
        form_encode(&params.redirect_uri),
    );
    let request = Request::post(GOOGLE_TOKEN_URL)
        .header(ACCEPT, "application/json")
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .body(Bytes::from(body))
        .map_err(|e| format!("failed to build OAuth token request: {e}"))?;

    let response = params
        .ctx
        .transport
        .send(request)
        .await
        .map_err(|e| format!("OAuth token request failed: {e}"))?;
    let status = response.status();
    let mut stream = response.into_body();
    let mut bytes = Vec::new();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(|e| format!("failed to read OAuth token response: {e}"))?;
        bytes.extend_from_slice(&chunk);
    }
    let text = String::from_utf8_lossy(&bytes).into_owned();
    if !status.is_success() {
        return Err(format!(
            "OAuth token exchange failed ({status}): {}",
            uno_protocol::redact_secrets(&text)
        ));
    }

    #[derive(Serialize, Deserialize)]
    struct StoredOAuthToken {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        access_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        refresh_token: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        expires_in: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_type: Option<String>,
    }

    let parsed: StoredOAuthToken =
        serde_json::from_str(&text).map_err(|e| format!("invalid OAuth token JSON: {e}"))?;
    let has_token = parsed
        .access_token
        .as_deref()
        .is_some_and(|v| !v.is_empty())
        || parsed
            .refresh_token
            .as_deref()
            .is_some_and(|v| !v.is_empty());
    if !has_token {
        return Err("OAuth response missing access_token and refresh_token".to_owned());
    }
    let minimal_json =
        serde_json::to_string(&parsed).map_err(|e| format!("serialize OAuth token failed: {e}"))?;

    params
        .ctx
        .service
        .save_oauth_account(
            &params.ctx.account_id,
            &params.ctx.label,
            "antigravity",
            AccountAuth::GoogleOAuth {
                token_json: minimal_json,
            },
        )
        .await
        .map(|view| view.id)
        .map_err(|e| format!("{e:?}"))
}

fn build_antigravity_auth(
    raw: &str,
    transport: Arc<dyn HttpTransport>,
) -> Result<AntigravityAuth, DaemonError> {
    let trimmed = raw.trim();
    if trimmed.starts_with('{') {
        AntigravityAuth::from_stored_token(trimmed, transport)
            .map_err(|e| DaemonError::bad_request(e.to_string()))
    } else if trimmed.starts_with("1//") {
        Ok(AntigravityAuth::from_refresh_token(trimmed, transport))
    } else {
        Ok(AntigravityAuth::from_access_token(trimmed, transport))
    }
}

fn filter_antigravity_models(models: Vec<AntigravityModel>) -> Vec<AccountModelView> {
    let mut by_base: HashMap<String, AntigravityModel> = HashMap::new();
    for model in models {
        let lower = model.id.trim().to_ascii_lowercase();
        let excluded = lower.is_empty()
            || lower.starts_with("chat_")
            || lower.starts_with("tab_")
            || lower.starts_with("rev19")
            || lower.starts_with("gemini-1.")
            || lower.starts_with("gemini-2.0")
            || lower.ends_with("-image")
            || lower.contains("-image-")
            || lower.contains("embedding")
            || lower.contains("imagen");
        if excluded {
            continue;
        }
        let base = strip_antigravity_variant(&model.id).to_owned();
        let replace = by_base
            .get(&base)
            .is_none_or(|prev| variant_priority(&model.id) > variant_priority(&prev.id));
        if replace {
            by_base.insert(base, model);
        }
    }
    let mut result: Vec<AntigravityModel> = by_base.into_values().collect();
    result.sort_by(|a, b| a.id.cmp(&b.id));
    result
        .into_iter()
        .map(|m| {
            let profile = ModelInfo::from(&m).context_profile();
            let display_name = clean_antigravity_name(m.display_name.as_deref().unwrap_or(&m.id));
            let quota_fraction = m
                .quota_info
                .as_ref()
                .and_then(|q| q.remaining_fraction.map(|f| f.clamp(0.0, 1.0)));
            let quota_reset = m.quota_info.and_then(|q| q.reset_time);
            AccountModelView {
                id: m.id,
                display_name,
                quota_fraction,
                quota_reset,
                max_input_tokens: profile.map(|p| p.max_input_tokens),
                max_output_tokens: profile.map(|p| p.max_output_tokens),
            }
        })
        .collect()
}

fn strip_antigravity_variant(id: &str) -> &str {
    let mut s = id.trim();
    for suffix in [
        "-tiered",
        "_tiered",
        "-thinking",
        "_thinking",
        "-xhigh",
        "-high",
        "-medium",
        "-low",
        "-minimal",
    ] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped;
        }
    }
    s
}

fn variant_priority(id: &str) -> u8 {
    let lower = id.to_ascii_lowercase();
    if lower.ends_with("-high") {
        5
    } else if lower.ends_with("-medium") {
        4
    } else if !lower.contains("thinking") {
        3
    } else {
        1
    }
}

fn clean_antigravity_name(raw: &str) -> String {
    let mut s = raw.trim().to_owned();
    for suffix in [" (Thinking)", " (High)", " (Medium)", " (Low)", " Thinking"] {
        if let Some(stripped) = s.strip_suffix(suffix) {
            s = stripped.trim().to_owned();
        }
    }
    s
}

async fn accept_oauth_code(listener: TcpListener, expected_state: &str) -> Result<String, String> {
    loop {
        let (mut socket, _) = listener
            .accept()
            .await
            .map_err(|e| format!("failed to accept OAuth callback: {e}"))?;
        let mut buf = [0u8; 4096];
        let n = socket
            .read(&mut buf)
            .await
            .map_err(|e| format!("failed to read OAuth callback: {e}"))?;
        let req_text = String::from_utf8_lossy(&buf[..n]);
        let path = req_text
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        let (route, query) = path.split_once('?').unwrap_or((path, ""));
        if route != "/oauth-callback" {
            let _ = socket
                .write_all(b"HTTP/1.1 404 Not Found\r\nConnection: close\r\n\r\n")
                .await;
            continue;
        }

        let mut code = None;
        let mut state = None;
        let mut oauth_error = None;
        for pair in query.split('&') {
            let Some((k, v)) = pair.split_once('=') else {
                continue;
            };
            match k {
                "code" => code = Some(percent_decode(v)),
                "state" => state = Some(percent_decode(v)),
                "error" => oauth_error = Some(percent_decode(v)),
                _ => {}
            }
        }

        if let Some(err) = oauth_error {
            let _ = send_callback_html(&mut socket, false).await;
            return Err(format!("OAuth provider rejected sign-in: {err}"));
        }

        let state_ok = state
            .as_deref()
            .is_some_and(|got| constant_time_str_eq(got, expected_state));
        let valid_code = code.filter(|c| !c.trim().is_empty());
        if !state_ok || valid_code.is_none() {
            let _ = send_callback_html(&mut socket, false).await;
            return Err("OAuth callback failed state verification or missed code".to_owned());
        }

        let _ = send_callback_html(&mut socket, true).await;
        return Ok(valid_code.unwrap_or_default());
    }
}

async fn send_callback_html(socket: &mut tokio::net::TcpStream, ok: bool) -> std::io::Result<()> {
    let body = if ok {
        "<html><body><h3>Authorization complete.</h3><p>You can close this tab and return to Unatrop.</p></body></html>"
    } else {
        "<html><body><h3>Authorization failed.</h3><p>You can close this tab and return to Unatrop.</p></body></html>"
    };
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    socket.write_all(resp.as_bytes()).await
}

fn generate_pkce_and_state() -> Result<(String, String, String), String> {
    let rng = ring::rand::SystemRandom::new();
    let mut state_bytes = [0u8; 16];
    rng.fill(&mut state_bytes)
        .map_err(|_| "failed to generate OAuth state".to_owned())?;
    let mut state = String::with_capacity(32);
    for b in state_bytes {
        use std::fmt::Write as _;
        let _ = write!(state, "{b:02x}");
    }

    let mut verifier_bytes = [0u8; 32];
    rng.fill(&mut verifier_bytes)
        .map_err(|_| "failed to generate PKCE verifier".to_owned())?;
    let verifier = URL_SAFE_NO_PAD.encode(verifier_bytes);
    let challenge = URL_SAFE_NO_PAD.encode(digest::digest(&digest::SHA256, verifier.as_bytes()));
    Ok((state, verifier, challenge))
}

fn constant_time_str_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn form_encode(input: &str) -> String {
    let mut out = String::with_capacity(input.len());
    for byte in input.bytes() {
        match byte {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(byte as char);
            }
            b' ' => out.push('+'),
            _ => out.push_str(&format!("%{byte:02X}")),
        }
    }
    out
}

fn percent_decode(input: &str) -> String {
    let mut out = Vec::with_capacity(input.len());
    let bytes = input.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (from_hex(bytes[i + 1]), from_hex(bytes[i + 2]))
        {
            out.push((h << 4) | l);
            i += 3;
            continue;
        }
        if bytes[i] == b'+' {
            out.push(b' ');
        } else {
            out.push(bytes[i]);
        }
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn from_hex(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}
