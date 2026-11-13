// SPDX-License-Identifier: AGPL-3.0-or-later
//! Deterministic real-file tests of channel metadata, invalidation and boundaries.
use concat_agent::configuration::{
    CONFIGURATION_FILE, ChannelDraft, ChannelId, ConfigurationError, ConfigurationStore,
    ModelProtocol, canonical_endpoint, validate_draft,
};
use serde_json::Value;
use std::fs;
use std::sync::{Arc, Barrier};

fn draft(endpoint: &str) -> ChannelDraft {
    ChannelDraft {
        label: "Isolated channel".into(),
        protocol: ModelProtocol::OpenAiCompletions,
        endpoint: endpoint.into(),
        model: "test-model/v1".into(),
        request_timeout_ms: 60_000,
        max_output_tokens: 4096,
    }
}

#[test]
fn explicit_channels_reopen_with_stable_identity_and_no_implicit_selection_or_grant() {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigurationStore::new(directory.path());
    let empty = store.load().unwrap();
    assert_eq!(empty.version(), 0);
    assert!(empty.channels().is_empty());
    assert!(!directory.path().join(CONFIGURATION_FILE).exists());
    let first = store
        .save_channel(&empty, None, draft("https://first.example.test/v1"))
        .unwrap();
    let id = first.channels()[0].id.clone();
    assert!(first.active().is_none());
    let second = store
        .save_channel(&first, None, draft("https://second.example.test/v1"))
        .unwrap();
    assert_eq!(second.channels().len(), 2);
    assert_ne!(second.channels()[0].id, second.channels()[1].id);
    let selected = store.select(&second, Some(&id)).unwrap();
    let reopened = ConfigurationStore::new(directory.path()).load().unwrap();
    assert_eq!(reopened, selected);
    assert_eq!(reopened.active().unwrap().id, id);
    assert_eq!(reopened.active().unwrap().model, "test-model/v1");
    let value: Value =
        serde_json::from_slice(&fs::read(directory.path().join(CONFIGURATION_FILE)).unwrap())
            .unwrap();
    for forbidden in [
        "secret",
        "apiKey",
        "credentials",
        "calls_authorized",
        "price",
        "budget",
    ] {
        assert!(value.get(forbidden).is_none());
        assert!(value["channels"][0].get(forbidden).is_none());
    }
}

#[test]
fn replacement_keeps_identity_and_invalidates_old_versions() {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigurationStore::new(directory.path());
    let first = store
        .save_channel(
            &store.load().unwrap(),
            None,
            draft("https://models.example.test/a"),
        )
        .unwrap();
    let id = first.channels()[0].id.clone();
    let mut changed = first.channels()[0].draft();
    changed.endpoint = "https://models.example.test/b".into();
    changed.model = "another-model".into();
    let next = store.save_channel(&first, Some(&id), changed).unwrap();
    assert_eq!(next.channels()[0].id, id);
    assert!(next.version() > first.version());
    assert!(next.channels()[0].version > first.channels()[0].version);
    assert_ne!(
        next.channels()[0].service_origin().unwrap(),
        first.channels()[0].service_origin().unwrap()
    );
    assert_eq!(
        store.assert_current(&first),
        Err(ConfigurationError::StaleConfiguration)
    );
    assert_eq!(
        store.select(&first, Some(&id)),
        Err(ConfigurationError::StaleConfiguration)
    );
    assert_eq!(store.load().unwrap(), next);
}

#[test]
fn changing_credentials_invalidates_the_reviewed_configuration_without_storing_a_secret() {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigurationStore::new(directory.path());
    let initial = store
        .save_channel(
            &store.load().unwrap(),
            None,
            draft("https://models.example.test/v1"),
        )
        .unwrap();
    let id = initial.channels()[0].id.clone();
    let updated = store.credential_changed(&initial, &id).unwrap();
    assert!(updated.channels()[0].version > initial.channels()[0].version);
    assert_eq!(updated.channels()[0].draft(), initial.channels()[0].draft());
    assert_eq!(
        store.assert_current(&initial),
        Err(ConfigurationError::StaleConfiguration)
    );
    assert_eq!(store.assert_current(&updated), Ok(()));
}

#[test]
fn deleting_the_selected_channel_does_not_activate_another_channel() {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigurationStore::new(directory.path());
    let one = store
        .save_channel(
            &store.load().unwrap(),
            None,
            draft("https://first.example.test/v1"),
        )
        .unwrap();
    let id = one.channels()[0].id.clone();
    let two = store
        .save_channel(&one, None, draft("https://second.example.test/v1"))
        .unwrap();
    let active = store.select(&two, Some(&id)).unwrap();
    let deleted = store.delete(&active, &id).unwrap();
    assert!(deleted.active().is_none());
    assert_eq!(deleted.channels().len(), 1);
    assert_eq!(
        store.select(&deleted, Some(&id)),
        Err(ConfigurationError::MissingChannel)
    );
}

#[test]
fn a_same_version_external_change_is_not_mistaken_for_the_reviewed_file() {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigurationStore::new(directory.path());
    let original = store
        .save_channel(
            &store.load().unwrap(),
            None,
            draft("https://models.example.test/v1"),
        )
        .unwrap();
    let path = directory.path().join(CONFIGURATION_FILE);
    let mut document: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    document["channels"][0]["label"] = "Changed elsewhere".into();
    fs::write(&path, serde_json::to_vec(&document).unwrap()).unwrap();
    assert_eq!(store.load().unwrap().version(), original.version());
    assert_eq!(
        store.assert_current(&original),
        Err(ConfigurationError::StaleConfiguration)
    );
    assert_eq!(
        store.delete(&original, &original.channels()[0].id),
        Err(ConfigurationError::StaleConfiguration)
    );
}

#[test]
fn concurrent_stale_editors_cannot_overwrite_the_winning_change() {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigurationStore::new(directory.path());
    let initial = store.load().unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let workers: Vec<_> = [
        "https://first.example.test/",
        "https://second.example.test/",
    ]
    .into_iter()
    .map(|endpoint| {
        let store = store.clone();
        let initial = initial.clone();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            barrier.wait();
            store.save_channel(&initial, None, draft(endpoint))
        })
    })
    .collect();
    barrier.wait();
    let results: Vec<_> = workers
        .into_iter()
        .map(|worker| worker.join().unwrap())
        .collect();
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert!(
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .all(|error| matches!(
                error,
                ConfigurationError::Busy | ConfigurationError::StaleConfiguration
            ))
    );
    assert_eq!(store.load().unwrap().channels().len(), 1);
}

#[test]
fn service_scope_is_canonical_and_includes_scheme_host_port_and_base_path() {
    let (endpoint, origin) = canonical_endpoint("https://MODELS.example.test:443/team/v1").unwrap();
    assert_eq!(endpoint, "https://models.example.test/team/v1");
    assert_eq!(origin, "https://models.example.test:443/team/v1");
    assert_eq!(
        origin,
        canonical_endpoint("https://models.example.test/team/v1")
            .unwrap()
            .1
    );
    assert_ne!(
        origin,
        canonical_endpoint("https://models.example.test:8443/team/v1")
            .unwrap()
            .1
    );
    assert_ne!(
        origin,
        canonical_endpoint("https://models.example.test/other/v1")
            .unwrap()
            .1
    );
    assert_eq!(
        canonical_endpoint("http://[::1]:8090/v1").unwrap().1,
        "http://[::1]:8090/v1"
    );
    assert!(canonical_endpoint("http://127.0.0.1:8090/v1").is_ok());
    assert!(canonical_endpoint("http://localhost:8090/v1").is_ok());
}

#[test]
fn unsafe_or_ambiguous_endpoint_forms_are_rejected_without_echoing_the_value() {
    for endpoint in [
        "https://user:do-not-echo@example.test/v1",
        "https://example.test:0/v1",
        "https://example.test/v1?api_key=do-not-echo",
        "https://example.test/v1#secret",
        "http://remote.example.test/v1",
        "file:///private/key",
        "javascript:alert(1)",
        " https://example.test/v1",
        "https://example.test/v1\n",
        "",
        "https://",
    ] {
        let error = canonical_endpoint(endpoint).unwrap_err();
        assert_eq!(error, ConfigurationError::InvalidAddress);
        assert!(!error.to_string().contains("do-not-echo"));
    }
}

#[test]
fn numeric_and_text_boundaries_are_validated_before_writing() {
    let valid = draft("https://models.example.test/v1");
    for timeout in [0, 999, 600_001, u32::MAX] {
        let mut invalid = valid.clone();
        invalid.request_timeout_ms = timeout;
        assert_eq!(
            validate_draft(invalid),
            Err(ConfigurationError::InvalidTimeout)
        );
    }
    for output in [0, 1_000_001, u32::MAX] {
        let mut invalid = valid.clone();
        invalid.max_output_tokens = output;
        assert_eq!(
            validate_draft(invalid),
            Err(ConfigurationError::InvalidOutputLimit)
        );
    }
    for label in ["".to_owned(), "  ".into(), "a\nb".into(), "x".repeat(81)] {
        let mut invalid = valid.clone();
        invalid.label = label;
        assert_eq!(
            validate_draft(invalid),
            Err(ConfigurationError::InvalidLabel)
        );
    }
    for model in [
        "".to_owned(),
        "model with space".into(),
        "a\nb".into(),
        "x".repeat(257),
    ] {
        let mut invalid = valid.clone();
        invalid.model = model;
        assert_eq!(
            validate_draft(invalid),
            Err(ConfigurationError::InvalidModel)
        );
    }
    assert_eq!(
        ModelProtocol::parse("unknown-protocol"),
        Err(ConfigurationError::UnsupportedProtocol)
    );
    assert!(ChannelId::parse("../another-project").is_err());
    assert!(ChannelId::parse(&"A".repeat(32)).is_err());
}

#[test]
fn unknown_schema_secret_fields_and_damaged_documents_are_never_migrated_or_overwritten() {
    let directory = tempfile::tempdir().unwrap();
    let store = ConfigurationStore::new(directory.path());
    let initial = store
        .save_channel(
            &store.load().unwrap(),
            None,
            draft("https://models.example.test/v1"),
        )
        .unwrap();
    let path = directory.path().join(CONFIGURATION_FILE);
    let valid: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    let mut future = valid.clone();
    future["schema_version"] = 999.into();
    let mut secret = valid.clone();
    secret["channels"][0]["apiKey"] = "never-persist-a-real-key".into();
    let mut grant = valid.clone();
    grant["calls_authorized"] = true.into();
    let mut duplicate = valid.clone();
    duplicate["channels"]
        .as_array_mut()
        .unwrap()
        .push(valid["channels"][0].clone());
    let mut dangling = valid.clone();
    dangling["active_channel"] = "f".repeat(32).into();
    for bytes in [
        serde_json::to_vec(&future).unwrap(),
        serde_json::to_vec(&secret).unwrap(),
        serde_json::to_vec(&grant).unwrap(),
        serde_json::to_vec(&duplicate).unwrap(),
        serde_json::to_vec(&dangling).unwrap(),
        b"{broken".to_vec(),
        vec![b'x'; 128 * 1024 + 1],
    ] {
        fs::write(&path, &bytes).unwrap();
        assert!(store.load().is_err());
        assert!(
            store
                .save_channel(&initial, None, draft("https://other.example.test/"))
                .is_err()
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
    }
}
