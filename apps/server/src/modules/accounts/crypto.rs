//! AES-256-GCM + Windows DPAPI secret storage compatible with `~/.unatrop/config/master.key`.

use std::{fs, path::Path};

use base64::{Engine as _, engine::general_purpose::STANDARD};
use ring::{
    aead::{AES_256_GCM, Aad, LessSafeKey, Nonce, UnboundKey},
    rand::{SecureRandom, SystemRandom},
};
use zeroize::Zeroizing;

const ENC_PREFIX: &str = "enc:v1:";
const DPAPI_PREFIX: &str = "dpapi:v1:";
const KEY_FILE_NAME: &str = "master.key";
const KEY_LEN: usize = 32;
const NONCE_LEN: usize = 12;

#[cfg(windows)]
fn dpapi_protect(plain: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(plain.len())
            .map_err(|_| "master key exceeds DPAPI size".to_owned())?,
        pbData: plain.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptProtectData(
            &raw const input,
            std::ptr::null(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut output,
        )
    };
    if ok == 0 || output.pbData.is_null() {
        return Err("DPAPI CryptProtectData failed".to_owned());
    }
    let slice = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let protected = slice.to_vec();
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(protected)
}

#[cfg(windows)]
fn dpapi_unprotect(cipher: &[u8]) -> Result<Vec<u8>, String> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let input = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(cipher.len())
            .map_err(|_| "protected key exceeds limit".to_owned())?,
        pbData: cipher.as_ptr().cast_mut(),
    };
    let mut output = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };
    let ok = unsafe {
        CryptUnprotectData(
            &raw const input,
            std::ptr::null_mut(),
            std::ptr::null(),
            std::ptr::null_mut(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut output,
        )
    };
    if ok == 0 || output.pbData.is_null() {
        return Err("DPAPI CryptUnprotectData failed".to_owned());
    }
    let slice = unsafe { std::slice::from_raw_parts(output.pbData, output.cbData as usize) };
    let plain = slice.to_vec();
    unsafe {
        LocalFree(output.pbData.cast());
    }
    Ok(plain)
}

fn encode_master_key_for_disk(raw: &[u8; KEY_LEN]) -> Result<String, String> {
    #[cfg(windows)]
    {
        let protected = dpapi_protect(raw)?;
        Ok(format!("{DPAPI_PREFIX}{}", STANDARD.encode(protected)))
    }
    #[cfg(not(windows))]
    {
        Ok(STANDARD.encode(raw))
    }
}

fn decode_master_key_from_disk(encoded: &str) -> Result<([u8; KEY_LEN], bool), String> {
    let trimmed = encoded.trim();
    if let Some(b64) = trimmed.strip_prefix(DPAPI_PREFIX) {
        #[cfg(windows)]
        {
            let cipher = STANDARD
                .decode(b64.trim())
                .map_err(|e| format!("invalid protected master key: {e}"))?;
            let raw = dpapi_unprotect(&cipher)?;
            if raw.len() != KEY_LEN {
                return Err("master key length mismatch".to_owned());
            }
            let mut buf = [0u8; KEY_LEN];
            buf.copy_from_slice(&raw);
            return Ok((buf, false));
        }
        #[cfg(not(windows))]
        {
            let _ = b64;
            return Err("DPAPI-protected master key is only supported on Windows".to_owned());
        }
    }

    let raw = STANDARD
        .decode(trimmed)
        .map_err(|e| format!("invalid master key: {e}"))?;
    if raw.len() != KEY_LEN {
        return Err("master key length mismatch".to_owned());
    }
    let mut buf = [0u8; KEY_LEN];
    buf.copy_from_slice(&raw);
    Ok((buf, cfg!(windows)))
}

pub(crate) struct SecretBox {
    key: Zeroizing<[u8; KEY_LEN]>,
}

impl SecretBox {
    pub(crate) fn for_dir(dir: &Path) -> Result<Self, String> {
        fs::create_dir_all(dir).map_err(|e| format!("failed to create config dir: {e}"))?;
        restrict_dir_permissions(dir);
        let key_path = dir.join(KEY_FILE_NAME);
        let key = if key_path.exists() {
            let encoded = fs::read_to_string(&key_path)
                .map_err(|e| format!("failed to read master.key: {e}"))?;
            let (raw_buf, needs_migration) = decode_master_key_from_disk(&encoded)?;
            let mut buf = Zeroizing::new(raw_buf);
            if needs_migration && let Ok(upgraded) = encode_master_key_for_disk(&buf) {
                let tmp = dir.join("master.key.tmp");
                if fs::write(&tmp, upgraded).is_ok() {
                    let _ = fs::rename(&tmp, &key_path);
                    restrict_file_permissions(&key_path);
                }
            }
            let _ = &mut buf;
            buf
        } else {
            let mut buf = Zeroizing::new([0u8; KEY_LEN]);
            SystemRandom::new()
                .fill(buf.as_mut())
                .map_err(|_| "failed to generate master key".to_owned())?;
            let encoded = encode_master_key_for_disk(&buf)?;
            fs::write(&key_path, encoded)
                .map_err(|e| format!("failed to write master.key: {e}"))?;
            restrict_file_permissions(&key_path);
            buf
        };
        Ok(Self { key })
    }

    pub(crate) fn encrypt_if_plain(&self, value: &str) -> Result<String, String> {
        if value.is_empty() || value.starts_with(ENC_PREFIX) {
            return Ok(value.to_owned());
        }
        let mut nonce_bytes = [0u8; NONCE_LEN];
        SystemRandom::new()
            .fill(&mut nonce_bytes)
            .map_err(|_| "failed to generate nonce".to_owned())?;

        let unbound = UnboundKey::new(&AES_256_GCM, self.key.as_ref())
            .map_err(|_| "invalid encryption key".to_owned())?;
        let sealing_key = LessSafeKey::new(unbound);

        let mut in_out = value.as_bytes().to_vec();
        sealing_key
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::empty(),
                &mut in_out,
            )
            .map_err(|_| "failed to encrypt secret".to_owned())?;

        let mut payload = Vec::with_capacity(NONCE_LEN + in_out.len());
        payload.extend_from_slice(&nonce_bytes);
        payload.extend_from_slice(&in_out);
        Ok(format!("{ENC_PREFIX}{}", STANDARD.encode(payload)))
    }

    pub(crate) fn decrypt_if_encrypted(&self, value: &str) -> Result<String, String> {
        let Some(b64) = value.strip_prefix(ENC_PREFIX) else {
            return Ok(value.to_owned());
        };
        let mut payload = STANDARD
            .decode(b64.trim())
            .map_err(|e| format!("invalid encrypted secret: {e}"))?;
        if payload.len() <= NONCE_LEN {
            return Err("truncated encrypted secret".to_owned());
        }
        let (nonce_slice, cipher_slice) = payload.split_at_mut(NONCE_LEN);
        let mut nonce_bytes = [0u8; NONCE_LEN];
        nonce_bytes.copy_from_slice(nonce_slice);

        let unbound = UnboundKey::new(&AES_256_GCM, self.key.as_ref())
            .map_err(|_| "invalid decryption key".to_owned())?;
        let opening_key = LessSafeKey::new(unbound);
        let plain = opening_key
            .open_in_place(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::empty(),
                cipher_slice,
            )
            .map_err(|_| "failed to decrypt secret".to_owned())?;

        String::from_utf8(plain.to_vec()).map_err(|e| format!("decrypted secret is not UTF-8: {e}"))
    }
}

pub(crate) fn restrict_dir_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o700);
            let _ = fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}

pub(crate) fn restrict_file_permissions(path: &Path) {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = fs::metadata(path) {
            let mut perms = meta.permissions();
            perms.set_mode(0o600);
            let _ = fs::set_permissions(path, perms);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = path;
    }
}
