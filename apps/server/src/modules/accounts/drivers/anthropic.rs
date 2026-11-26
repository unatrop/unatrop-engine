//! Anthropic Messages API provider driver.

use std::sync::Arc;

use async_trait::async_trait;
use uno_provider::Provider;
use uno_provider_anthropic::AnthropicProvider;
use uno_transport::HttpTransport;

use super::{AccountCache, ProviderDriver, catalog_models_cache, resolve_base_url};
use crate::{
    app::DaemonError,
    modules::accounts::dto::{AccountAuth, AccountRecord, ProviderPresetView},
};

pub(super) struct AnthropicDriver;

#[async_trait]
impl ProviderDriver for AnthropicDriver {
    fn kind(&self) -> &'static str {
        "anthropic"
    }

    fn icon(&self) -> &'static str {
        "claude"
    }

    fn default_label(&self) -> &'static str {
        "Anthropic"
    }

    fn default_base_url(&self) -> &'static str {
        "https://api.anthropic.com"
    }

    fn presets(&self) -> Vec<ProviderPresetView> {
        vec![
            ProviderPresetView {
                id: "anthropic".to_owned(),
                title: "Anthropic (API Key)".to_owned(),
                kind: self.kind().to_owned(),
                icon: self.icon().to_owned(),
                default_label: "Anthropic".to_owned(),
                default_base_url: self.default_base_url().to_owned(),
                default_model: "claude-sonnet-4-5".to_owned(),
                auth_mode: "api_key".to_owned(),
                allows_empty_key: false,
                oauth_start_path: None,
                manual_token_auth_kind: None,
            },
            ProviderPresetView {
                id: "custom_anthropic".to_owned(),
                title: "Custom Anthropic".to_owned(),
                kind: self.kind().to_owned(),
                icon: self.icon().to_owned(),
                default_label: "Custom Anthropic".to_owned(),
                default_base_url: self.default_base_url().to_owned(),
                default_model: "claude-sonnet-4-5".to_owned(),
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
        prompt_cache: Option<bool>,
    ) -> Result<Arc<dyn Provider>, DaemonError> {
        let base_url = resolve_base_url(record, self);
        let AccountAuth::ApiKey { key } = &record.auth else {
            return Err(DaemonError::bad_request(
                "incompatible auth method for Anthropic account",
            ));
        };
        Ok(Arc::new(
            AnthropicProvider::new("anthropic", base_url, key.clone(), transport)
                .prompt_cache(prompt_cache.unwrap_or(true)),
        ))
    }

    async fn refresh(
        &self,
        record: &AccountRecord,
        transport: Arc<dyn HttpTransport>,
    ) -> AccountCache {
        let base_url = resolve_base_url(record, self);
        let AccountAuth::ApiKey { key } = &record.auth else {
            return AccountCache::default();
        };
        let provider = AnthropicProvider::new("anthropic", base_url, key.clone(), transport);
        catalog_models_cache(provider.list_models().await)
    }
}
