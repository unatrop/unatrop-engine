//! Provider-agnostic account CRUD, caching, and driver delegation.

use std::{collections::HashMap, path::PathBuf, sync::Arc};

use tokio::sync::RwLock;
use uno_provider::{ContextProfile, Provider};
use uno_reqwest_transport::ReqwestTransport;

use super::{
    drivers::{AccountCache, OAuthFlowRegistry, OAuthStartContext, ProviderRegistry},
    dto::{
        AccountAuth, AccountModelView, AccountRecord, AccountView, OAuthPollResponse,
        OAuthStartResponse, ProviderPresetView, UpsertAccountRequest,
    },
    store::AccountStore,
};
use crate::app::DaemonError;

pub struct AccountService {
    store: AccountStore,
    transport: Arc<ReqwestTransport>,
    registry: ProviderRegistry,
    records: RwLock<Vec<AccountRecord>>,
    caches: RwLock<HashMap<String, AccountCache>>,
    oauth_flows: Arc<OAuthFlowRegistry>,
}

impl AccountService {
    pub fn new(config_dir: PathBuf, transport: Arc<ReqwestTransport>) -> Self {
        let store = AccountStore::new(config_dir);
        let initial = match store.load() {
            Ok(records) => records,
            Err(err) => {
                tracing::warn!("failed to load accounts store: {err}");
                Vec::new()
            }
        };
        Self {
            store,
            transport,
            registry: ProviderRegistry::default(),
            records: RwLock::new(initial),
            caches: RwLock::new(HashMap::new()),
            oauth_flows: Arc::new(OAuthFlowRegistry::default()),
        }
    }

    pub fn list_presets(&self) -> Vec<ProviderPresetView> {
        self.registry.all_presets()
    }

    pub async fn list_views(&self) -> Vec<AccountView> {
        let records = self.records.read().await.clone();
        let caches = self.caches.read().await;
        records
            .into_iter()
            .map(|r| {
                let cache = caches.get(&r.id).cloned().unwrap_or_default();
                self.to_account_view(&r, cache)
            })
            .collect()
    }

    pub async fn upsert(&self, req: UpsertAccountRequest) -> Result<AccountView, DaemonError> {
        let label = req.label.trim().to_owned();
        if label.is_empty() {
            return Err(DaemonError::bad_request("account label is required"));
        }
        let driver = self.registry.get(&req.kind)?;

        let mut records = self.records.write().await;
        let requested_id = req.id.as_deref().map(sanitize_id).filter(|s| !s.is_empty());
        let existing_idx = requested_id
            .as_deref()
            .and_then(|id| records.iter().position(|r| r.id == id));

        let id = match requested_id {
            Some(explicit) => explicit,
            None => next_unique_id(&sanitize_id(&label), &records),
        };

        let existing_auth = existing_idx.map(|idx| records[idx].auth.clone());
        let auth = resolve_upsert_auth(&req, driver.allows_no_auth(), existing_auth)?;

        let base_url = req
            .base_url
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(|s| s.trim_end_matches('/').to_owned());

        let mut models = existing_idx
            .map(|idx| records[idx].models.clone())
            .unwrap_or_default();
        for m in req.models {
            let trimmed = m.trim();
            if !trimmed.is_empty() && !models.iter().any(|existing| existing == trimmed) {
                models.push(trimmed.to_owned());
            }
        }

        let record = AccountRecord {
            id: id.clone(),
            label,
            kind: driver.kind().to_owned(),
            base_url,
            auth,
            models,
        };

        if let Some(idx) = existing_idx {
            records[idx] = record.clone();
        } else {
            records.push(record.clone());
        }
        self.store.save(&records).map_err(DaemonError::internal)?;
        drop(records);

        Ok(self.view_for_record(&record).await)
    }

    pub async fn add_model(&self, id: &str, model_id: &str) -> Result<AccountView, DaemonError> {
        let model_id = model_id.trim();
        if model_id.is_empty() {
            return Err(DaemonError::bad_request("model ID cannot be empty"));
        }
        let mut records = self.records.write().await;
        let Some(record) = records.iter_mut().find(|r| r.id == id) else {
            return Err(DaemonError::not_found(format!("account `{id}` not found")));
        };
        if !record.models.iter().any(|m| m == model_id) {
            record.models.push(model_id.to_owned());
        }
        let updated = record.clone();
        self.store.save(&records).map_err(DaemonError::internal)?;
        drop(records);

        Ok(self.view_for_record(&updated).await)
    }

    pub async fn delete(&self, id: &str) -> Result<bool, DaemonError> {
        let mut records = self.records.write().await;
        let before = records.len();
        records.retain(|r| r.id != id);
        if records.len() == before {
            return Ok(false);
        }
        self.store.save(&records).map_err(DaemonError::internal)?;
        drop(records);
        self.caches.write().await.remove(id);
        Ok(true)
    }

    pub(super) async fn save_oauth_account(
        &self,
        requested_id: &str,
        label: &str,
        kind: &str,
        auth: AccountAuth,
    ) -> Result<AccountView, DaemonError> {
        let driver = self.registry.get(kind)?;
        let mut records = self.records.write().await;
        let cleaned_id = sanitize_id(requested_id);
        let id = if cleaned_id.is_empty() {
            next_unique_id(driver.kind(), &records)
        } else {
            cleaned_id
        };
        let label = if label.trim().is_empty() {
            driver.default_label().to_owned()
        } else {
            label.trim().to_owned()
        };

        let existing_idx = records.iter().position(|r| r.id == id);
        let models = existing_idx
            .map(|idx| records[idx].models.clone())
            .unwrap_or_default();
        let base_url = existing_idx.and_then(|idx| records[idx].base_url.clone());

        let record = AccountRecord {
            id: id.clone(),
            label,
            kind: driver.kind().to_owned(),
            base_url,
            auth,
            models,
        };
        if let Some(idx) = existing_idx {
            records[idx] = record.clone();
        } else {
            records.push(record.clone());
        }
        self.store.save(&records).map_err(DaemonError::internal)?;
        drop(records);

        Ok(self.view_for_record(&record).await)
    }

    pub async fn start_oauth(
        self: &Arc<Self>,
        kind: &str,
        id: Option<String>,
        label: Option<String>,
    ) -> Result<OAuthStartResponse, DaemonError> {
        let driver = self.registry.get(kind)?;
        let account_id = self.allocate_oauth_id(id.as_deref(), driver.kind()).await;
        let label = label
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| driver.default_label())
            .to_owned();

        driver
            .start_oauth(OAuthStartContext {
                service: Arc::clone(self),
                transport: Arc::clone(&self.transport),
                flows: Arc::clone(&self.oauth_flows),
                account_id,
                label,
            })
            .await
    }

    pub async fn poll_oauth(&self, flow_id: &str) -> Result<OAuthPollResponse, DaemonError> {
        self.oauth_flows
            .get(flow_id)
            .await
            .ok_or_else(|| DaemonError::not_found(format!("OAuth flow `{flow_id}` not found")))
    }

    pub async fn refresh(&self, id: &str) -> Result<AccountView, DaemonError> {
        let record = self.find_record(id).await?;
        let driver = self.registry.get(&record.kind)?;
        let cache = driver.refresh(&record, self.transport.clone()).await;
        self.caches
            .write()
            .await
            .insert(record.id.clone(), cache.clone());
        Ok(self.to_account_view(&record, cache))
    }

    pub async fn build_provider(
        &self,
        id: &str,
        prompt_cache: Option<bool>,
    ) -> Result<Arc<dyn Provider>, DaemonError> {
        let record = self.find_record(id).await?;
        let driver = self.registry.get(&record.kind)?;
        driver.build_provider(&record, self.transport.clone(), prompt_cache)
    }

    pub async fn context_profile_for(&self, id: &str, model_id: &str) -> Option<ContextProfile> {
        let caches = self.caches.read().await;
        let cache = caches.get(id)?;
        let model = cache.models.iter().find(|m| m.id == model_id)?;
        let max_in = model.max_input_tokens?;
        let max_out = model
            .max_output_tokens
            .unwrap_or(ContextProfile::DEFAULT.max_output_tokens);
        Some(ContextProfile::new(max_in, max_out))
    }

    async fn find_record(&self, id: &str) -> Result<AccountRecord, DaemonError> {
        self.records
            .read()
            .await
            .iter()
            .find(|r| r.id == id)
            .cloned()
            .ok_or_else(|| DaemonError::not_found(format!("account `{id}` not found")))
    }

    async fn view_for_record(&self, record: &AccountRecord) -> AccountView {
        let cache = self
            .caches
            .read()
            .await
            .get(&record.id)
            .cloned()
            .unwrap_or_default();
        self.to_account_view(record, cache)
    }

    async fn allocate_oauth_id(&self, requested: Option<&str>, base: &str) -> String {
        let records = self.records.read().await;
        requested
            .map(sanitize_id)
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| next_unique_id(base, &records))
    }

    fn to_account_view(&self, record: &AccountRecord, mut cache: AccountCache) -> AccountView {
        let driver = self.registry.get(&record.kind).ok();
        let icon = driver.as_deref().map_or("globe", |d| d.icon()).to_owned();
        let default_url = driver.as_deref().map_or("", |d| d.default_base_url());

        let mut models = Vec::with_capacity(cache.models.len() + record.models.len());
        models.append(&mut cache.models);
        for manual in &record.models {
            if !models.iter().any(|m| m.id == *manual) {
                models.push(AccountModelView {
                    id: manual.clone(),
                    display_name: manual.clone(),
                    quota_fraction: None,
                    quota_reset: None,
                    max_input_tokens: None,
                    max_output_tokens: None,
                });
            }
        }
        AccountView {
            id: record.id.clone(),
            label: record.label.clone(),
            kind: record.kind.clone(),
            icon,
            auth_kind: record.auth.auth_kind().to_owned(),
            auth_badge: record.auth.auth_badge().to_owned(),
            base_url: record
                .base_url
                .clone()
                .unwrap_or_else(|| default_url.to_owned()),
            account_label: cache.account_label,
            quota: cache.quota,
            models,
            error: cache.error,
        }
    }
}

fn resolve_upsert_auth(
    req: &UpsertAccountRequest,
    allows_no_auth: bool,
    existing_auth: Option<AccountAuth>,
) -> Result<AccountAuth, DaemonError> {
    let secret = req
        .secret
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);

    match req.auth_kind.trim().to_ascii_lowercase().as_str() {
        "none" => {
            if !allows_no_auth {
                return Err(DaemonError::bad_request(
                    "auth_kind=none is not supported for this provider",
                ));
            }
            Ok(AccountAuth::None)
        }
        "google_token" => match (secret, existing_auth) {
            (Some(token_json), _) => Ok(AccountAuth::GoogleOAuth { token_json }),
            (None, Some(prev @ AccountAuth::GoogleOAuth { .. })) => Ok(prev),
            _ => Err(DaemonError::bad_request(
                "secret token/JSON is required for Google token",
            )),
        },
        "keep" => existing_auth
            .ok_or_else(|| DaemonError::bad_request("auth_kind=keep requires an existing account")),
        _ => match (secret, existing_auth) {
            (Some(key), _) => Ok(AccountAuth::ApiKey { key }),
            (None, Some(prev)) => Ok(prev),
            (None, None) => Err(DaemonError::bad_request("API key secret is required")),
        },
    }
}

fn sanitize_id(raw: &str) -> String {
    let mut out = String::new();
    let mut prev_sep = false;
    for ch in raw.trim().to_ascii_lowercase().chars() {
        if ch.is_ascii_alphanumeric() {
            out.push(ch);
            prev_sep = false;
        } else if (ch == '-' || ch == '_' || ch == ' ') && !prev_sep && !out.is_empty() {
            out.push('_');
            prev_sep = true;
        }
    }
    out.trim_end_matches('_').to_owned()
}

fn next_unique_id(base: &str, existing: &[AccountRecord]) -> String {
    let base = if base.is_empty() { "account" } else { base };
    if !existing.iter().any(|r| r.id == base) {
        return base.to_owned();
    }
    for idx in 2.. {
        let candidate = format!("{base}_{idx}");
        if !existing.iter().any(|r| r.id == candidate) {
            return candidate;
        }
    }
    base.to_owned()
}
