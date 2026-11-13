// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native model-channel metadata. This module never stores a secret, grants
//! network authorization, contacts a provider, or starts an agent scheduler.
//!
//! Mutations use a permanent process lock, an expected revision and content
//! digest, and an atomic file replacement. The native settings integration
//! must retire the old runtime before changing a configuration or credential.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::{Host, Url};

/// Dedicated non-secret metadata file, separate from preferences and Pi.
pub const CONFIGURATION_FILE: &str = "model-channels.json";
/// The only format understood here; other versions are never migrated.
pub const CONFIGURATION_SCHEMA: u32 = 1;
const LOCK_FILE: &str = ".model-channels.lock";
const MAX_DOCUMENT_BYTES: usize = 128 * 1024;
const MAX_CHANNELS: usize = 32;

/// Stable native identity; labels, model names and addresses are not identities.
#[derive(Clone, Debug, Eq, PartialEq, Ord, PartialOrd, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ChannelId(String);

impl ChannelId {
    /// Generate a fresh, product-local identity with operating-system entropy.
    pub fn generate() -> Result<Self, ConfigurationError> {
        let mut bytes = [0_u8; 16];
        getrandom::fill(&mut bytes).map_err(|_| ConfigurationError::RandomUnavailable)?;
        Ok(Self(
            bytes.iter().map(|byte| format!("{byte:02x}")).collect(),
        ))
    }

    /// Accept only the exact native identity syntax, never a path or label.
    pub fn parse(value: &str) -> Result<Self, ConfigurationError> {
        if value.len() != 32
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(ConfigurationError::InvalidIdentity);
        }
        Ok(Self(value.to_owned()))
    }

    /// The stable non-secret identity, suitable for a credential scope.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl TryFrom<String> for ChannelId {
    type Error = ConfigurationError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse(&value)
    }
}

impl From<ChannelId> for String {
    fn from(value: ChannelId) -> Self {
        value.0
    }
}

/// Explicit protocols already present in the pinned original Pi adapter.
/// A protocol selection alone is not a tested provider or a network grant.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub enum ModelProtocol {
    /// The pinned original adapter's chat-completions protocol.
    #[serde(rename = "openai-completions")]
    OpenAiCompletions,
    /// The pinned original adapter's responses protocol.
    #[serde(rename = "openai-responses")]
    OpenAiResponses,
    /// The pinned original adapter's messages protocol.
    #[serde(rename = "anthropic-messages")]
    AnthropicMessages,
}

impl ModelProtocol {
    /// The exact protocol identifier consumed by the original adapter.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
        }
    }

    /// Refuse a protocol outside the reviewed initial set.
    pub fn parse(value: &str) -> Result<Self, ConfigurationError> {
        match value {
            "openai-completions" => Ok(Self::OpenAiCompletions),
            "openai-responses" => Ok(Self::OpenAiResponses),
            "anthropic-messages" => Ok(Self::AnthropicMessages),
            _ => Err(ConfigurationError::UnsupportedProtocol),
        }
    }
}

/// A native user's proposed non-secret fields, validated before persistence.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChannelDraft {
    /// User-visible name, not a provider catalogue or credential identity.
    pub label: String,
    /// Explicit original protocol; no protocol is guessed from an address.
    pub protocol: ModelProtocol,
    /// Explicit complete service base, including its path.
    pub endpoint: String,
    /// Exact model identifier supplied by the user.
    pub model: String,
    /// Request deadline in milliseconds, from one to six hundred seconds.
    pub request_timeout_ms: u32,
    /// User-selected output ceiling, without an inferred price or budget.
    pub max_output_tokens: u32,
}

/// One saved channel. This structure has no secret or network-authorization field.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ModelChannel {
    /// Stable native identity retained when the user edits this channel.
    pub id: ChannelId,
    /// Latest global configuration version that changed this channel or its key.
    pub version: u64,
    /// Non-secret user-visible name.
    pub label: String,
    /// Explicit original adapter protocol.
    pub protocol: ModelProtocol,
    /// Canonical complete service base.
    pub endpoint: String,
    /// Exact configured model identifier.
    pub model: String,
    /// Request deadline in milliseconds.
    pub request_timeout_ms: u32,
    /// Output ceiling; never a fee authorization.
    pub max_output_tokens: u32,
}

impl ModelChannel {
    /// Includes the effective port and complete base path. A change of path on
    /// the same host cannot reuse the previous credential scope.
    pub fn service_origin(&self) -> Result<String, ConfigurationError> {
        canonical_endpoint(&self.endpoint).map(|endpoint| endpoint.1)
    }

    /// Copy only editable non-secret fields, excluding identity and version.
    pub fn draft(&self) -> ChannelDraft {
        ChannelDraft {
            label: self.label.clone(),
            protocol: self.protocol,
            endpoint: self.endpoint.clone(),
            model: self.model.clone(),
            request_timeout_ms: self.request_timeout_ms,
            max_output_tokens: self.max_output_tokens,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Document {
    schema_version: u32,
    configuration_version: u64,
    active_channel: Option<ChannelId>,
    channels: Vec<ModelChannel>,
}

impl Default for Document {
    fn default() -> Self {
        Self {
            schema_version: CONFIGURATION_SCHEMA,
            configuration_version: 0,
            active_channel: None,
            channels: Vec::new(),
        }
    }
}

/// Read-only metadata and the exact file identity reviewed by the native UI.
/// The digest also refuses an external edit that failed to increase a version.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ConfigurationSnapshot {
    document: Document,
    digest: [u8; 32],
}

impl ConfigurationSnapshot {
    /// Durable global revision; zero means there is no configuration file yet.
    pub fn version(&self) -> u64 {
        self.document.configuration_version
    }

    /// All explicitly saved channels, in stable UI order.
    pub fn channels(&self) -> &[ModelChannel] {
        &self.document.channels
    }

    /// Find a channel by its stable native identity.
    pub fn channel(&self, id: &ChannelId) -> Option<&ModelChannel> {
        self.channels().iter().find(|channel| &channel.id == id)
    }

    /// The explicitly selected channel; selection is not execution authority.
    pub fn active(&self) -> Option<&ModelChannel> {
        self.document
            .active_channel
            .as_ref()
            .and_then(|id| self.channel(id))
    }
}

/// Fixed, non-secret failure categories; rejected data and paths are not echoed.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ConfigurationError {
    /// An identity does not use the native opaque syntax.
    InvalidIdentity,
    /// A label is empty, oversized or contains control characters.
    InvalidLabel,
    /// The requested protocol is outside the reviewed set.
    UnsupportedProtocol,
    /// The address is unsafe, secret-bearing or not an explicit service base.
    InvalidAddress,
    /// The model identifier is empty, oversized or contains whitespace/controls.
    InvalidModel,
    /// The deadline is outside the supported bounds.
    InvalidTimeout,
    /// The output ceiling is outside the supported bounds.
    InvalidOutputLimit,
    /// The bounded configuration cannot hold another channel.
    TooManyChannels,
    /// The persisted file is malformed, oversized or internally inconsistent.
    InvalidDocument,
    /// The persisted format needs an explicitly reviewed upgrade.
    UnsupportedSchema,
    /// The referenced saved identity does not exist.
    MissingChannel,
    /// The actual file no longer matches the reviewed revision and digest.
    StaleConfiguration,
    /// The persistent revision cannot increase without wrapping.
    VersionExhausted,
    /// Another process or handle owns the permanent configuration lock.
    Busy,
    /// Reading, locking or preparing the replacement failed.
    StorageUnavailable,
    /// Replacement occurred, but durable completion or readback was not confirmed.
    CommitUnconfirmed,
    /// Operating-system entropy was unavailable.
    RandomUnavailable,
}

impl fmt::Display for ConfigurationError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(match self {
            Self::InvalidIdentity => "渠道身份无效，请重新读取设置。",
            Self::InvalidLabel => "渠道名称须为1至80个字符，不能包含控制字符。",
            Self::UnsupportedProtocol => "请选择当前支持的明确模型协议。",
            Self::InvalidAddress => "服务基址无效：请使用安全网页地址，且不包含用户名、密码、查询参数或片段；普通网页协议只允许本机回环地址。",
            Self::InvalidModel => "请填写1至256个可见字符的模型标识，不能包含空白或控制字符。",
            Self::InvalidTimeout => "请求超时须在1至600秒之间。",
            Self::InvalidOutputLimit => "单次输出上限须在1至1000000个词元之间。",
            Self::TooManyChannels => "最多可保存32个模型渠道。",
            Self::InvalidDocument => "模型设置文件无效，已拒绝覆盖；请先检查原文件。",
            Self::UnsupportedSchema => "模型设置版本不受支持，未迁移或覆盖原文件。",
            Self::MissingChannel => "所选渠道已不存在，请重新读取设置。",
            Self::StaleConfiguration => "模型设置已变化，旧操作已拒绝；请重新读取当前设置。",
            Self::VersionExhausted => "模型设置版本已达上限，未保存。",
            Self::Busy => "另一项设置写入正在进行，请稍后重新读取。",
            Self::StorageUnavailable => "模型设置存储不可用，未确认保存。",
            Self::CommitUnconfirmed => "模型设置替换后未能确认落盘，请重新读取核对；旧运行资格已撤销。",
            Self::RandomUnavailable => "无法生成安全的渠道身份，未创建渠道。",
        })
    }
}

impl std::error::Error for ConfigurationError {}

/// Validate and canonicalize an explicit service base, without guessing a
/// provider, adding an API path, resolving DNS, or making a request.
pub fn canonical_endpoint(input: &str) -> Result<(String, String), ConfigurationError> {
    if input.is_empty()
        || input.len() > 2048
        || input.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ConfigurationError::InvalidAddress);
    }
    let url = Url::parse(input).map_err(|_| ConfigurationError::InvalidAddress)?;
    let local = match url.host() {
        Some(Host::Domain("localhost")) => true,
        Some(Host::Ipv4(address)) => address.is_loopback(),
        Some(Host::Ipv6(address)) => address.is_loopback(),
        _ => false,
    };
    if (url.scheme() != "https" && !(url.scheme() == "http" && local))
        || url.host().is_none()
        || !url.username().is_empty()
        || url.password().is_some()
        || url.query().is_some()
        || url.fragment().is_some()
    {
        return Err(ConfigurationError::InvalidAddress);
    }
    let port = url
        .port_or_known_default()
        .ok_or(ConfigurationError::InvalidAddress)?;
    if port == 0 {
        return Err(ConfigurationError::InvalidAddress);
    }
    let host = url.host().ok_or(ConfigurationError::InvalidAddress)?;
    let origin = format!("{}://{}:{}{}", url.scheme(), host, port, url.path());
    Ok((url.to_string(), origin))
}

/// Validate bounded fields and normalize only the label and service address.
pub fn validate_draft(mut draft: ChannelDraft) -> Result<ChannelDraft, ConfigurationError> {
    draft.label = draft.label.trim().to_owned();
    if draft.label.is_empty()
        || draft.label.chars().count() > 80
        || draft.label.chars().any(char::is_control)
    {
        return Err(ConfigurationError::InvalidLabel);
    }
    if draft.model.is_empty()
        || draft.model.chars().count() > 256
        || draft
            .model
            .chars()
            .any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ConfigurationError::InvalidModel);
    }
    if !(1_000..=600_000).contains(&draft.request_timeout_ms) {
        return Err(ConfigurationError::InvalidTimeout);
    }
    if !(1..=1_000_000).contains(&draft.max_output_tokens) {
        return Err(ConfigurationError::InvalidOutputLimit);
    }
    draft.endpoint = canonical_endpoint(&draft.endpoint)?.0;
    Ok(draft)
}

fn validate_document(document: &Document) -> Result<(), ConfigurationError> {
    if document.schema_version != CONFIGURATION_SCHEMA {
        return Err(ConfigurationError::UnsupportedSchema);
    }
    if document.configuration_version == 0 || document.channels.len() > MAX_CHANNELS {
        return Err(ConfigurationError::InvalidDocument);
    }
    let mut identities = BTreeSet::new();
    for channel in &document.channels {
        if !identities.insert(&channel.id)
            || channel.version == 0
            || channel.version > document.configuration_version
            || validate_draft(channel.draft())? != channel.draft()
        {
            return Err(ConfigurationError::InvalidDocument);
        }
    }
    if document
        .active_channel
        .as_ref()
        .is_some_and(|id| !identities.contains(id))
    {
        return Err(ConfigurationError::InvalidDocument);
    }
    Ok(())
}

/// Read/replace one dedicated non-secret configuration under a permanent lock.
#[derive(Clone, Debug)]
pub struct ConfigurationStore {
    directory: PathBuf,
}

impl ConfigurationStore {
    /// Use a dedicated application directory, never a project/media directory.
    pub fn new(directory: impl AsRef<Path>) -> Self {
        Self {
            directory: directory.as_ref().to_path_buf(),
        }
    }

    /// Read without creating a file or granting any execution authority.
    pub fn load(&self) -> Result<ConfigurationSnapshot, ConfigurationError> {
        let path = self.directory.join(CONFIGURATION_FILE);
        let metadata = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(ConfigurationSnapshot {
                    document: Document::default(),
                    digest: Sha256::digest([]).into(),
                });
            }
            Err(_) => return Err(ConfigurationError::StorageUnavailable),
        };
        if !metadata.is_file()
            || metadata.file_type().is_symlink()
            || metadata.len() > MAX_DOCUMENT_BYTES as u64
        {
            return Err(ConfigurationError::InvalidDocument);
        }
        let mut bytes = Vec::new();
        File::open(path)
            .and_then(|file| {
                file.take((MAX_DOCUMENT_BYTES + 1) as u64)
                    .read_to_end(&mut bytes)
            })
            .map_err(|_| ConfigurationError::StorageUnavailable)?;
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(ConfigurationError::InvalidDocument);
        }
        let document = serde_json::from_slice::<Document>(&bytes)
            .map_err(|_| ConfigurationError::InvalidDocument)?;
        validate_document(&document)?;
        Ok(ConfigurationSnapshot {
            document,
            digest: Sha256::digest(bytes).into(),
        })
    }

    /// Re-read and reject both a changed revision and same-version content edits.
    pub fn assert_current(
        &self,
        expected: &ConfigurationSnapshot,
    ) -> Result<(), ConfigurationError> {
        if &self.load()? != expected {
            return Err(ConfigurationError::StaleConfiguration);
        }
        Ok(())
    }

    /// Create a fresh identity, or replace only the identified channel. Saving
    /// does not select a new channel implicitly and never grants network access.
    pub fn save_channel(
        &self,
        expected: &ConfigurationSnapshot,
        id: Option<&ChannelId>,
        draft: ChannelDraft,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
        let draft = validate_draft(draft)?;
        self.mutate(expected, |document, version| {
            let id = match id {
                Some(id) => {
                    if !document.channels.iter().any(|channel| &channel.id == id) {
                        return Err(ConfigurationError::MissingChannel);
                    }
                    id.clone()
                }
                None => {
                    if document.channels.len() >= MAX_CHANNELS {
                        return Err(ConfigurationError::TooManyChannels);
                    }
                    ChannelId::generate()?
                }
            };
            let channel = ModelChannel {
                id: id.clone(),
                version,
                label: draft.label,
                protocol: draft.protocol,
                endpoint: draft.endpoint,
                model: draft.model,
                request_timeout_ms: draft.request_timeout_ms,
                max_output_tokens: draft.max_output_tokens,
            };
            if let Some(existing) = document
                .channels
                .iter_mut()
                .find(|channel| channel.id == id)
            {
                *existing = channel;
            } else {
                document.channels.push(channel);
            }
            Ok(())
        })
    }

    /// Persist an explicit selection or clear it; neither action grants calls.
    pub fn select(
        &self,
        expected: &ConfigurationSnapshot,
        id: Option<&ChannelId>,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
        self.mutate(expected, |document, _| {
            if id.is_some_and(|id| !document.channels.iter().any(|channel| &channel.id == id)) {
                return Err(ConfigurationError::MissingChannel);
            }
            document.active_channel = id.cloned();
            Ok(())
        })
    }

    /// Remove exactly one identity, clearing it if active without choosing another.
    pub fn delete(
        &self,
        expected: &ConfigurationSnapshot,
        id: &ChannelId,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
        self.mutate(expected, |document, _| {
            let index = document
                .channels
                .iter()
                .position(|channel| &channel.id == id)
                .ok_or(ConfigurationError::MissingChannel)?;
            document.channels.remove(index);
            if document.active_channel.as_ref() == Some(id) {
                document.active_channel = None;
            }
            Ok(())
        })
    }

    /// Bump the reviewed version before changing a bound secure credential.
    /// The caller must still report the real credential-store result separately.
    pub fn credential_changed(
        &self,
        expected: &ConfigurationSnapshot,
        id: &ChannelId,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
        self.mutate(expected, |document, version| {
            let channel = document
                .channels
                .iter_mut()
                .find(|channel| &channel.id == id)
                .ok_or(ConfigurationError::MissingChannel)?;
            channel.version = version;
            Ok(())
        })
    }

    fn mutate(
        &self,
        expected: &ConfigurationSnapshot,
        change: impl FnOnce(&mut Document, u64) -> Result<(), ConfigurationError>,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
        fs::create_dir_all(&self.directory).map_err(|_| ConfigurationError::StorageUnavailable)?;
        let _lock = self.lock()?;
        let current = self.load()?;
        if &current != expected {
            return Err(ConfigurationError::StaleConfiguration);
        }
        let mut document = current.document;
        let version = document
            .configuration_version
            .checked_add(1)
            .ok_or(ConfigurationError::VersionExhausted)?;
        change(&mut document, version)?;
        document.configuration_version = version;
        validate_document(&document)?;
        self.commit_document(&document, || Ok(()))
    }

    fn commit_document(
        &self,
        document: &Document,
        before_readback: impl FnOnce() -> Result<(), ConfigurationError>,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
        let expected_digest = self.write_document(document, || Ok(()))?;
        before_readback()?;
        let saved = self
            .load()
            .map_err(|_| ConfigurationError::CommitUnconfirmed)?;
        if &saved.document != document || saved.digest != expected_digest {
            return Err(ConfigurationError::CommitUnconfirmed);
        }
        Ok(saved)
    }

    fn lock(&self) -> Result<File, ConfigurationError> {
        let path = self.directory.join(LOCK_FILE);
        if let Ok(metadata) = fs::symlink_metadata(&path)
            && (!metadata.is_file() || metadata.file_type().is_symlink())
        {
            return Err(ConfigurationError::StorageUnavailable);
        }
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;
            options.share_mode(0x0000_0001 | 0x0000_0002);
        }
        let file = options
            .open(path)
            .map_err(|_| ConfigurationError::StorageUnavailable)?;
        match crate::try_lock_owner_file(&file) {
            Ok(()) => Ok(file),
            Err(TryLockError::WouldBlock) => Err(ConfigurationError::Busy),
            Err(TryLockError::Error(_)) => Err(ConfigurationError::StorageUnavailable),
        }
    }

    fn write_document(
        &self,
        document: &Document,
        before_replace: impl FnOnce() -> Result<(), ConfigurationError>,
    ) -> Result<[u8; 32], ConfigurationError> {
        let mut bytes =
            serde_json::to_vec_pretty(document).map_err(|_| ConfigurationError::InvalidDocument)?;
        bytes.push(b'\n');
        if bytes.len() > MAX_DOCUMENT_BYTES {
            return Err(ConfigurationError::InvalidDocument);
        }
        let mut temporary = tempfile::Builder::new()
            .prefix(".bk-cat-model-")
            .tempfile_in(&self.directory)
            .map_err(|_| ConfigurationError::StorageUnavailable)?;
        temporary
            .write_all(&bytes)
            .and_then(|()| temporary.as_file().sync_all())
            .map_err(|_| ConfigurationError::StorageUnavailable)?;
        before_replace()?;
        let file = temporary
            .persist(self.directory.join(CONFIGURATION_FILE))
            .map_err(|_| ConfigurationError::StorageUnavailable)?;
        file.sync_all()
            .map_err(|_| ConfigurationError::CommitUnconfirmed)?;
        #[cfg(unix)]
        File::open(&self.directory)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| ConfigurationError::CommitUnconfirmed)?;
        // Windows exposes file-content sync here; no power-loss guarantee for
        // a directory entry is inferred from the process-crash tests.
        Ok(Sha256::digest(&bytes).into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_different_valid_document_read_back_after_replacement_is_not_save_confirmation() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigurationStore::new(directory.path());
        let first = store.select(&store.load().unwrap(), None).unwrap();
        let mut requested = first.document.clone();
        requested.configuration_version += 1;
        let mut external = requested.clone();
        external.configuration_version += 1;
        let other_bytes = serde_json::to_vec(&external).unwrap();
        assert_eq!(
            store.commit_document(&requested, || {
                fs::write(directory.path().join(CONFIGURATION_FILE), &other_bytes).unwrap();
                Ok(())
            }),
            Err(ConfigurationError::CommitUnconfirmed)
        );
        assert_eq!(store.load().unwrap().document, external);
        assert_eq!(
            fs::read(directory.path().join(CONFIGURATION_FILE)).unwrap(),
            other_bytes
        );
    }

    #[test]
    fn failed_atomic_replacement_keeps_the_previous_complete_document() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigurationStore::new(directory.path());
        let initial = store.select(&store.load().unwrap(), None).unwrap();
        let original = fs::read(directory.path().join(CONFIGURATION_FILE)).unwrap();
        let mut replacement = initial.document.clone();
        replacement.configuration_version += 1;
        assert_eq!(
            store.write_document(&replacement, || Err(ConfigurationError::StorageUnavailable)),
            Err(ConfigurationError::StorageUnavailable)
        );
        assert_eq!(
            fs::read(directory.path().join(CONFIGURATION_FILE)).unwrap(),
            original
        );
        assert_eq!(store.load().unwrap(), initial);
        assert!(!fs::read_dir(directory.path()).unwrap().any(|entry| {
            entry
                .unwrap()
                .file_name()
                .to_string_lossy()
                .starts_with(".bk-cat-model-")
        }));
    }

    #[test]
    fn permanent_configuration_lock_refuses_overlapping_writers() {
        let directory = tempfile::tempdir().unwrap();
        let store = ConfigurationStore::new(directory.path());
        let held = store.lock().unwrap();
        assert_eq!(
            store.select(&store.load().unwrap(), None),
            Err(ConfigurationError::Busy)
        );
        drop(held);
        assert!(store.select(&store.load().unwrap(), None).is_ok());
        assert!(directory.path().join(LOCK_FILE).is_file());
    }
}
