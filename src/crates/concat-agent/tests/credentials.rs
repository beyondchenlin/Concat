// SPDX-License-Identifier: AGPL-3.0-or-later
//! Credential scope/error tests and explicitly requested isolated OS-vault checks.

use concat_agent::credentials::{
    CredentialError, CredentialScope, CredentialStore, MAX_SECRET_BYTES, Secret,
};

#[test]
fn scope_separates_channel_and_complete_destination_without_exposing_them() {
    let original = CredentialScope::new("channel-a", "https://example.invalid/v1").unwrap();
    assert_eq!(
        original,
        CredentialScope::new("channel-a", "https://example.invalid/v1").unwrap()
    );
    for (channel, source) in [
        ("channel-b", "https://example.invalid/v1"),
        ("channel-a", "https://other.invalid/v1"),
        ("channel-a", "https://example.invalid:8443/v1"),
        ("channel-a", "https://example.invalid/v2"),
        ("channel-a", "http://example.invalid/v1"),
    ] {
        assert_ne!(original, CredentialScope::new(channel, source).unwrap());
    }
    assert_eq!(original.key_id().len(), 64);
    assert!(
        original
            .key_id()
            .bytes()
            .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
    );
    assert!(!format!("{original:?}").contains("example.invalid"));
    assert!(!format!("{original:?}").contains("channel-a"));
    // Field lengths make concatenation ambiguity impossible.
    assert_ne!(
        CredentialScope::new("ab", "c").unwrap(),
        CredentialScope::new("a", "bc").unwrap()
    );
}

#[test]
fn invalid_scope_and_secret_errors_do_not_echo_the_input() {
    for channel in ["", "../other", "a\0b", "a b"] {
        assert_eq!(
            CredentialScope::new(channel, "https://example.invalid").unwrap_err(),
            CredentialError::InvalidScope
        );
    }
    for source in ["", " trailing ", "line\nbreak"] {
        assert_eq!(
            CredentialScope::new("channel", source).unwrap_err(),
            CredentialError::InvalidScope
        );
    }
    for input in [
        String::new(),
        "  ".into(),
        "line\nbreak".into(),
        "\0".into(),
        "x".repeat(MAX_SECRET_BYTES + 1),
    ] {
        let error = Secret::new(input).unwrap_err();
        assert_eq!(error, CredentialError::InvalidSecret);
        assert!(!format!("{error}").contains("line\nbreak"));
    }
    use std::hash::{BuildHasher, Hasher};
    let random = std::collections::hash_map::RandomState::new()
        .build_hasher()
        .finish();
    let generated = format!("isolated-debug-check-{random:016x}");
    let secret = Secret::new(generated.clone()).unwrap();
    assert!(!format!("{secret:?}").contains(&generated));
    assert!(secret.expose_secret() == generated);
}

struct RefusingStore;
impl CredentialStore for RefusingStore {
    fn read(&self, _: &CredentialScope) -> Result<Option<Secret>, CredentialError> {
        Err(CredentialError::Unavailable)
    }
    fn write(&self, _: &CredentialScope, _: &Secret) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
    fn delete(&self, _: &CredentialScope) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
}

#[test]
fn status_never_converts_a_backend_failure_to_missing() {
    let scope = CredentialScope::new("channel", "https://example.invalid/v1").unwrap();
    assert_eq!(
        RefusingStore.status(&scope),
        Err(CredentialError::Unavailable)
    );
}

#[cfg(target_os = "windows")]
mod actual_windows {
    use super::*;
    use concat_agent::credentials::{CredentialStatus, WindowsCredentialStore};
    use std::process::Command;
    use windows::Win32::Security::Credentials::{
        CRED_PERSIST_LOCAL_MACHINE, CRED_TYPE_GENERIC, CREDENTIALW, CredWriteW,
    };
    use windows::core::PWSTR;

    fn scope(suffix: &str) -> CredentialScope {
        let namespace = std::env::var("BK_CAT_CREDENTIAL_TEST_NAMESPACE")
            .expect("explicit isolated test namespace is required");
        assert!(namespace.len() == 32 && namespace.bytes().all(|b| b.is_ascii_hexdigit()));
        CredentialScope::new(
            &namespace,
            &format!("https://credential-test.invalid/{namespace}/{suffix}"),
        )
        .unwrap()
    }

    struct Cleanup(CredentialScope);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            if WindowsCredentialStore.delete(&self.0).is_err() {
                eprintln!("isolated credential cleanup failed");
            }
        }
    }

    #[test]
    #[ignore = "explicit current-user OS vault verification with newly generated isolated values"]
    fn windows_system_credential_roundtrip_and_source_binding() {
        let store = WindowsCredentialStore;
        let scope = scope("original");
        assert_eq!(
            store.status(&scope).unwrap(),
            CredentialStatus::Missing,
            "refuse to overwrite any pre-existing test item"
        );
        let cleanup = Cleanup(scope.clone());
        let generated = std::env::var("BK_CAT_CREDENTIAL_TEST_VALUE")
            .expect("newly generated isolated test value is required");
        let secret = Secret::new(generated).unwrap();
        store.write(&scope, &secret).unwrap();
        assert_eq!(store.status(&scope).unwrap(), CredentialStatus::Present);
        let reopened = WindowsCredentialStore.read(&scope).unwrap().unwrap();
        assert!(
            reopened.expose_secret() == secret.expose_secret(),
            "system read did not preserve the value"
        );
        let moved = super::actual_windows::scope("changed-destination");
        assert_eq!(
            store.status(&moved).unwrap(),
            CredentialStatus::Missing,
            "changing the service destination must not reuse a key"
        );

        let child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--ignored",
                "--exact",
                "actual_windows::windows_credential_child_reads_existing_item",
                "--nocapture",
            ])
            .output()
            .unwrap();
        assert!(
            child.status.success(),
            "second process could not read its explicitly scoped test item"
        );
        assert!(String::from_utf8_lossy(&child.stdout).contains("ISOLATED_CREDENTIAL_CHILD_OK"));

        let replacement = Secret::new(format!("{}-replacement", secret.expose_secret())).unwrap();
        store.write(&scope, &replacement).unwrap();
        let changed = store.read(&scope).unwrap().unwrap();
        assert!(
            changed.expose_secret() == replacement.expose_secret(),
            "explicit replacement failed"
        );

        store.delete(&scope).unwrap();
        assert_eq!(store.status(&scope).unwrap(), CredentialStatus::Missing);
        store.delete(&scope).unwrap();
        drop(cleanup);
        println!("ISOLATED_WINDOWS_VAULT_ROUNDTRIP_PASS");
    }

    #[test]
    #[ignore = "helper invoked explicitly by the real cross-process test"]
    fn windows_credential_child_reads_existing_item() {
        let expected = Secret::new(std::env::var("BK_CAT_CREDENTIAL_TEST_VALUE").unwrap()).unwrap();
        let actual = WindowsCredentialStore
            .read(&scope("original"))
            .unwrap()
            .unwrap();
        assert!(
            actual.expose_secret() == expected.expose_secret(),
            "cross-process value mismatch"
        );
        println!("ISOLATED_CREDENTIAL_CHILD_OK");
    }

    #[test]
    #[ignore = "explicit isolated malformed-vault-item verification"]
    fn windows_malformed_system_item_is_error_not_missing() {
        let scope = scope("invalid-encoding");
        let store = WindowsCredentialStore;
        assert_eq!(
            store.status(&scope).unwrap(),
            CredentialStatus::Missing,
            "refuse to overwrite any pre-existing test item"
        );
        let cleanup = Cleanup(scope.clone());
        let mut target: Vec<u16> = format!("BKCAT:model-credential:v1:{}", scope.key_id())
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut invalid = [0xffu8, 0xfeu8];
        let malformed = CREDENTIALW {
            Type: CRED_TYPE_GENERIC,
            TargetName: PWSTR(target.as_mut_ptr()),
            CredentialBlobSize: invalid.len() as u32,
            CredentialBlob: invalid.as_mut_ptr(),
            Persist: CRED_PERSIST_LOCAL_MACHINE,
            ..CREDENTIALW::default()
        };
        // SAFETY: this writes only the freshly checked random test target.
        unsafe { CredWriteW(&malformed, 0) }.unwrap();
        assert_eq!(store.status(&scope), Err(CredentialError::Corrupt));
        store.delete(&scope).unwrap();
        assert_eq!(store.status(&scope).unwrap(), CredentialStatus::Missing);
        drop(cleanup);
        println!("ISOLATED_WINDOWS_VAULT_CORRUPTION_PASS");
    }
}
