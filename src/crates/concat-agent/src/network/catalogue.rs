// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah
//! Native configuration requests: bounded discovery and explicit probe actions.
//! No project, media, agent owner or edit permit is accepted by this module.
use crate::configuration::catalogue::{
    CatalogueModel, CatalogueSource, DirectorySnapshot, VerificationCapability,
};
use crate::configuration::providers::{DirectoryProtocol, directory_protocol, preset};
use crate::configuration::{ConfigurationStore, ModelChannel, ModelProtocol, validate_model_id};
use crate::credentials::{CredentialScope, CredentialStore, Secret};
use reqwest::header::{HeaderMap, HeaderValue};
use reqwest::{Client, Method};
use serde_json::{Value, json};
use std::collections::BTreeSet;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

const MAX_BYTES: usize = 8 * 1024 * 1024;
const DIRECTORY_TIMEOUT: Duration = Duration::from_secs(15);
const DASHSCOPE_PAGE_SIZE: u64 = 100;
const PROBE_NAME: &str = "bkcut_connection_probe";

#[derive(Default)]
/// Cancellation signal shared by the native UI and configuration request.
pub struct ConfigurationCancellation(AtomicBool);
impl ConfigurationCancellation {
    /// Request prompt cancellation without waiting on the calling UI thread.
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }
    /// Inspect the cancellation signal without acquiring a UI or project lock.
    pub fn cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
/// Stable non-secret failure categories for directory requests and probes.
pub enum CatalogueError {
    /// A refresh already owns this connection version.
    Busy,
    /// Native cancellation was requested before publication.
    Cancelled,
    /// The operation exceeded its total deadline.
    Timeout,
    /// The native connection changed while the operation was in flight.
    Stale,
    /// The scoped credential was absent or unreadable.
    Credential,
    /// Local storage could not safely complete the refresh.
    Storage,
    /// Transport failed without exposing provider response text.
    Network,
    /// A service response status, without response text, URL or headers.
    /// Retain the exact number separately from the translated failure category.
    HttpStatus(u16),
    /// The service rejected authentication.
    Authentication,
    /// The service reported a billing failure.
    Billing,
    /// The service denied model or account access.
    Permission,
    /// The service rate limit was reached.
    RateLimit,
    /// A redirect was refused before forwarding credentials.
    Redirect,
    /// Local probe parameters cannot form a supported request; no request was sent.
    InvalidProbeParameters,
    /// Response shape or pagination failed validation.
    InvalidResponse,
    /// The complete response exceeded a native size bound.
    TooLarge,
    /// This product/protocol does not support the requested operation.
    Unsupported,
    /// The configured output limit was exhausted before verification completed.
    OutputExhausted,
    /// The fixed verification response did not meet the expected result.
    ProbeFailed,
}
impl CatalogueError {
    /// Language-independent code interpreted by the native translated interface.
    pub fn code(self) -> &'static str {
        match self {
            Self::Busy => "catalogue.busy",
            Self::Cancelled => "catalogue.cancelled",
            Self::Timeout => "catalogue.timeout",
            Self::Stale => "catalogue.stale",
            Self::Credential => "catalogue.credential",
            Self::Network => "catalogue.network",
            Self::HttpStatus(300..=399) => "catalogue.redirect",
            Self::HttpStatus(401) => "agent.errorAuthentication",
            Self::HttpStatus(402) => "agent.errorBilling",
            Self::HttpStatus(403) => "agent.errorAccess",
            Self::HttpStatus(429) => "catalogue.rateLimit",
            Self::HttpStatus(400..=499) => "catalogue.requestRejected",
            Self::HttpStatus(500..=599) => "catalogue.serverError",
            Self::HttpStatus(_) => "catalogue.unexpectedStatus",
            Self::Storage => "modelSettings.storageUnavailable",
            Self::Authentication => "agent.errorAuthentication",
            Self::Billing => "agent.errorBilling",
            Self::Permission => "agent.errorAccess",
            Self::RateLimit => "catalogue.rateLimit",
            Self::Redirect => "catalogue.redirect",
            Self::InvalidProbeParameters => "catalogue.invalidProbeParameters",
            Self::InvalidResponse => "catalogue.invalidResponse",
            Self::TooLarge => "catalogue.tooLarge",
            Self::Unsupported => "catalogue.unsupported",
            Self::ProbeFailed => "catalogue.probeFailed",
            Self::OutputExhausted => "catalogue.outputExhausted",
        }
    }

    /// Present only when the production request observed this HTTP response.
    /// Transport errors, cancellation and output validation never invent one.
    pub fn http_status(self) -> Option<u16> {
        match self {
            Self::HttpStatus(status) => Some(status),
            // Retain the earlier public variants for source compatibility.
            Self::Authentication => Some(401),
            Self::Billing => Some(402),
            Self::Permission => Some(403),
            Self::RateLimit => Some(429),
            _ => None,
        }
    }
}

/// Current wall clock for non-secret freshness and verification facts.
pub fn now_seconds() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |v| v.as_secs())
}

// A permanent OS lock spans the request and atomic publication. It also
// prevents a second app instance from publishing a newer directory while the
// first instance still holds a complete but not yet published result.
struct RefreshGuard {
    _file: File,
}
impl RefreshGuard {
    fn acquire(store: &ConfigurationStore, channel: &ModelChannel) -> Result<Self, CatalogueError> {
        let path = store
            .directory()
            .join(format!(".catalogue-refresh-{}.lock", channel.id.as_str()));
        fs::create_dir_all(store.directory()).map_err(|_| CatalogueError::Storage)?;
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (!metadata.is_file() || metadata.file_type().is_symlink())
        {
            return Err(CatalogueError::Storage);
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0x0000_0001 | 0x0000_0002);
        }
        let file = options.open(path).map_err(|_| CatalogueError::Storage)?;
        match crate::try_lock_owner_file(&file) {
            Ok(()) => Ok(Self { _file: file }),
            Err(TryLockError::WouldBlock) => Err(CatalogueError::Busy),
            Err(TryLockError::Error(_)) => Err(CatalogueError::Storage),
        }
    }
}

/// Fetch all pages under one 15-second deadline and atomically publish while
/// holding this connection's refresh ownership, including failed outcomes.
/// Duplicate requests do not replace the active request's refresh status.
pub fn refresh_directory(
    store: &ConfigurationStore,
    channel: &ModelChannel,
    credentials: &dyn CredentialStore,
    cancel: &Arc<ConfigurationCancellation>,
) -> Result<DirectorySnapshot, CatalogueError> {
    refresh_directory_with(store, channel, credentials, cancel, || {})
}

fn refresh_directory_with(
    store: &ConfigurationStore,
    channel: &ModelChannel,
    credentials: &dyn CredentialStore,
    cancel: &Arc<ConfigurationCancellation>,
    before_publish: impl FnOnce(),
) -> Result<DirectorySnapshot, CatalogueError> {
    let _guard = RefreshGuard::acquire(store, channel)?;
    let result = fetch_directory(store, channel, credentials, cancel);
    before_publish();
    let result = result.and_then(|directory| {
        check(store, channel, cancel)?;
        Ok(directory)
    });
    store
        .publish_catalogue(
            channel,
            now_seconds(),
            result.clone().map_err(|error| error.code()),
        )
        .map_err(|error| {
            if error == crate::configuration::ConfigurationError::StaleConfiguration {
                CatalogueError::Stale
            } else {
                CatalogueError::Storage
            }
        })?;
    result
}

// Fetching is private: native UI callers cannot accidentally release ownership
// before publication or publish a late result after a newer refresh.
fn fetch_directory(
    store: &ConfigurationStore,
    channel: &ModelChannel,
    credentials: &dyn CredentialStore,
    cancel: &Arc<ConfigurationCancellation>,
) -> Result<DirectorySnapshot, CatalogueError> {
    check(store, channel, cancel)?;
    let protocol = directory_protocol(&channel.provider_id, channel.protocol);
    if protocol == DirectoryProtocol::Recommended {
        let provider = preset(&channel.provider_id).ok_or(CatalogueError::Unsupported)?;
        return Ok(DirectorySnapshot {
            source: CatalogueSource::PublicRecommended,
            source_url: provider.documentation.into(),
            fetched_at: now_seconds(),
            models: provider
                .recommendations
                .iter()
                .map(|id| CatalogueModel::manual(id))
                .collect(),
        });
    }
    let secret = read_secret(channel, credentials)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| CatalogueError::Network)?;
    runtime.block_on(async {
        bounded(cancel, DIRECTORY_TIMEOUT, async {
            let client = client()?;
            let mut url = directory_url(channel, protocol)?;
            let source_url = url.to_string();
            let mut visited = BTreeSet::new();
            let mut ids = BTreeSet::new();
            let mut collected = Vec::new();
            let mut received = 0;
            let mut numbered_total = None;
            loop {
                check(store, channel, cancel)?;
                if !visited.insert(url.to_string()) || visited.len() > 200 {
                    return Err(CatalogueError::InvalidResponse);
                }
                let value = request(
                    &client,
                    Method::GET,
                    url.clone(),
                    None,
                    channel,
                    &secret,
                    &mut received,
                )
                .await?;
                check(store, channel, cancel)?;
                if protocol == DirectoryProtocol::DashScope {
                    let page = dashscope_page(&value)?;
                    if page.number != visited.len() as u64
                        || page.size != DASHSCOPE_PAGE_SIZE
                        || numbered_total.is_some_and(|total| total != page.total)
                    {
                        return Err(CatalogueError::InvalidResponse);
                    }
                    numbered_total = Some(page.total);
                }
                let (models, next) = parse_page(protocol, &value, &source_url)?;
                for model in models {
                    if !ids.insert(model.id.clone()) {
                        return Err(CatalogueError::InvalidResponse);
                    }
                    collected.push(model);
                    if collected.len() > 20000 {
                        return Err(CatalogueError::TooLarge);
                    }
                }
                match next {
                    None => break,
                    Some(cursor) => {
                        if cursor.is_empty()
                            || cursor.len() > 2048
                            || cursor.chars().any(char::is_control)
                        {
                            return Err(CatalogueError::InvalidResponse);
                        }
                        url = directory_url(channel, protocol)?;
                        if protocol == DirectoryProtocol::DashScope {
                            url.query_pairs_mut()
                                .clear()
                                .append_pair("page_no", &cursor)
                                .append_pair("page_size", &DASHSCOPE_PAGE_SIZE.to_string());
                        } else {
                            url.query_pairs_mut().append_pair(
                                match protocol {
                                    DirectoryProtocol::Google => "pageToken",
                                    DirectoryProtocol::Anthropic => "after_id",
                                    _ => "after",
                                },
                                &cursor,
                            );
                        }
                    }
                }
            }
            Ok(DirectorySnapshot {
                // A complete response from the configured service. The legacy
                // variant is not an endorsement of custom/third-party origins.
                source: CatalogueSource::Official,
                source_url,
                fetched_at: now_seconds(),
                models: collected,
            })
        })
        .await
    })
}

fn check(
    store: &ConfigurationStore,
    channel: &ModelChannel,
    cancel: &ConfigurationCancellation,
) -> Result<(), CatalogueError> {
    if cancel.cancelled() {
        return Err(CatalogueError::Cancelled);
    }
    store
        .assert_connection_current(channel)
        .map_err(|_| CatalogueError::Stale)
}

async fn bounded<T>(
    cancel: &ConfigurationCancellation,
    timeout: Duration,
    work: impl std::future::Future<Output = Result<T, CatalogueError>>,
) -> Result<T, CatalogueError> {
    tokio::select! {
        biased;
        _ = async { while !cancel.cancelled() { tokio::time::sleep(Duration::from_millis(25)).await; } } => Err(CatalogueError::Cancelled),
        result = tokio::time::timeout(timeout, work) => result.unwrap_or(Err(CatalogueError::Timeout)),
    }
}

fn read_secret(
    channel: &ModelChannel,
    credentials: &dyn CredentialStore,
) -> Result<Secret, CatalogueError> {
    let origin = channel
        .service_origin()
        .map_err(|_| CatalogueError::Credential)?;
    let scope = CredentialScope::new(channel.id.as_str(), &origin)
        .map_err(|_| CatalogueError::Credential)?;
    credentials
        .read(&scope)
        .map_err(|_| CatalogueError::Credential)?
        .ok_or(CatalogueError::Credential)
}

fn client() -> Result<Client, CatalogueError> {
    Client::builder()
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .no_proxy()
        .pool_max_idle_per_host(0)
        .connect_timeout(DIRECTORY_TIMEOUT)
        .build()
        .map_err(|_| CatalogueError::Network)
}

fn auth_headers(channel: &ModelChannel, secret: &Secret) -> Result<HeaderMap, CatalogueError> {
    let mut headers = HeaderMap::new();
    let (name, value) = match channel.protocol {
        ModelProtocol::AnthropicMessages => (
            "x-api-key",
            zeroize::Zeroizing::new(secret.expose_secret().to_owned()),
        ),
        ModelProtocol::GoogleGenerativeAi => (
            "x-goog-api-key",
            zeroize::Zeroizing::new(secret.expose_secret().to_owned()),
        ),
        _ => (
            "authorization",
            zeroize::Zeroizing::new(format!("Bearer {}", secret.expose_secret())),
        ),
    };
    let mut value = HeaderValue::from_str(&value).map_err(|_| CatalogueError::Credential)?;
    value.set_sensitive(true);
    headers.insert(name, value);
    headers.insert("accept", HeaderValue::from_static("application/json"));
    if channel.protocol == ModelProtocol::AnthropicMessages {
        headers.insert("anthropic-version", HeaderValue::from_static("2023-06-01"));
    }
    Ok(headers)
}

async fn request(
    client: &Client,
    method: Method,
    url: url::Url,
    body: Option<Value>,
    channel: &ModelChannel,
    secret: &Secret,
    received: &mut usize,
) -> Result<Value, CatalogueError> {
    let mut request = client
        .request(method, url)
        .headers(auth_headers(channel, secret)?);
    if let Some(body) = body {
        request = request
            .header("content-type", "application/json")
            .body(body.to_string());
    }
    let mut response = request.send().await.map_err(|_| CatalogueError::Network)?;
    let status = response.status().as_u16();
    if !(200..=299).contains(&status) {
        // Never read an error body: it can contain echoed keys or provider
        // internals. A bounded numeric status is sufficient for classification.
        return Err(CatalogueError::HttpStatus(status));
    }
    let mut bytes = Vec::new();
    while let Some(chunk) = response
        .chunk()
        .await
        .map_err(|_| CatalogueError::Network)?
    {
        *received = received
            .checked_add(chunk.len())
            .ok_or(CatalogueError::TooLarge)?;
        if *received > MAX_BYTES {
            return Err(CatalogueError::TooLarge);
        }
        bytes.extend_from_slice(&chunk);
    }
    // A malicious echo must not persist a secret as a model name or metadata.
    if String::from_utf8_lossy(&bytes).contains(secret.expose_secret()) {
        return Err(CatalogueError::InvalidResponse);
    }
    let value: Value =
        serde_json::from_slice(&bytes).map_err(|_| CatalogueError::InvalidResponse)?;
    // JSON escapes can hide the credential from the raw-byte check. Inspect
    // every decoded string and key before selecting or persisting any fields.
    if contains_credential(&value, secret.expose_secret()) {
        return Err(CatalogueError::InvalidResponse);
    }
    Ok(value)
}

fn contains_credential(value: &Value, secret: &str) -> bool {
    match value {
        Value::String(text) => text.contains(secret),
        Value::Array(items) => items.iter().any(|item| contains_credential(item, secret)),
        Value::Object(entries) => entries
            .iter()
            .any(|(key, value)| key.contains(secret) || contains_credential(value, secret)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

fn directory_url(
    channel: &ModelChannel,
    protocol: DirectoryProtocol,
) -> Result<url::Url, CatalogueError> {
    if protocol == DirectoryProtocol::DashScope {
        // Keep the exact configured regional origin and credential scope. Only
        // the documented compatible/native path changes; no cross-host request.
        let base = channel
            .endpoint
            .trim_end_matches('/')
            .strip_suffix("/compatible-mode/v1")
            .ok_or(CatalogueError::Unsupported)?;
        let mut url = url::Url::parse(&format!("{base}/api/v1/models"))
            .map_err(|_| CatalogueError::InvalidResponse)?;
        url.query_pairs_mut()
            .append_pair("page_no", "1")
            .append_pair("page_size", &DASHSCOPE_PAGE_SIZE.to_string());
        return Ok(url);
    }
    let suffix = if protocol == DirectoryProtocol::Anthropic {
        "/v1/models"
    } else {
        "/models"
    };
    let mut url = url::Url::parse(&format!(
        "{}{suffix}",
        channel.endpoint.trim_end_matches('/')
    ))
    .map_err(|_| CatalogueError::InvalidResponse)?;
    match protocol {
        DirectoryProtocol::Anthropic => {
            url.query_pairs_mut().append_pair("limit", "1000");
        }
        DirectoryProtocol::Google => {
            url.query_pairs_mut().append_pair("pageSize", "1000");
        }
        _ => (),
    }
    Ok(url)
}

// DashScope's total is checked against every full page and retained across the
// request. A successful prefix can never be published as the complete directory.
struct NumberedPage {
    number: u64,
    size: u64,
    total: u64,
    next: Option<String>,
}

fn dashscope_page(value: &Value) -> Result<NumberedPage, CatalogueError> {
    if value["success"] != true || !value["code"].is_null() {
        return Err(CatalogueError::InvalidResponse);
    }
    let output = &value["output"];
    let entries = output["models"]
        .as_array()
        .ok_or(CatalogueError::InvalidResponse)?;
    let total = output["total"]
        .as_u64()
        .ok_or(CatalogueError::InvalidResponse)?;
    let number = output["page_no"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or(CatalogueError::InvalidResponse)?;
    let size = output["page_size"]
        .as_u64()
        .filter(|n| *n > 0)
        .ok_or(CatalogueError::InvalidResponse)?;
    if total > 20000 {
        return Err(CatalogueError::TooLarge);
    }
    let offset = (number - 1)
        .checked_mul(size)
        .ok_or(CatalogueError::InvalidResponse)?;
    if (total == 0 && (number != 1 || !entries.is_empty()))
        || (total > 0 && offset >= total)
        || entries.len() as u64 != size.min(total.saturating_sub(offset))
    {
        return Err(CatalogueError::InvalidResponse);
    }
    let end = offset
        .checked_add(entries.len() as u64)
        .ok_or(CatalogueError::InvalidResponse)?;
    Ok(NumberedPage {
        number,
        size,
        total,
        next: (end < total).then(|| (number + 1).to_string()),
    })
}

fn optional_limit(value: Option<&Value>) -> Result<Option<u32>, CatalogueError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => {
            let number = value
                .as_u64()
                .and_then(|v| u32::try_from(v).ok())
                .ok_or(CatalogueError::InvalidResponse)?;
            Ok((number > 0).then_some(number))
        }
    }
}

fn ark_limit(
    entry: &Value,
    field: &str,
    nested_field: &str,
) -> Result<Option<u32>, CatalogueError> {
    let direct = optional_limit(entry.get(field))?;
    let nested = optional_limit(entry.pointer(&format!("/token_limits/{nested_field}")))?;
    if direct.zip(nested).is_some_and(|(a, b)| a != b) {
        return Err(CatalogueError::InvalidResponse);
    }
    Ok(direct.or(nested))
}

fn advertised_flag(value: Option<&Value>) -> Result<Option<bool>, CatalogueError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Bool(value)) => Ok(Some(*value)),
        _ => Err(CatalogueError::InvalidResponse),
    }
}

fn advertised_list(value: Option<&Value>, member: &str) -> Result<Option<bool>, CatalogueError> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(items)) if items.iter().all(Value::is_string) => {
            Ok(Some(items.iter().any(|item| item == member)))
        }
        _ => Err(CatalogueError::InvalidResponse),
    }
}

/// Decode one reviewed response and retain its exact provenance and cursor.
pub fn parse_page(
    protocol: DirectoryProtocol,
    value: &Value,
    source: &str,
) -> Result<(Vec<CatalogueModel>, Option<String>), CatalogueError> {
    let numbered = if protocol == DirectoryProtocol::DashScope {
        Some(dashscope_page(value)?)
    } else {
        None
    };
    if matches!(
        protocol,
        DirectoryProtocol::Ark | DirectoryProtocol::OpenAiComplete
    ) && value["object"] != "list"
    {
        return Err(CatalogueError::InvalidResponse);
    }
    // Ark and MiniMax document complete responses, not an OpenAI cursor protocol.
    // Never guess a continuation or publish a prefix if the service changes it.
    if matches!(
        protocol,
        DirectoryProtocol::Ark | DirectoryProtocol::DashScope | DirectoryProtocol::OpenAiComplete
    ) && value.get("has_more").is_some_and(|v| v != false)
    {
        return Err(CatalogueError::Unsupported);
    }
    let entries = match protocol {
        DirectoryProtocol::Google => &value["models"],
        DirectoryProtocol::DashScope => &value["output"]["models"],
        _ => &value["data"],
    }
    .as_array()
    .ok_or(CatalogueError::InvalidResponse)?;
    let mut models = Vec::new();
    for entry in entries {
        if protocol == DirectoryProtocol::Google
            && !entry["supportedGenerationMethods"]
                .as_array()
                .is_some_and(|methods| methods.iter().any(|m| m == "generateContent"))
        {
            continue;
        }
        let raw_id = entry[match protocol {
            DirectoryProtocol::Google => "name",
            DirectoryProtocol::DashScope => "model",
            _ => "id",
        }]
        .as_str()
        .ok_or(CatalogueError::InvalidResponse)?;
        let id = if protocol == DirectoryProtocol::Google {
            raw_id
                .strip_prefix("models/")
                .ok_or(CatalogueError::InvalidResponse)?
        } else {
            raw_id
        };
        validate_model_id(id).map_err(|_| CatalogueError::InvalidResponse)?;
        let number = |names: &[&str]| {
            names
                .iter()
                .find_map(|name| entry.get(*name).and_then(Value::as_u64))
                .and_then(|v| u32::try_from(v).ok())
                .filter(|v| *v > 0)
        };
        let (context_window, max_output_tokens) = match protocol {
            DirectoryProtocol::Ark => (
                ark_limit(entry, "context_window", "context_window")?,
                ark_limit(entry, "max_output_tokens", "max_output_token_length")?,
            ),
            DirectoryProtocol::DashScope => (
                optional_limit(entry.pointer("/model_info/context_window"))?,
                optional_limit(entry.pointer("/model_info/max_output_tokens"))?,
            ),
            _ => (
                number(&[
                    "context_window",
                    "context_length",
                    "max_input_tokens",
                    "inputTokenLimit",
                ]),
                number(&["max_output_tokens", "max_tokens", "outputTokenLimit"]),
            ),
        };
        let name = entry["display_name"]
            .as_str()
            .or(entry["displayName"].as_str())
            .or(entry["name"].as_str())
            .unwrap_or(id);
        if name.len() > 1024 || name.chars().any(char::is_control) {
            return Err(CatalogueError::InvalidResponse);
        }
        let (tools, image_input) = match protocol {
            DirectoryProtocol::Ark => (
                advertised_flag(entry.pointer("/features/tools/function_calling"))?,
                advertised_list(entry.pointer("/modalities/input_modalities"), "image")?,
            ),
            DirectoryProtocol::DashScope => (
                advertised_list(entry.get("features"), "function-calling")?,
                advertised_list(
                    entry.pointer("/inference_metadata/request_modality"),
                    "Image",
                )?,
            ),
            _ => {
                let tools = entry["supported_parameters"]
                    .as_array()
                    .map(|params| params.iter().any(|p| p == "tools"))
                    .or_else(|| {
                        entry
                            .pointer("/capabilities/tool_use/supported")
                            .and_then(Value::as_bool)
                    });
                let modalities = entry
                    .get("input_modalities")
                    .or_else(|| entry.pointer("/architecture/input_modalities"));
                (
                    tools,
                    modalities
                        .and_then(Value::as_array)
                        .map(|items| items.iter().any(|p| p == "image")),
                )
            }
        };
        models.push(CatalogueModel {
            id: id.into(),
            name: name.into(),
            context_window,
            max_output_tokens,
            tools,
            image_input,
            // Preserve this existing traceability prefix in cache/task snapshots.
            // It identifies service metadata; it does not authenticate a vendor.
            context_source: context_window.map(|_| format!("official-directory:{source}")),
        });
    }
    let cursor_fields = ["next", "next_cursor", "next_page", "nextPage"];
    if cursor_fields.iter().any(|key| {
        value
            .get(*key)
            .is_some_and(|next| !next.is_null() && next != "" && next != false)
    }) || (matches!(
        protocol,
        DirectoryProtocol::Ark | DirectoryProtocol::DashScope | DirectoryProtocol::OpenAiComplete
    ) && value
        .get("nextPageToken")
        .is_some_and(|next| !next.is_null() && next != ""))
    {
        return Err(CatalogueError::Unsupported);
    }
    let next = if let Some(page) = numbered {
        page.next
    } else if protocol == DirectoryProtocol::Google {
        match value.get("nextPageToken") {
            Some(Value::String(token)) if !token.is_empty() => Some(token.clone()),
            None | Some(Value::Null) => None,
            Some(Value::String(_)) => None,
            _ => return Err(CatalogueError::InvalidResponse),
        }
    } else if matches!(
        protocol,
        DirectoryProtocol::Ark | DirectoryProtocol::OpenAiComplete
    ) {
        None
    } else {
        match value.get("has_more") {
            Some(Value::Bool(true)) => Some(
                value["last_id"]
                    .as_str()
                    .or_else(|| entries.last().and_then(|v| v["id"].as_str()))
                    .ok_or(CatalogueError::InvalidResponse)?
                    .to_owned(),
            ),
            Some(Value::Bool(false)) | None => None,
            _ => return Err(CatalogueError::InvalidResponse),
        }
    };
    if next.is_some() && entries.is_empty() {
        return Err(CatalogueError::InvalidResponse);
    }
    Ok((models, next))
}

/// This action is called only by an explicit native validation button. Its
/// fixed test content has no project identity and cannot execute an edit tool.
pub fn verify_model(
    store: &ConfigurationStore,
    channel: &ModelChannel,
    capability: VerificationCapability,
    credentials: &dyn CredentialStore,
    cancel: &Arc<ConfigurationCancellation>,
) -> Result<(), CatalogueError> {
    validate_model_id(&channel.model).map_err(|_| CatalogueError::InvalidProbeParameters)?;
    check(store, channel, cancel)?;
    let (url, body) = probe_request(channel, capability)?;
    let secret = read_secret(channel, credentials)?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|_| CatalogueError::Network)?;
    runtime.block_on(async {
        bounded(
            cancel,
            Duration::from_millis(u64::from(channel.request_timeout_ms)),
            async {
                let value = request(
                    &client()?,
                    Method::POST,
                    url,
                    Some(body),
                    channel,
                    &secret,
                    &mut 0,
                )
                .await?;
                check(store, channel, cancel)?;
                if probe_output_exhausted(channel.protocol, &value) {
                    return Err(CatalogueError::OutputExhausted);
                }
                if probe_response(channel.protocol, capability, &value) {
                    Ok(())
                } else {
                    Err(CatalogueError::ProbeFailed)
                }
            },
        )
        .await
    })
}

fn probe_request(
    channel: &ModelChannel,
    capability: VerificationCapability,
) -> Result<(url::Url, Value), CatalogueError> {
    if channel.protocol == ModelProtocol::OpenAiResponses && channel.max_output_tokens < 16 {
        return Err(CatalogueError::InvalidProbeParameters);
    }
    let tools = capability == VerificationCapability::Tools;
    let prompt = if tools {
        "Call bkcut_connection_probe once with value exactly ok. Do not call any other tool."
    } else {
        "Reply with OK."
    };
    let schema = json!({"type":"object","properties":{"value":{"type":"string","enum":["ok"]}},"required":["value"],"additionalProperties":false});
    // The explicit saved per-request limit includes reasoning tokens. A hidden
    // 64-token clamp makes reasoning-only/truncated results impossible to fix.
    let limit = channel.max_output_tokens;
    let (suffix, mut body) = match channel.protocol {
        ModelProtocol::OpenAiCompletions => (
            "/chat/completions".into(),
            json!({"model":channel.model,"messages":[{"role":"user","content":prompt}],"stream":false,"max_tokens":limit}),
        ),
        ModelProtocol::OpenAiResponses => (
            "/responses".into(),
            json!({"model":channel.model,"input":prompt,"max_output_tokens":limit.max(16),"store":false}),
        ),
        ModelProtocol::AnthropicMessages => (
            "/v1/messages".into(),
            json!({"model":channel.model,"messages":[{"role":"user","content":prompt}],"max_tokens":limit}),
        ),
        ModelProtocol::GoogleGenerativeAi => {
            if !channel
                .model
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, b'-' | b'_' | b'.'))
            {
                return Err(CatalogueError::InvalidProbeParameters);
            }
            (
                format!("/models/{}:generateContent", channel.model),
                json!({"contents":[{"role":"user","parts":[{"text":prompt}]}],"generationConfig":{"maxOutputTokens":limit}}),
            )
        }
    };
    if tools {
        match channel.protocol {
            ModelProtocol::OpenAiCompletions => {
                // DeepSeek enables thinking by default and rejects named tool
                // choice in that mode. This active probe needs one exact harmless
                // function, so use its documented non-thinking mode only at the
                // exact official service bases. Do not infer from model names,
                // rewrite legacy provider identity, or affect ordinary tasks.
                if matches!(
                    channel.endpoint.trim_end_matches('/'),
                    "https://api.deepseek.com" | "https://api.deepseek.com/v1"
                ) {
                    body["thinking"] = json!({"type":"disabled"});
                }
                body["tools"] = json!([{"type":"function","function":{"name":PROBE_NAME,"description":"Validate structured arguments; no side effects.","parameters":schema}}]);
                body["tool_choice"] = json!({"type":"function","function":{"name":PROBE_NAME}});
            }
            ModelProtocol::OpenAiResponses => {
                body["tools"] = json!([{"type":"function","name":PROBE_NAME,"description":"Validate structured arguments; no side effects.","parameters":schema,"strict":false}]);
                body["tool_choice"] = json!({"type":"function","name":PROBE_NAME});
            }
            ModelProtocol::AnthropicMessages => {
                body["tools"] = json!([{"name":PROBE_NAME,"description":"Validate structured arguments; no side effects.","input_schema":schema}]);
                body["tool_choice"] = json!({"type":"tool","name":PROBE_NAME});
            }
            ModelProtocol::GoogleGenerativeAi => {
                body["tools"] = json!([{"functionDeclarations":[{"name":PROBE_NAME,"description":"Validate structured arguments; no side effects.","parametersJsonSchema":schema}]}]);
                body["toolConfig"] = json!({"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":[PROBE_NAME]}});
            }
        }
    }
    let url = url::Url::parse(&format!(
        "{}{suffix}",
        channel.endpoint.trim_end_matches('/')
    ))
    .map_err(|_| CatalogueError::InvalidProbeParameters)?;
    Ok((url, body))
}

fn probe_output_exhausted(protocol: ModelProtocol, value: &Value) -> bool {
    match protocol {
        ModelProtocol::OpenAiCompletions => value["choices"][0]["finish_reason"] == "length",
        ModelProtocol::OpenAiResponses => {
            value["status"] == "incomplete"
                && value["incomplete_details"]["reason"] == "max_output_tokens"
        }
        ModelProtocol::AnthropicMessages => value["stop_reason"] == "max_tokens",
        ModelProtocol::GoogleGenerativeAi => value["candidates"][0]["finishReason"] == "MAX_TOKENS",
    }
}

fn probe_response(
    protocol: ModelProtocol,
    capability: VerificationCapability,
    value: &Value,
) -> bool {
    let valid = |name: &Value, args: &Value| name == PROBE_NAME && args == &json!({"value":"ok"});
    if capability == VerificationCapability::Tools {
        return match protocol {
            ModelProtocol::OpenAiCompletions => value
                .pointer("/choices/0/message/tool_calls")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items.len() == 1
                        && valid(
                            &items[0]["function"]["name"],
                            &serde_json::from_str::<Value>(
                                items[0]["function"]["arguments"].as_str().unwrap_or(""),
                            )
                            .unwrap_or(Value::Null),
                        )
                }),
            ModelProtocol::OpenAiResponses => value["output"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .filter(|item| item["type"] == "function_call")
                    .count()
                    == 1
                    && items.iter().any(|item| {
                        item["type"] == "function_call"
                            && valid(
                                &item["name"],
                                &serde_json::from_str::<Value>(
                                    item["arguments"].as_str().unwrap_or(""),
                                )
                                .unwrap_or(Value::Null),
                            )
                    })
            }),
            ModelProtocol::AnthropicMessages => value["content"].as_array().is_some_and(|items| {
                items
                    .iter()
                    .filter(|item| item["type"] == "tool_use")
                    .count()
                    == 1
                    && items.iter().any(|item| {
                        item["type"] == "tool_use" && valid(&item["name"], &item["input"])
                    })
            }),
            ModelProtocol::GoogleGenerativeAi => value
                .pointer("/candidates/0/content/parts")
                .and_then(Value::as_array)
                .is_some_and(|items| {
                    items
                        .iter()
                        .filter(|item| item.get("functionCall").is_some())
                        .count()
                        == 1
                        && items.iter().any(|item| {
                            valid(&item["functionCall"]["name"], &item["functionCall"]["args"])
                        })
                }),
        };
    }
    let nonempty = |v: &Value| v.as_str().is_some_and(|s| !s.trim().is_empty());
    match protocol {
        ModelProtocol::OpenAiCompletions => nonempty(&value["choices"][0]["message"]["content"]),
        ModelProtocol::OpenAiResponses => value["output"].as_array().is_some_and(|items| {
            items.iter().any(|item| {
                item["content"]
                    .as_array()
                    .is_some_and(|parts| parts.iter().any(|part| nonempty(&part["text"])))
            })
        }),
        ModelProtocol::AnthropicMessages => value["content"].as_array().is_some_and(|parts| {
            parts
                .iter()
                .any(|part| part["type"] == "text" && nonempty(&part["text"]))
        }),
        ModelProtocol::GoogleGenerativeAi => value
            .pointer("/candidates/0/content/parts")
            .and_then(Value::as_array)
            .is_some_and(|parts| parts.iter().any(|part| nonempty(&part["text"]))),
    }
}

#[cfg(test)]
mod refresh_publication_tests {
    use super::*;
    use crate::configuration::ChannelDraft;
    use crate::credentials::CredentialError;
    use std::sync::mpsc;

    struct UnusedCredential;
    impl CredentialStore for UnusedCredential {
        fn read(&self, _: &CredentialScope) -> Result<Option<Secret>, CredentialError> {
            panic!("public recommendations must not read a secret")
        }
        fn write(&self, _: &CredentialScope, _: &Secret) -> Result<(), CredentialError> {
            panic!("discovery must not write a secret")
        }
        fn delete(&self, _: &CredentialScope) -> Result<(), CredentialError> {
            panic!("discovery must not remove a secret")
        }
    }
    fn fixture() -> (tempfile::TempDir, ConfigurationStore, ModelChannel) {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigurationStore::new(directory.path());
        let p = preset("zhipu").unwrap();
        let snapshot = store
            .save_connection(
                &store.load().unwrap(),
                None,
                ChannelDraft {
                    label: "Isolated refresh ownership".into(),
                    protocol: p.protocol,
                    endpoint: p.endpoint.into(),
                    model: String::new(),
                    request_timeout_ms: 30000,
                    max_output_tokens: 1024,
                },
                p.id,
                p.region,
            )
            .unwrap();
        (directory, store, snapshot.channels()[0].clone())
    }

    #[test]
    fn native_provider_directory_routes_keep_the_exact_configured_origin_and_scope() {
        let (_directory, _store, mut channel) = fixture();
        for (provider, expected) in [
            (
                "qwen",
                "https://dashscope.aliyuncs.com/api/v1/models?page_no=1&page_size=100",
            ),
            ("doubao", "https://ark.cn-beijing.volces.com/api/v3/models"),
            ("minimax", "https://api.minimax.cn/v1/models"),
        ] {
            let preset = preset(provider).unwrap();
            channel.provider_id = provider.into();
            channel.endpoint = preset.endpoint.into();
            channel.protocol = preset.protocol;
            let before = channel.clone();
            let url = directory_url(&channel, preset.directory).unwrap();
            assert_eq!(url.as_str(), expected);
            assert_eq!(
                url.origin(),
                url::Url::parse(&channel.endpoint).unwrap().origin()
            );
            assert_eq!(
                channel, before,
                "directory routing cannot migrate credentials or inference"
            );
            assert!(preset.recommendations.is_empty());
        }
        channel.endpoint = "https://proxy.example.test/custom".into();
        assert_eq!(
            directory_url(&channel, DirectoryProtocol::DashScope),
            Err(CatalogueError::Unsupported)
        );
    }

    #[test]
    fn completed_fetch_keeps_exclusive_ownership_until_atomic_publication() {
        let (_directory, store, channel) = fixture();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let worker_store = store.clone();
        let worker_channel = channel.clone();
        let worker = std::thread::spawn(move || {
            refresh_directory_with(
                &worker_store,
                &worker_channel,
                &UnusedCredential,
                &Arc::default(),
                || {
                    ready_tx.send(()).unwrap();
                    release_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                },
            )
        });
        ready_rx.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(store.catalogue(&channel).unwrap().directory.is_none());
        let before = store.catalogue(&channel).unwrap();
        assert_eq!(
            refresh_directory(&store, &channel, &UnusedCredential, &Arc::default()).unwrap_err(),
            CatalogueError::Busy
        );
        assert_eq!(
            store.catalogue(&channel).unwrap(),
            before,
            "a duplicate must not publish even an error outcome"
        );
        release_tx.send(()).unwrap();
        let complete = worker.join().unwrap().unwrap();
        assert_eq!(store.catalogue(&channel).unwrap().directory, Some(complete));
        refresh_directory(&store, &channel, &UnusedCredential, &Arc::default()).unwrap();
    }

    #[test]
    fn cancellation_after_fetch_retains_the_previous_complete_directory() {
        let (_directory, store, channel) = fixture();
        let prior =
            refresh_directory(&store, &channel, &UnusedCredential, &Arc::default()).unwrap();
        let cancel = Arc::new(ConfigurationCancellation::default());
        let result = refresh_directory_with(&store, &channel, &UnusedCredential, &cancel, || {
            cancel.cancel()
        });
        assert_eq!(result.unwrap_err(), CatalogueError::Cancelled);
        let facts = store.catalogue(&channel).unwrap();
        assert_eq!(facts.directory, Some(prior));
        assert_eq!(
            facts.refresh.unwrap().error_code.as_deref(),
            Some("catalogue.cancelled")
        );
    }

    #[test]
    fn explicit_probe_uses_saved_limit_and_distinguishes_reasoning_exhaustion() {
        let (_directory, _store, mut channel) = fixture();
        channel.model = "fixture-model".into();
        channel.max_output_tokens = 8192;
        for protocol in [
            ModelProtocol::OpenAiCompletions,
            ModelProtocol::OpenAiResponses,
            ModelProtocol::AnthropicMessages,
            ModelProtocol::GoogleGenerativeAi,
        ] {
            channel.protocol = protocol;
            let (_, body) = probe_request(&channel, VerificationCapability::Inference).unwrap();
            let limit = match protocol {
                ModelProtocol::OpenAiResponses => &body["max_output_tokens"],
                ModelProtocol::GoogleGenerativeAi => &body["generationConfig"]["maxOutputTokens"],
                _ => &body["max_tokens"],
            };
            assert_eq!(limit, 8192);
        }
        for (protocol, response) in [
            (
                ModelProtocol::OpenAiCompletions,
                json!({"choices":[{"finish_reason":"length","message":{"content":null,"reasoning_content":"thinking"}}]}),
            ),
            (
                ModelProtocol::OpenAiResponses,
                json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[{"type":"reasoning"}]}),
            ),
            (
                ModelProtocol::AnthropicMessages,
                json!({"stop_reason":"max_tokens","content":[{"type":"thinking","thinking":"thinking"}]}),
            ),
            (
                ModelProtocol::GoogleGenerativeAi,
                json!({"candidates":[{"finishReason":"MAX_TOKENS","content":{"parts":[{"thought":true,"text":"thinking"}]}}]}),
            ),
        ] {
            assert!(probe_output_exhausted(protocol, &response));
        }
        let truncated = json!({"choices":[{"finish_reason":"length","message":{"tool_calls":[{"function":{"name":PROBE_NAME,"arguments":"{\"value\":"}}]}}]});
        assert!(probe_output_exhausted(
            ModelProtocol::OpenAiCompletions,
            &truncated
        ));
        assert!(!probe_response(
            ModelProtocol::OpenAiCompletions,
            VerificationCapability::Tools,
            &truncated
        ));
        let normal = json!({"choices":[{"finish_reason":"stop","message":{"content":"OK"}}]});
        assert!(!probe_output_exhausted(
            ModelProtocol::OpenAiCompletions,
            &normal
        ));
        assert!(probe_response(
            ModelProtocol::OpenAiCompletions,
            VerificationCapability::Inference,
            &normal
        ));
    }
    #[test]
    fn deepseek_named_tool_probe_uses_exact_official_base_without_guessing_model_or_provider() {
        let (_directory, _store, mut channel) = fixture();
        channel.provider_id = "custom".into();
        channel.protocol = ModelProtocol::OpenAiCompletions;
        for address in [
            "https://api.deepseek.com",
            "https://api.deepseek.com/",
            "https://api.deepseek.com/v1",
            "https://api.deepseek.com/v1/",
        ] {
            channel.endpoint = address.into();
            for model in ["deepseek-flash", "explicit-future-model"] {
                channel.model = model.into();
                let before = channel.clone();
                let (url, tools) = probe_request(&channel, VerificationCapability::Tools).unwrap();
                assert_eq!(tools["thinking"], json!({"type":"disabled"}));
                assert_eq!(tools["tool_choice"]["function"]["name"], PROBE_NAME);
                assert_eq!(url.host_str(), Some("api.deepseek.com"));
                assert_eq!(
                    channel, before,
                    "compatibility never migrates provider or key scope"
                );
                let (_, inference) =
                    probe_request(&channel, VerificationCapability::Inference).unwrap();
                assert!(inference.get("thinking").is_none());
            }
        }
        channel.model = "deepseek-flash".into();
        for address in [
            "https://proxy.example.test",
            "https://api.deepseek.com.evil.example",
            "https://api.deepseek.com/custom",
            "https://api.deepseek.com:8443",
            "http://127.0.0.1",
            "https://api.openai.com/v1",
        ] {
            channel.endpoint = address.into();
            let (_, tools) = probe_request(&channel, VerificationCapability::Tools).unwrap();
            assert!(
                tools.get("thinking").is_none(),
                "model name does not choose a vendor quirk"
            );
        }
        channel.endpoint = "https://api.deepseek.com".into();
        for protocol in [
            ModelProtocol::OpenAiResponses,
            ModelProtocol::AnthropicMessages,
            ModelProtocol::GoogleGenerativeAi,
        ] {
            channel.protocol = protocol;
            let (_, tools) = probe_request(&channel, VerificationCapability::Tools).unwrap();
            assert!(
                tools.get("thinking").is_none(),
                "quirk is specific to chat completions"
            );
        }
    }
    #[test]
    fn malformed_probe_parameters_are_local_rejections_without_http_status() {
        let (_directory, store, mut channel) = fixture();
        channel.model = "invalid identifier".into();
        let error = verify_model(
            &store,
            &channel,
            VerificationCapability::Tools,
            &UnusedCredential,
            &Arc::default(),
        )
        .unwrap_err();
        assert_eq!(error, CatalogueError::InvalidProbeParameters);
        assert_eq!(error.http_status(), None);
        channel.model = "valid-model".into();
        channel.protocol = ModelProtocol::OpenAiCompletions;
        channel.endpoint = "invalid address".into();
        let error = probe_request(&channel, VerificationCapability::Inference).unwrap_err();
        assert_eq!(error, CatalogueError::InvalidProbeParameters);
        assert_eq!(error.code(), "catalogue.invalidProbeParameters");
        assert_eq!(error.http_status(), None);
    }
}
