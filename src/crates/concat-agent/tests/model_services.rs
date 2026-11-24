// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah
//! Real-file and loopback HTTP configuration tests. No real service or secret.
use concat_agent::configuration::catalogue::{
    CatalogueModel, CatalogueSource, DirectorySnapshot, VerificationCapability, VerificationRecord,
    cache_key,
};
use concat_agent::configuration::providers::{
    DirectoryProtocol, PROVIDERS, directory_protocol, preset,
};
use concat_agent::configuration::{
    CONFIGURATION_FILE, ChannelDraft, ConfigurationStore, ModelChannel, ModelProtocol,
};
use concat_agent::credentials::{CredentialError, CredentialScope, CredentialStore, Secret};
use concat_agent::network::catalogue::{
    CatalogueError, ConfigurationCancellation, parse_page, refresh_directory, verify_model,
};
use serde_json::{Value, json};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

const SECRET: &str = "isolated-model-services-test-not-an-account-key";
#[derive(Default)]
struct Credentials {
    reads: AtomicUsize,
}
impl CredentialStore for Credentials {
    fn read(&self, _: &CredentialScope) -> Result<Option<Secret>, CredentialError> {
        self.reads.fetch_add(1, Ordering::Relaxed);
        Secret::new(SECRET.into()).map(Some)
    }
    fn write(&self, _: &CredentialScope, _: &Secret) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
    fn delete(&self, _: &CredentialScope) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
}
fn draft(endpoint: &str, protocol: ModelProtocol) -> ChannelDraft {
    ChannelDraft {
        label: "Isolated connection".into(),
        protocol,
        endpoint: endpoint.into(),
        model: "fixture-model".into(),
        request_timeout_ms: 30_000,
        max_output_tokens: 128,
    }
}
fn connection(
    endpoint: &str,
    protocol: ModelProtocol,
) -> (tempfile::TempDir, ConfigurationStore, ModelChannel) {
    let dir = tempfile::tempdir().unwrap();
    let store = ConfigurationStore::new(dir.path());
    let snapshot = store
        .save_channel(&store.load().unwrap(), None, draft(endpoint, protocol))
        .unwrap();
    let channel = snapshot.channels()[0].clone();
    (dir, store, channel)
}
fn directory(ids: &[&str], at: u64) -> DirectorySnapshot {
    DirectorySnapshot {
        source: CatalogueSource::Official,
        source_url: "https://example.test/v1/models".into(),
        fetched_at: at,
        models: ids.iter().map(|id| CatalogueModel::manual(id)).collect(),
    }
}
fn listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}
fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match listener.accept() {
            Ok((socket, _)) => {
                socket.set_nonblocking(false).unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(20)))
                    .unwrap();
                socket
                    .set_write_timeout(Some(Duration::from_secs(20)))
                    .unwrap();
                return socket;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline);
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("loopback accept: {error}"),
        }
    }
}
fn request(socket: &mut TcpStream) -> (String, String, Value) {
    let mut bytes = Vec::new();
    let end = loop {
        if let Some(index) = bytes.windows(4).position(|v| v == b"\r\n\r\n") {
            break index + 4;
        }
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() < 64 * 1024);
    };
    let headers = String::from_utf8(bytes[..end].to_vec()).unwrap();
    let length = headers
        .lines()
        .find_map(|line| {
            line.to_lowercase()
                .strip_prefix("content-length:")
                .map(|value| value.trim().parse::<usize>().unwrap())
        })
        .unwrap_or(0);
    while bytes.len() < end + length {
        let mut buffer = [0; 4096];
        let count = socket.read(&mut buffer).unwrap();
        assert!(count > 0);
        bytes.extend_from_slice(&buffer[..count]);
    }
    let body = if length == 0 {
        Value::Null
    } else {
        serde_json::from_slice(&bytes[end..end + length]).unwrap()
    };
    (headers.lines().next().unwrap().into(), headers, body)
}
fn respond(socket: &mut TcpStream, status: u16, value: Value) {
    let bytes = value.to_string();
    let _ = write!(
        socket,
        "HTTP/1.1 {status} Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        bytes.len(),
        bytes
    );
}

#[test]
fn migration_preserves_every_legacy_identity_default_and_exact_backup() {
    let (dir, store, first) = connection(
        "https://same.example.test/v1",
        ModelProtocol::OpenAiCompletions,
    );
    let second = store
        .save_channel(
            &store.load().unwrap(),
            None,
            draft(&first.endpoint, first.protocol),
        )
        .unwrap();
    let selected = store.select(&second, Some(&first.id)).unwrap();
    let mut legacy = serde_json::to_value(json!({"schema_version":2,"configuration_version":selected.version(),"active_channel":first.id,"channels":selected.channels()})).unwrap();
    legacy["schema_version"] = json!(1);
    for channel in legacy["channels"].as_array_mut().unwrap() {
        for name in [
            "provider_id",
            "region",
            "credential_version",
            "manual_models",
            "context_overrides",
        ] {
            channel.as_object_mut().unwrap().remove(name);
        }
    }
    let original = serde_json::to_vec_pretty(&legacy).unwrap();
    std::fs::write(dir.path().join(CONFIGURATION_FILE), &original).unwrap();
    let projected = store.load().unwrap();
    assert_eq!(projected.channels().len(), 2);
    assert_eq!(projected.active().unwrap().id, first.id);
    assert_eq!(projected.channels()[0].provider_id, "custom");
    assert_ne!(projected.channels()[0].id, projected.channels()[1].id);
    assert_eq!(
        std::fs::read(dir.path().join(CONFIGURATION_FILE)).unwrap(),
        original,
        "reading alone must not overwrite legacy bytes"
    );
    let next = store
        .select_model(&projected, &first.id, "next-model")
        .unwrap();
    assert_eq!(
        std::fs::read(dir.path().join("model-channels.v1.backup.json")).unwrap(),
        original
    );
    assert_eq!(
        next.channel(&first.id).unwrap().version,
        first.version,
        "a default selection is not a connection mutation"
    );
    assert_eq!(next.active().unwrap().model, "next-model");
    assert_eq!(
        CredentialScope::new(first.id.as_str(), &first.service_origin().unwrap()).unwrap(),
        CredentialScope::new(
            next.active().unwrap().id.as_str(),
            &next.active().unwrap().service_origin().unwrap()
        )
        .unwrap()
    );
}

#[test]
fn connection_without_a_model_is_saved_for_discovery_and_not_a_default() {
    let dir = tempfile::tempdir().unwrap();
    let store = ConfigurationStore::new(dir.path());
    let mut draft = draft("https://api.deepseek.com", ModelProtocol::OpenAiCompletions);
    draft.model.clear();
    let next = store
        .save_connection(&store.load().unwrap(), None, draft, "deepseek", "global")
        .unwrap();
    let selected = store.select(&next, Some(&next.channels()[0].id)).unwrap();
    assert!(selected.active().is_none());
    assert_eq!(selected.channels().len(), 1);
}

#[test]
fn defaults_and_other_connections_do_not_revoke_a_captured_connection() {
    let (_dir, store, first) = connection(
        "https://first.example.test/v1",
        ModelProtocol::OpenAiCompletions,
    );
    let snapshot = store
        .save_channel(
            &store.load().unwrap(),
            None,
            draft("https://second.example.test/v1", first.protocol),
        )
        .unwrap();
    let changed = store
        .select_model(&snapshot, &snapshot.channels()[1].id, "new-default")
        .unwrap();
    store.assert_connection_current(&first).unwrap();
    store
        .set_context_override(&changed, &first.id, "fixture-model", Some(8192))
        .unwrap();
    assert!(
        store.assert_connection_current(&first).is_err(),
        "explicit capacity changes revoke previous task configuration"
    );
}

#[test]
fn same_revision_external_limit_changes_are_not_accepted_by_connection_check() {
    let (dir, store, channel) =
        connection("https://example.test/v1", ModelProtocol::OpenAiCompletions);
    let path = dir.path().join(CONFIGURATION_FILE);
    let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["channels"][0]["max_output_tokens"] = json!(256);
    std::fs::write(&path, value.to_string()).unwrap();
    assert!(store.assert_connection_current(&channel).is_err());
}

#[test]
fn failed_refresh_keeps_complete_cache_timestamp_and_independent_verification() {
    let (_dir, store, channel) =
        connection("https://example.test/v1", ModelProtocol::OpenAiCompletions);
    let saved = directory(&["fixture-model", "other"], 1000);
    store
        .publish_catalogue(&channel, 1000, Ok(saved.clone()))
        .unwrap();
    store
        .record_verification(
            &channel,
            VerificationRecord {
                connection_version: channel.version,
                credential_version: channel.credential_version,
                model: channel.model.clone(),
                capability: VerificationCapability::Inference,
                checked_at: 1001,
                succeeded: true,
                error_code: None,
            },
        )
        .unwrap();
    let failed = store
        .publish_catalogue(&channel, 1010, Err("catalogue.network"))
        .unwrap();
    assert_eq!(failed.directory, Some(saved));
    assert_eq!(
        failed.refresh.unwrap().error_code.as_deref(),
        Some("catalogue.network")
    );
    assert_eq!(failed.verification.len(), 1);
    assert!(failed.verification[0].succeeded);
    assert!(failed.directory.as_ref().unwrap().is_fresh(1000 + 86400));
    assert!(!failed.directory.as_ref().unwrap().is_fresh(1000 + 86401));
    assert!(!failed.directory.as_ref().unwrap().is_fresh(999));
    let empty = store
        .publish_catalogue(&channel, 1020, Ok(directory(&[], 1020)))
        .unwrap();
    assert!(empty.directory.unwrap().models.is_empty());
    assert!(
        store
            .flattened_models(&store.load().unwrap())
            .unwrap()
            .iter()
            .any(|choice| choice.model == channel.model)
    );
}

#[test]
fn cache_is_isolated_by_connection_address_region_and_credential_revision() {
    let (_dir, store, channel) =
        connection("https://example.test/v1", ModelProtocol::OpenAiCompletions);
    store
        .publish_catalogue(&channel, 1, Ok(directory(&["visible-to-first-account"], 1)))
        .unwrap();
    let next = store
        .save_channel(&store.load().unwrap(), None, channel.draft())
        .unwrap();
    assert!(
        store
            .catalogue(&next.channels()[1])
            .unwrap()
            .directory
            .is_none()
    );
    let changed = store.credential_changed(&next, &channel.id).unwrap();
    assert!(
        store
            .catalogue(changed.channel(&channel.id).unwrap())
            .unwrap()
            .directory
            .is_none()
    );
    let mut other = channel.clone();
    other.endpoint.push_str("/path");
    assert_ne!(cache_key(&channel), cache_key(&other));
    other = channel.clone();
    other.region = "another-region".into();
    assert_ne!(cache_key(&channel), cache_key(&other));
    assert_eq!(
        store.catalogue(&channel).unwrap().directory.unwrap().models[0].id,
        "visible-to-first-account"
    );
}

#[test]
fn directory_metadata_preserves_provenance_unknown_fields_and_protocol_specific_pagination() {
    let (models, next) = parse_page(DirectoryProtocol::Google, &json!({"models":[
        {"name":"models/gemini-model","displayName":"Model","inputTokenLimit":1048576,"outputTokenLimit":8192,"supportedGenerationMethods":["generateContent"]},
        {"name":"models/embedding-only","supportedGenerationMethods":["embedContent"]}
    ],"nextPageToken":"next+token"}), "https://generativelanguage.googleapis.com/v1beta/models").unwrap();
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].id, "gemini-model");
    assert_eq!(models[0].context_window, Some(1048576));
    assert!(
        models[0]
            .context_source
            .as_ref()
            .unwrap()
            .starts_with("official-directory:")
    );
    assert_eq!(models[0].tools, None);
    assert_eq!(next.as_deref(), Some("next+token"));
    let (models, next) = parse_page(DirectoryProtocol::Anthropic, &json!({"data":[{"id":"claude-model","max_input_tokens":200000,"max_tokens":8192,"capabilities":{"tool_use":{"supported":true}}}],"has_more":true,"last_id":"claude-model"}), "https://api.anthropic.com/v1/models").unwrap();
    assert_eq!(models[0].tools, Some(true));
    assert_eq!(next.as_deref(), Some("claude-model"));
    let (models, _) = parse_page(
        DirectoryProtocol::OpenAi,
        &json!({"data":[{"id":"unknown-capacity"}]}),
        "https://example.test/models",
    )
    .unwrap();
    assert_eq!(models[0].context_window, None);
    assert_eq!(models[0].context_source, None);
    assert!(
        parse_page(
            DirectoryProtocol::OpenAi,
            &json!({"data":[],"has_more":true,"last_id":"x"}),
            "x"
        )
        .is_err()
    );
    assert!(
        parse_page(
            DirectoryProtocol::OpenAi,
            &json!({"data":[{"id":"x"}],"next":"unreviewed-pagination"}),
            "x"
        )
        .is_err()
    );
}

// Only these isolated documents map a real provider adapter onto loopback.
// Product save_connection still requires the exact official preset address.
fn provider_fixture(
    endpoint: &str,
    provider: &str,
) -> (tempfile::TempDir, ConfigurationStore, ModelChannel) {
    let p = preset(provider).unwrap();
    let (dir, store, _) = connection(endpoint, p.protocol);
    let path = dir.path().join(CONFIGURATION_FILE);
    let mut value: Value = serde_json::from_slice(&std::fs::read(&path).unwrap()).unwrap();
    value["channels"][0]["provider_id"] = json!(provider);
    value["channels"][0]["region"] = json!(p.region);
    std::fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    let channel = store.load().unwrap().channels()[0].clone();
    (dir, store, channel)
}

fn dashscope_response(page: u64, total: u64, models: Vec<Value>) -> Value {
    json!({"success":true,"code":null,"message":null,"output":{
        "page_no":page,"page_size":100,"total":total,"models":models
    }})
}

fn dashscope_first_page(total: u64) -> Value {
    dashscope_response(
        1,
        total,
        (0..100)
            .map(|index| json!({"model":format!("model-{index}")}))
            .collect(),
    )
}

#[test]
fn ark_directory_uses_real_metadata_and_keeps_missing_capacity_unknown() {
    assert_eq!(preset("doubao").unwrap().directory, DirectoryProtocol::Ark);
    assert_eq!(
        directory_protocol("custom", ModelProtocol::OpenAiCompletions),
        DirectoryProtocol::OpenAi
    );
    let source = "https://ark.cn-beijing.volces.com/api/v3/models";
    let response = json!({"object":"list","data":[
        {"id":"doubao-version-a","name":"Doubao A","context_window":1048576,"max_output_tokens":262144,
         "token_limits":{"context_window":1048576,"max_output_token_length":262144},
         "features":{"tools":{"function_calling":true}},"modalities":{"input_modalities":["text","image","video"]}},
        {"id":"doubao-version-b","token_limits":{"context_window":32768,"max_output_token_length":8192},
         "features":{"tools":{"function_calling":false}},"modalities":{"input_modalities":["text"]}},
        {"id":"unknown-context","context_window":0,"max_output_tokens":2048}
    ]});
    let (models, next) = parse_page(DirectoryProtocol::Ark, &response, source).unwrap();
    assert_eq!(next, None);
    assert_eq!(models.len(), 3);
    assert_eq!(models[0].id, "doubao-version-a");
    assert_eq!(models[0].name, "Doubao A");
    assert_eq!(models[0].context_window, Some(1048576));
    assert_eq!(models[0].max_output_tokens, Some(262144));
    assert_eq!(models[0].tools, Some(true));
    assert_eq!(models[0].image_input, Some(true));
    assert_eq!(
        models[0].context_source.as_deref(),
        Some("official-directory:https://ark.cn-beijing.volces.com/api/v3/models")
    );
    assert_eq!(models[1].context_window, Some(32768));
    assert_eq!(models[1].tools, Some(false));
    assert_eq!(models[1].image_input, Some(false));
    assert_eq!(
        models[2].context_window, None,
        "an output limit cannot stand in for context"
    );
    assert_eq!(models[2].context_source, None);
    assert_eq!(models[2].tools, None);
    let (custom, _) = parse_page(DirectoryProtocol::OpenAi, &response, source).unwrap();
    assert_eq!(
        custom[1].context_window, None,
        "custom compatibility cannot guess Ark's nested schema"
    );
    assert_eq!(custom[1].tools, None);
}

#[test]
fn dashscope_native_metadata_and_numbered_page_boundaries_are_explicit() {
    assert_eq!(
        preset("qwen").unwrap().directory,
        DirectoryProtocol::DashScope
    );
    let response = dashscope_response(
        1,
        1,
        vec![json!({"model":"qwen-version","name":"Qwen version",
        "features":["cache","function-calling"],"inference_metadata":{"request_modality":["Image","Text"]},
        "model_info":{"context_window":1000000,"max_input_tokens":991808,"max_output_tokens":131072}})],
    );
    let (models, next) = parse_page(
        DirectoryProtocol::DashScope,
        &response,
        "https://dashscope.aliyuncs.com/api/v1/models",
    )
    .unwrap();
    assert_eq!(next, None);
    assert_eq!(models[0].id, "qwen-version");
    assert_eq!(
        models[0].context_window,
        Some(1000000),
        "input allowance is not the full context window"
    );
    assert_eq!(models[0].max_output_tokens, Some(131072));
    assert_eq!(models[0].tools, Some(true));
    assert_eq!(models[0].image_input, Some(true));
    let response = dashscope_response(
        1,
        1,
        vec![json!({"model":"unknown-context","features":[],
        "model_info":{"context_window":null,"max_input_tokens":2000,"max_output_tokens":1024}})],
    );
    let (models, _) = parse_page(DirectoryProtocol::DashScope, &response, "local").unwrap();
    assert_eq!(models[0].context_window, None);
    assert_eq!(models[0].context_source, None);
    assert_eq!(models[0].tools, Some(false));
    assert_eq!(models[0].image_input, None);
    assert_eq!(
        parse_page(
            DirectoryProtocol::DashScope,
            &dashscope_first_page(101),
            "local"
        )
        .unwrap()
        .1
        .as_deref(),
        Some("2")
    );
    assert!(
        parse_page(
            DirectoryProtocol::DashScope,
            &dashscope_response(1, 0, vec![]),
            "local"
        )
        .unwrap()
        .0
        .is_empty()
    );
    for bad in [
        dashscope_response(1, 2, vec![json!({"model":"partial"})]),
        dashscope_response(2, 101, vec![]),
        dashscope_response(2, 0, vec![]),
        json!({"success":false,"code":"Rejected","output":{"page_no":1,"page_size":100,"total":0,"models":[]}}),
    ] {
        assert_eq!(
            parse_page(DirectoryProtocol::DashScope, &bad, "local"),
            Err(CatalogueError::InvalidResponse)
        );
    }
}

#[test]
fn provider_specific_malformed_capacity_or_continuation_never_becomes_success() {
    for entry in [
        json!({"id":"x","context_window":32768,"token_limits":{"context_window":65536}}),
        json!({"id":"x","context_window":"32768"}),
        json!({"id":"x","token_limits":{"context_window":-1}}),
        json!({"id":"x","token_limits":{"context_window":4294967296_u64}}),
        json!({"id":"x","features":{"tools":{"function_calling":"true"}}}),
        json!({"id":"x","modalities":{"input_modalities":[1]}}),
    ] {
        assert_eq!(
            parse_page(
                DirectoryProtocol::Ark,
                &json!({"object":"list","data":[entry]}),
                "local"
            ),
            Err(CatalogueError::InvalidResponse)
        );
    }
    for response in [
        json!({"object":"list","data":[{"id":"x"}],"has_more":true,"last_id":"x"}),
        json!({"object":"list","data":[{"id":"x"}],"nextPageToken":"next"}),
        json!({"object":"list","data":[{"id":"x"}],"next_cursor":"next"}),
    ] {
        assert_eq!(
            parse_page(DirectoryProtocol::Ark, &response, "local"),
            Err(CatalogueError::Unsupported)
        );
    }
    assert_eq!(
        parse_page(DirectoryProtocol::Ark, &json!({"data":[]}), "local"),
        Err(CatalogueError::InvalidResponse)
    );
}

#[test]
fn real_ark_directory_reads_one_authenticated_complete_response_and_keeps_inference_unverified() {
    for models in [
        vec![
            json!({"id":"live-listed-model","name":"Listed model","token_limits":{"context_window":65536},
        "features":{"tools":{"function_calling":true}}}),
        ],
        vec![],
    ] {
        let listener = listener();
        let endpoint = format!("http://{}/api/v3", listener.local_addr().unwrap());
        let (dir, store, channel) = provider_fixture(&endpoint, "doubao");
        let configuration_before = std::fs::read(dir.path().join(CONFIGURATION_FILE)).unwrap();
        let expected_count = models.len();
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            let (line, headers, body) = request(&mut socket);
            assert_eq!(line, "GET /api/v3/models HTTP/1.1");
            assert!(headers.contains(&format!("Bearer {SECRET}")));
            assert_eq!(body, Value::Null);
            respond(&mut socket, 200, json!({"object":"list","data":models}));
            assert!(
                matches!(listener.accept(), Err(error) if error.kind() == std::io::ErrorKind::WouldBlock)
            );
        });
        let credentials = Credentials::default();
        let result = refresh_directory(&store, &channel, &credentials, &Arc::default()).unwrap();
        assert_eq!(credentials.reads.load(Ordering::Relaxed), 1);
        assert_eq!(result.models.len(), expected_count);
        assert_eq!(result.source, CatalogueSource::Official);
        assert!(result.is_fresh(result.fetched_at));
        assert_eq!(result.source_url, format!("{endpoint}/models"));
        let facts = store.catalogue(&channel).unwrap();
        assert_eq!(facts.directory, Some(result));
        assert!(
            facts.verification.is_empty(),
            "directory capability metadata cannot create execution verification"
        );
        assert_eq!(
            std::fs::read(dir.path().join(CONFIGURATION_FILE)).unwrap(),
            configuration_before
        );
        server.join().unwrap();
    }
}

#[test]
fn real_minimax_complete_directory_does_not_invent_capacity_or_execute_inference() {
    assert_eq!(
        preset("minimax").unwrap().endpoint,
        "https://api.minimax.cn/v1"
    );
    assert_eq!(
        preset("minimax").unwrap().directory,
        DirectoryProtocol::OpenAiComplete
    );
    for incomplete in [false, true] {
        let listener = listener();
        let endpoint = format!("http://{}/v1", listener.local_addr().unwrap());
        let (_dir, store, channel) = provider_fixture(&endpoint, "minimax");
        let previous = directory(&["previous"], 10);
        store
            .publish_catalogue(&channel, 10, Ok(previous.clone()))
            .unwrap();
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            let (line, headers, body) = request(&mut socket);
            assert_eq!(line, "GET /v1/models HTTP/1.1");
            assert!(headers.contains(&format!("Bearer {SECRET}")));
            assert_eq!(body, Value::Null);
            let mut response = json!({"object":"list","data":[{"id":"MiniMax-M3.1-Flash",
                "object":"model","created":1789556118,"owned_by":"minimax"}]});
            if incomplete {
                response["has_more"] = json!(true);
            }
            respond(&mut socket, 200, response);
        });
        let credentials = Credentials::default();
        let result = refresh_directory(&store, &channel, &credentials, &Arc::default());
        assert_eq!(credentials.reads.load(Ordering::Relaxed), 1);
        let facts = store.catalogue(&channel).unwrap();
        assert!(facts.verification.is_empty());
        if incomplete {
            assert_eq!(result, Err(CatalogueError::Unsupported));
            assert_eq!(facts.directory, Some(previous));
        } else {
            let result = result.unwrap();
            assert_eq!(result.models[0].id, "MiniMax-M3.1-Flash");
            assert_eq!(result.models[0].context_window, None);
            assert_eq!(result.models[0].context_source, None);
            assert_eq!(result.models[0].tools, None);
            assert_eq!(result.models[0].image_input, None);
            assert_eq!(result.models[0].max_output_tokens, None);
            assert_eq!(facts.directory, Some(result));
        }
        server.join().unwrap();
    }
}

#[test]
fn real_dashscope_pages_share_one_scope_and_publish_only_after_the_last_page() {
    let listener = listener();
    let endpoint = format!(
        "http://{}/compatible-mode/v1",
        listener.local_addr().unwrap()
    );
    let (dir, store, channel) = provider_fixture(&endpoint, "qwen");
    let before = std::fs::read(dir.path().join(CONFIGURATION_FILE)).unwrap();
    let previous = directory(&["previous"], 10);
    store
        .publish_catalogue(&channel, 10, Ok(previous.clone()))
        .unwrap();
    let (ready_tx, ready_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        for page in 1..=2 {
            let mut socket = accept(&listener);
            let (line, headers, body) = request(&mut socket);
            assert_eq!(
                line,
                format!("GET /api/v1/models?page_no={page}&page_size=100 HTTP/1.1")
            );
            assert!(headers.contains(&format!("Bearer {SECRET}")));
            assert_eq!(body, Value::Null);
            if page == 1 {
                respond(&mut socket, 200, dashscope_first_page(101));
            } else {
                ready_tx.send(()).unwrap();
                release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                respond(
                    &mut socket,
                    200,
                    dashscope_response(
                        2,
                        101,
                        vec![json!({"model":"last-model",
                    "model_info":{"context_window":1000000,"max_output_tokens":8192}})],
                    ),
                );
            }
        }
    });
    let credentials = Arc::new(Credentials::default());
    let worker_credentials = credentials.clone();
    let worker_store = store.clone();
    let worker_channel = channel.clone();
    let worker = std::thread::spawn(move || {
        refresh_directory(
            &worker_store,
            &worker_channel,
            worker_credentials.as_ref(),
            &Arc::default(),
        )
    });
    ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(store.catalogue(&channel).unwrap().directory, Some(previous));
    release_tx.send(()).unwrap();
    let result = worker.join().unwrap().unwrap();
    assert_eq!(result.models.len(), 101);
    assert_eq!(result.models.last().unwrap().id, "last-model");
    assert_eq!(result.models.last().unwrap().context_window, Some(1000000));
    assert_eq!(credentials.reads.load(Ordering::Relaxed), 1);
    assert_eq!(store.catalogue(&channel).unwrap().directory, Some(result));
    assert_eq!(
        std::fs::read(dir.path().join(CONFIGURATION_FILE)).unwrap(),
        before
    );
    server.join().unwrap();
}

#[test]
fn incomplete_numbered_pages_and_duplicate_models_preserve_previous_cache_and_freshness() {
    let mut changed_size = dashscope_response(2, 101, vec![json!({"model":"last"})]);
    changed_size["output"]["page_size"] = json!(50);
    for (status, second) in [
        (
            200,
            dashscope_response(
                2,
                102,
                vec![json!({"model":"last"}), json!({"model":"extra"})],
            ),
        ),
        (200, dashscope_response(2, 101, vec![])),
        (
            200,
            dashscope_response(2, 101, vec![json!({"model":"model-0"})]),
        ),
        (
            200,
            dashscope_response(1, 101, vec![json!({"model":"last"})]),
        ),
        (200, changed_size),
        (500, json!({"error":"isolated service failure"})),
    ] {
        let listener = listener();
        let endpoint = format!(
            "http://{}/compatible-mode/v1",
            listener.local_addr().unwrap()
        );
        let (_dir, store, channel) = provider_fixture(&endpoint, "qwen");
        let previous = directory(&["previous"], 10);
        store
            .publish_catalogue(&channel, 10, Ok(previous.clone()))
            .unwrap();
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            request(&mut socket);
            respond(&mut socket, 200, dashscope_first_page(101));
            drop(socket);
            let mut socket = accept(&listener);
            request(&mut socket);
            respond(&mut socket, status, second);
        });
        let error = refresh_directory(&store, &channel, &Credentials::default(), &Arc::default())
            .unwrap_err();
        assert!(matches!(
            error,
            CatalogueError::InvalidResponse | CatalogueError::HttpStatus(500)
        ));
        let facts = store.catalogue(&channel).unwrap();
        assert_eq!(facts.directory, Some(previous));
        assert_eq!(
            facts.refresh.unwrap().error_code.as_deref(),
            Some(error.code())
        );
        server.join().unwrap();
    }
}

#[test]
fn real_http_pagination_is_complete_before_any_cache_publication() {
    let listener = listener();
    let (_dir, store, channel) = connection(
        &format!("http://{}/v1", listener.local_addr().unwrap()),
        ModelProtocol::OpenAiCompletions,
    );
    let server = std::thread::spawn(move || {
        for index in 0..2 {
            let mut socket = accept(&listener);
            let (line, headers, body) = request(&mut socket);
            assert!(line.starts_with("GET /v1/models"));
            assert_eq!(body, Value::Null);
            assert!(headers.contains(SECRET));
            if index == 0 {
                assert!(!line.contains("after="));
                respond(
                    &mut socket,
                    200,
                    json!({"data":[{"id":"first"}],"has_more":true,"last_id":"first"}),
                );
            } else {
                assert!(line.contains("after=first"));
                respond(
                    &mut socket,
                    200,
                    json!({"data":[{"id":"second"}],"has_more":false}),
                );
            }
        }
    });
    let result =
        refresh_directory(&store, &channel, &Credentials::default(), &Arc::default()).unwrap();
    assert_eq!(
        result
            .models
            .iter()
            .map(|m| m.id.as_str())
            .collect::<Vec<_>>(),
        vec!["first", "second"]
    );
    assert_eq!(store.catalogue(&channel).unwrap().directory, Some(result));
    server.join().unwrap();
}

#[test]
fn second_page_failure_and_secret_echo_cannot_overwrite_the_previous_directory() {
    for echo in [false, true] {
        let listener = listener();
        let (_dir, store, channel) = connection(
            &format!("http://{}/v1", listener.local_addr().unwrap()),
            ModelProtocol::OpenAiCompletions,
        );
        let previous = directory(&["previous"], 10);
        store
            .publish_catalogue(&channel, 10, Ok(previous.clone()))
            .unwrap();
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            request(&mut socket);
            if echo {
                respond(&mut socket, 200, json!({"data":[{"id":SECRET}]}));
                return;
            }
            respond(
                &mut socket,
                200,
                json!({"data":[{"id":"partial"}],"has_more":true,"last_id":"partial"}),
            );
            drop(socket);
            let mut socket = accept(&listener);
            request(&mut socket);
            respond(&mut socket, 500, json!({"error":"fixture failure"}));
        });
        let error = refresh_directory(&store, &channel, &Credentials::default(), &Arc::default())
            .unwrap_err();
        assert_eq!(
            store
                .catalogue(&channel)
                .unwrap()
                .refresh
                .unwrap()
                .error_code
                .as_deref(),
            Some(error.code())
        );
        assert_eq!(store.catalogue(&channel).unwrap().directory, Some(previous));
        server.join().unwrap();
    }
}

#[test]
fn escaped_credential_echo_in_decoded_values_and_keys_never_reaches_the_cache() {
    let mut key_echo = json!({"data":[{"id":"new-model"}],"extra":{}});
    key_echo["extra"]
        .as_object_mut()
        .unwrap()
        .insert(SECRET.into(), json!(true));
    for response in [
        json!({"data":[{"id":SECRET}]}),
        json!({"data":[{"id":"new-model","name":SECRET}]}),
        json!({"data":[{"id":"new-model"}],"extra":[{"nested":[SECRET]}]}),
        key_echo,
    ] {
        let listener = listener();
        let (dir, store, channel) = connection(
            &format!("http://{}/v1", listener.local_addr().unwrap()),
            ModelProtocol::OpenAiCompletions,
        );
        let previous = directory(&["previous"], 10);
        store
            .publish_catalogue(&channel, 10, Ok(previous.clone()))
            .unwrap();
        let configuration_before = std::fs::read(dir.path().join(CONFIGURATION_FILE)).unwrap();
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            let (_, headers, _) = request(&mut socket);
            assert!(
                headers.contains(SECRET),
                "the isolated credential must be sent only as authentication"
            );
            let encoded = response.to_string().replace('-', "\\u002d");
            assert!(
                !encoded.contains(SECRET),
                "raw-byte matching cannot reject this response"
            );
            write!(socket, "HTTP/1.1 200 Fixture\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}", encoded.len(), encoded).unwrap();
        });
        assert_eq!(
            refresh_directory(&store, &channel, &Credentials::default(), &Arc::default()),
            Err(CatalogueError::InvalidResponse)
        );
        let facts = store.catalogue(&channel).unwrap();
        assert_eq!(facts.directory, Some(previous));
        assert_eq!(
            facts.refresh.unwrap().error_code.as_deref(),
            Some("catalogue.invalidResponse")
        );
        let configuration_after = std::fs::read(dir.path().join(CONFIGURATION_FILE)).unwrap();
        assert_eq!(configuration_after, configuration_before);
        let cache_path = dir
            .path()
            .join("catalogues")
            .join(channel.id.as_str())
            .join(format!("{}.json", cache_key(&channel)));
        let cache = std::fs::read(cache_path).unwrap();
        assert!(!String::from_utf8_lossy(&configuration_after).contains(SECRET));
        assert!(!String::from_utf8_lossy(&cache).contains(SECRET));
        server.join().unwrap();
    }
}

#[test]
fn cancellation_interrupts_an_inflight_page_and_duplicate_refresh_is_rejected() {
    let listener = listener();
    let (_dir, store, channel) = connection(
        &format!("http://{}/v1", listener.local_addr().unwrap()),
        ModelProtocol::OpenAiCompletions,
    );
    let (started_tx, started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        request(&mut socket);
        started_tx.send(()).unwrap();
        release_rx.recv().unwrap();
    });
    let cancel = Arc::new(ConfigurationCancellation::default());
    let worker_cancel = cancel.clone();
    let worker_store = store.clone();
    let worker_channel = channel.clone();
    let worker = std::thread::spawn(move || {
        refresh_directory(
            &worker_store,
            &worker_channel,
            &Credentials::default(),
            &worker_cancel,
        )
    });
    started_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert_eq!(
        refresh_directory(&store, &channel, &Credentials::default(), &Arc::default()).unwrap_err(),
        CatalogueError::Busy
    );
    let at = Instant::now();
    cancel.cancel();
    assert_eq!(
        worker.join().unwrap().unwrap_err(),
        CatalogueError::Cancelled
    );
    assert!(at.elapsed() < Duration::from_secs(2));
    release_tx.send(()).unwrap();
    server.join().unwrap();
    assert!(store.catalogue(&channel).unwrap().directory.is_none());
}

#[test]
fn total_directory_deadline_covers_all_pages_instead_of_restarting_per_page() {
    let listener = listener();
    let (_dir, store, channel) = connection(
        &format!("http://{}/v1", listener.local_addr().unwrap()),
        ModelProtocol::OpenAiCompletions,
    );
    let server = std::thread::spawn(move || {
        for index in 0..2 {
            let mut socket = accept(&listener);
            request(&mut socket);
            std::thread::sleep(Duration::from_secs(8));
            respond(
                &mut socket,
                200,
                if index == 0 {
                    json!({"data":[{"id":"first"}],"has_more":true,"last_id":"first"})
                } else {
                    json!({"data":[{"id":"second"}]})
                },
            );
        }
    });
    let start = Instant::now();
    let error =
        refresh_directory(&store, &channel, &Credentials::default(), &Arc::default()).unwrap_err();
    assert_eq!(error, CatalogueError::Timeout);
    assert!(start.elapsed() >= Duration::from_secs(14));
    assert!(start.elapsed() < Duration::from_secs(18));
    assert!(store.catalogue(&channel).unwrap().directory.is_none());
    server.join().unwrap();
}

#[test]
fn public_recommendations_are_marked_and_make_no_credential_read_or_network_call() {
    let credentials = Credentials::default();
    assert_eq!(PROVIDERS.len(), 12);
    for provider in PROVIDERS
        .iter()
        .filter(|p| p.directory == DirectoryProtocol::Recommended)
    {
        let dir = tempfile::tempdir().unwrap();
        let store = ConfigurationStore::new(dir.path());
        let saved = store
            .save_connection(
                &store.load().unwrap(),
                None,
                draft(provider.endpoint, provider.protocol),
                provider.id,
                provider.region,
            )
            .unwrap();
        let result =
            refresh_directory(&store, &saved.channels()[0], &credentials, &Arc::default()).unwrap();
        assert_eq!(result.source, CatalogueSource::PublicRecommended);
        assert!(!result.is_fresh(result.fetched_at));
    }
    assert_eq!(credentials.reads.load(Ordering::Relaxed), 0);
}

#[test]
fn explicit_tool_probe_uses_fixed_text_and_cannot_execute_a_native_edit() {
    let listener = listener();
    let (_dir, store, channel) = connection(
        &format!("http://{}/v1", listener.local_addr().unwrap()),
        ModelProtocol::OpenAiCompletions,
    );
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        let (line, _, body) = request(&mut socket);
        assert_eq!(line, "POST /v1/chat/completions HTTP/1.1");
        assert_eq!(body["stream"], false);
        assert_eq!(body["max_tokens"], 128);
        assert_eq!(body["tools"].as_array().unwrap().len(), 1);
        assert_eq!(
            body["tools"][0]["function"]["name"],
            "bkcut_connection_probe"
        );
        assert!(!body.to_string().contains("edit_apply_guarded"));
        assert!(!body.to_string().contains("projectIdentity"));
        respond(
            &mut socket,
            200,
            json!({"choices":[{"message":{"tool_calls":[{"function":{"name":"bkcut_connection_probe","arguments":"{\"value\":\"ok\"}"}}]}}]}),
        );
    });
    verify_model(
        &store,
        &channel,
        VerificationCapability::Tools,
        &Credentials::default(),
        &Arc::default(),
    )
    .unwrap();
    assert!(
        store.catalogue(&channel).unwrap().verification.is_empty(),
        "probe does not conflate verification persistence with discovery"
    );
    server.join().unwrap();
}

#[test]
fn explicit_capacity_has_its_own_provenance_and_does_not_forge_directory_verification() {
    let (_dir, store, channel) =
        connection("https://example.test/v1", ModelProtocol::OpenAiCompletions);
    let snapshot = store
        .set_context_override(
            &store.load().unwrap(),
            &channel.id,
            &channel.model,
            Some(8192),
        )
        .unwrap();
    let selected = snapshot
        .selected_model(&channel.id, &channel.model)
        .unwrap();
    let configuration =
        concat_agent::network::runtime_configuration_for(&store, &snapshot, &selected);
    assert_eq!(configuration["contextWindow"], 8192);
    assert_eq!(
        configuration["contextSource"],
        "user-conservative:fixture-model"
    );
    assert!(store.catalogue(&selected).unwrap().verification.is_empty());
    assert!(store.catalogue(&selected).unwrap().directory.is_none());
}

#[test]
fn connection_delete_serializes_vault_removal_and_configuration_removal() {
    use concat_agent::configuration::ConfigurationError;
    let (_dir, store, channel) =
        connection("https://example.test/v1", ModelProtocol::OpenAiCompletions);
    let original = store.load().unwrap();
    let (entered_tx, entered_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let worker_store = store.clone();
    let worker_id = channel.id.clone();
    let worker = std::thread::spawn(move || {
        worker_store.delete_with_credential(&original, &worker_id, |_| {
            entered_tx.send(()).unwrap();
            release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
            Ok::<_, ()>(())
        })
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    assert!(
        store.assert_connection_current(&channel).is_err(),
        "old authorizations must be revoked before deleting a key"
    );
    let live = store.load().unwrap();
    let writes = AtomicUsize::new(0);
    assert_eq!(
        store
            .credential_transaction(&live, &channel.id, |_| {
                writes.fetch_add(1, Ordering::Relaxed);
            })
            .unwrap_err(),
        ConfigurationError::Busy
    );
    assert_eq!(
        writes.load(Ordering::Relaxed),
        0,
        "a concurrent writer must not replace the key while deletion holds the lock"
    );
    release_tx.send(()).unwrap();
    let (deleted, outcome) = worker.join().unwrap().unwrap();
    assert!(outcome.is_ok());
    assert!(deleted.channel(&channel.id).is_none());
    let deletes = AtomicUsize::new(0);
    assert_eq!(
        store
            .delete_with_credential(&live, &channel.id, |_| {
                deletes.fetch_add(1, Ordering::Relaxed);
                Ok::<_, ()>(())
            })
            .unwrap_err(),
        ConfigurationError::StaleConfiguration
    );
    assert_eq!(
        deletes.load(Ordering::Relaxed),
        0,
        "a stale attempt must not access secure storage"
    );
}

#[test]
fn failed_connection_key_delete_retains_a_revoked_retryable_connection() {
    let (_dir, store, channel) =
        connection("https://example.test/v1", ModelProtocol::OpenAiCompletions);
    let original = store.load().unwrap();
    let (retained, outcome) = store
        .delete_with_credential(&original, &channel.id, |_| Err("isolated vault failure"))
        .unwrap();
    assert_eq!(outcome, Err("isolated vault failure"));
    assert_eq!(
        retained.channel(&channel.id).unwrap().endpoint,
        channel.endpoint
    );
    assert!(retained.channel(&channel.id).unwrap().credential_version > channel.credential_version);
    assert!(store.assert_connection_current(&channel).is_err());
    let (deleted, outcome) = store
        .delete_with_credential(&retained, &channel.id, |_| Ok::<_, ()>(()))
        .unwrap();
    assert!(outcome.is_ok());
    assert!(deleted.channel(&channel.id).is_none());
}

#[test]
fn retained_model_membership_requires_a_complete_official_directory() {
    let (_dir, store, channel) =
        connection("https://example.test/v1", ModelProtocol::OpenAiCompletions);
    let snapshot = store.load().unwrap();
    let choices = || store.flattened_models(&snapshot).unwrap();
    assert_eq!(choices()[0].listed_in_directory, None);
    store
        .publish_catalogue(&channel, 10, Err("catalogue.network"))
        .unwrap();
    assert_eq!(choices()[0].listed_in_directory, None);
    store
        .publish_catalogue(&channel, 11, Ok(directory(&["fixture-model"], 11)))
        .unwrap();
    assert_eq!(choices()[0].listed_in_directory, Some(true));
    store
        .publish_catalogue(&channel, 12, Err("catalogue.network"))
        .unwrap();
    assert_eq!(
        choices()[0].listed_in_directory,
        Some(true),
        "a failed partial refresh cannot declare a model missing"
    );
    store
        .publish_catalogue(&channel, 13, Ok(directory(&[], 13)))
        .unwrap();
    assert_eq!(choices()[0].model, channel.model);
    assert_eq!(choices()[0].listed_in_directory, Some(false));
    assert_eq!(
        store.load().unwrap(),
        snapshot,
        "directory membership never changes saved defaults or references"
    );
}

#[test]
fn explicit_probe_authentication_failure_remains_distinct_from_output_exhaustion() {
    let listener = listener();
    let (_dir, store, channel) = connection(
        &format!("http://{}/v1", listener.local_addr().unwrap()),
        ModelProtocol::OpenAiCompletions,
    );
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        request(&mut socket);
        respond(
            &mut socket,
            401,
            json!({"error":"isolated authentication failure"}),
        );
    });
    assert_eq!(
        verify_model(
            &store,
            &channel,
            VerificationCapability::Inference,
            &Credentials::default(),
            &Arc::default()
        ),
        Err(CatalogueError::HttpStatus(401))
    );
    server.join().unwrap();
}

#[test]
fn current_verification_failure_and_historical_success_remain_separate() {
    let (_dir, store, channel) =
        connection("https://example.test/v1", ModelProtocol::OpenAiCompletions);
    for (capability, checked_at, succeeded) in [
        (VerificationCapability::Inference, 100, true),
        (VerificationCapability::Tools, 101, true),
        (VerificationCapability::Inference, 102, false),
    ] {
        store
            .record_verification(
                &channel,
                VerificationRecord {
                    connection_version: channel.version,
                    credential_version: channel.credential_version,
                    model: channel.model.clone(),
                    capability,
                    checked_at,
                    succeeded,
                    error_code: (!succeeded).then(|| "catalogue.outputExhausted".into()),
                },
            )
            .unwrap();
    }
    store
        .publish_catalogue(&channel, 103, Err("catalogue.network"))
        .unwrap();
    let facts = store.catalogue(&channel).unwrap();
    let inference =
        facts.verification_summary(&channel, &channel.model, VerificationCapability::Inference);
    assert_eq!(inference.latest_current.unwrap().checked_at, 102);
    assert!(!inference.latest_current.unwrap().succeeded);
    assert_eq!(inference.last_success.unwrap().checked_at, 100);
    assert_eq!(
        facts
            .verification_summary(&channel, &channel.model, VerificationCapability::Tools)
            .latest_current
            .unwrap()
            .checked_at,
        101
    );
    let changed = store
        .credential_changed(&store.load().unwrap(), &channel.id)
        .unwrap();
    let channel = changed.channel(&channel.id).unwrap();
    let mut current = store.catalogue(channel).unwrap();
    current.verification = store.verification_history(channel).unwrap();
    for capability in [
        VerificationCapability::Inference,
        VerificationCapability::Tools,
    ] {
        let summary = current.verification_summary(channel, &channel.model, capability);
        assert!(
            summary.latest_current.is_none(),
            "new credential is not verified by an old result"
        );
        assert!(
            summary.last_success.is_some(),
            "historical successful time must remain available"
        );
    }
}

#[test]
fn http_failures_keep_exact_safe_status_without_response_or_credential_echo() {
    for (status, code) in [
        (302, "catalogue.redirect"),
        (400, "catalogue.requestRejected"),
        (401, "agent.errorAuthentication"),
        (402, "agent.errorBilling"),
        (403, "agent.errorAccess"),
        (404, "catalogue.requestRejected"),
        (405, "catalogue.requestRejected"),
        (409, "catalogue.requestRejected"),
        (429, "catalogue.rateLimit"),
        (500, "catalogue.serverError"),
        (503, "catalogue.serverError"),
        (599, "catalogue.serverError"),
    ] {
        let listener = listener();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let worker = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            let (line, _, _) = request(&mut socket);
            assert!(line.starts_with("POST /chat/completions"));
            respond(
                &mut socket,
                status,
                json!({"error":{"message":SECRET,"raw_private_detail":"must never persist"}}),
            );
        });
        let (_directory, store, channel) = connection(&endpoint, ModelProtocol::OpenAiCompletions);
        let credentials = Credentials::default();
        let error = verify_model(
            &store,
            &channel,
            VerificationCapability::Tools,
            &credentials,
            &Arc::default(),
        )
        .unwrap_err();
        worker.join().unwrap();
        assert_eq!(error.http_status(), Some(status));
        assert_eq!(error.code(), code);
        assert!(!format!("{error:?}").contains(SECRET));
        assert_eq!(credentials.reads.load(Ordering::Relaxed), 1);
        store
            .record_verification(
                &channel,
                VerificationRecord {
                    connection_version: channel.version,
                    credential_version: channel.credential_version,
                    model: channel.model.clone(),
                    capability: VerificationCapability::Tools,
                    checked_at: 1,
                    succeeded: false,
                    error_code: Some(error.code().into()),
                },
            )
            .unwrap();
        let persisted = serde_json::to_string(&store.catalogue(&channel).unwrap()).unwrap();
        assert!(!persisted.contains(SECRET));
        assert!(!persisted.contains("must never persist"));
        assert!(persisted.contains(code));
    }
    assert_eq!(CatalogueError::Network.http_status(), None);
    assert_eq!(CatalogueError::Timeout.http_status(), None);
    assert_eq!(CatalogueError::Cancelled.http_status(), None);
    assert_eq!(
        CatalogueError::HttpStatus(600).code(),
        "catalogue.unexpectedStatus"
    );
}

#[test]
fn unsupported_local_probe_parameters_do_not_read_credentials_or_send_requests() {
    for (protocol, model, limit) in [
        (ModelProtocol::OpenAiResponses, "fixture-model", 1),
        (
            ModelProtocol::GoogleGenerativeAi,
            "models/fixture-model",
            128,
        ),
    ] {
        let server = listener();
        let endpoint = format!("http://{}", server.local_addr().unwrap());
        let (_directory, store, channel) = connection(&endpoint, protocol);
        let mut changed = draft(&endpoint, protocol);
        changed.model = model.into();
        changed.max_output_tokens = limit;
        let snapshot = store
            .save_channel(&store.load().unwrap(), Some(&channel.id), changed)
            .unwrap();
        let channel = snapshot.channel(&channel.id).unwrap();
        let credentials = Credentials::default();
        for capability in [
            VerificationCapability::Inference,
            VerificationCapability::Tools,
        ] {
            let error = verify_model(&store, channel, capability, &credentials, &Arc::default())
                .unwrap_err();
            assert_eq!(error, CatalogueError::InvalidProbeParameters);
            assert_eq!(error.code(), "catalogue.invalidProbeParameters");
            assert_eq!(error.http_status(), None);
        }
        assert_eq!(credentials.reads.load(Ordering::Relaxed), 0);
        assert!(matches!(server.accept(), Err(e) if e.kind()==std::io::ErrorKind::WouldBlock));
        assert_eq!(
            store
                .load()
                .unwrap()
                .channel(&channel.id)
                .unwrap()
                .max_output_tokens,
            limit,
            "verification never raises the user limit"
        );
    }
}
