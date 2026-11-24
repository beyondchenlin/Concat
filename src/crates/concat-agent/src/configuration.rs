// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native model-channel metadata. This module never stores a secret, grants
//! network authorization, contacts a provider, or starts an agent scheduler.
//!
//! Mutations use a permanent process lock, an expected revision and content
//! digest, and an atomic file replacement. The native settings integration
//! must retire the old runtime before changing a configuration or credential.

pub mod catalogue;
pub mod providers;

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File, OpenOptions, TryLockError};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use url::{Host, Url};

/// Dedicated non-secret metadata file, separate from preferences and Pi.
pub const CONFIGURATION_FILE: &str = "model-channels.json";
/// Version two adds provider/region and credential revision while preserving identities.
pub const CONFIGURATION_SCHEMA: u32 = 2;
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
    /// The pinned original Google Generative AI adapter.
    #[serde(rename = "google-generative-ai")]
    GoogleGenerativeAi,
}

impl ModelProtocol {
    /// The exact protocol identifier consumed by the original adapter.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OpenAiCompletions => "openai-completions",
            Self::OpenAiResponses => "openai-responses",
            Self::AnthropicMessages => "anthropic-messages",
            Self::GoogleGenerativeAi => "google-generative-ai",
        }
    }

    /// Refuse a protocol outside the reviewed initial set.
    pub fn parse(value: &str) -> Result<Self, ConfigurationError> {
        match value {
            "openai-completions" => Ok(Self::OpenAiCompletions),
            "openai-responses" => Ok(Self::OpenAiResponses),
            "anthropic-messages" => Ok(Self::AnthropicMessages),
            "google-generative-ai" => Ok(Self::GoogleGenerativeAi),
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
    /// Provider preset; legacy channels migrate to custom without guessing.
    #[serde(default)]
    pub provider_id: String,
    /// Product/region identifier, isolated per connection.
    #[serde(default)]
    pub region: String,
    /// Changes only when a secure credential is changed or removed.
    #[serde(default)]
    pub credential_version: u64,
    /// Explicitly retained models; directory refresh never removes these references.
    #[serde(default)]
    pub manual_models: Vec<String>,
    /// Explicit conservative capacities entered by the user, never a verified catalogue fact.
    #[serde(default)]
    pub context_overrides: BTreeMap<String, u32>,
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

    /// All connections, including ones awaiting a model selection.
    pub fn connections(&self) -> &[ModelChannel] {
        self.channels()
    }

    /// Resolve a native session selection without changing the application default.
    pub fn selected_model(
        &self,
        id: &ChannelId,
        model: &str,
    ) -> Result<ModelChannel, ConfigurationError> {
        let mut channel = self
            .channel(id)
            .cloned()
            .ok_or(ConfigurationError::MissingChannel)?;
        validate_model_id(model)?;
        channel.model = model.to_owned();
        Ok(channel)
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
            .filter(|channel| !channel.model.is_empty())
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

impl ConfigurationError {
    /// Stable failure identity; the native interface chooses its display language.
    pub fn code(self) -> &'static str {
        match self {
            Self::InvalidIdentity => "modelSettings.invalidIdentity",
            Self::InvalidLabel => "modelSettings.invalidLabel",
            Self::UnsupportedProtocol => "modelSettings.unsupportedProtocol",
            Self::InvalidAddress => "modelSettings.invalidAddress",
            Self::InvalidModel => "modelSettings.invalidModel",
            Self::InvalidTimeout => "modelSettings.invalidTimeout",
            Self::InvalidOutputLimit => "modelSettings.invalidOutputLimit",
            Self::TooManyChannels => "modelSettings.tooManyChannels",
            Self::InvalidDocument => "modelSettings.invalidDocument",
            Self::UnsupportedSchema => "modelSettings.unsupportedSchema",
            Self::MissingChannel => "modelSettings.missingChannel",
            Self::StaleConfiguration => "modelSettings.staleConfiguration",
            Self::VersionExhausted => "modelSettings.versionExhausted",
            Self::Busy => "modelSettings.busy",
            Self::StorageUnavailable => "modelSettings.storageUnavailable",
            Self::CommitUnconfirmed => "modelSettings.commitUnconfirmed",
            Self::RandomUnavailable => "modelSettings.randomUnavailable",
        }
    }
}
impl fmt::Display for ConfigurationError {
    fn fmt(&self, out: &mut fmt::Formatter<'_>) -> fmt::Result {
        out.write_str(self.code())
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

/// Validate an exact model identifier without guessing its provider or capacity.
pub fn validate_model_id(model: &str) -> Result<(), ConfigurationError> {
    if model.is_empty()
        || model.chars().count() > 256
        || model.chars().any(|c| c.is_whitespace() || c.is_control())
    {
        return Err(ConfigurationError::InvalidModel);
    }
    Ok(())
}

/// Validate a connection which may still await its first model selection.
pub fn validate_connection_draft(
    mut draft: ChannelDraft,
) -> Result<ChannelDraft, ConfigurationError> {
    let waiting = draft.model.is_empty();
    if waiting {
        draft.model = "awaiting-model-selection".into();
    }
    let mut validated = validate_draft(draft)?;
    if waiting {
        validated.model.clear();
    }
    Ok(validated)
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
            || validate_connection_draft(channel.draft())? != channel.draft()
            || providers::preset(&channel.provider_id).is_none()
            || channel.region.is_empty()
            || channel.region.len() > 80
            || channel.region.chars().any(char::is_control)
            || channel.credential_version == 0
            || channel.credential_version > channel.version
            || channel.manual_models.len() > 1024
            || channel
                .manual_models
                .iter()
                .any(|id| validate_model_id(id).is_err())
            || channel.context_overrides.len() > 1024
            || channel.context_overrides.iter().any(|(id, value)| {
                validate_model_id(id).is_err() || !(1..=100_000_000).contains(value)
            })
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
        let mut document = serde_json::from_slice::<Document>(&bytes)
            .map_err(|_| ConfigurationError::InvalidDocument)?;
        if document.schema_version == 1 {
            // Pure projection on read: the first successful atomic mutation writes v2.
            // Each legacy id and credential scope survives exactly; no host grouping.
            document.schema_version = CONFIGURATION_SCHEMA;
            for channel in &mut document.channels {
                channel.provider_id = "custom".into();
                channel.region = "custom".into();
                channel.credential_version = channel.version;
                if !channel.model.is_empty() {
                    channel.manual_models = vec![channel.model.clone()];
                }
            }
        }
        validate_document(&document)?;
        Ok(ConfigurationSnapshot {
            document,
            digest: Sha256::digest(bytes).into(),
        })
    }

    /// Only connection configuration/credential changes revoke its tasks. Defaults and directory freshness do not.
    pub fn assert_connection_current(
        &self,
        expected: &ModelChannel,
    ) -> Result<(), ConfigurationError> {
        let current = self.load()?;
        let actual = current
            .channel(&expected.id)
            .ok_or(ConfigurationError::MissingChannel)?;
        if actual.version != expected.version
            || actual.endpoint != expected.endpoint
            || actual.protocol != expected.protocol
            || actual.credential_version != expected.credential_version
            || actual.region != expected.region
            || actual.provider_id != expected.provider_id
            || actual.label != expected.label
            || actual.request_timeout_ms != expected.request_timeout_ms
            || actual.max_output_tokens != expected.max_output_tokens
            || actual.context_overrides != expected.context_overrides
        {
            return Err(ConfigurationError::StaleConfiguration);
        }
        Ok(())
    }

    /// Non-secret catalogue cache under the product settings directory.
    pub fn directory(&self) -> &Path {
        &self.directory
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
        let existing = id.and_then(|id| expected.channel(id));
        self.save_connection(
            expected,
            id,
            draft,
            existing.map_or("custom", |c| c.provider_id.as_str()),
            existing.map_or("custom", |c| c.region.as_str()),
        )
    }

    /// Save a provider connection; selecting a model may follow discovery.
    pub fn save_connection(
        &self,
        expected: &ConfigurationSnapshot,
        id: Option<&ChannelId>,
        draft: ChannelDraft,
        provider_id: &str,
        region: &str,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
        let draft = validate_connection_draft(draft)?;
        let preset =
            providers::preset(provider_id).ok_or(ConfigurationError::UnsupportedProtocol)?;
        if region != preset.region
            || (provider_id != "custom"
                && (draft.protocol != preset.protocol
                    || draft.endpoint.trim_end_matches('/') != preset.endpoint))
        {
            return Err(ConfigurationError::InvalidAddress);
        }
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
            let previous = document.channels.iter().find(|channel| channel.id == id);
            let credential_version = previous.map_or(version, |channel| channel.credential_version);
            let connection_version = previous
                .filter(|channel| {
                    channel.provider_id == provider_id
                        && channel.region == region
                        && channel.label == draft.label
                        && channel.protocol == draft.protocol
                        && channel.endpoint == draft.endpoint
                        && channel.request_timeout_ms == draft.request_timeout_ms
                        && channel.max_output_tokens == draft.max_output_tokens
                })
                .map_or(version, |channel| channel.version);
            let mut manual_models = previous
                .filter(|channel| {
                    channel.endpoint == draft.endpoint && channel.protocol == draft.protocol
                })
                .map_or_else(Vec::new, |channel| channel.manual_models.clone());
            if !draft.model.is_empty() && !manual_models.contains(&draft.model) {
                manual_models.push(draft.model.clone());
            }
            let context_overrides = previous
                .filter(|channel| {
                    channel.endpoint == draft.endpoint && channel.protocol == draft.protocol
                })
                .map_or_else(BTreeMap::new, |channel| channel.context_overrides.clone());
            let channel = ModelChannel {
                id: id.clone(),
                provider_id: provider_id.to_owned(),
                region: region.to_owned(),
                credential_version,
                manual_models,
                context_overrides,
                version: connection_version,
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

    /// Change only the application default model. Its connection version and
    /// running-task authorizations stay unchanged; tasks capture their model.
    pub fn select_model(
        &self,
        expected: &ConfigurationSnapshot,
        id: &ChannelId,
        model: &str,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
        validate_model_id(model)?;
        self.mutate(expected, |document, _| {
            let channel = document
                .channels
                .iter_mut()
                .find(|channel| &channel.id == id)
                .ok_or(ConfigurationError::MissingChannel)?;
            channel.model = model.to_owned();
            if !channel.manual_models.iter().any(|saved| saved == model) {
                channel.manual_models.push(model.to_owned());
            }
            document.active_channel = Some(id.clone());
            Ok(())
        })
    }

    /// Explicit, model-scoped conservative input for services without capacity metadata.
    pub fn set_context_override(
        &self,
        expected: &ConfigurationSnapshot,
        id: &ChannelId,
        model: &str,
        capacity: Option<u32>,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
        validate_model_id(model)?;
        if capacity.is_some_and(|value| !(1..=100_000_000).contains(&value)) {
            return Err(ConfigurationError::InvalidOutputLimit);
        }
        self.mutate(expected, |document, version| {
            let channel = document
                .channels
                .iter_mut()
                .find(|channel| &channel.id == id)
                .ok_or(ConfigurationError::MissingChannel)?;
            match capacity {
                Some(value) => {
                    channel.context_overrides.insert(model.to_owned(), value);
                }
                None => {
                    channel.context_overrides.remove(model);
                }
            }
            channel.version = version;
            if !channel.manual_models.iter().any(|saved| saved == model) {
                channel.manual_models.push(model.to_owned());
            }
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
            channel.credential_version = version;
            Ok(())
        })
    }

    /// Serialize a vault operation with its version invalidation. Even a failed
    /// vault write leaves the old version revoked; no secret enters this store.
    pub fn credential_transaction<T>(
        &self,
        expected: &ConfigurationSnapshot,
        id: &ChannelId,
        operation: impl FnOnce(&ModelChannel) -> T,
    ) -> Result<(ConfigurationSnapshot, T), ConfigurationError> {
        fs::create_dir_all(&self.directory).map_err(|_| ConfigurationError::StorageUnavailable)?;
        let _lock = self.lock()?;
        let next = self.mutate_locked(expected, |document, version| {
            let channel = document
                .channels
                .iter_mut()
                .find(|channel| &channel.id == id)
                .ok_or(ConfigurationError::MissingChannel)?;
            channel.version = version;
            channel.credential_version = version;
            Ok(())
        })?;
        let result = operation(next.channel(id).ok_or(ConfigurationError::MissingChannel)?);
        self.assert_current(&next)?;
        Ok((next, result))
    }

    /// Revoke a connection before touching its secure credential, keep the
    /// configuration lock through deletion/readback, then remove the connection.
    /// A failed vault operation retains the revoked connection for an explicit
    /// retry; another instance cannot write a replacement key in the middle.
    pub fn delete_with_credential<E>(
        &self,
        expected: &ConfigurationSnapshot,
        id: &ChannelId,
        delete_credential: impl FnOnce(&ModelChannel) -> Result<(), E>,
    ) -> Result<(ConfigurationSnapshot, Result<(), E>), ConfigurationError> {
        fs::create_dir_all(&self.directory).map_err(|_| ConfigurationError::StorageUnavailable)?;
        let _lock = self.lock()?;
        let revoked = self.mutate_locked(expected, |document, version| {
            let channel = document
                .channels
                .iter_mut()
                .find(|channel| &channel.id == id)
                .ok_or(ConfigurationError::MissingChannel)?;
            channel.version = version;
            channel.credential_version = version;
            Ok(())
        })?;
        let outcome = delete_credential(
            revoked
                .channel(id)
                .ok_or(ConfigurationError::MissingChannel)?,
        );
        self.assert_current(&revoked)?;
        if outcome.is_err() {
            return Ok((revoked, outcome));
        }
        let next = self.mutate_locked(&revoked, |document, _| {
            document.channels.retain(|channel| &channel.id != id);
            if document.active_channel.as_ref() == Some(id) {
                document.active_channel = None;
            }
            Ok(())
        })?;
        Ok((next, outcome))
    }

    fn mutate(
        &self,
        expected: &ConfigurationSnapshot,
        change: impl FnOnce(&mut Document, u64) -> Result<(), ConfigurationError>,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
        fs::create_dir_all(&self.directory).map_err(|_| ConfigurationError::StorageUnavailable)?;
        let _lock = self.lock()?;
        self.mutate_locked(expected, change)
    }

    fn mutate_locked(
        &self,
        expected: &ConfigurationSnapshot,
        change: impl FnOnce(&mut Document, u64) -> Result<(), ConfigurationError>,
    ) -> Result<ConfigurationSnapshot, ConfigurationError> {
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
        // Retain the exact v1 bytes before the first replacement. No secrets live here.
        let path = self.directory.join(CONFIGURATION_FILE);
        if let Ok(bytes) = fs::read(&path)
            && serde_json::from_slice::<serde_json::Value>(&bytes)
                .ok()
                .and_then(|v| v.get("schema_version").and_then(serde_json::Value::as_u64))
                == Some(1)
        {
            let backup = self.directory.join("model-channels.v1.backup.json");
            match OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&backup)
            {
                Ok(mut file) => {
                    file.write_all(&bytes)
                        .and_then(|()| file.sync_all())
                        .map_err(|_| ConfigurationError::StorageUnavailable)?;
                }
                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                    if fs::read(backup).map_err(|_| ConfigurationError::StorageUnavailable)?
                        != bytes
                    {
                        return Err(ConfigurationError::StaleConfiguration);
                    }
                }
                Err(_) => return Err(ConfigurationError::StorageUnavailable),
            }
        }
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
