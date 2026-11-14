// SPDX-License-Identifier: AGPL-3.0-or-later
//! Actual loopback HTTP transport; no provider, purchased quota or real secret.
//! These exercise the production native network boundary, not a fake fetch.

use std::collections::BTreeMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use concat_agent::configuration::{
    ChannelDraft, ConfigurationSnapshot, ConfigurationStore, ModelProtocol,
};
use concat_agent::credentials::{CredentialError, CredentialScope, CredentialStore, Secret};
use concat_agent::network::{ModelNetwork, ModelRequest};
use concat_agent::{Reconciliation, RecoveryDecision, SessionOwner};
use serde_json::{Value, json};

const TEST_SECRET: &str = "sk-bkcat-isolated-unit-secret-v1";
const WAIT: Duration = Duration::from_secs(5);
const PROJECT: &str = "bkcat-project:11111111111111111111111111111111";

struct FixtureCredentials(CredentialScope);

impl CredentialStore for FixtureCredentials {
    fn read(&self, scope: &CredentialScope) -> Result<Option<Secret>, CredentialError> {
        if scope == &self.0 {
            Secret::new(TEST_SECRET.into()).map(Some)
        } else {
            Err(CredentialError::InvalidScope)
        }
    }

    fn write(&self, _: &CredentialScope, _: &Secret) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }

    fn delete(&self, _: &CredentialScope) -> Result<(), CredentialError> {
        Err(CredentialError::Unavailable)
    }
}

struct Fixture {
    _directory: tempfile::TempDir,
    owner: Arc<SessionOwner>,
    store: ConfigurationStore,
    snapshot: ConfigurationSnapshot,
    network: Arc<ModelNetwork>,
}

impl Fixture {
    fn new(listener: &TcpListener, protocol: ModelProtocol) -> Self {
        let directory = tempfile::tempdir().unwrap();
        let owner_root = directory.path().join("owner");
        std::fs::create_dir(&owner_root).unwrap();
        let owner = Arc::new(SessionOwner::acquire(&owner_root).unwrap());
        owner
            .confirm_recovery(
                &owner.token(),
                Reconciliation {
                    project_identity: PROJECT.into(),
                    project_revision: "1".into(),
                    session_revision: "1".into(),
                },
                RecoveryDecision::DiscardInterrupted,
            )
            .unwrap();
        let store = ConfigurationStore::new(directory.path().join("configuration"));
        let initial = store
            .save_channel(
                &store.load().unwrap(),
                None,
                ChannelDraft {
                    label: "Isolated transport fixture".into(),
                    protocol,
                    endpoint: format!("http://{}/chosen/team", listener.local_addr().unwrap()),
                    model: "fixture-model".into(),
                    request_timeout_ms: 10_000,
                    max_output_tokens: 128,
                },
            )
            .unwrap();
        let snapshot = store
            .select(&initial, Some(&initial.channels()[0].id))
            .unwrap();
        let channel = snapshot.active().unwrap();
        let credentials = Arc::new(FixtureCredentials(
            CredentialScope::new(channel.id.as_str(), &channel.service_origin().unwrap()).unwrap(),
        ));
        let network = Arc::new(
            ModelNetwork::authorize(
                Arc::clone(&owner),
                owner.token(),
                store.clone(),
                snapshot.clone(),
                credentials,
                4,
            )
            .unwrap(),
        );
        Self {
            _directory: directory,
            owner,
            store,
            snapshot,
            network,
        }
    }

    fn request(&self) -> ModelRequest {
        let channel = self.snapshot.active().unwrap();
        let (suffix, body) = match channel.protocol {
            ModelProtocol::OpenAiCompletions => (
                "/chat/completions",
                json!({"model":"fixture-model","messages":[{"role":"user","content":"hello"}],"stream":true,"max_completion_tokens":128}),
            ),
            ModelProtocol::OpenAiResponses => (
                "/responses",
                json!({"model":"fixture-model","input":[{"role":"user","content":"hello"}],"stream":true,"max_output_tokens":128,"store":false}),
            ),
            ModelProtocol::AnthropicMessages => (
                "/v1/messages?beta=true",
                json!({"model":"fixture-model","messages":[{"role":"user","content":"hello"}],"stream":true,"max_tokens":128,"thinking":{"type":"disabled"}}),
            ),
        };
        ModelRequest {
            url: format!("{}{suffix}", channel.endpoint.trim_end_matches('/')),
            method_name: "POST".into(),
            headers: vec![("content-type".into(), "application/json".into())],
            body: body.to_string(),
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.network.cancel();
        // Wait only in the test's teardown. Production UI cancellation never
        // waits for a network thread, and its permit must drain for real.
        let _ = self.owner.finish_handoff(WAIT);
    }
}

struct CapturedRequest {
    line: String,
    headers: BTreeMap<String, String>,
    body: Value,
}

fn listener() -> TcpListener {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    listener
}

fn accept(listener: &TcpListener) -> TcpStream {
    let deadline = Instant::now() + WAIT;
    loop {
        match listener.accept() {
            Ok((stream, _)) => {
                stream.set_read_timeout(Some(WAIT)).unwrap();
                stream.set_write_timeout(Some(WAIT)).unwrap();
                return stream;
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                assert!(Instant::now() < deadline, "fixture received no request");
                std::thread::sleep(Duration::from_millis(2));
            }
            Err(error) => panic!("fixture accept failed: {error}"),
        }
    }
}

fn read_request(stream: &mut TcpStream) -> CapturedRequest {
    let mut bytes = Vec::new();
    let header_end = loop {
        if let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n") {
            break index + 4;
        }
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "request ended before headers");
        bytes.extend_from_slice(&buffer[..count]);
        assert!(bytes.len() <= 1024 * 1024 + 8192);
    };
    let header = std::str::from_utf8(&bytes[..header_end]).unwrap();
    let mut lines = header.split("\r\n");
    let line = lines.next().unwrap().to_owned();
    let headers: BTreeMap<_, _> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let length = headers["content-length"].parse::<usize>().unwrap();
    while bytes.len() < header_end + length {
        let mut buffer = [0_u8; 4096];
        let count = stream.read(&mut buffer).unwrap();
        assert_ne!(count, 0, "request body ended early");
        bytes.extend_from_slice(&buffer[..count]);
    }
    CapturedRequest {
        line,
        headers,
        body: serde_json::from_slice(&bytes[header_end..header_end + length]).unwrap(),
    }
}

fn start_response(stream: &mut TcpStream) {
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
        .unwrap();
    stream.flush().unwrap();
}

fn chunk(stream: &mut TcpStream, bytes: &[u8]) -> std::io::Result<()> {
    write!(stream, "{:x}\r\n", bytes.len())?;
    stream.write_all(bytes)?;
    stream.write_all(b"\r\n")?;
    stream.flush()
}

fn finish(stream: &mut TcpStream) {
    stream.write_all(b"0\r\n\r\n").unwrap();
    stream.flush().unwrap();
}

fn event(value: Value) -> Vec<u8> {
    format!("data: {value}\n\n").into_bytes()
}

fn text_event(text: &str) -> Vec<u8> {
    event(json!({"choices":[{"index":0,"delta":{"content":text}}]}))
}

fn stream_id(head: &Value) -> &str {
    head["streamId"].as_str().unwrap()
}

fn drain(network: &ModelNetwork, id: &str) -> (Vec<u8>, Option<String>) {
    let mut bytes = Vec::new();
    loop {
        match network.read(id) {
            Ok(value) if value["done"] == true => return (bytes, None),
            Ok(value) => bytes.extend(BASE64.decode(value["chunk"].as_str().unwrap()).unwrap()),
            Err(error) => return (bytes, Some(error)),
        }
    }
}

fn wait_operations(owner: &SessionOwner, count: usize) {
    let deadline = Instant::now() + WAIT;
    while owner.status().unwrap().active_operations != count {
        assert!(Instant::now() < deadline, "native operations did not drain");
        std::thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn streams_real_bytes_before_socket_end_and_injects_only_native_scoped_credential() {
    let listener = listener();
    let fixture = Fixture::new(&listener, ModelProtocol::OpenAiCompletions);
    let (captured_tx, captured_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let first = b"data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"hello\"}}]}\r\n\r\n";
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        captured_tx.send(read_request(&mut socket)).unwrap();
        start_response(&mut socket);
        // Split CRLF and JSON across genuine HTTP body chunks.
        chunk(&mut socket, &first[..17]).unwrap();
        chunk(&mut socket, &first[17..first.len() - 1]).unwrap();
        chunk(&mut socket, &first[first.len() - 1..]).unwrap();
        release_rx.recv_timeout(WAIT).unwrap();
        chunk(&mut socket, b"data: [DONE]\n\n").unwrap();
        finish(&mut socket);
    });
    let head = fixture.network.open(fixture.request()).unwrap();
    assert_eq!(head["status"], 200);
    assert!(!head.to_string().contains(TEST_SECRET));
    let captured = captured_rx.recv_timeout(WAIT).unwrap();
    assert_eq!(captured.line, "POST /chosen/team/chat/completions HTTP/1.1");
    assert_eq!(
        captured.headers["authorization"],
        format!("Bearer {TEST_SECRET}")
    );
    assert!(!captured.headers.contains_key("x-api-key"));
    assert_eq!(captured.body["model"], "fixture-model");
    assert_eq!(captured.body["max_completion_tokens"], 128);
    let frame = fixture.network.read(stream_id(&head)).unwrap();
    assert_eq!(
        BASE64.decode(frame["chunk"].as_str().unwrap()).unwrap(),
        first
    );
    assert_eq!(frame["done"], false);
    // Receiving the first frame above is possible while the server is still
    // waiting here, proving that production transfer does not buffer to EOF.
    release_tx.send(()).unwrap();
    let (rest, failure) = drain(&fixture.network, stream_id(&head));
    assert_eq!(rest, b"data: [DONE]\n\n");
    assert!(failure.is_none());
    server.join().unwrap();
}

#[test]
fn rejects_full_destination_and_fee_escalations_before_socket_or_budget_consumption() {
    let listener = listener();
    let other = self::listener();
    let fixture = Fixture::new(&listener, ModelProtocol::OpenAiCompletions);
    for wrong_url in [
        format!(
            "http://{}/chosen/team/chat/completions",
            other.local_addr().unwrap()
        ),
        format!(
            "http://{}/another/team/chat/completions",
            listener.local_addr().unwrap()
        ),
        format!("{}?another=destination", fixture.request().url),
    ] {
        let mut request = fixture.request();
        request.url = wrong_url;
        assert!(fixture.network.open(request).is_err());
    }
    for (field, value) in [
        ("n", json!(2)),
        ("background", json!(true)),
        ("webhook", json!("https://elsewhere.invalid")),
        ("service_tier", json!("priority")),
        ("model", json!("unselected-model")),
        ("max_completion_tokens", json!(129)),
        ("tools", json!([{"type":"web_search"}])),
        (
            "tools",
            json!([{"type":"function","function":{"name":"unregistered_function","parameters":{"type":"object"}}}]),
        ),
    ] {
        let mut request = fixture.request();
        let mut body: Value = serde_json::from_str(&request.body).unwrap();
        body[field] = value;
        request.body = body.to_string();
        assert!(fixture.network.open(request).is_err());
    }
    let mut method = fixture.request();
    method.method_name = "GET".into();
    assert!(fixture.network.open(method).is_err());
    let mut authentication = fixture.request();
    authentication
        .headers
        .push(("authorization".into(), "Bearer page-supplied".into()));
    assert!(fixture.network.open(authentication).is_err());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
    assert_eq!(
        other.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );

    let server = std::thread::spawn(move || {
        for _ in 0..4 {
            let mut socket = accept(&listener);
            read_request(&mut socket);
            start_response(&mut socket);
            chunk(&mut socket, b"data: [DONE]\n\n").unwrap();
            finish(&mut socket);
        }
    });
    for _ in 0..4 {
        let head = fixture.network.open(fixture.request()).unwrap();
        assert!(drain(&fixture.network, stream_id(&head)).1.is_none());
    }
    let error = fixture.network.open(fixture.request()).unwrap_err();
    assert!(error.contains("请求次数"));
    server.join().unwrap();
}

#[test]
fn redirect_and_error_bodies_never_forward_credentials_or_provider_error_text() {
    for status in [307, 401, 429] {
        let listener = listener();
        let redirect_target = self::listener();
        let target = redirect_target.local_addr().unwrap();
        let fixture = Fixture::new(&listener, ModelProtocol::OpenAiCompletions);
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            read_request(&mut socket);
            let body = format!("provider echoed {TEST_SECRET}");
            write!(socket,
                "HTTP/1.1 {status} Fixture\r\nLocation: http://{target}/stolen\r\nX-Echo: {TEST_SECRET}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).unwrap();
            socket.flush().unwrap();
        });
        let error = fixture.network.open(fixture.request()).unwrap_err();
        assert!(!error.contains(TEST_SECRET));
        assert!(!error.contains("provider echoed"));
        match status {
            307 => assert!(error.contains("跳转")),
            401 => assert!(error.contains("凭据")),
            429 => assert!(error.contains("额度")),
            _ => unreachable!(),
        }
        server.join().unwrap();
        assert_eq!(
            redirect_target.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
    }
}

#[test]
fn secret_echo_is_rejected_before_page_bytes_even_through_json_and_split_deltas() {
    let escaped: String = TEST_SECRET
        .bytes()
        .map(|byte| format!("\\u{byte:04x}"))
        .collect();
    let raw = text_event(TEST_SECRET);
    let mut cases = vec![
        vec![raw[..13].to_vec(), raw[13..].to_vec()],
        vec![
            format!("data: {{\"choices\":[{{\"delta\":{{\"content\":\"{escaped}\"}}}}]}}\n\n")
                .into_bytes(),
        ],
        vec![format!("data: {{\"{escaped}\":\"not-a-value\"}}\n\n").into_bytes()],
        vec![
            text_event(&TEST_SECRET[..10]),
            text_event(&TEST_SECRET[10..]),
        ],
    ];
    // Arguments are JSON strings within the outer SSE JSON. Their Unicode
    // escapes and even one escape itself cross separate provider deltas.
    let inner = format!("{{\"text\":\"{escaped}\"}}");
    let arguments = |value: &str| {
        event(
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":value}}]}}]}),
        )
    };
    cases.push(vec![
        arguments(&inner[..11]),
        arguments(&inner[11..43]),
        arguments(&inner[43..]),
    ]);
    let indexed = |index: u64, value: &str| {
        event(
            json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":index,"function":{"arguments":value}}]}}]}),
        )
    };
    cases.push(vec![
        indexed(0, &TEST_SECRET[..10]),
        indexed(1, "another tool's input"),
        indexed(0, &TEST_SECRET[10..]),
    ]);
    for pieces in cases {
        let listener = listener();
        let fixture = Fixture::new(&listener, ModelProtocol::OpenAiCompletions);
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            read_request(&mut socket);
            start_response(&mut socket);
            for bytes in pieces {
                if chunk(&mut socket, &bytes).is_err() {
                    return;
                }
            }
            // The client can terminate as soon as the complete echo is seen.
            let _ = socket.write_all(b"0\r\n\r\n");
        });
        let head = fixture.network.open(fixture.request()).unwrap();
        let (bytes, failure) = drain(&fixture.network, stream_id(&head));
        let failure = failure.expect("secret echo must fail the real transport");
        assert!(
            bytes.is_empty(),
            "no incomplete credential-bearing event reaches the page"
        );
        assert!(!failure.contains(TEST_SECRET));
        assert!(failure.contains("系统凭据"));
        server.join().unwrap();
    }
}

#[test]
fn provider_initial_values_and_final_joins_cannot_complete_an_escaped_credential() {
    let first = &TEST_SECRET[..10];
    let rest = &TEST_SECRET[10..];
    let cases = [
        (
            ModelProtocol::AnthropicMessages,
            vec![
                event(
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"text","text":first}}),
                ),
                event(
                    json!({"type":"content_block_delta","index":1,"delta":{"type":"text_delta","text":"separate block"}}),
                ),
                event(
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":rest}}),
                ),
            ],
        ),
        (
            ModelProtocol::AnthropicMessages,
            vec![
                event(
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":first,"signature":""}}),
                ),
                event(
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"thinking_delta","thinking":rest}}),
                ),
            ],
        ),
        (
            ModelProtocol::AnthropicMessages,
            vec![
                event(
                    json!({"type":"content_block_start","index":0,"content_block":{"type":"thinking","thinking":"","signature":first}}),
                ),
                event(
                    json!({"type":"content_block_delta","index":0,"delta":{"type":"signature_delta","signature":rest}}),
                ),
            ],
        ),
        (
            ModelProtocol::OpenAiResponses,
            vec![
                event(
                    json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"item-0","call_id":"call-0","name":"project_get","arguments":format!("{{\"text\":\"{first}")}}),
                ),
                event(
                    json!({"type":"response.function_call_arguments.delta","output_index":1,"delta":"different slot"}),
                ),
                event(
                    json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":format!("{rest}\"}}") }),
                ),
            ],
        ),
        (
            ModelProtocol::OpenAiResponses,
            vec![event(
                json!({"type":"response.output_item.done","output_index":0,"item":{"type":"message","content":[{"type":"output_text","text":first},{"type":"output_text","text":rest}]}}),
            )],
        ),
    ];
    for (protocol, pieces) in cases {
        let listener = listener();
        let fixture = Fixture::new(&listener, protocol);
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            read_request(&mut socket);
            start_response(&mut socket);
            for bytes in pieces {
                if chunk(&mut socket, &bytes).is_err() {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n");
        });
        let head = fixture.network.open(fixture.request()).unwrap();
        let (bytes, failure) = drain(&fixture.network, stream_id(&head));
        assert!(bytes.is_empty());
        let failure = failure.expect("provider assembly must not reconstruct the credential");
        assert!(failure.contains("系统凭据"));
        assert!(!failure.contains(TEST_SECRET));
        server.join().unwrap();
    }
}

#[test]
fn ambiguous_provider_stream_indices_and_tool_aliases_are_refused_before_delivery() {
    let indexed = |index: Value, id: Option<&str>, arguments: &str| {
        let mut call = json!({"index":index,"function":{"arguments":arguments}});
        if let Some(id) = id {
            call["id"] = id.into();
        }
        event(json!({"choices":[{"index":0,"delta":{"tool_calls":[call]}}]}))
    };
    let first = &TEST_SECRET[..10];
    let rest = &TEST_SECRET[10..];
    let mut cases = vec![
        (
            ModelProtocol::OpenAiCompletions,
            vec![
                indexed(json!(0), Some("call-a"), first),
                indexed(json!(1), Some("call-a"), rest),
            ],
        ),
        (
            ModelProtocol::OpenAiCompletions,
            vec![
                indexed(json!(0), Some("call-a"), first),
                indexed(json!(0), Some("call-b"), rest),
            ],
        ),
        (
            ModelProtocol::OpenAiCompletions,
            vec![
                indexed(json!(0), Some("call-a"), first),
                event(
                    json!({"choices":[{"index":0,"delta":{"tool_calls":[{"id":"call-a","function":{"arguments":rest}}]}}]}),
                ),
            ],
        ),
        (
            ModelProtocol::AnthropicMessages,
            vec![event(
                json!({"type":"content_block_start","content_block":{"type":"text","text":first}}),
            )],
        ),
        (
            ModelProtocol::OpenAiResponses,
            vec![event(
                json!({"type":"response.function_call_arguments.delta","output_index":1.5,"delta":first}),
            )],
        ),
    ];
    for invalid in [
        json!(-1),
        json!(1.0),
        json!(9_007_199_254_740_992_u64),
        Value::Null,
    ] {
        cases.push((
            ModelProtocol::OpenAiCompletions,
            vec![indexed(invalid, Some("call-a"), first)],
        ));
    }
    for (protocol, pieces) in cases {
        let listener = listener();
        let fixture = Fixture::new(&listener, protocol);
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            read_request(&mut socket);
            start_response(&mut socket);
            for bytes in pieces {
                if chunk(&mut socket, &bytes).is_err() {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n");
        });
        let head = fixture.network.open(fixture.request()).unwrap();
        let (bytes, failure) = drain(&fixture.network, stream_id(&head));
        assert!(bytes.is_empty());
        let failure = failure.expect("unknown alias must fail closed");
        assert!(failure.contains("编号") || failure.contains("标识"));
        assert!(!failure.contains(TEST_SECRET));
        server.join().unwrap();
    }
}

#[test]
fn cancellation_interrupts_header_wait_and_owner_stop_interrupts_body_wait() {
    let listener = listener();
    let fixture = Fixture::new(&listener, ModelProtocol::OpenAiCompletions);
    let (accepted_tx, accepted_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        read_request(&mut socket);
        accepted_tx.send(()).unwrap();
        release_rx.recv_timeout(WAIT).unwrap();
    });
    let network = Arc::clone(&fixture.network);
    let request = fixture.request();
    let (result_tx, result_rx) = mpsc::channel();
    let caller = std::thread::spawn(move || result_tx.send(network.open(request)).unwrap());
    accepted_rx.recv_timeout(WAIT).unwrap();
    let started = Instant::now();
    fixture.network.cancel();
    assert!(started.elapsed() < Duration::from_millis(250));
    assert!(
        result_rx
            .recv_timeout(WAIT)
            .unwrap()
            .unwrap_err()
            .contains("停止")
    );
    wait_operations(&fixture.owner, 0);
    release_tx.send(()).unwrap();
    caller.join().unwrap();
    server.join().unwrap();

    let listener = self::listener();
    let fixture = Fixture::new(&listener, ModelProtocol::OpenAiCompletions);
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        read_request(&mut socket);
        start_response(&mut socket);
        release_rx.recv_timeout(WAIT).unwrap();
    });
    let head = fixture.network.open(fixture.request()).unwrap();
    let network = Arc::clone(&fixture.network);
    let id = stream_id(&head).to_owned();
    let (result_tx, result_rx) = mpsc::channel();
    let caller = std::thread::spawn(move || result_tx.send(network.read(&id)).unwrap());
    assert_eq!(fixture.owner.status().unwrap().active_operations, 1);
    fixture.owner.request_stop();
    assert!(result_rx.recv_timeout(WAIT).unwrap().is_err());
    fixture.owner.finish_handoff(WAIT).unwrap();
    release_tx.send(()).unwrap();
    caller.join().unwrap();
    server.join().unwrap();
}

#[test]
fn terminal_reader_cancel_admits_next_request_while_old_socket_is_still_open() {
    let listener = listener();
    let fixture = Fixture::new(&listener, ModelProtocol::OpenAiCompletions);
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut first = accept(&listener);
        read_request(&mut first);
        start_response(&mut first);
        chunk(&mut first, b"data: [DONE]\n\n").unwrap();
        // No HTTP terminating chunk and no socket close for the first request.
        let mut second = accept(&listener);
        read_request(&mut second);
        start_response(&mut second);
        chunk(&mut second, &text_event("second answer")).unwrap();
        release_rx.recv_timeout(WAIT).unwrap();
        chunk(&mut second, b"data: [DONE]\n\n").unwrap();
        finish(&mut second);
        drop(first);
    });
    let first = fixture.network.open(fixture.request()).unwrap();
    let frame = fixture.network.read(stream_id(&first)).unwrap();
    assert_eq!(
        BASE64.decode(frame["chunk"].as_str().unwrap()).unwrap(),
        b"data: [DONE]\n\n"
    );
    let started = Instant::now();
    fixture.network.cancel_stream(stream_id(&first)).unwrap();
    assert!(started.elapsed() < Duration::from_millis(250));
    let second = fixture.network.open(fixture.request()).unwrap();
    let frame = fixture.network.read(stream_id(&second)).unwrap();
    assert!(!frame["done"].as_bool().unwrap());
    // Once the first thread has really dropped its permit, its Active guard
    // must leave the second request's admission intact.
    wait_operations(&fixture.owner, 1);
    assert!(
        fixture
            .network
            .open(fixture.request())
            .unwrap_err()
            .contains("正在接收")
    );
    release_tx.send(()).unwrap();
    assert!(drain(&fixture.network, stream_id(&second)).1.is_none());
    server.join().unwrap();
}

#[test]
fn changed_configuration_retires_old_grant_before_followup_or_late_body_delivery() {
    let listener = listener();
    let fixture = Fixture::new(&listener, ModelProtocol::OpenAiCompletions);
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        read_request(&mut socket);
        start_response(&mut socket);
        release_rx.recv_timeout(WAIT).unwrap();
        let _ = chunk(&mut socket, &text_event("late answer"));
        let _ = socket.write_all(b"0\r\n\r\n");
    });
    let head = fixture.network.open(fixture.request()).unwrap();
    let network = Arc::clone(&fixture.network);
    let id = stream_id(&head).to_owned();
    let caller = std::thread::spawn(move || network.read(&id));
    fixture
        .store
        .credential_changed(&fixture.snapshot, &fixture.snapshot.active().unwrap().id)
        .unwrap();
    release_tx.send(()).unwrap();
    assert!(caller.join().unwrap().is_err());
    assert!(fixture.network.open(fixture.request()).is_err());
    wait_operations(&fixture.owner, 0);
    server.join().unwrap();

    let listener = self::listener();
    let fixture = Fixture::new(&listener, ModelProtocol::OpenAiCompletions);
    let channel = fixture.snapshot.active().unwrap();
    let mut changed = channel.draft();
    changed.endpoint.push_str("/replacement");
    fixture
        .store
        .save_channel(&fixture.snapshot, Some(&channel.id), changed)
        .unwrap();
    assert!(fixture.network.open(fixture.request()).is_err());
    assert_eq!(
        listener.accept().unwrap_err().kind(),
        std::io::ErrorKind::WouldBlock
    );
}

#[test]
fn enforces_event_and_response_bounds_and_cancels_backpressured_stream() {
    for huge_single_event in [true, false] {
        let listener = listener();
        let fixture = Fixture::new(&listener, ModelProtocol::OpenAiCompletions);
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            read_request(&mut socket);
            start_response(&mut socket);
            let bytes = text_event(&"A".repeat(if huge_single_event {
                1024 * 1024
            } else {
                512 * 1024
            }));
            for _ in 0..if huge_single_event { 1 } else { 65 } {
                if chunk(&mut socket, &bytes).is_err() {
                    return;
                }
            }
            let _ = socket.write_all(b"0\r\n\r\n");
        });
        let head = fixture.network.open(fixture.request()).unwrap();
        let (bytes, failure) = drain(&fixture.network, stream_id(&head));
        let failure = failure.expect("a response limit must terminate the real stream");
        assert!(bytes.len() <= 32 * 1024 * 1024);
        if huge_single_event {
            assert!(bytes.is_empty());
            assert!(
                failure.contains("单个事件") && failure.contains("超过允许大小"),
                "{failure}"
            );
        } else {
            assert_eq!(failure, "模型响应超过允许大小。");
            assert!(
                bytes.len() >= 31 * 1024 * 1024,
                "the full-response limit path must receive prior valid events"
            );
        }
        server.join().unwrap();
    }
    let listener = self::listener();
    let fixture = Fixture::new(&listener, ModelProtocol::OpenAiCompletions);
    let (written_tx, written_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();
    let server = std::thread::spawn(move || {
        let mut socket = accept(&listener);
        read_request(&mut socket);
        start_response(&mut socket);
        chunk(&mut socket, &text_event(&"A".repeat(512 * 1024))).unwrap();
        written_tx.send(()).unwrap();
        release_rx.recv_timeout(WAIT).unwrap();
    });
    let head = fixture.network.open(fixture.request()).unwrap();
    written_rx.recv_timeout(WAIT).unwrap();
    // No network.read here: a production sender can be awaiting the bounded
    // mailbox. Cancellation must still release its real owner permit.
    fixture.network.cancel_stream(stream_id(&head)).unwrap();
    wait_operations(&fixture.owner, 0);
    assert!(fixture.network.read(stream_id(&head)).is_err());
    release_tx.send(()).unwrap();
    server.join().unwrap();
}

#[test]
fn original_responses_and_anthropic_shapes_admit_only_registered_local_tools() {
    for protocol in [
        ModelProtocol::OpenAiResponses,
        ModelProtocol::AnthropicMessages,
    ] {
        let listener = listener();
        let fixture = Fixture::new(&listener, protocol);
        for hosted in ["web_search", "code_interpreter", "computer_20250124"] {
            let mut request = fixture.request();
            let mut body: Value = serde_json::from_str(&request.body).unwrap();
            body["tools"] =
                json!([{"type":hosted,"name":"project_get","input_schema":{"type":"object"}}]);
            request.body = body.to_string();
            assert!(fixture.network.open(request).is_err());
        }
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        let server = std::thread::spawn(move || {
            let mut socket = accept(&listener);
            let captured = read_request(&mut socket);
            let (path, header) = match protocol {
                ModelProtocol::OpenAiResponses => ("/chosen/team/responses", "authorization"),
                ModelProtocol::AnthropicMessages => {
                    ("/chosen/team/v1/messages?beta=true", "x-api-key")
                }
                _ => unreachable!(),
            };
            assert_eq!(captured.line, format!("POST {path} HTTP/1.1"));
            assert!(captured.headers[header].ends_with(TEST_SECRET));
            assert_eq!(captured.body["tools"][0]["name"], "project_get");
            start_response(&mut socket);
            chunk(&mut socket, &event(json!({"type": if protocol == ModelProtocol::OpenAiResponses { "response.completed" } else { "message_stop" }}))).unwrap();
            finish(&mut socket);
        });
        let mut request = fixture.request();
        let mut body: Value = serde_json::from_str(&request.body).unwrap();
        body["tools"] = if protocol == ModelProtocol::OpenAiResponses {
            json!([{"type":"function","name":"project_get","description":"Read native project","parameters":{"type":"object","properties":{}},"strict":false}])
        } else {
            request
                .headers
                .push(("anthropic-version".into(), "2023-06-01".into()));
            json!([{"name":"project_get","description":"Read native project","input_schema":{"type":"object","properties":{}},"eager_input_streaming":true}])
        };
        request.body = body.to_string();
        let head = fixture.network.open(request).unwrap();
        assert!(drain(&fixture.network, stream_id(&head)).1.is_none());
        server.join().unwrap();
    }
}
