// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native, task-scoped model transport. There is no browser-network fallback,
//! retry, redirect, provider discovery, or credential returned to JavaScript.
//! A permit is created only by a native submission, never by settings or recovery.

use std::collections::BTreeMap;
use std::future::Future;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;
use std::time::Duration;

use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use reqwest::header::{HeaderMap, HeaderName, HeaderValue};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::sync::{Notify, mpsc};

use crate::configuration::{
    ConfigurationSnapshot, ConfigurationStore, ModelChannel, ModelProtocol,
};
use crate::credentials::{CredentialScope, CredentialStore, Secret};
use crate::{OperationPermit, OwnerToken, SessionOwner};

/// Product ceiling, shown beside the native send action. It is not a price quote.
pub const MAX_REQUESTS_PER_TASK: u32 = 4;
/// Maximum request JSON, including the original persisted conversation.
pub const MAX_REQUEST_BYTES: usize = 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 32 * 1024 * 1024;
const CHUNK_BYTES: usize = 64 * 1024;
const CANCELLED: &str = "本次模型调用已停止；未继续发送后续请求。";

/// The private native-wire request after removing the native ownership fields.
#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct ModelRequest {
    /// Complete URL emitted by the pinned original provider adapter.
    pub url: String,
    /// Kept separate from the native-wire operation's `method` field.
    pub method_name: String,
    /// Only protocol content headers; browser credentials are forbidden.
    pub headers: Vec<(String, String)>,
    /// The original provider's bounded JSON request, never a local path.
    pub body: String,
}

struct Cancellation {
    stopped: AtomicBool,
    changed: Notify,
}

impl Cancellation {
    fn stop(&self) {
        self.stopped.store(true, Ordering::Release);
        // A stored notification also covers cancellation immediately before
        // the worker first polls its cancellation future.
        self.changed.notify_one();
    }
}

enum Frame {
    Head(Value),
    Chunk(Vec<u8>),
    End,
    Failed(&'static str),
}

struct Stream {
    receiver: mpsc::Receiver<Frame>,
    cancellation: Arc<Cancellation>,
    failure: Arc<Mutex<Option<&'static str>>>,
}

struct TransportPolicy {
    channel: ModelChannel,
    store: ConfigurationStore,
    snapshot: ConfigurationSnapshot,
}

/// One explicit native task's bounded model access. Debug is deliberately absent.
pub struct ModelNetwork {
    owner: Arc<SessionOwner>,
    token: OwnerToken,
    store: ConfigurationStore,
    snapshot: ConfigurationSnapshot,
    channel: ModelChannel,
    credentials: Arc<dyn CredentialStore>,
    cancellation: Arc<Cancellation>,
    remaining: AtomicU32,
    active: Arc<AtomicU64>,
    next_request: AtomicU64,
    streams: Mutex<BTreeMap<String, Stream>>,
    stream_cancellations: Mutex<BTreeMap<String, (u64, Arc<Cancellation>)>>,
}

/// Non-secret exact configuration for the fixed original provider adapter.
pub fn runtime_configuration(snapshot: &ConfigurationSnapshot) -> Option<Value> {
    snapshot.active().map(|channel| {
        json!({
            "id": channel.id.as_str(), "version": channel.version.to_string(),
            "configurationVersion": snapshot.version().to_string(),
            "protocol": channel.protocol.as_str(), "endpoint": channel.endpoint,
            "model": channel.model, "requestTimeoutMs": channel.request_timeout_ms,
            "maxOutputTokens": channel.max_output_tokens,
        })
    })
}

impl ModelNetwork {
    /// This constructor is a native-only submission boundary. The caller must
    /// additionally capture the project identity, open session and task mode.
    pub fn authorize(
        owner: Arc<SessionOwner>,
        token: OwnerToken,
        store: ConfigurationStore,
        snapshot: ConfigurationSnapshot,
        credentials: Arc<dyn CredentialStore>,
        request_limit: u32,
    ) -> Result<Self, String> {
        if !(1..=MAX_REQUESTS_PER_TASK).contains(&request_limit) {
            return Err("本次模型请求次数须为1至4次。".into());
        }
        let channel = snapshot.active().cloned().ok_or("请先明确选择模型渠道。")?;
        if channel.protocol == ModelProtocol::OpenAiResponses && channel.max_output_tokens < 16 {
            return Err("当前响应协议至少需要16个输出词元，请修改输出上限后重新授权。".into());
        }
        store
            .assert_current(&snapshot)
            .map_err(|error| error.to_string())?;
        let scope = scope(&channel)?;
        if credentials
            .read(&scope)
            .map_err(|error| error.to_string())?
            .is_none()
        {
            return Err("当前渠道及完整服务地址没有系统凭据，请先保存本项目凭据。".into());
        }
        owner
            .begin(&token)
            .map_err(|error| error.to_string())?
            .with_valid(|| ())
            .map_err(|error| error.to_string())?;
        Ok(Self {
            owner,
            token,
            store,
            snapshot,
            channel,
            credentials,
            cancellation: Arc::new(Cancellation {
                stopped: AtomicBool::new(false),
                changed: Notify::new(),
            }),
            remaining: AtomicU32::new(request_limit),
            active: Arc::new(AtomicU64::new(0)),
            next_request: AtomicU64::new(1),
            streams: Mutex::new(BTreeMap::new()),
            stream_cancellations: Mutex::new(BTreeMap::new()),
        })
    }

    /// Validate the unchanged configuration and native owner before every frame.
    pub fn check_current(&self) -> Result<(), String> {
        if self.cancellation.stopped.load(Ordering::Acquire) {
            return Err(CANCELLED.into());
        }
        self.owner
            .begin(&self.token)
            .map_err(|_| CANCELLED.to_owned())?
            .with_valid(|| ())
            .map_err(|_| CANCELLED.to_owned())?;
        if let Err(error) = self.store.assert_current(&self.snapshot) {
            self.cancel();
            return Err(error.to_string());
        }
        Ok(())
    }

    /// The mailbox waits only for response headers. The HTTP worker is
    /// independently cancellable even while headers or a body chunk are pending.
    pub fn open(&self, mut request: ModelRequest) -> Result<Value, String> {
        self.check_current()?;
        let (headers, canonical_body) = validate_request(&self.channel, &request)?;
        // Send exactly the parsed, validated object. A service cannot interpret
        // duplicate JSON keys differently from the native admission checks.
        request.body = canonical_body;
        let secret = self
            .credentials
            .read(&scope(&self.channel)?)
            .map_err(|error| error.to_string())?
            .ok_or("当前配置的系统凭据已不存在。")?;
        self.check_current()?;
        let sequence = self
            .next_request
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                value.checked_add(1)
            })
            .map_err(|_| "模型请求序号已达上限。".to_owned())?;
        self.active
            .compare_exchange(0, sequence, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| "本次任务已有正在接收的模型请求。".to_owned())?;
        let start = (|| {
            let permit = self
                .owner
                .begin(&self.token)
                .map_err(|_| CANCELLED.to_owned())?;
            permit
                .with_valid(|| {
                    self.remaining
                        .fetch_update(Ordering::AcqRel, Ordering::Acquire, |value| {
                            value.checked_sub(1)
                        })
                })
                .map_err(|_| CANCELLED.to_owned())?
                .map_err(|_| {
                    "已达到本次明确授权的模型请求次数；请检查结果后重新发送。".to_owned()
                })?;
            let id = random_id()?;
            let stream_cancel = Arc::new(Cancellation {
                stopped: AtomicBool::new(false),
                changed: Notify::new(),
            });
            self.stream_cancellations
                .lock()
                .map_err(|_| "模型接收状态不可用。")?
                .insert(id.clone(), (sequence, Arc::clone(&stream_cancel)));
            let reader_cancel = Arc::clone(&stream_cancel);
            let (sender, mut receiver) = mpsc::channel(2);
            // A terminal error cannot depend on free space in the bounded
            // body mailbox. Recording it never waits for the UI consumer.
            let failure = Arc::new(Mutex::new(None));
            let worker_failure = Arc::clone(&failure);
            let cancellation = Arc::clone(&self.cancellation);
            let active = Arc::clone(&self.active);
            let policy = TransportPolicy {
                channel: self.channel.clone(),
                store: self.store.clone(),
                snapshot: self.snapshot.clone(),
            };
            std::thread::Builder::new().name("bk-cat-model-request".into()).spawn(move || {
                struct Active(Arc<AtomicU64>, u64);
                impl Drop for Active { fn drop(&mut self) { let _ = self.0.compare_exchange(self.1, 0, Ordering::AcqRel, Ordering::Acquire); } }
                let _active = Active(active, sequence);
                let runtime = tokio::runtime::Builder::new_current_thread().enable_all().build();
                match runtime {
                    Ok(runtime) => runtime.block_on(async {
                        let stop = permit.stop_signal();
                        let cancelled = async {
                            loop {
                                if cancellation.stopped.load(Ordering::Acquire) || stream_cancel.stopped.load(Ordering::Acquire) || stop.requested() { break; }
                                tokio::select! {
                                    _ = cancellation.changed.notified() => {},
                                    _ = stream_cancel.changed.notified() => {},
                                    _ = tokio::time::sleep(Duration::from_millis(25)) => {},
                                }
                            }
                        };
                        let result = tokio::select! {
                            biased;
                            _ = cancelled => Err(CANCELLED),
                            result = tokio::time::timeout(Duration::from_millis(u64::from(policy.channel.request_timeout_ms)),
                                transfer(&policy, request, headers, secret, &permit, &sender)) => {
                                result.unwrap_or(Err("模型请求超过已配置时限，未自动重试。"))
                            }
                        };
                        if let Err(message) = result {
                            // If the reader disappeared, dropping the sender still
                            // releases the operation permit and native ownership.
                            fail_stream(&sender, &worker_failure, message);
                        }
                    }),
                    Err(_) => { fail_stream(&sender, &worker_failure, "无法启动原生模型网络组件。"); }
                }
            }).map_err(|_| "无法创建模型请求工作线程。".to_owned())?;
            match receiver.blocking_recv() {
                Some(Frame::Head(mut head)) => {
                    self.check_current()?;
                    head["streamId"] = json!(id);
                    self.streams
                        .lock()
                        .map_err(|_| "模型接收状态不可用。".to_owned())?
                        .insert(
                            id,
                            Stream {
                                receiver,
                                cancellation: reader_cancel,
                                failure,
                            },
                        );
                    Ok(head)
                }
                Some(Frame::Failed(message)) => {
                    self.remove_stream(&id);
                    Err(message.into())
                }
                _ => {
                    self.remove_stream(&id);
                    Err(terminal_failure(&failure)
                        .unwrap_or("模型请求未返回可验证的响应头。")
                        .into())
                }
            }
        })();
        // A failed start may have left no worker to release this reservation.
        if start.is_err() {
            let _ = self
                .active
                .compare_exchange(sequence, 0, Ordering::AcqRel, Ordering::Acquire);
        }
        start
    }

    /// Backpressure: at most two bounded chunks wait between HTTP and the page.
    pub fn read(&self, stream_id: &str) -> Result<Value, String> {
        self.check_current()?;
        let mut stream = self
            .streams
            .lock()
            .map_err(|_| "模型接收状态不可用。".to_owned())?
            .remove(stream_id)
            .ok_or("模型接收编号无效或已结束。")?;
        if stream.cancellation.stopped.load(Ordering::Acquire) {
            return Err(CANCELLED.into());
        }
        let frame = stream.receiver.blocking_recv();
        self.check_current()?;
        if stream.cancellation.stopped.load(Ordering::Acquire) {
            return Err(CANCELLED.into());
        }
        match frame {
            Some(Frame::Chunk(chunk)) => {
                let mut streams = self
                    .streams
                    .lock()
                    .map_err(|_| "模型接收状态不可用。".to_owned())?;
                // A concurrent cancellation sets the flag before removing the
                // entry. Recheck under its removal lock so a just-completed
                // read cannot resurrect the retired response afterward.
                if stream.cancellation.stopped.load(Ordering::Acquire) {
                    return Err(CANCELLED.into());
                }
                streams.insert(stream_id.to_owned(), stream);
                Ok(json!({"chunk": BASE64.encode(chunk), "done": false}))
            }
            Some(Frame::End) => {
                self.remove_stream(stream_id);
                Ok(json!({"chunk":"", "done":true}))
            }
            Some(Frame::Failed(message)) => {
                self.remove_stream(stream_id);
                Err(message.into())
            }
            _ => {
                self.remove_stream(stream_id);
                Err(terminal_failure(&stream.failure)
                    .unwrap_or("模型响应中途结束，未自动重试；请检查本次任务状态。")
                    .into())
            }
        }
    }

    /// Immediate revocation, independent of the serial SQLite/message worker.
    pub fn cancel(&self) {
        self.cancellation.stop();
    }

    /// Body-consumer cancellation ends only that response. Original providers
    /// may cancel a reader after receiving their terminal event; that must not
    /// revoke the task before Pi persists its completed answer.
    pub fn cancel_stream(&self, id: &str) -> Result<(), String> {
        if let Some((sequence, cancellation)) = self
            .stream_cancellations
            .lock()
            .map_err(|_| "模型接收状态不可用。")?
            .remove(id)
        {
            cancellation.stop();
            // Retire only this response admission. The worker retains its real
            // owner permit until the socket operation has actually drained.
            let _ = self
                .active
                .compare_exchange(sequence, 0, Ordering::AcqRel, Ordering::Acquire);
        }
        self.streams
            .lock()
            .map_err(|_| "模型接收状态不可用。")?
            .remove(id);
        Ok(())
    }

    fn remove_stream(&self, id: &str) {
        if let Ok(mut cancellations) = self.stream_cancellations.lock()
            && let Some((sequence, _)) = cancellations.remove(id)
        {
            let _ = self
                .active
                .compare_exchange(sequence, 0, Ordering::AcqRel, Ordering::Acquire);
        }
    }
}

fn fail_stream(
    sender: &mpsc::Sender<Frame>,
    failure: &Mutex<Option<&'static str>>,
    message: &'static str,
) {
    *failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(message);
    let _ = sender.try_send(Frame::Failed(message));
}

fn terminal_failure(failure: &Mutex<Option<&'static str>>) -> Option<&'static str> {
    *failure
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

impl Drop for ModelNetwork {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn scope(channel: &ModelChannel) -> Result<CredentialScope, String> {
    CredentialScope::new(
        channel.id.as_str(),
        &channel
            .service_origin()
            .map_err(|error| error.to_string())?,
    )
    .map_err(|error| error.to_string())
}

fn random_id() -> Result<String, String> {
    let mut bytes = [0_u8; 24];
    getrandom::fill(&mut bytes).map_err(|_| "无法生成原生请求编号。".to_owned())?;
    Ok(bytes.iter().map(|byte| format!("{byte:02x}")).collect())
}

fn expected_url(channel: &ModelChannel) -> String {
    let suffix = match channel.protocol {
        ModelProtocol::OpenAiCompletions => "/chat/completions",
        ModelProtocol::OpenAiResponses => "/responses",
        // The pinned original Pi adapter uses the SDK beta messages entry.
        ModelProtocol::AnthropicMessages => "/v1/messages?beta=true",
    };
    format!("{}{suffix}", channel.endpoint.trim_end_matches('/'))
}

fn validate_request(
    channel: &ModelChannel,
    request: &ModelRequest,
) -> Result<(HeaderMap, String), String> {
    if request.method_name != "POST" || request.url != expected_url(channel) {
        return Err("模型请求不属于已授权渠道的完整服务地址。".into());
    }
    if request.body.is_empty()
        || request.body.len() > MAX_REQUEST_BYTES
        || request.headers.len() > 4
    {
        return Err("模型请求超过本次允许的大小或头部范围。".into());
    }
    let body: Value =
        serde_json::from_str(&request.body).map_err(|_| "模型请求不是有效的数据对象。")?;
    if !body.is_object() || body["model"] != channel.model || body["stream"] != true {
        return Err("模型请求与已选择的模型或流式模式不一致。".into());
    }
    validate_body(channel.protocol, &body)?;
    let field = match channel.protocol {
        ModelProtocol::OpenAiCompletions => {
            if body.get("max_completion_tokens").is_some() {
                "max_completion_tokens"
            } else {
                "max_tokens"
            }
        }
        ModelProtocol::OpenAiResponses => "max_output_tokens",
        ModelProtocol::AnthropicMessages => "max_tokens",
    };
    let ceiling = body[field].as_u64().ok_or("模型请求缺少明确输出上限。")?;
    if ceiling == 0 || ceiling > u64::from(channel.max_output_tokens) {
        return Err("模型请求超过已授权的输出上限。".into());
    }
    for alternate in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        if alternate != field && body.get(alternate).is_some() {
            return Err("模型请求包含冲突的输出上限。".into());
        }
    }
    // These fields can select another charging destination or multiple outputs.
    if body.get("n").is_some_and(|value| value.as_u64() != Some(1))
        || ["webhook", "background", "service_tier"]
            .iter()
            .any(|field| body.get(*field).is_some())
    {
        return Err("模型请求包含未授权的附加执行方式。".into());
    }
    let mut headers = HeaderMap::new();
    for (name, value) in &request.headers {
        let lower = name.to_ascii_lowercase();
        if !matches!(
            lower.as_str(),
            "accept" | "content-type" | "anthropic-version" | "anthropic-beta"
        ) || value.len() > 1024
            || value.chars().any(char::is_control)
        {
            return Err("模型请求包含未授权的头部。".into());
        }
        if lower.starts_with("anthropic-") && channel.protocol != ModelProtocol::AnthropicMessages {
            return Err("模型协议头部与所选渠道不一致。".into());
        }
        let name = HeaderName::from_bytes(lower.as_bytes()).map_err(|_| "模型请求头部无效。")?;
        if headers.contains_key(&name) {
            return Err("模型请求包含重复头部。".into());
        }
        headers.insert(
            name,
            HeaderValue::from_str(value).map_err(|_| "模型请求头部无效。")?,
        );
    }
    if headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        != Some("application/json")
    {
        return Err("模型请求必须使用明确的数据对象格式。".into());
    }
    let canonical = serde_json::to_string(&body).map_err(|_| "模型请求无法生成确定的数据对象。")?;
    Ok((headers, canonical))
}

fn allowed_keys(value: &Value, allowed: &[&str]) -> Result<(), String> {
    let object = value.as_object().ok_or("模型请求字段必须是数据对象。")?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("模型请求包含尚未授权的协议字段。".into());
    }
    Ok(())
}

fn local_tool_name(value: &Value) -> bool {
    matches!(
        value.as_str(),
        Some("concat_version" | "project_get" | "timeline_read" | "edit_apply_guarded")
    )
}

fn validate_body(protocol: ModelProtocol, body: &Value) -> Result<(), String> {
    let fields: &[&str] = match protocol {
        ModelProtocol::OpenAiCompletions => &[
            "model",
            "messages",
            "stream",
            "stream_options",
            "store",
            "max_tokens",
            "max_completion_tokens",
            "temperature",
            "tools",
            "tool_choice",
        ],
        ModelProtocol::OpenAiResponses => &[
            "model",
            "input",
            "stream",
            "store",
            "max_output_tokens",
            "temperature",
            "tools",
            "tool_choice",
        ],
        ModelProtocol::AnthropicMessages => &[
            "model",
            "messages",
            "system",
            "stream",
            "max_tokens",
            "temperature",
            "tools",
            "tool_choice",
            "thinking",
        ],
    };
    allowed_keys(body, fields)?;
    if body.get("store").is_some_and(|value| value != false)
        || body
            .get("thinking")
            .is_some_and(|value| value != &json!({"type":"disabled"}))
    {
        return Err("模型请求包含未授权的保存或推理方式。".into());
    }
    if let Some(options) = body.get("stream_options")
        && options != &json!({"include_usage":true})
    {
        return Err("模型流式参数不在已核验范围内。".into());
    }
    let messages = match protocol {
        ModelProtocol::OpenAiResponses => &body["input"],
        _ => &body["messages"],
    };
    if !messages.is_array() {
        return Err("模型请求缺少已核验的消息数组。".into());
    }
    if let Some(tools) = body.get("tools") {
        let tools = tools
            .as_array()
            .filter(|tools| tools.len() <= 4)
            .ok_or("模型工具列表超出本机授权范围。")?;
        let mut names = std::collections::BTreeSet::new();
        for tool in tools {
            let function = match protocol {
                ModelProtocol::OpenAiCompletions => {
                    allowed_keys(tool, &["type", "function"])?;
                    if tool["type"] != "function" {
                        return Err("服务端收费工具不在本次授权范围内。".into());
                    }
                    allowed_keys(
                        &tool["function"],
                        &["name", "description", "parameters", "strict"],
                    )?;
                    &tool["function"]
                }
                ModelProtocol::OpenAiResponses => {
                    allowed_keys(
                        tool,
                        &["type", "name", "description", "parameters", "strict"],
                    )?;
                    if tool["type"] != "function" {
                        return Err("服务端收费工具不在本次授权范围内。".into());
                    }
                    tool
                }
                ModelProtocol::AnthropicMessages => {
                    allowed_keys(
                        tool,
                        &[
                            "name",
                            "description",
                            "input_schema",
                            "strict",
                            "eager_input_streaming",
                        ],
                    )?;
                    tool
                }
            };
            let schema = if protocol == ModelProtocol::AnthropicMessages {
                &function["input_schema"]
            } else {
                &function["parameters"]
            };
            if !local_tool_name(&function["name"])
                || !schema.is_object()
                || schema["type"] != "object"
                || !names.insert(function["name"].as_str().unwrap_or_default())
                || function
                    .get("strict")
                    .is_some_and(|value| !value.is_boolean())
                || function
                    .get("eager_input_streaming")
                    .is_some_and(|value| !value.is_boolean())
            {
                return Err("模型只能调用当前应用注册的本机函数工具。".into());
            }
        }
    }
    if let Some(choice) = body.get("tool_choice") {
        let valid = match protocol {
            ModelProtocol::OpenAiCompletions => {
                matches!(choice.as_str(), Some("auto" | "none" | "required"))
                    || (choice["type"] == "function"
                        && local_tool_name(&choice["function"]["name"])
                        && allowed_keys(choice, &["type", "function"]).is_ok()
                        && allowed_keys(&choice["function"], &["name"]).is_ok())
            }
            ModelProtocol::OpenAiResponses => {
                matches!(choice.as_str(), Some("auto" | "none" | "required"))
                    || (choice["type"] == "function"
                        && local_tool_name(&choice["name"])
                        && allowed_keys(choice, &["type", "name"]).is_ok())
            }
            ModelProtocol::AnthropicMessages => {
                (matches!(choice["type"].as_str(), Some("auto" | "any" | "none"))
                    && allowed_keys(choice, &["type"]).is_ok())
                    || (choice["type"] == "tool"
                        && local_tool_name(&choice["name"])
                        && allowed_keys(choice, &["type", "name"]).is_ok())
            }
        };
        if !valid {
            return Err("模型工具选择超出本机授权范围。".into());
        }
    }
    // Native deferred/hosted tools must also be absent from transcript entries;
    // custom messages cannot smuggle a provider tool declaration through input.
    fn reject_hosted(value: &Value) -> Result<(), String> {
        match value {
            Value::Object(fields) => {
                if let Some(kind) = fields.get("type").and_then(Value::as_str)
                    && (kind.starts_with("web_")
                        || kind.starts_with("code_interpreter")
                        || kind.starts_with("computer")
                        || kind.starts_with("tool_search")
                        || matches!(kind, "tool_addition" | "mcp_call" | "mcp_approval_request"))
                {
                    return Err("消息包含未授权的服务端工具或动态工具声明。".into());
                }
                for child in fields.values() {
                    reject_hosted(child)?;
                }
            }
            Value::Array(items) => {
                for child in items {
                    reject_hosted(child)?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    reject_hosted(messages)
}

async fn transfer(
    policy: &TransportPolicy,
    request: ModelRequest,
    mut headers: HeaderMap,
    secret: Secret,
    permit: &OperationPermit,
    sender: &mpsc::Sender<Frame>,
) -> Result<(), &'static str> {
    let (name, authorization) = match policy.channel.protocol {
        ModelProtocol::AnthropicMessages => (
            "x-api-key",
            zeroize::Zeroizing::new(secret.expose_secret().to_owned()),
        ),
        _ => (
            "authorization",
            zeroize::Zeroizing::new(format!("Bearer {}", secret.expose_secret())),
        ),
    };
    let mut authorization =
        HeaderValue::from_str(&authorization).map_err(|_| "系统凭据无法用于当前模型协议。")?;
    authorization.set_sensitive(true);
    headers.insert(HeaderName::from_static(name), authorization);
    let client = reqwest::Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .connect_timeout(Duration::from_secs(20))
        .pool_max_idle_per_host(0)
        .build()
        .map_err(|_| "无法初始化安全模型连接。")?;
    let request = client
        .post(request.url)
        .headers(headers)
        .body(request.body)
        .build()
        .map_err(|_| "无法构造已授权模型请求。")?;
    // Poll initiation under the same native permit used by the edit/storage
    // bridge. Never hold its mutex while awaiting DNS, TLS, headers or data.
    let mut execute = std::pin::pin!(client.execute(request));
    let mut response = std::future::poll_fn(|context| {
        match permit.with_valid(|| {
            if policy.store.assert_current(&policy.snapshot).is_err() {
                return Poll::Ready(Err("模型配置已改变，旧调用许可已失效。"));
            }
            execute
                .as_mut()
                .poll(context)
                .map(|result| result.map_err(|_| "模型网络请求失败，未自动重试。"))
        }) {
            Ok(Poll::Pending) => Poll::Pending,
            Ok(Poll::Ready(result)) => Poll::Ready(result),
            Err(_) => Poll::Ready(Err(CANCELLED)),
        }
    })
    .await?;
    permit.with_valid(|| ()).map_err(|_| CANCELLED)?;
    let status = response.status().as_u16();
    if !(200..300).contains(&status) {
        return Err(match status {
            300..=399 => "模型服务要求跳转，已拒绝向其他目的地转发凭据。",
            401 | 403 => "模型服务拒绝了当前凭据或权限。",
            429 => "模型服务额度或速率限制，任务已停止且未自动重试。",
            _ => "模型服务返回失败状态，任务未完成且未自动重试。",
        });
    }
    let content_type = response
        .headers()
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if content_type.split(';').next().map(str::trim) != Some("text/event-stream") {
        return Err("模型服务没有返回当前协议要求的流式内容。");
    }
    sender
        .send(Frame::Head(
            json!({"status":status,"headers":[["content-type","text/event-stream"]]}),
        ))
        .await
        .map_err(|_| CANCELLED)?;
    let mut filter = SecretFilter::new(secret.expose_secret().as_bytes());
    let mut received = 0_usize;
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| "模型响应接收失败，未自动重试。")?
    {
        permit.with_valid(|| ()).map_err(|_| CANCELLED)?;
        received = received
            .checked_add(chunk.len())
            .ok_or("模型响应超过允许大小。")?;
        if received > MAX_RESPONSE_BYTES {
            return Err("模型响应超过允许大小。");
        }
        let safe = filter.push(&chunk, false)?;
        send_chunks(sender, safe).await?;
    }
    let safe = filter.push(&[], true)?;
    send_chunks(sender, safe).await?;
    permit.with_valid(|| ()).map_err(|_| CANCELLED)?;
    sender.send(Frame::End).await.map_err(|_| CANCELLED)
}

async fn send_chunks(sender: &mpsc::Sender<Frame>, bytes: Vec<u8>) -> Result<(), &'static str> {
    for chunk in bytes.chunks(CHUNK_BYTES) {
        sender
            .send(Frame::Chunk(chunk.to_vec()))
            .await
            .map_err(|_| CANCELLED)?;
    }
    Ok(())
}

/// Inspect complete SSE frames before any bytes reach the embedded page.
/// JSON escapes are decoded, and prefixes of a credential stay native across
/// successive values at the same JSON path. No withheld frame is released
/// until that prefix is disproved or the response ends without completing it.
struct SecretFilter {
    secret: zeroize::Zeroizing<Vec<u8>>,
    failure: Vec<usize>,
    pending: Vec<u8>,
    held: Vec<u8>,
    prefixes: BTreeMap<String, FragmentPrefix>,
    tool_ids: BTreeMap<String, u64>,
    tool_indices: BTreeMap<u64, String>,
}

#[derive(Default)]
struct FragmentPrefix {
    raw: usize,
    decoded: usize,
    escape: String,
}

impl SecretFilter {
    const MAX_EVENT_BYTES: usize = 1024 * 1024;
    const REFUSED: &'static str = "模型响应包含系统凭据，已停止接收并拒绝保存该内容。";

    fn new(secret: &[u8]) -> Self {
        let mut failure = vec![0; secret.len()];
        let mut matched = 0;
        for index in 1..secret.len() {
            while matched > 0 && secret[index] != secret[matched] {
                matched = failure[matched - 1];
            }
            if secret[index] == secret[matched] {
                matched += 1;
            }
            failure[index] = matched;
        }
        Self {
            secret: zeroize::Zeroizing::new(secret.to_vec()),
            failure,
            pending: Vec::new(),
            held: Vec::new(),
            prefixes: BTreeMap::new(),
            tool_ids: BTreeMap::new(),
            tool_indices: BTreeMap::new(),
        }
    }

    fn scan(&self, bytes: &[u8], mut matched: usize) -> Result<usize, &'static str> {
        if self.secret.is_empty() {
            return Err(Self::REFUSED);
        }
        for byte in bytes {
            while matched > 0 && *byte != self.secret[matched] {
                matched = self.failure[matched - 1];
            }
            if *byte == self.secret[matched] {
                matched += 1;
            }
            if matched == self.secret.len() {
                return Err(Self::REFUSED);
            }
        }
        Ok(matched)
    }

    fn inspect_json(&mut self, value: &Value, path: &str) -> Result<(), &'static str> {
        if path.len() > 8192 || self.prefixes.len() > 4096 {
            return Err("模型流式内容超出安全解析范围。");
        }
        match value {
            Value::String(value) => {
                // Original adapters append deltas but replace metadata. A
                // harmless id ending in a key prefix must not hold the stream.
                let incremental = path.ends_with("/delta") || path.contains("/delta/");
                if incremental {
                    let mut previous = self.prefixes.remove(path).unwrap_or_default();
                    previous.raw = self.scan(value.as_bytes(), previous.raw)?;
                    previous.escape.push_str(value);
                    let (decoded, pending) = decode_unicode_escapes(&previous.escape, false)?;
                    previous.decoded = self.scan(decoded.as_bytes(), previous.decoded)?;
                    previous.escape = pending;
                    if previous.raw != 0 || previous.decoded != 0 || !previous.escape.is_empty() {
                        self.prefixes.insert(path.to_owned(), previous);
                    }
                } else {
                    self.scan(value.as_bytes(), 0)?;
                    let (decoded, _) = decode_unicode_escapes(value, true)?;
                    self.scan(decoded.as_bytes(), 0)?;
                }
            }
            Value::Object(fields) => {
                for (key, value) in fields {
                    self.scan(key.as_bytes(), 0)?;
                    let (decoded, _) = decode_unicode_escapes(key, true)?;
                    self.scan(decoded.as_bytes(), 0)?;
                    // JSON-pointer escaping prevents distinct field names from
                    // sharing an accidental concatenation identity.
                    let key = key.replace('~', "~0").replace('/', "~1");
                    self.inspect_json(value, &format!("{path}/{key}"))?;
                }
            }
            Value::Array(values) => {
                for (index, value) in values.iter().enumerate() {
                    // The pinned Completions adapter groups tool fragments by
                    // their declared index, not their array position. Separate
                    // calls can each occupy array position zero in turn.
                    let index = if path.ends_with("/tool_calls") {
                        stream_index(&value["index"])?
                    } else {
                        index as u64
                    };
                    self.inspect_json(value, &format!("{path}/{index}"))?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn inspect_event(&mut self, event: &[u8]) -> Result<bool, &'static str> {
        self.scan(event, 0)?;
        let text = std::str::from_utf8(event).map_err(|_| "模型流式内容不是有效文本编码。")?;
        let mut data = String::new();
        for line in text.split(['\r', '\n']) {
            if let Some(value) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(value.strip_prefix(' ').unwrap_or(value));
            }
        }
        if data.trim() == "[DONE]" {
            return Ok(true);
        }
        if !data.is_empty() {
            let value: Value =
                serde_json::from_str(&data).map_err(|_| "模型流式事件不是有效的数据对象。")?;
            self.inspect_stream_identity(&value)?;
            // Responses and Anthropic append into declared output/block slots;
            // interleaving another slot must not discharge a withheld prefix.
            let slot = value
                .get("output_index")
                .or_else(|| value.get("index"))
                .and_then(Value::as_u64)
                .map(|index| format!("/slot/{index}"))
                .unwrap_or_default();
            self.inspect_initial_fragments(&value, &slot)?;
            self.inspect_json(&value, &slot)?;
            return Ok(matches!(
                value.get("type").and_then(Value::as_str),
                Some(
                    "message_stop"
                        | "response.completed"
                        | "response.failed"
                        | "response.incomplete"
                )
            ));
        }
        Ok(false)
    }

    /// The original adapters can alias numeric tool streams through string
    /// ids. This bridge supports only their unambiguous indexed form; it must
    /// refuse an unknown alias instead of checking a different assembly path.
    fn inspect_stream_identity(&mut self, value: &Value) -> Result<(), &'static str> {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if matches!(
            kind,
            "content_block_start" | "content_block_delta" | "content_block_stop"
        ) {
            stream_index(&value["index"])?;
        }
        if kind.starts_with("response.")
            && (value.get("output_index").is_some()
                || kind.starts_with("response.output_item.")
                || kind.starts_with("response.output_text.")
                || kind.starts_with("response.refusal.")
                || kind.starts_with("response.reasoning_text.")
                || kind.starts_with("response.reasoning_summary_text.")
                || kind.starts_with("response.function_call_arguments.")
                || kind.starts_with("response.custom_tool_call_input."))
        {
            stream_index(&value["output_index"])?;
        }
        if let Some(choices) = value.get("choices").and_then(Value::as_array) {
            for choice in choices {
                let Some(calls) = choice["delta"].get("tool_calls") else {
                    continue;
                };
                if calls.is_null() {
                    continue;
                }
                let calls = calls
                    .as_array()
                    .ok_or("模型工具增量缺少可核验的编号列表。")?;
                for call in calls {
                    let index = stream_index(&call["index"])?;
                    let Some(id) = call.get("id") else {
                        continue;
                    };
                    let id = id
                        .as_str()
                        .filter(|id| id.len() <= 1024)
                        .ok_or("模型工具流标识无法核验，已停止接收。")?;
                    if id.is_empty() {
                        continue;
                    }
                    if self.tool_ids.get(id).is_some_and(|known| *known != index)
                        || self
                            .tool_indices
                            .get(&index)
                            .is_some_and(|known| known != id)
                    {
                        return Err("模型工具流编号与标识发生冲突，已停止接收。");
                    }
                    if self.tool_ids.len() >= 4096 && !self.tool_ids.contains_key(id) {
                        return Err("模型工具流标识超过安全解析范围。");
                    }
                    self.tool_ids.insert(id.to_owned(), index);
                    self.tool_indices.insert(index, id.to_owned());
                }
            }
        }
        Ok(())
    }

    /// Seed exactly the same logical path that the pinned provider appends to.
    /// Otherwise an initial block value could expose the first half of a key
    /// before a later delta completed it under a different JSON field name.
    fn inspect_initial_fragments(&mut self, value: &Value, slot: &str) -> Result<(), &'static str> {
        let kind = value
            .get("type")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "content_block_start" => {
                for (field, destination) in [
                    ("text", "text"),
                    ("thinking", "thinking"),
                    ("signature", "signature"),
                    ("data", "signature"),
                ] {
                    if let Some(initial) = value["content_block"].get(field) {
                        let path = format!("{slot}/delta/{destination}");
                        self.prefixes.remove(&path);
                        self.inspect_json(initial, &path)?;
                    }
                }
            }
            "response.output_item.added" => {
                let field = match value["item"]["type"].as_str() {
                    Some("function_call") => Some("arguments"),
                    Some("custom_tool_call") => Some("input"),
                    _ => None,
                };
                if let Some(initial) = field.and_then(|field| value["item"].get(field)) {
                    let path = format!("{slot}/delta");
                    self.prefixes.remove(&path);
                    self.inspect_json(initial, &path)?;
                }
            }
            "response.function_call_arguments.done" | "response.custom_tool_call_input.done" => {
                let field = if kind == "response.function_call_arguments.done" {
                    "arguments"
                } else {
                    "input"
                };
                if let Some(replacement) = value.get(field) {
                    let path = format!("{slot}/delta");
                    self.prefixes.remove(&path);
                    self.inspect_json(replacement, &path)?;
                }
            }
            "response.output_item.done" if value["item"]["type"] == "message" => {
                if let Some(content) = value["item"]["content"].as_array() {
                    // The original adapter joins completed text/refusal parts
                    // without a separator before displaying or saving them.
                    let text: String = content
                        .iter()
                        .filter_map(|part| {
                            let field = if part["type"] == "output_text" {
                                "text"
                            } else {
                                "refusal"
                            };
                            part.get(field).and_then(Value::as_str)
                        })
                        .collect();
                    self.inspect_json(&Value::String(text), "/native-completed-message")?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn push(&mut self, bytes: &[u8], finish: bool) -> Result<Vec<u8>, &'static str> {
        self.pending.extend_from_slice(bytes);
        let mut safe = Vec::new();
        while let Some(end) = event_end(&self.pending, finish) {
            if end > Self::MAX_EVENT_BYTES || self.held.len() + end > Self::MAX_EVENT_BYTES {
                return Err("模型单个事件或待核验内容超过允许大小。");
            }
            let event = self.pending.drain(..end).collect::<Vec<_>>();
            let terminal = self.inspect_event(&event)?;
            self.held.extend_from_slice(&event);
            if terminal {
                self.prefixes.clear();
            }
            if self.prefixes.is_empty() {
                safe.append(&mut self.held);
            }
        }
        if self.pending.len() > Self::MAX_EVENT_BYTES {
            return Err("模型单个事件超过允许大小。");
        }
        if finish {
            if self.pending.iter().any(|byte| !byte.is_ascii_whitespace()) {
                return Err("模型流式事件未完整结束，未保存不完整结果。");
            }
            // An incomplete prefix is not a full credential. All decoded
            // concatenations have been checked before the final flush.
            self.held.append(&mut self.pending);
            safe.append(&mut self.held);
            self.prefixes.clear();
        }
        Ok(safe)
    }
}

fn stream_index(value: &Value) -> Result<u64, &'static str> {
    value
        .as_u64()
        .filter(|index| *index <= 9_007_199_254_740_991)
        .ok_or("模型增量缺少非负安全整数编号，已拒绝不明确的拼接方式。")
}

/// Tool arguments are JSON strings inside the SSE JSON string. Decode their
/// Unicode escapes once more, retaining incomplete escapes across deltas.
fn decode_unicode_escapes(value: &str, finish: bool) -> Result<(String, String), &'static str> {
    let mut result = String::new();
    let mut rest = value;
    while let Some(index) = rest.find('\\') {
        result.push_str(&rest[..index]);
        rest = &rest[index..];
        if rest.len() == 1 && !finish {
            return Ok((result, rest.to_owned()));
        }
        if !rest.starts_with("\\u") {
            result.push('\\');
            rest = &rest[1..];
            continue;
        }
        if rest.len() < 6 {
            if !finish {
                return Ok((result, rest.to_owned()));
            }
            result.push_str(rest);
            return Ok((result, String::new()));
        }
        if !rest.as_bytes()[2..6].iter().all(u8::is_ascii_hexdigit) {
            result.push('\\');
            rest = &rest[1..];
            continue;
        }
        let code = u16::from_str_radix(&rest[2..6], 16).map_err(|_| "模型字符转义无效。")?;
        let needed = if (0xd800..=0xdbff).contains(&code) {
            12
        } else {
            6
        };
        if rest.len() < needed {
            if !finish {
                return Ok((result, rest.to_owned()));
            }
            return Err("模型字符转义未完整结束。");
        }
        if !rest.is_char_boundary(needed) {
            return Err("模型字符转义无效。");
        }
        let decoded: String = serde_json::from_str(&format!("\"{}\"", &rest[..needed]))
            .map_err(|_| "模型字符转义无效。")?;
        result.push_str(&decoded);
        rest = &rest[needed..];
    }
    result.push_str(rest);
    Ok((result, String::new()))
}

/// Find a blank-line SSE boundary, accepting the protocol's CR, LF and CRLF
/// line endings without splitting a CRLF across transport chunks.
fn event_end(bytes: &[u8], finish: bool) -> Option<usize> {
    let mut line = 0;
    let mut cursor = 0;
    while cursor < bytes.len() {
        let length = match bytes[cursor] {
            b'\n' => 1,
            b'\r' if cursor + 1 == bytes.len() && !finish => return None,
            b'\r' if bytes.get(cursor + 1) == Some(&b'\n') => 2,
            b'\r' => 1,
            _ => {
                cursor += 1;
                continue;
            }
        };
        if cursor == line {
            return Some(cursor + length);
        }
        cursor += length;
        line = cursor;
    }
    None
}

impl Drop for SecretFilter {
    fn drop(&mut self) {
        use zeroize::Zeroize;
        self.pending.zeroize();
        self.held.zeroize();
        for prefix in self.prefixes.values_mut() {
            prefix.escape.zeroize();
        }
    }
}
