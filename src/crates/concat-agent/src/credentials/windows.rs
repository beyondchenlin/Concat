// SPDX-License-Identifier: AGPL-3.0-or-later

//! Current-user Windows Credential Manager, with non-roaming persistence.
//! Never enumerate or import credentials belonging to another product.

use std::ptr::NonNull;

use windows::Win32::Foundation::ERROR_NOT_FOUND;
use windows::Win32::Security::Credentials::{
    CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredDeleteW, CredFree, CredReadW,
    CredWriteW,
};
use windows::core::{HRESULT, PCWSTR, PWSTR};
use zeroize::Zeroize;

use super::{CredentialError, CredentialScope, CredentialStore, MAX_SECRET_BYTES, Secret};

/// Current-user, non-roaming Windows Credential Manager backend.
pub struct WindowsCredentialStore;

fn target(scope: &CredentialScope) -> Vec<u16> {
    format!("BKCAT:model-credential:v1:{}", scope.key_id())
        .encode_utf16()
        .chain(Some(0))
        .collect()
}

/// CredReadW returns a single allocation owned by CredFree. Wipe its secret
/// bytes before releasing it, including malformed-value and early-error paths.
struct ReadCredential(NonNull<CREDENTIALW>);

impl Drop for ReadCredential {
    fn drop(&mut self) {
        // SAFETY: the system returned this complete allocation on success;
        // its documented blob pointer/length remain valid until CredFree.
        unsafe {
            let credential = self.0.as_ref();
            if credential.CredentialBlobSize > 0 && !credential.CredentialBlob.is_null() {
                std::slice::from_raw_parts_mut(
                    credential.CredentialBlob,
                    credential.CredentialBlobSize as usize,
                )
                .zeroize();
            }
            CredFree(self.0.as_ptr().cast());
        }
    }
}

impl CredentialStore for WindowsCredentialStore {
    fn read(&self, scope: &CredentialScope) -> Result<Option<Secret>, CredentialError> {
        let target = target(scope);
        let mut allocation = std::ptr::null_mut();
        // SAFETY: the target is NUL-terminated and allocation is a live out slot.
        let result = unsafe {
            CredReadW(
                PCWSTR(target.as_ptr()),
                CRED_TYPE_GENERIC,
                None,
                &mut allocation,
            )
        };
        if let Err(error) = result {
            return if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) {
                Ok(None)
            } else {
                Err(CredentialError::Unavailable)
            };
        }
        let owned = ReadCredential(NonNull::new(allocation).ok_or(CredentialError::Corrupt)?);
        // SAFETY: owned retains the returned allocation until after copying.
        let stored = unsafe { owned.0.as_ref() };
        let length = stored.CredentialBlobSize as usize;
        if stored.Type != CRED_TYPE_GENERIC
            || length == 0
            || length > MAX_SECRET_BYTES
            || stored.CredentialBlob.is_null()
        {
            return Err(CredentialError::Corrupt);
        }
        // SAFETY: the system's allocation contains this documented blob.
        let bytes = unsafe { std::slice::from_raw_parts(stored.CredentialBlob, length) };
        let value = std::str::from_utf8(bytes).map_err(|_| CredentialError::Corrupt)?;
        Secret::new(value.to_owned())
            .map(Some)
            .map_err(|_| CredentialError::Corrupt)
    }

    fn write(&self, scope: &CredentialScope, secret: &Secret) -> Result<(), CredentialError> {
        let mut target = target(scope);
        let mut user: Vec<u16> = "BK CAT".encode_utf16().chain(Some(0)).collect();
        let bytes = secret.expose_secret().as_bytes();
        let credential = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            CredentialBlobSize: bytes.len() as u32,
            CredentialBlob: bytes.as_ptr().cast_mut(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            UserName: PWSTR(user.as_mut_ptr()),
            ..CREDENTIALW::default()
        };
        // SAFETY: CredWriteW only reads all referenced buffers during this
        // call. Persistence is current-user/current-machine, never roaming.
        unsafe { CredWriteW(&credential, 0) }.map_err(|_| CredentialError::Unavailable)
    }

    fn delete(&self, scope: &CredentialScope) -> Result<(), CredentialError> {
        let target = target(scope);
        // SAFETY: target is a live NUL-terminated product-specific identity.
        match unsafe { CredDeleteW(PCWSTR(target.as_ptr()), CRED_TYPE_GENERIC, None) } {
            Ok(()) => Ok(()),
            Err(error) if error.code() == HRESULT::from_win32(ERROR_NOT_FOUND.0) => Ok(()),
            Err(_) => Err(CredentialError::Unavailable),
        }
    }
}
