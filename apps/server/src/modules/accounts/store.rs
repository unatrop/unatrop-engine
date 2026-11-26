//! Encrypted persistence (`accounts.json`) and one-time import of legacy desktop token files.

use std::{
    fs,
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use super::crypto::{SecretBox, restrict_file_permissions};
use super::dto::{AccountAuth, AccountRecord};

#[derive(Default, Serialize, Deserialize)]
struct StoredAccountsFile {
    #[serde(default)]
    accounts: Vec<AccountRecord>,
}

pub(crate) struct AccountStore {
    config_dir: PathBuf,
    file_path: PathBuf,
}

impl AccountStore {
    pub(crate) fn new(config_dir: PathBuf) -> Self {
        let file_path = config_dir.join("accounts.json");
        Self {
            config_dir,
            file_path,
        }
    }

    pub(crate) fn load(&self) -> Result<Vec<AccountRecord>, String> {
        if !self.file_path.exists() {
            let migrated = self.import_legacy_tokens();
            if !migrated.is_empty() {
                let _ = self.save(&migrated);
            }
            return Ok(migrated);
        }

        let raw = fs::read_to_string(&self.file_path)
            .map_err(|e| format!("failed to read accounts.json: {e}"))?;
        let mut parsed: StoredAccountsFile =
            serde_json::from_str(&raw).map_err(|e| format!("invalid accounts.json: {e}"))?;
        let secret_box = SecretBox::for_dir(&self.config_dir)?;
        for record in &mut parsed.accounts {
            decrypt_auth(&secret_box, &mut record.auth)?;
        }
        Ok(parsed.accounts)
    }

    pub(crate) fn save(&self, accounts: &[AccountRecord]) -> Result<(), String> {
        fs::create_dir_all(&self.config_dir)
            .map_err(|e| format!("failed to create config directory: {e}"))?;
        let secret_box = SecretBox::for_dir(&self.config_dir)?;
        let mut encrypted = accounts.to_vec();
        for record in &mut encrypted {
            encrypt_auth(&secret_box, &mut record.auth)?;
        }
        let body = serde_json::to_string_pretty(&StoredAccountsFile {
            accounts: encrypted,
        })
        .map_err(|e| format!("failed to serialize accounts.json: {e}"))?;

        let tmp = self.config_dir.join("accounts.json.tmp");
        fs::write(&tmp, body).map_err(|e| format!("failed to write accounts temp file: {e}"))?;
        fs::rename(&tmp, &self.file_path)
            .map_err(|e| format!("failed to persist accounts.json: {e}"))?;
        restrict_file_permissions(&self.file_path);
        Ok(())
    }

    fn import_legacy_tokens(&self) -> Vec<AccountRecord> {
        let mut imported = Vec::new();
        let Ok(secret_box) = SecretBox::for_dir(&self.config_dir) else {
            return imported;
        };

        if let Some(token) = read_legacy_secret(&secret_box, &self.config_dir.join("copilot-token"))
        {
            imported.push(AccountRecord {
                id: "copilot".to_owned(),
                label: "GitHub Copilot".to_owned(),
                kind: "copilot".to_owned(),
                base_url: None,
                auth: AccountAuth::GithubOAuth { token },
                models: Vec::new(),
            });
        }
        if let Some(token_json) =
            read_legacy_secret(&secret_box, &self.config_dir.join("antigravity-token.json"))
        {
            imported.push(AccountRecord {
                id: "antigravity".to_owned(),
                label: "Google Antigravity".to_owned(),
                kind: "antigravity".to_owned(),
                base_url: None,
                auth: AccountAuth::GoogleOAuth { token_json },
                models: Vec::new(),
            });
        }
        imported
    }
}

fn read_legacy_secret(secret_box: &SecretBox, path: &Path) -> Option<String> {
    let raw = fs::read_to_string(path).ok()?;
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    secret_box
        .decrypt_if_encrypted(trimmed)
        .ok()
        .map(|s| s.trim().to_owned())
        .filter(|s| !s.is_empty())
}

fn encrypt_auth(secret_box: &SecretBox, auth: &mut AccountAuth) -> Result<(), String> {
    match auth {
        AccountAuth::ApiKey { key } => {
            *key = secret_box.encrypt_if_plain(key)?;
        }
        AccountAuth::GithubOAuth { token } => {
            *token = secret_box.encrypt_if_plain(token)?;
        }
        AccountAuth::GoogleOAuth { token_json } => {
            *token_json = secret_box.encrypt_if_plain(token_json)?;
        }
        AccountAuth::None => {}
    }
    Ok(())
}

fn decrypt_auth(secret_box: &SecretBox, auth: &mut AccountAuth) -> Result<(), String> {
    match auth {
        AccountAuth::ApiKey { key } => {
            *key = secret_box.decrypt_if_encrypted(key)?;
        }
        AccountAuth::GithubOAuth { token } => {
            *token = secret_box.decrypt_if_encrypted(token)?;
        }
        AccountAuth::GoogleOAuth { token_json } => {
            *token_json = secret_box.decrypt_if_encrypted(token_json)?;
        }
        AccountAuth::None => {}
    }
    Ok(())
}
