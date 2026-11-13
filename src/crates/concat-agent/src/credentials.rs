// SPDX-License-Identifier: AGPL-3.0-or-later

//! Product-scoped system credentials. Configuration and credentials are
//! separate: storing a secret never grants network or scheduling authority.
//! There is deliberately no plaintext-file or environment-variable fallback.

use std::fmt;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use zeroize::Zeroizing;

#[cfg(target_os = "windows")]
mod windows;
#[cfg(target_os = "windows")]
pub use windows::WindowsCredentialStore;

/// Maximum portable credential payload accepted by both platform stores.
pub const MAX_SECRET_BYTES: usize = 2560;

/// Fixed error categories; neither backend errors nor supplied secrets are
/// interpolated into user messages or Debug output.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialError {
    /// The channel or service identity is not a bounded, valid scope.
    InvalidScope,
    /// The supplied secret is empty, too long or contains control characters.
    InvalidSecret,
    /// This platform has no installed secure storage adapter.
    Unsupported,
    /// Access or persistence failed; never interpreted as an absent secret.
    Unavailable,
    /// Existing bytes cannot be authenticated, decoded or validated.
    Corrupt,
}

impl fmt::Display for CredentialError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::InvalidScope => "凭据渠道或服务来源无效",
            Self::InvalidSecret => "密钥不能为空、包含控制字符或超过 2560 字节",
            Self::Unsupported => "当前平台尚未接入系统安全凭据存储",
            Self::Unavailable => "无法访问系统安全凭据存储，未降级为明文文件",
            Self::Corrupt => "已存凭据无法验证或解密，请重新设置",
        })
    }
}

impl std::error::Error for CredentialError {}

/// Hashes the complete, configuration-normalized service base, including its
/// path. Changing a destination must not silently reuse a previous key.
/// Only the digest survives construction; the vault target exposes no URL.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialScope {
    key_id: String,
}

impl CredentialScope {
    /// Bind a channel instance to the complete normalized service base.
    pub fn new(channel_id: &str, service_origin: &str) -> Result<Self, CredentialError> {
        if channel_id.is_empty()
            || channel_id.len() > 128
            || !channel_id
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
            || service_origin.is_empty()
            || service_origin.len() > 4096
            || service_origin.trim() != service_origin
            || service_origin.chars().any(char::is_control)
        {
            return Err(CredentialError::InvalidScope);
        }
        let mut digest = Sha256::new();
        digest.update(b"app.concat.editor/bk-cat/model-credential/v1\0");
        for value in [channel_id, service_origin] {
            digest.update((value.len() as u64).to_be_bytes());
            digest.update(value.as_bytes());
        }
        Ok(Self {
            key_id: format!("{:x}", digest.finalize()),
        })
    }

    /// Non-secret opaque storage identity; never a credential or network grant.
    pub fn key_id(&self) -> &str {
        &self.key_id
    }
}

impl fmt::Debug for CredentialScope {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("CredentialScope([product-scoped])")
    }
}

/// An owned secret with a redacted Debug implementation and zeroing on drop.
/// It intentionally implements neither Clone, Display nor serialization.
pub struct Secret(Zeroizing<String>);

impl Secret {
    /// Take ownership, validate and arrange erasure on every exit path.
    pub fn new(value: String) -> Result<Self, CredentialError> {
        let value = Zeroizing::new(value);
        if value.is_empty()
            || value.len() > MAX_SECRET_BYTES
            || value.trim().is_empty()
            || value.chars().any(char::is_control)
        {
            return Err(CredentialError::InvalidSecret);
        }
        Ok(Self(value))
    }

    /// Explicit native-only access. Do not pass this value to UI status,
    /// logs, project serialization, conversation storage or model tools.
    pub fn expose_secret(&self) -> &str {
        self.0.as_str()
    }
}

impl fmt::Debug for Secret {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str("Secret([redacted])")
    }
}

/// A read-and-decrypt result, distinct from any backend error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CredentialStatus {
    /// No secret exists for this exact channel and service binding.
    Missing,
    /// The exact stored item was read and validated successfully.
    Present,
}

/// Trusted native settings own mutation. These methods grant no authority to
/// send a model request. Read failures must never be represented as Missing.
pub trait CredentialStore: Send + Sync {
    /// Read only this product-scoped item; absence and failure differ.
    fn read(&self, scope: &CredentialScope) -> Result<Option<Secret>, CredentialError>;
    /// Explicitly store or replace this item in the system vault.
    fn write(&self, scope: &CredentialScope, secret: &Secret) -> Result<(), CredentialError>;
    /// Idempotent only for an absent item, not for a backend failure.
    fn delete(&self, scope: &CredentialScope) -> Result<(), CredentialError>;

    /// Actually read and validate the item, then discard the secret.
    fn status(&self, scope: &CredentialScope) -> Result<CredentialStatus, CredentialError> {
        Ok(if self.read(scope)?.is_some() {
            CredentialStatus::Present
        } else {
            CredentialStatus::Missing
        })
    }
}

#[cfg(target_os = "android")]
static PLATFORM_STORE: std::sync::OnceLock<Arc<dyn CredentialStore>> = std::sync::OnceLock::new();

/// Keep the first application-scoped adapter for the entire process.
/// Returning that exact adapter makes later activity installation idempotent.
#[cfg(any(target_os = "android", test))]
fn install_once(
    slot: &std::sync::OnceLock<Arc<dyn CredentialStore>>,
    store: Arc<dyn CredentialStore>,
) -> Arc<dyn CredentialStore> {
    Arc::clone(slot.get_or_init(|| store))
}

/// Android installs an application-context/KeyStore adapter before the native
/// window. Repeated installation returns the existing adapter, never replaces it.
#[cfg(target_os = "android")]
pub fn install_platform_store(
    store: Arc<dyn CredentialStore>,
) -> Result<Arc<dyn CredentialStore>, CredentialError> {
    Ok(install_once(&PLATFORM_STORE, store))
}

/// Obtain the secure platform adapter without opening a network channel.
pub fn platform_store() -> Result<Arc<dyn CredentialStore>, CredentialError> {
    #[cfg(target_os = "windows")]
    {
        Ok(Arc::new(WindowsCredentialStore))
    }
    #[cfg(target_os = "android")]
    {
        PLATFORM_STORE
            .get()
            .cloned()
            .ok_or(CredentialError::Unavailable)
    }
    #[cfg(not(any(target_os = "windows", target_os = "android")))]
    {
        Err(CredentialError::Unsupported)
    }
}

#[cfg(test)]
mod installation_tests {
    use super::*;

    struct EmptyStore;
    impl CredentialStore for EmptyStore {
        fn read(&self, _: &CredentialScope) -> Result<Option<Secret>, CredentialError> {
            Ok(None)
        }
        fn write(&self, _: &CredentialScope, _: &Secret) -> Result<(), CredentialError> {
            Err(CredentialError::Unavailable)
        }
        fn delete(&self, _: &CredentialScope) -> Result<(), CredentialError> {
            Err(CredentialError::Unavailable)
        }
    }

    #[test]
    fn repeated_application_installation_keeps_the_original_store() {
        let slot = std::sync::OnceLock::new();
        let original: Arc<dyn CredentialStore> = Arc::new(EmptyStore);
        let later_activity: Arc<dyn CredentialStore> = Arc::new(EmptyStore);
        let first = install_once(&slot, Arc::clone(&original));
        let second = install_once(&slot, Arc::clone(&later_activity));
        assert!(Arc::ptr_eq(&first, &original));
        assert!(Arc::ptr_eq(&second, &original));
        assert!(!Arc::ptr_eq(&second, &later_activity));
        assert!(Arc::ptr_eq(slot.get().unwrap(), &original));
    }
}
