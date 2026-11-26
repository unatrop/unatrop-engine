//! OpenAI & OpenAI-compatible (DeepSeek, Ollama, local) provider driver.

use std::sync::Arc;

use async_trait::async_trait;
use uno_provider::Provider;
use uno_provider_openai::OpenAiCompatibleProvider;
use uno_transport::HttpTransport;

use super::{AccountCache, ProviderDriver, catalog_models_cache, resolve_base_url};
use crate::{
    app::DaemonError,
    modules::accounts::dto::{AccountAuth, AccountRecord, ProviderPresetView},
};

pub(super) struct OpenAiDriver;

#[async_trait]
impl ProviderDriver for OpenAiDriver {
    fn kind(&self) -> &'static str {
        "openai"
    }

    fn icon(&self) -> &'static str {
        "codex"
    }

    fn default_label(&self) -> &'static str {
        "OpenAI"
    }

    fn default_base_url(&self) -> &'static str {
        "https://api.openai.com/v1"
    }

    fn allows_no_auth(&self) -> bool {
        true
    }

    fn presets(&self) -> Vec<ProviderPresetView> {
        vec![
            ProviderPresetView {
                id: "openai".to_owned(),
                title: "OpenAI (API Key)".to_owned(),
                kind: self.kind().to_owned(),
                icon: self.icon().to_owned(),
                default_label: "OpenAI".to_owned(),
                default_base_url: self.default_base_url().to_owned(),
                default_model: "gpt-5.2".to_owned(),
                auth_mode: "api_key".to_owned(),
                allows_empty_key: false,
                oauth_start_path: None,
                manual_token_auth_kind: None,
            },
            ProviderPresetView {
                id: "deepseek".to_owned(),
                title: "DeepSeek (API Key)".to_owned(),
                kind: self.kind().to_owned(),
                icon: "deepseek".to_owned(),
                default_label: "DeepSeek".to_owned(),
                default_base_url: "https://api.deepseek.com/v1".to_owned(),
                default_model: "deepseek-chat".to_owned(),
                auth_mode: "api_key".to_owned(),
                allows_empty_key: false,
                oauth_start_path: None,
                manual_token_auth_kind: None,
            },
            ProviderPresetView {
                id: "custom_openai".to_owned(),
                title: "Custom OpenAI / Local".to_owned(),
                kind: self.kind().to_owned(),
                icon: "globe".to_owned(),
                default_label: "Local OpenAI".to_owned(),
                default_base_url: "http://127.0.0.1:11434/v1".to_owned(),
                default_model: "qwen3:32b".to_owned(),
                auth_mode: "api_key".to_owned(),
                allows_empty_key: true,
                oauth_start_path: None,
                manual_token_auth_kind: None,
            },
        ]
    }

    fn build_provider(
        &self,
        record: &AccountRecord,
        transport: Arc<dyn HttpTransport>,
        prompt_cache: Option<bool>,
    ) -> Result<Arc<dyn Provider>, DaemonError> {
        let base_url = resolve_base_url(record, self);
        match &record.auth {
            AccountAuth::None => Ok(Arc::new(
                OpenAiCompatibleProvider::new("openai", base_url, "", transport)
                    .no_auth(true)
                    .prompt_cache(prompt_cache),
            )),
            AccountAuth::ApiKey { key } => Ok(Arc::new(
                OpenAiCompatibleProvider::new("openai", base_url, key.clone(), transport)
                    .prompt_cache(prompt_cache),
            )),
            _ => Err(DaemonError::bad_request(
                "incompatible auth method for OpenAI account",
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
            AccountAuth::ApiKey { key } => {
                let provider =
                    OpenAiCompatibleProvider::new("openai", base_url, key.clone(), transport);
                catalog_models_cache(provider.list_models().await)
            }
            AccountAuth::None => {
                let provider =
                    OpenAiCompatibleProvider::new("openai", base_url, "", transport).no_auth(true);
                catalog_models_cache(provider.list_models().await)
            }
            _ => AccountCache::default(),
        }
    }
}
