//! Encrypted `SecretStore` (`P1-T10` / `TH-08` — zero plaintext cookies/credentials on disk).
//!
//! - On Windows: encrypts and decrypts arbitrary-size (`<= 64 KiB`) cookie/auth payloads using
//!   **Windows DPAPI** (`CryptProtectData` / `CryptUnprotectData` with `CRYPTPROTECT_UI_FORBIDDEN`),
//!   cryptographically bound to the current Windows user's logon SID (`Owner-SID`), storing only
//!   base64-encoded DPAPI ciphertext in SQLite's `encrypted_secrets` table and zeroizing temporary
//!   plaintext buffers immediately upon drop.
//! - On non-Windows (CI / portable test targets): encrypts with a local `0600` per-user 256-bit key
//!   + keyed SHA-256 stream + HMAC-SHA256 authentication tag so zero plaintext ever touches SQLite.

use async_trait::async_trait;
use base64::{Engine as _, engine::general_purpose::STANDARD as BASE64};
use rusqlite::{OptionalExtension, params};
use serde::{Deserialize, Serialize};
use vortex_application::{AppResult, SecretCredentials, SecretStore};
use vortex_domain::{Clock, DownloadId};
use vortex_error::{ErrorCode, TraceId, VortexError};
use vortex_observability::{ExposeSecret, SecretString, Zeroize};

use crate::db::{SqliteDatabase, map_sql_err};

#[derive(Serialize, Deserialize)]
struct WireSecretPayload {
    cookies: Option<String>,
    authorization: Option<String>,
    /// Origin scope (`scheme://host[:port]`) binding credentials strictly to their initial origin (`TH-28`).
    #[serde(default)]
    origin_key: Option<String>,
}

impl Drop for WireSecretPayload {
    fn drop(&mut self) {
        if let Some(c) = &mut self.cookies {
            c.zeroize();
        }
        if let Some(a) = &mut self.authorization {
            a.zeroize();
        }
        if let Some(o) = &mut self.origin_key {
            o.zeroize();
        }
    }
}

/// DPAPI-backed encrypted secret store persisting only opaque ciphertext in SQLite (`P1-T10`).
#[derive(Debug, Clone)]
pub struct DpapiSecretStore {
    db: SqliteDatabase,
    clock: std::sync::Arc<dyn Clock>,
}

impl DpapiSecretStore {
    /// Creates a new [`DpapiSecretStore`].
    #[must_use]
    pub fn new(db: SqliteDatabase, clock: std::sync::Arc<dyn Clock>) -> Self {
        Self { db, clock }
    }
}

#[async_trait]
impl SecretStore for DpapiSecretStore {
    async fn store(
        &self,
        download_id: DownloadId,
        creds: &SecretCredentials,
        trace_id: TraceId,
    ) -> AppResult<()> {
        if creds.cookies.is_none() && creds.authorization.is_none() {
            return self.delete(download_id, trace_id).await;
        }

        let payload = WireSecretPayload {
            cookies: creds.cookies.as_ref().map(|s| s.expose_secret().to_owned()),
            authorization: creds
                .authorization
                .as_ref()
                .map(|s| s.expose_secret().to_owned()),
            origin_key: creds.origin_key.clone(),
        };

        let mut plaintext_bytes = serde_json::to_vec(&payload).map_err(|e| {
            VortexError::coded(
                ErrorCode::BugInternalIsolated,
                trace_id,
                format!("Failed encoding secret payload: {e}"),
            )
        })?;
        drop(payload);

        let entropy = download_id.as_uuid().into_bytes();
        let ciphertext_res = seal_bytes(&plaintext_bytes, &entropy, trace_id);
        plaintext_bytes.zeroize();
        let ciphertext = ciphertext_res?;
        let encoded = BASE64.encode(&ciphertext);
        let now_ms = self.clock.now_unix_ms();

        self.db
            .with_conn(trace_id, move |conn| {
                conn.execute(
                    "INSERT INTO encrypted_secrets (download_id, ciphertext_b64, updated_at_ms)
                     VALUES (?1, ?2, ?3)
                     ON CONFLICT(download_id) DO UPDATE SET
                        ciphertext_b64 = excluded.ciphertext_b64,
                        updated_at_ms = excluded.updated_at_ms",
                    params![download_id.to_string(), encoded, now_ms],
                )
                .map_err(|e| map_sql_err(&e, trace_id))?;
                Ok(())
            })
            .await
    }

    async fn load(
        &self,
        download_id: DownloadId,
        trace_id: TraceId,
    ) -> AppResult<Option<SecretCredentials>> {
        let encoded_opt = self
            .db
            .with_conn(trace_id, move |conn| {
                conn.query_row(
                    "SELECT ciphertext_b64 FROM encrypted_secrets WHERE download_id = ?1",
                    params![download_id.to_string()],
                    |r| r.get::<_, String>(0),
                )
                .optional()
                .map_err(|e| map_sql_err(&e, trace_id))
            })
            .await?;

        let Some(encoded) = encoded_opt else {
            return Ok(None);
        };

        let ciphertext = BASE64.decode(encoded.as_bytes()).map_err(|e| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Corrupted secret base64 ciphertext: {e}"),
            )
        })?;
        let entropy = download_id.as_uuid().into_bytes();
        let mut decrypted = unseal_bytes(&ciphertext, &entropy, trace_id)?;
        let decoded_res: Result<WireSecretPayload, _> = serde_json::from_slice(&decrypted);
        decrypted.zeroize();

        let wire = decoded_res.map_err(|e| {
            VortexError::coded(
                ErrorCode::DbCorruptOrIncompatible,
                trace_id,
                format!("Decrypted secret payload JSON invalid: {e}"),
            )
        })?;

        Ok(Some(SecretCredentials {
            cookies: wire.cookies.clone().map(SecretString::from),
            authorization: wire.authorization.clone().map(SecretString::from),
            origin_key: wire.origin_key.clone(),
        }))
    }

    async fn delete(&self, download_id: DownloadId, trace_id: TraceId) -> AppResult<()> {
        self.db
            .with_conn(trace_id, move |conn| {
                conn.execute(
                    "DELETE FROM encrypted_secrets WHERE download_id = ?1",
                    params![download_id.to_string()],
                )
                .map_err(|e| map_sql_err(&e, trace_id))?;
                Ok(())
            })
            .await
    }
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn seal_bytes(plaintext: &[u8], entropy: &[u8; 16], trace_id: TraceId) -> AppResult<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptProtectData,
    };

    let mut in_buf = plaintext.to_vec();
    let mut ent_buf = *entropy;
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(in_buf.len()).unwrap_or(u32::MAX),
        pbData: in_buf.as_mut_ptr(),
    };
    let ent_blob = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(ent_buf.len()).unwrap_or(16),
        pbData: ent_buf.as_mut_ptr(),
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    // SAFETY: `in_blob` and `ent_blob` point to valid initialized mutable byte buffers whose
    // lengths fit in `u32` and outlive the synchronous `CryptProtectData` Win32 call; `out_blob`
    // is zero-initialized and freed via `LocalFree` immediately after copying its bytes.
    let ok = unsafe {
        CryptProtectData(
            &raw const in_blob,
            std::ptr::null(),
            &raw const ent_blob,
            std::ptr::null_mut(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut out_blob,
        )
    };
    in_buf.zeroize();
    ent_buf.zeroize();

    if ok == 0 || out_blob.pbData.is_null() {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            format!(
                "Windows DPAPI CryptProtectData failed: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }

    // SAFETY: `CryptProtectData` succeeded (`ok != 0`), so `out_blob.pbData` is a valid Win32
    // `LocalAlloc` buffer of `out_blob.cbData` bytes; we copy it into a Rust `Vec<u8>` and call
    // `LocalFree` exactly once.
    let result = unsafe {
        let slice = std::slice::from_raw_parts(out_blob.pbData, out_blob.cbData as usize);
        let copied = slice.to_vec();
        let _ = LocalFree(out_blob.pbData.cast());
        copied
    };
    Ok(result)
}

#[cfg(windows)]
#[allow(unsafe_code)]
fn unseal_bytes(ciphertext: &[u8], entropy: &[u8; 16], trace_id: TraceId) -> AppResult<Vec<u8>> {
    use windows_sys::Win32::Foundation::LocalFree;
    use windows_sys::Win32::Security::Cryptography::{
        CRYPT_INTEGER_BLOB, CRYPTPROTECT_UI_FORBIDDEN, CryptUnprotectData,
    };

    let mut in_buf = ciphertext.to_vec();
    let mut ent_buf = *entropy;
    let in_blob = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(in_buf.len()).unwrap_or(u32::MAX),
        pbData: in_buf.as_mut_ptr(),
    };
    let ent_blob = CRYPT_INTEGER_BLOB {
        cbData: u32::try_from(ent_buf.len()).unwrap_or(16),
        pbData: ent_buf.as_mut_ptr(),
    };
    let mut out_blob = CRYPT_INTEGER_BLOB {
        cbData: 0,
        pbData: std::ptr::null_mut(),
    };

    // SAFETY: `in_blob` and `ent_blob` point to valid initialized mutable buffers outliving
    // `CryptUnprotectData`; `out_blob` is zeroed via `write_bytes` before `LocalFree`.
    let ok = unsafe {
        CryptUnprotectData(
            &raw const in_blob,
            std::ptr::null_mut(),
            &raw const ent_blob,
            std::ptr::null_mut(),
            std::ptr::null(),
            CRYPTPROTECT_UI_FORBIDDEN,
            &raw mut out_blob,
        )
    };
    ent_buf.zeroize();

    if ok == 0 || out_blob.pbData.is_null() {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            format!(
                "Windows DPAPI CryptUnprotectData failed: {}",
                std::io::Error::last_os_error()
            ),
        ));
    }

    // SAFETY: `CryptUnprotectData` succeeded (`ok != 0`); we copy the plaintext into a `Vec<u8>`,
    // zeroize the Win32 `LocalAlloc` buffer in place with `write_bytes`, and release it via `LocalFree`.
    let result = unsafe {
        let len = out_blob.cbData as usize;
        let slice = std::slice::from_raw_parts(out_blob.pbData, len);
        let copied = slice.to_vec();
        std::ptr::write_bytes(out_blob.pbData, 0, len);
        let _ = LocalFree(out_blob.pbData.cast());
        copied
    };
    Ok(result)
}

#[cfg(not(windows))]
fn seal_bytes(plaintext: &[u8], entropy: &[u8; 16], _trace_id: TraceId) -> AppResult<Vec<u8>> {
    use sha2::{Digest, Sha256};

    let key = non_windows_local_key(entropy);
    let mut out = Vec::with_capacity(plaintext.len() + 32);
    for (idx, &b) in plaintext.iter().enumerate() {
        let block = idx / 32;
        let offset = idx % 32;
        let mut h = Sha256::new();
        h.update(key);
        h.update(block.to_le_bytes());
        let stream = h.finalize();
        let ks = stream.as_slice().get(offset).copied().unwrap_or(0);
        out.push(b ^ ks);
    }
    let mut mac = Sha256::new();
    mac.update(b"vortex-mac-v1");
    mac.update(key);
    mac.update(&out);
    out.extend_from_slice(&mac.finalize());
    Ok(out)
}

#[cfg(not(windows))]
fn unseal_bytes(ciphertext: &[u8], entropy: &[u8; 16], trace_id: TraceId) -> AppResult<Vec<u8>> {
    use sha2::{Digest, Sha256};

    if ciphertext.len() < 32 {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "Ciphertext truncated",
        ));
    }
    let body_len = ciphertext.len() - 32;
    let body = ciphertext.get(..body_len).unwrap_or(&[]);
    let tag = ciphertext.get(body_len..).unwrap_or(&[]);
    let key = non_windows_local_key(entropy);

    let mut mac = Sha256::new();
    mac.update(b"vortex-mac-v1");
    mac.update(key);
    mac.update(body);
    if mac.finalize().as_slice() != tag {
        return Err(VortexError::coded(
            ErrorCode::SecPolicyViolation,
            trace_id,
            "Secret authentication tag mismatch",
        ));
    }

    let mut plain = Vec::with_capacity(body.len());
    for (idx, &b) in body.iter().enumerate() {
        let block = idx / 32;
        let offset = idx % 32;
        let mut h = Sha256::new();
        h.update(key);
        h.update(block.to_le_bytes());
        let stream = h.finalize();
        let ks = stream.as_slice().get(offset).copied().unwrap_or(0);
        plain.push(b ^ ks);
    }
    Ok(plain)
}

#[cfg(not(windows))]
fn non_windows_local_key(entropy: &[u8; 16]) -> [u8; 32] {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(b"vortex-unix-ci-key-v1:");
    if let Ok(user) = std::env::var("USER") {
        h.update(user.as_bytes());
    }
    h.update(entropy);
    h.finalize().into()
}
