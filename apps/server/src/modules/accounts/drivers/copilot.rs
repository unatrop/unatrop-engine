//! GitHub Copilot provider driver (OAuth Device Flow + API Token, quota, and model filtering).

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use uno_provider::Provider;
use uno_provider_copilot::{CopilotAuth, CopilotModel, CopilotProvider, DeviceFlowPoll};
use uno_transport::HttpTransport;

use super::{AccountCache, OAuthFlowRegistry, OAuthStartContext, ProviderDriver, resolve_base_url};
use crate::{
    app::DaemonError,
    modules::accounts::dto::{
        AccountAuth, AccountModelView, AccountQuotaView, AccountRecord, OAuthStartResponse,
        ProviderPresetView,
    },
};

pub(super) struct CopilotDriver;

#[async_trait]
impl ProviderDriver for CopilotDriver {
    fn kind(&self) -> &'static str {
        "copilot"
    }

    fn icon(&self) -> &'static str {
        "copilot"
    }

    fn default_label(&self) -> &'static str {
        "GitHub Copilot"
    }

    fn default_base_url(&self) -> &'static str {
        "https://api.githubcopilot.com"
    }

    fn presets(&self) -> Vec<ProviderPresetView> {
        vec![
            ProviderPresetView {
                id: "copilot_account".to_owned(),
                title: "GitHub Copilot (Account)".to_owned(),
                kind: self.kind().to_owned(),
                icon: self.icon().to_owned(),
                default_label: "GitHub Copilot".to_owned(),
                default_base_url: self.default_base_url().to_owned(),
                default_model: "claude-sonnet-4.5".to_owned(),
                auth_mode: "oauth_device".to_owned(),
                allows_empty_key: false,
                oauth_start_path: Some("/v1/accounts/oauth/copilot/start".to_owned()),
                manual_token_auth_kind: None,
            },
            ProviderPresetView {
                id: "copilot_api".to_owned(),
                title: "GitHub Copilot (API Token)".to_owned(),
                kind: self.kind().to_owned(),
                icon: self.icon().to_owned(),
                default_label: "Copilot API".to_owned(),
                default_base_url: self.default_base_url().to_owned(),
                default_model: "claude-sonnet-4.5".to_owned(),
                auth_mode: "api_key".to_owned(),
                allows_empty_key: false,
                oauth_start_path: None,
                manual_token_auth_kind: None,
            },
        ]
    }

    fn build_provider(
        &self,
        record: &AccountRecord,
        transport: Arc<dyn HttpTransport>,
        _prompt_cache: Option<bool>,
    ) -> Result<Arc<dyn Provider>, DaemonError> {
        let base_url = resolve_base_url(record, self);
        match &record.auth {
            AccountAuth::GithubOAuth { token } => {
                let auth = CopilotAuth::from_github_token(token.clone(), Arc::clone(&transport));
                Ok(Arc::new(CopilotProvider::new(auth, transport)))
            }
            AccountAuth::ApiKey { key } => {
                let auth =
                    CopilotAuth::from_api_token(key.clone(), base_url, Arc::clone(&transport));
                Ok(Arc::new(CopilotProvider::new(auth, transport)))
            }
            _ => Err(DaemonError::bad_request(
                "incompatible auth method for Copilot account",
            )),
        }
    }

    async fn refresh(
        &self,
        record: &AccountRecord,
        transport: Arc<dyn HttpTransport>,
    ) -> AccountCache {
        let base_url = resolve_base_url(record, self);
        match &record.auth {
            AccountAuth::GithubOAuth { token } => refresh_oauth(token, transport).await,
            AccountAuth::ApiKey { key } => refresh_api(key, base_url, transport).await,
            _ => AccountCache::default(),
        }
    }

    async fn start_oauth(&self, ctx: OAuthStartContext) -> Result<OAuthStartResponse, DaemonError> {
        let session = CopilotAuth::start_device_flow(ctx.transport.as_ref())
            .await
            .map_err(|e| {
                DaemonError::internal(format!("failed to start Copilot device flow: {e}"))
            })?;

        let flow_id = OAuthFlowRegistry::next_flow_id("copilot");
        let response = OAuthStartResponse {
            flow_id: flow_id.clone(),
            user_code: Some(session.user_code.clone()),
            verification_uri: Some(session.verification_uri.clone()),
            auth_url: None,
        };

        ctx.flows.insert_pending(&flow_id, &ctx.account_id).await;
        tokio::spawn(async move {
            let mut interval = Duration::from_secs(session.interval.max(1));
            let deadline =
                tokio::time::Instant::now() + Duration::from_secs(session.expires_in.max(60));
            let outcome = loop {
                if tokio::time::Instant::now() >= deadline {
                    break Err("GitHub Copilot login timed out".to_owned());
                }
                tokio::time::sleep(interval).await;
                match session.poll_once(ctx.transport.as_ref()).await {
                    Ok(DeviceFlowPoll::Pending) => {}
                    Ok(DeviceFlowPoll::SlowDown { extra_secs }) => {
                        interval += Duration::from_secs(extra_secs);
                    }
                    Ok(DeviceFlowPoll::Authorized { github_token }) => {
                        match ctx
                            .service
                            .save_oauth_account(
                                &ctx.account_id,
                                &ctx.label,
                                "copilot",
                                AccountAuth::GithubOAuth {
                                    token: github_token,
                                },
                            )
                            .await
                        {
                            Ok(saved) => break Ok(saved.id),
                            Err(err) => break Err(format!("{err:?}")),
                        }
                    }
                    Err(err) => break Err(err.to_string()),
                }
            };
            ctx.flows.finish(&flow_id, outcome).await;
        });

        Ok(response)
    }
}

async fn refresh_oauth(token: &str, transport: Arc<dyn HttpTransport>) -> AccountCache {
    let auth = CopilotAuth::from_github_token(token.to_owned(), Arc::clone(&transport));
    let mut account_label = None;
    let mut quota = None;
    let mut error = None;

    match auth.fetch_account().await {
        Ok(Some(acc)) => {
            account_label = acc.login.map(|l| match acc.plan {
                Some(plan) => format!("@{l} · {plan}"),
                None => format!("@{l}"),
            });
            quota = Some(AccountQuotaView {
                percent: acc.quota_percent,
                remaining: acc.quota_remaining,
                entitlement: acc.quota_entitlement,
                unlimited: acc.quota_unlimited,
                reset_date: acc.quota_reset_date,
            });
        }
        Ok(None) => {}
        Err(err) => error = Some(err.to_string()),
    }

    let provider = CopilotProvider::new(auth, transport);
    let models = match provider.list_models().await {
        Ok(list) => filter_copilot_models(list),
        Err(err) => {
            error = Some(error.unwrap_or_else(|| err.to_string()));
            Vec::new()
        }
    };
    AccountCache {
        account_label,
        quota,
        models,
        error,
    }
}

async fn refresh_api(
    key: &str,
    base_url: String,
    transport: Arc<dyn HttpTransport>,
) -> AccountCache {
    let auth = CopilotAuth::from_api_token(key.to_owned(), base_url, Arc::clone(&transport));
    let provider = CopilotProvider::new(auth, transport);
    match provider.list_models().await {
        Ok(list) => AccountCache {
            models: filter_copilot_models(list),
            ..Default::default()
        },
        Err(err) => AccountCache {
            error: Some(err.to_string()),
            ..Default::default()
        },
    }
}

fn filter_copilot_models(models: Vec<CopilotModel>) -> Vec<AccountModelView> {
    let mut out = Vec::new();
    for m in models {
        let lower = m.id.to_ascii_lowercase();
        let excluded = !m.is_chat_completions_available()
            || !m.model_picker_enabled
            || lower == "auto"
            || lower.starts_with("gpt-3.5")
            || lower == "gpt-4"
            || lower.starts_with("gpt-4o");
        if excluded
            || out
                .iter()
                .any(|existing: &AccountModelView| existing.id == m.id)
        {
            continue;
        }
        let display_name = m
            .name
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or(&m.id)
            .to_owned();
        out.push(AccountModelView {
            id: m.id,
            display_name,
            quota_fraction: None,
            quota_reset: None,
            max_input_tokens: None,
            max_output_tokens: None,
        });
    }
    out
}
