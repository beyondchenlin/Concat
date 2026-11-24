// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah
//! Configuration-only facts. Discovery and active verification are independent.
use super::{
    ChannelId, ConfigurationError, ConfigurationSnapshot, ConfigurationStore, ModelChannel,
    validate_model_id,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io::{Read, Write};

/// One native choice. Refresh never erases manually retained or selected models.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ModelChoice {
    /// Stable native connection owning this model choice.
    pub connection: ChannelId,
    /// Exact model identifier; never translated or inferred from a name.
    pub model: String,
    /// User-visible connection/model label, separate from identity.
    pub label: String,
    /// Directory provenance, separate from inference verification.
    pub source: Option<CatalogueSource>,
    /// The directory is older than its freshness window or is not an authenticated directory.
    pub stale: bool,
    /// Presence in the last complete configured-service directory, independent of origin.
    /// None means no complete service response has established membership.
    pub listed_in_directory: Option<bool>,
}

/// Model directory freshness window; refresh is user-triggered, never a timer.
pub const FRESH_SECONDS: u64 = 24 * 60 * 60;
const MAX_CACHE_BYTES: usize = 8 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Directory origin, never a model execution success claim.
pub enum CatalogueSource {
    /// Complete response from the configured service, including custom endpoints.
    /// The persisted variant is retained for compatibility. It does not certify
    /// that a custom/third-party endpoint is operated by the named vendor.
    Official,
    /// Explicit public recommendations from official documentation.
    PublicRecommended,
    /// User-entered retained identifier.
    Manual,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Non-secret directory metadata with explicitly unknown capabilities.
pub struct CatalogueModel {
    /// Exact identifier returned by the directory or entered manually.
    pub id: String,
    /// Service-provided display name; never used as a credential identity.
    pub name: String,
    /// Known capacity in tokens, or None when the service did not supply it.
    pub context_window: Option<u32>,
    /// Known provider output capacity, not the user-selected output ceiling.
    pub max_output_tokens: Option<u32>,
    /// Service-advertised tool support; not an executed tool verification.
    pub tools: Option<bool>,
    /// Service-advertised image input support, or unknown.
    pub image_input: Option<bool>,
    /// Traceable source of the context capacity when present.
    pub context_source: Option<String>,
}

impl CatalogueModel {
    /// Retain a user-entered identifier without manufacturing capabilities.
    pub fn manual(id: &str) -> Self {
        Self {
            id: id.into(),
            name: id.into(),
            context_window: None,
            max_output_tokens: None,
            tools: None,
            image_input: None,
            context_source: None,
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// A complete all-page directory result suitable for atomic publication.
pub struct DirectorySnapshot {
    /// Directory provenance, separate from inference verification.
    pub source: CatalogueSource,
    /// Reviewed endpoint or official documentation source.
    pub source_url: String,
    /// Successful complete fetch time in Unix seconds.
    pub fetched_at: u64,
    /// The complete directory; empty is a valid successful result.
    pub models: Vec<CatalogueModel>,
}

impl DirectorySnapshot {
    /// True only for a complete configured-service response within 24 hours.
    pub fn is_fresh(&self, now: u64) -> bool {
        self.source == CatalogueSource::Official
            && now >= self.fetched_at
            && now - self.fetched_at <= FRESH_SECONDS
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
/// Capabilities independently checked by explicit native actions.
pub enum VerificationCapability {
    /// The model returns ordinary text for fixed test content.
    Inference,
    /// The model returns the exact harmless test function and its arguments.
    Tools,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Durable result bound to one model, configuration and credential revision.
pub struct VerificationRecord {
    /// Connection revision under which this explicit check ran.
    pub connection_version: u64,
    /// Credential/account revision under which this check ran.
    pub credential_version: u64,
    /// Exact model identifier; never translated or inferred from a name.
    pub model: String,
    /// The independently checked inference or tool capability.
    pub capability: VerificationCapability,
    /// Explicit verification time in Unix seconds.
    pub checked_at: u64,
    /// Whether the minimal check met its expected protocol result.
    pub succeeded: bool,
    /// Stable error code, not a provider response or credential-bearing body.
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Last refresh result; failure never advances the successful fetch timestamp.
pub struct RefreshOutcome {
    /// Start/result observation time in Unix seconds.
    pub attempted_at: u64,
    /// Stable failure code, or None after a complete successful operation.
    pub error_code: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
/// Independent directory, refresh and explicit verification state.
pub struct CatalogueFacts {
    /// Last completely published directory, retained after failed refresh.
    pub directory: Option<DirectorySnapshot>,
    /// Latest refresh result, independent of directory freshness.
    pub refresh: Option<RefreshOutcome>,
    /// Explicit check history for this cache scope.
    pub verification: Vec<VerificationRecord>,
}

/// Current validity and historical success are independent visible facts.
pub struct VerificationSummary<'a> {
    /// Most recent result for the current exact connection and credential.
    pub latest_current: Option<&'a VerificationRecord>,
    /// Most recent successful result, which may belong to a previous revision.
    pub last_success: Option<&'a VerificationRecord>,
}

impl CatalogueFacts {
    /// Summarize one model/capability without promoting history to current proof.
    pub fn verification_summary(
        &self,
        channel: &ModelChannel,
        model: &str,
        capability: VerificationCapability,
    ) -> VerificationSummary<'_> {
        let records = self
            .verification
            .iter()
            .filter(|record| record.model == model && record.capability == capability);
        VerificationSummary {
            latest_current: records
                .clone()
                .filter(|record| {
                    record.connection_version == channel.version
                        && record.credential_version == channel.credential_version
                })
                .max_by_key(|record| record.checked_at),
            last_success: records
                .filter(|record| record.succeeded)
                .max_by_key(|record| record.checked_at),
        }
    }
}

/// Isolate directory data without hashing or serializing a secret.
pub fn cache_key(channel: &ModelChannel) -> String {
    // Never hash or serialize a credential. A per-connection secure-store revision
    // also isolates accounts when the provider exposes no public account id.
    let scope = serde_json::json!([
        channel.id.as_str(),
        channel.endpoint,
        channel.region,
        channel.provider_id,
        channel.protocol.as_str(),
        channel.credential_version
    ]);
    format!("{:x}", Sha256::digest(scope.to_string().as_bytes()))
}

impl ConfigurationStore {
    /// Project all connections and their retained models into stable choices.
    pub fn flattened_models(
        &self,
        snapshot: &ConfigurationSnapshot,
    ) -> Result<Vec<ModelChoice>, ConfigurationError> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let mut choices = Vec::new();
        for connection in snapshot.connections() {
            let facts = self.catalogue(connection)?;
            let mut seen = std::collections::BTreeSet::new();
            let official_directory = facts
                .directory
                .as_ref()
                .is_some_and(|d| d.source == CatalogueSource::Official);
            if let Some(directory) = facts.directory {
                let stale = !directory.is_fresh(now);
                for model in directory.models {
                    seen.insert(model.id.clone());
                    choices.push(ModelChoice {
                        connection: connection.id.clone(),
                        label: format!("{} · {}", connection.label, model.name),
                        model: model.id,
                        source: Some(directory.source),
                        stale,
                        listed_in_directory: official_directory.then_some(true),
                    });
                }
            }
            for model in connection
                .manual_models
                .iter()
                .chain(std::iter::once(&connection.model))
                .filter(|id| !id.is_empty())
            {
                if seen.insert(model.clone()) {
                    choices.push(ModelChoice {
                        connection: connection.id.clone(),
                        model: model.clone(),
                        label: format!("{} · {}", connection.label, model),
                        source: Some(CatalogueSource::Manual),
                        stale: true,
                        listed_in_directory: official_directory.then_some(false),
                    });
                }
            }
        }
        Ok(choices)
    }

    /// Read only the cache belonging to this exact account/connection scope.
    pub fn catalogue(&self, channel: &ModelChannel) -> Result<CatalogueFacts, ConfigurationError> {
        let path = self
            .directory()
            .join("catalogues")
            .join(channel.id.as_str())
            .join(format!("{}.json", cache_key(channel)));
        read_facts(&path)
    }

    /// Previous credential/address versions remain history, never current proof.
    pub fn verification_history(
        &self,
        channel: &ModelChannel,
    ) -> Result<Vec<VerificationRecord>, ConfigurationError> {
        let path = self
            .directory()
            .join("catalogues")
            .join(channel.id.as_str());
        let entries = match fs::read_dir(path) {
            Ok(entries) => entries,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(_) => return Err(ConfigurationError::StorageUnavailable),
        };
        let mut records = Vec::new();
        for entry in entries {
            let path = entry
                .map_err(|_| ConfigurationError::StorageUnavailable)?
                .path();
            if path.extension().and_then(|value| value.to_str()) == Some("json") {
                records.extend(read_facts(&path)?.verification);
            }
        }
        records.sort_by_key(|record| {
            (
                record.checked_at,
                record.connection_version,
                record.credential_version,
            )
        });
        Ok(records)
    }

    /// Only complete fetch results reach this boundary. Failed refreshes retain
    /// the last complete directory and every independent verification fact.
    pub fn publish_catalogue(
        &self,
        channel: &ModelChannel,
        at: u64,
        result: Result<DirectorySnapshot, &str>,
    ) -> Result<CatalogueFacts, ConfigurationError> {
        self.change_facts(channel, |facts| match result {
            Ok(directory) => {
                facts.directory = Some(directory);
                facts.refresh = Some(RefreshOutcome {
                    attempted_at: at,
                    error_code: None,
                });
            }
            Err(code) => {
                facts.refresh = Some(RefreshOutcome {
                    attempted_at: at,
                    error_code: Some(code.into()),
                });
            }
        })
    }

    /// Append an explicit probe result only while its connection is current.
    pub fn record_verification(
        &self,
        channel: &ModelChannel,
        record: VerificationRecord,
    ) -> Result<CatalogueFacts, ConfigurationError> {
        if record.connection_version != channel.version
            || record.credential_version != channel.credential_version
        {
            return Err(ConfigurationError::StaleConfiguration);
        }
        self.change_facts(channel, |facts| {
            facts.verification.push(record);
        })
    }

    fn change_facts(
        &self,
        channel: &ModelChannel,
        change: impl FnOnce(&mut CatalogueFacts),
    ) -> Result<CatalogueFacts, ConfigurationError> {
        fs::create_dir_all(self.directory()).map_err(|_| ConfigurationError::StorageUnavailable)?;
        let _lock = self.lock()?;
        self.assert_connection_current(channel)?;
        let mut facts = self.catalogue(channel)?;
        change(&mut facts);
        validate_facts(&facts)?;
        let dir = self
            .directory()
            .join("catalogues")
            .join(channel.id.as_str());
        fs::create_dir_all(&dir).map_err(|_| ConfigurationError::StorageUnavailable)?;
        let bytes =
            serde_json::to_vec_pretty(&facts).map_err(|_| ConfigurationError::InvalidDocument)?;
        if bytes.len() > MAX_CACHE_BYTES {
            return Err(ConfigurationError::InvalidDocument);
        }
        let mut temp = tempfile::NamedTempFile::new_in(&dir)
            .map_err(|_| ConfigurationError::StorageUnavailable)?;
        temp.write_all(&bytes)
            .and_then(|()| temp.as_file().sync_all())
            .map_err(|_| ConfigurationError::StorageUnavailable)?;
        self.assert_connection_current(channel)?;
        temp.persist(dir.join(format!("{}.json", cache_key(channel))))
            .map_err(|_| ConfigurationError::StorageUnavailable)?
            .sync_all()
            .map_err(|_| ConfigurationError::CommitUnconfirmed)?;
        #[cfg(unix)]
        File::open(&dir)
            .and_then(|directory| directory.sync_all())
            .map_err(|_| ConfigurationError::CommitUnconfirmed)?;
        if self.catalogue(channel)? != facts {
            return Err(ConfigurationError::CommitUnconfirmed);
        }
        Ok(facts)
    }
}

fn read_facts(path: &std::path::Path) -> Result<CatalogueFacts, ConfigurationError> {
    let meta = match fs::symlink_metadata(path) {
        Ok(meta) => meta,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CatalogueFacts::default());
        }
        Err(_) => return Err(ConfigurationError::StorageUnavailable),
    };
    if !meta.is_file() || meta.file_type().is_symlink() || meta.len() > MAX_CACHE_BYTES as u64 {
        return Err(ConfigurationError::InvalidDocument);
    }
    let mut bytes = Vec::new();
    File::open(path)
        .and_then(|file| {
            file.take((MAX_CACHE_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
        })
        .map_err(|_| ConfigurationError::StorageUnavailable)?;
    if bytes.len() > MAX_CACHE_BYTES {
        return Err(ConfigurationError::InvalidDocument);
    }
    let facts: CatalogueFacts =
        serde_json::from_slice(&bytes).map_err(|_| ConfigurationError::InvalidDocument)?;
    validate_facts(&facts)?;
    Ok(facts)
}

fn validate_facts(facts: &CatalogueFacts) -> Result<(), ConfigurationError> {
    if let Some(directory) = &facts.directory {
        if directory.models.len() > 20000 || directory.source_url.len() > 2048 {
            return Err(ConfigurationError::InvalidDocument);
        }
        let mut ids = std::collections::BTreeSet::new();
        for model in &directory.models {
            validate_model_id(&model.id)?;
            if !ids.insert(&model.id)
                || model.name.len() > 1024
                || model.name.chars().any(char::is_control)
                || model.context_window == Some(0)
                || model.max_output_tokens == Some(0)
                || model
                    .context_source
                    .as_ref()
                    .is_some_and(|source| source.len() > 2048)
            {
                return Err(ConfigurationError::InvalidDocument);
            }
        }
    }
    for record in &facts.verification {
        validate_model_id(&record.model)?;
    }
    Ok(())
}

/// One shared native/adapter capacity contract. User overrides and configured
/// directory facts precede the exact authored metadata in the fixed Pi commit.
pub fn resolve_context(
    channel: &ModelChannel,
    model: &str,
    facts: &CatalogueFacts,
) -> Option<(u32, String)> {
    if let Some(capacity) = channel.context_overrides.get(model) {
        return Some((*capacity, format!("user-conservative:{model}")));
    }
    if let Some(metadata) = facts
        .directory
        .as_ref()
        .filter(|directory| directory.source == CatalogueSource::Official)
        .and_then(|directory| directory.models.iter().find(|entry| entry.id == model))
        && let (Some(capacity), Some(source)) =
            (metadata.context_window, metadata.context_source.as_ref())
    {
        return Some((capacity, source.clone()));
    }
    let provider = match channel.provider_id.as_str() {
        "deepseek" | "openai" | "anthropic" | "google" | "openrouter" => {
            channel.provider_id.as_str()
        }
        "moonshot" => "moonshotai-cn",
        "minimax" => "minimax-cn",
        _ => return None,
    };
    let preset = super::providers::preset(&channel.provider_id)?;
    if channel.protocol != preset.protocol
        || channel.endpoint.trim_end_matches('/') != preset.endpoint
    {
        return None;
    }
    let authored: serde_json::Value = serde_json::from_str(include_str!(
        "../../../../agent-runtime/src/pinned-model-context.json"
    ))
    .ok()?;
    let entry = authored["models"].as_array()?.iter().find(|entry| {
        entry["provider"] == provider
            && entry["model"] == model
            && entry["protocol"] == channel.protocol.as_str()
            && entry["endpoint"] == preset.endpoint
    })?;
    Some((
        u32::try_from(entry["contextWindow"].as_u64()?).ok()?,
        entry["source"].as_str()?.to_owned(),
    ))
}
