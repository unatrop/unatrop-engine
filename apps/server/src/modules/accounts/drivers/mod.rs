//! Pluggable provider driver trait, registry, and OAuth flow tracker.

mod anthropic;
mod antigravity;
mod copilot;
mod gemini;
mod openai;

use std::{
    collections::HashMap,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
};

use async_trait::async_trait;
use tokio::sync::RwLock;
use uno_provider::{ModelInfo, Provider};
use uno_reqwest_transport::ReqwestTransport;
use uno_transport::HttpTransport;

use super::{
    dto::{
        AccountModelView, AccountQuotaView, AccountRecord, OAuthPollResponse, OAuthStartResponse,
        ProviderPresetView,
    },
    service::AccountService,
};
use crate::app::DaemonError;

#[derive(Clone, Default)]
pub(super) struct AccountCache {
    pub account_label: Option<String>,
    pub quota: Option<AccountQuotaView>,
    pub models: Vec<AccountModelView>,
    pub error: Option<String>,
}

pub(super) struct OAuthStartContext {
    pub service: Arc<AccountService>,
    pub transport: Arc<ReqwestTransport>,
    pub flows: Arc<OAuthFlowRegistry>,
    pub account_id: String,
    pub label: String,
}

#[async_trait]
pub(super) trait ProviderDriver: Send + Sync {
    fn kind(&self) -> &'static str;
    fn icon(&self) -> &'static str;
    fn default_label(&self) -> &'static str;
    fn default_base_url(&self) -> &'static str;

    fn allows_no_auth(&self) -> bool {
        false
    }

    fn presets(&self) -> Vec<ProviderPresetView>;

    fn build_provider(
        &self,
        record: &AccountRecord,
        transport: Arc<dyn HttpTransport>,
        prompt_cache: Option<bool>,
    ) -> Result<Arc<dyn Provider>, DaemonError>;

    async fn refresh(
        &self,
        record: &AccountRecord,
        transport: Arc<dyn HttpTransport>,
    ) -> AccountCache;

    async fn start_oauth(
        &self,
        _ctx: OAuthStartContext,
    ) -> Result<OAuthStartResponse, DaemonError> {
        Err(DaemonError::bad_request(format!(
            "OAuth is not supported for provider `{}`",
            self.kind()
        )))
    }
}

pub(super) struct ProviderRegistry {
    drivers: Vec<Arc<dyn ProviderDriver>>,
}

impl Default for ProviderRegistry {
    fn default() -> Self {
        Self {
            drivers: vec![
                Arc::new(copilot::CopilotDriver),
                Arc::new(antigravity::AntigravityDriver),
                Arc::new(gemini::GeminiDriver),
                Arc::new(openai::OpenAiDriver),
                Arc::new(anthropic::AnthropicDriver),
            ],
        }
    }
}

impl ProviderRegistry {
    pub(super) fn get(&self, kind: &str) -> Result<Arc<dyn ProviderDriver>, DaemonError> {
        let needle = kind.trim().to_ascii_lowercase();
        self.drivers
            .iter()
            .find(|d| d.kind() == needle)
            .cloned()
            .ok_or_else(|| DaemonError::bad_request(format!("unsupported provider `{kind}`")))
    }

    pub(super) fn all_presets(&self) -> Vec<ProviderPresetView> {
        self.drivers.iter().flat_map(|d| d.presets()).collect()
    }
}

static FLOW_COUNTER: AtomicU64 = AtomicU64::new(1);

#[derive(Clone, Debug)]
struct FlowState {
    status: String,
    account_id: Option<String>,
    error: Option<String>,
}

#[derive(Default)]
pub(super) struct OAuthFlowRegistry {
    flows: RwLock<HashMap<String, FlowState>>,
}

impl OAuthFlowRegistry {
    pub(super) fn next_flow_id(prefix: &str) -> String {
        let n = FLOW_COUNTER.fetch_add(1, Ordering::Relaxed);
        format!("{prefix}-{n}")
    }

    pub(super) async fn insert_pending(&self, flow_id: &str, account_id: &str) {
        self.flows.write().await.insert(
            flow_id.to_owned(),
            FlowState {
                status: "pending".to_owned(),
                account_id: Some(account_id.to_owned()),
                error: None,
            },
        );
    }

    pub(super) async fn finish(&self, flow_id: &str, result: Result<String, String>) {
        if let Some(entry) = self.flows.write().await.get_mut(flow_id) {
            match result {
                Ok(account_id) => {
                    entry.status = "connected".to_owned();
                    entry.account_id = Some(account_id);
                    entry.error = None;
                }
                Err(err) => {
                    entry.status = "failed".to_owned();
                    entry.error = Some(err);
                }
            }
        }
    }

    pub(super) async fn get(&self, flow_id: &str) -> Option<OAuthPollResponse> {
        self.flows
            .read()
            .await
            .get(flow_id)
            .map(|s| OAuthPollResponse {
                status: s.status.clone(),
                account_id: s.account_id.clone(),
                error: s.error.clone(),
            })
    }
}

pub(super) fn resolve_base_url(record: &AccountRecord, driver: &dyn ProviderDriver) -> String {
    record
        .base_url
        .clone()
        .unwrap_or_else(|| driver.default_base_url().to_owned())
}

pub(super) fn catalog_models_cache(
    res: Result<Vec<ModelInfo>, uno_provider::ProviderError>,
) -> AccountCache {
    match res {
        Ok(list) => AccountCache {
            models: list
                .into_iter()
                .map(|m| AccountModelView {
                    display_name: m.display_name.clone().unwrap_or_else(|| m.id.clone()),
                    id: m.id,
                    quota_fraction: None,
                    quota_reset: None,
                    max_input_tokens: m.max_input_tokens,
                    max_output_tokens: m.max_output_tokens,
                })
                .collect(),
            ..Default::default()
        },
        Err(err) => AccountCache {
            error: Some(err.to_string()),
            ..Default::default()
        },
    }
}
