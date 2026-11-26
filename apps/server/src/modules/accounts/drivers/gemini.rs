//! Official Google Gemini API (`x-goog-api-key`) provider driver.

use std::sync::Arc;

use async_trait::async_trait;
use uno_provider::Provider;
use uno_provider_antigravity::GeminiApiProvider;
use uno_transport::HttpTransport;

use super::{AccountCache, ProviderDriver, catalog_models_cache, resolve_base_url};
use crate::{
    app::DaemonError,
    modules::accounts::dto::{AccountAuth, AccountRecord, ProviderPresetView},
};

pub(super) struct GeminiDriver;

#[async_trait]
impl ProviderDriver for GeminiDriver {
    fn kind(&self) -> &'static str {
        "gemini"
    }

    fn icon(&self) -> &'static str {
        "gemini"
    }

    fn default_label(&self) -> &'static str {
        "Google Gemini API"
    }

    fn default_base_url(&self) -> &'static str {
        "https://generativelanguage.googleapis.com/v1beta"
    }

    fn presets(&self) -> Vec<ProviderPresetView> {
        vec![ProviderPresetView {
            id: "google_gemini".to_owned(),
            title: "Google Gemini (API Key)".to_owned(),
            kind: self.kind().to_owned(),
            icon: self.icon().to_owned(),
            default_label: self.default_label().to_owned(),
            default_base_url: self.default_base_url().to_owned(),
            default_model: "gemini-2.5-pro".to_owned(),
            auth_mode: "api_key".to_owned(),
            allows_empty_key: false,
            oauth_start_path: None,
            manual_token_auth_kind: None,
        }]
    }

    fn build_provider(
        &self,
        record: &AccountRecord,
        transport: Arc<dyn HttpTransport>,
        _prompt_cache: Option<bool>,
    ) -> Result<Arc<dyn Provider>, DaemonError> {
        let base_url = resolve_base_url(record, self);
        let AccountAuth::ApiKey { key } = &record.auth else {
            return Err(DaemonError::bad_request(
                "incompatible auth method for Gemini account",
            ));
        };
        Ok(Arc::new(GeminiApiProvider::new(
            base_url,
            key.clone(),
            transport,
        )))
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
        let provider = GeminiApiProvider::new(base_url, key.clone(), transport);
        catalog_models_cache(provider.list_models().await)
    }
}
