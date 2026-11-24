// SPDX-License-Identifier: AGPL-3.0-or-later
// SPDX-FileCopyrightText: 2026 Jareer Abdullah
//! Provider/connection/model settings. Native discovery never needs a project
//! or edit permit; inference and tool probes are separate explicit actions.

use crate::agent::AgentBridge;
use crate::embedded::EmbeddedRuntime;
use crate::host::{self, Shell};
use crate::i18n::{t, tf};
use crate::ui::{AgentSettingsUi, App, ModelChannelDraftData};
use concat_agent::configuration::catalogue::{
    CatalogueFacts, CatalogueSource, ModelChoice, VerificationCapability, VerificationRecord,
};
use concat_agent::configuration::providers::{PROVIDERS, preset};
use concat_agent::configuration::{
    ChannelDraft, ChannelId, ConfigurationError, ConfigurationSnapshot, ConfigurationStore,
    ModelChannel, ModelProtocol, validate_connection_draft,
};
use concat_agent::credentials::{
    CredentialError, CredentialScope, CredentialStatus, CredentialStore, Secret, platform_store,
};
use concat_agent::network::catalogue::{
    CatalogueError, ConfigurationCancellation, now_seconds, refresh_directory, verify_model,
};
use concat_agent::{Phase, SessionOwner};
use concat_api::AppDirs;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};
use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[derive(Default)]
pub struct AgentSettingsPane {
    snapshot: Option<ConfigurationSnapshot>,
    selected: Option<ChannelId>,
    request: u64,
    busy: bool,
    cancellation: Option<Arc<ConfigurationCancellation>>,
    choices: Vec<ModelChoice>,
    visible_choices: Vec<ModelChoice>,
    facts: CatalogueFacts,
    // Stable status keys, or untouched technical details from native failures.
    // Never recover identities by comparing translated display strings.
    error: String,
    message: String,
    credential_status: String,
}

impl AgentSettingsPane {
    fn show_error(&mut self, ui: &AgentSettingsUi, source: String) {
        self.error = source;
        ui.set_error(t(&self.error).into());
    }

    fn project_messages(&self, ui: &AgentSettingsUi) {
        ui.set_error(t(&self.error).into());
        ui.set_message(t(&self.message).into());
        ui.set_credential_status(t(&self.credential_status).into());
    }
}

pub(crate) fn configuration_store(dirs: &AppDirs) -> ConfigurationStore {
    ConfigurationStore::new(dirs.config.join("bk-cat").join("model-settings"))
}
fn config_error(error: ConfigurationError) -> String {
    error.code().to_owned()
}
fn credential_error(error: CredentialError) -> String {
    match error {
        CredentialError::InvalidScope => "modelSettings.credentialInvalidScope",
        CredentialError::InvalidSecret => "modelSettings.credentialInvalidSecret",
        CredentialError::Unsupported => "modelSettings.credentialUnsupported",
        CredentialError::Unavailable => "modelSettings.credentialUnavailable",
        CredentialError::Corrupt => "modelSettings.credentialCorrupt",
    }
    .to_owned()
}
fn scope(channel: &ModelChannel) -> Result<CredentialScope, String> {
    CredentialScope::new(
        channel.id.as_str(),
        &channel.service_origin().map_err(config_error)?,
    )
    .map_err(credential_error)
}
fn timeout_from_ui(value: &str) -> Result<u32, String> {
    let invalid = || "modelSettings.invalidTimeout".to_owned();
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|b| b.is_ascii_digit())
        || fraction.len() > 3
        || !fraction.bytes().all(|b| b.is_ascii_digit())
        || (value.contains('.') && fraction.is_empty())
    {
        return Err(invalid());
    }
    let whole = whole.parse::<u32>().map_err(|_| invalid())?;
    let fraction = if fraction.is_empty() {
        0
    } else {
        fraction.parse::<u32>().map_err(|_| invalid())? * 10_u32.pow(3 - fraction.len() as u32)
    };
    whole
        .checked_mul(1000)
        .and_then(|value| value.checked_add(fraction))
        .ok_or_else(invalid)
}
fn timeout_to_ui(milliseconds: u32) -> String {
    if milliseconds.is_multiple_of(1000) {
        (milliseconds / 1000).to_string()
    } else {
        format!("{}.{:03}", milliseconds / 1000, milliseconds % 1000)
            .trim_end_matches('0')
            .to_owned()
    }
}
fn protocol_index(protocol: ModelProtocol) -> i32 {
    match protocol {
        ModelProtocol::OpenAiCompletions => 1,
        ModelProtocol::OpenAiResponses => 2,
        ModelProtocol::AnthropicMessages => 3,
        ModelProtocol::GoogleGenerativeAi => 4,
    }
}
fn draft_from_ui(draft: &ModelChannelDraftData) -> Result<ChannelDraft, String> {
    let protocol = match draft.protocol {
        1 => ModelProtocol::OpenAiCompletions,
        2 => ModelProtocol::OpenAiResponses,
        3 => ModelProtocol::AnthropicMessages,
        4 => ModelProtocol::GoogleGenerativeAi,
        _ => return Err("modelSettings.unsupportedProtocol".to_owned()),
    };
    validate_connection_draft(ChannelDraft {
        label: draft.label.to_string(),
        protocol,
        endpoint: draft.endpoint.to_string(),
        model: draft.model.to_string(),
        request_timeout_ms: timeout_from_ui(&draft.timeout_seconds)?,
        max_output_tokens: draft
            .max_output_tokens
            .parse::<u32>()
            .map_err(|_| "modelSettings.invalidOutputLimit".to_owned())?,
    })
    .map_err(config_error)
}
fn draft_to_ui(channel: Option<&ModelChannel>) -> ModelChannelDraftData {
    channel.map_or_else(
        || ModelChannelDraftData {
            label: "".into(),
            protocol: 1,
            endpoint: "".into(),
            model: "".into(),
            timeout_seconds: "60".into(),
            max_output_tokens: "4096".into(),
        },
        |channel| ModelChannelDraftData {
            label: channel.label.as_str().into(),
            protocol: protocol_index(channel.protocol),
            endpoint: channel.endpoint.as_str().into(),
            model: channel.model.as_str().into(),
            timeout_seconds: timeout_to_ui(channel.request_timeout_ms).into(),
            max_output_tokens: channel.max_output_tokens.to_string().into(),
        },
    )
}
fn checked_connection(
    snapshot: &ConfigurationSnapshot,
    selected: Option<&ChannelId>,
    draft: &ModelChannelDraftData,
    allow_model_change: bool,
) -> Result<ModelChannel, String> {
    let saved = selected
        .and_then(|id| snapshot.channel(id))
        .ok_or_else(|| "modelSettings.saveFirst".to_owned())?;
    let mut checked = draft_from_ui(draft)?;
    let model = checked.model.clone();
    if allow_model_change {
        checked.model = saved.model.clone();
    }
    if checked != saved.draft() {
        return Err("modelSettings.saveChangesFirst".to_owned());
    }
    let mut selected = saved.clone();
    selected.model = model;
    Ok(selected)
}
fn check_saved_draft(
    snapshot: &ConfigurationSnapshot,
    selected: Option<&ChannelId>,
    draft: &ModelChannelDraftData,
) -> Result<(), String> {
    checked_connection(snapshot, selected, draft, false).map(|_| ())
}

struct Inspection {
    snapshot: Result<ConfigurationSnapshot, String>,
    selected: Option<ChannelId>,
    credential: Option<Result<CredentialStatus, CredentialError>>,
    facts: Result<CatalogueFacts, String>,
    choices: Result<Vec<ModelChoice>, String>,
    model_override: Option<String>,
}
fn inspect(
    store: &ConfigurationStore,
    wanted: Option<ChannelId>,
    choose_existing: bool,
) -> Inspection {
    let snapshot = store.load().map_err(config_error);
    let mut selected = None;
    let mut credential = None;
    let mut facts = Ok(CatalogueFacts::default());
    let mut choices = Ok(Vec::new());
    if let Ok(snapshot) = &snapshot {
        selected = wanted
            .filter(|id| snapshot.channel(id).is_some())
            .or_else(|| {
                choose_existing
                    .then(|| {
                        snapshot
                            .active()
                            .or_else(|| snapshot.channels().first())
                            .map(|c| c.id.clone())
                    })
                    .flatten()
            });
        if let Some(channel) = selected.as_ref().and_then(|id| snapshot.channel(id)) {
            credential = Some(platform_store().and_then(|credentials| {
                let origin = channel
                    .service_origin()
                    .map_err(|_| CredentialError::InvalidScope)?;
                credentials.status(&CredentialScope::new(channel.id.as_str(), &origin)?)
            }));
            facts = store
                .catalogue(channel)
                .and_then(|mut facts| {
                    facts.verification = store.verification_history(channel)?;
                    Ok(facts)
                })
                .map_err(config_error);
        }
        choices = store
            .flattened_models(snapshot)
            .map(|choices| {
                choices
                    .into_iter()
                    .filter(|c| selected.as_ref() == Some(&c.connection))
                    .collect()
            })
            .map_err(config_error);
    }
    Inspection {
        snapshot,
        selected,
        credential,
        facts,
        choices,
        model_override: None,
    }
}
fn string_model(values: Vec<String>) -> ModelRc<SharedString> {
    ModelRc::from(Rc::new(VecModel::from(
        values
            .into_iter()
            .map(SharedString::from)
            .collect::<Vec<_>>(),
    )))
}
fn provider_label(id: &str) -> String {
    match id {
        "deepseek" => t("modelSettings.provider.deepseek"),
        "openai" => t("modelSettings.provider.openai"),
        "anthropic" => t("modelSettings.provider.anthropic"),
        "google" => t("modelSettings.provider.google"),
        "qwen" => t("modelSettings.provider.qwen"),
        "doubao" => t("modelSettings.provider.doubao"),
        "zhipu" => t("modelSettings.provider.zhipu"),
        "moonshot" => t("modelSettings.provider.moonshot"),
        "minimax" => t("modelSettings.provider.minimax"),
        "siliconflow" => t("modelSettings.provider.siliconflow"),
        "openrouter" => t("modelSettings.provider.openrouter"),
        _ => t("modelSettings.provider.custom"),
    }
}
fn region_label(id: &str) -> String {
    match id {
        "global" => t("modelSettings.region.global"),
        "cn-beijing" => t("modelSettings.region.cnBeijing"),
        "cn" => t("modelSettings.region.cn"),
        _ => t("modelSettings.region.custom"),
    }
}
fn project_provider(ui: &AgentSettingsUi, id: &str) {
    let provider = preset(id).unwrap_or_else(|| preset("custom").expect("custom preset"));
    project_provider_labels(ui, provider.id);
    ui.set_provider_index(
        PROVIDERS
            .iter()
            .position(|p| p.id == provider.id)
            .unwrap_or(0) as i32,
    );
    ui.set_custom_provider(provider.id == "custom");
}
fn project_provider_labels(ui: &AgentSettingsUi, id: &str) {
    let provider = preset(id).unwrap_or_else(|| preset("custom").expect("custom preset"));
    ui.set_provider_labels(string_model(
        PROVIDERS.iter().map(|p| provider_label(p.id)).collect(),
    ));
    ui.set_region_label(
        tf(
            "modelSettings.supportedRegion",
            &[&region_label(provider.region)],
        )
        .into(),
    );
    let key = if provider.id == "anthropic" {
        "modelSettings.anthropicProduct"
    } else if provider.id == "custom" {
        "modelSettings.customProduct"
    } else {
        "modelSettings.standardProduct"
    };
    ui.set_provider_help(t(key).into());
}
fn project_choices(pane: &mut AgentSettingsPane, ui: &AgentSettingsUi, query: &str) {
    let query = query.to_lowercase();
    pane.visible_choices = pane
        .choices
        .iter()
        .filter(|choice| {
            query.is_empty()
                || choice.label.to_lowercase().contains(&query)
                || choice.model.to_lowercase().contains(&query)
        })
        .cloned()
        .collect();
    ui.set_model_labels(string_model(
        pane.visible_choices
            .iter()
            .map(|c| c.model.clone())
            .collect(),
    ));
    project_choice_details(&pane.visible_choices, ui);
    let model = ui.get_draft().model;
    ui.set_model_index(-1);
    ui.set_model_index(
        pane.visible_choices
            .iter()
            .position(|c| c.model == model.as_str())
            .map_or(-1, |i| i as i32),
    );
}
fn project_choice_details(choices: &[ModelChoice], ui: &AgentSettingsUi) {
    ui.set_model_details(string_model(
        choices
            .iter()
            .map(|choice| {
                let kind = match choice.source {
                    Some(CatalogueSource::Official) if !choice.stale => {
                        "modelSettings.catalogueOfficial"
                    }
                    Some(CatalogueSource::Official) => "modelSettings.catalogueCached",
                    Some(CatalogueSource::PublicRecommended) => {
                        "modelSettings.catalogueRecommended"
                    }
                    _ => "modelSettings.catalogueManual",
                };
                let source = t(kind);
                let detail = if choice.listed_in_directory == Some(false) {
                    format!("{} · {}", source, t("modelSettings.catalogueNotListed"))
                } else {
                    source
                };
                tf("modelSettings.choiceDetail", &[&choice.label, &detail])
            })
            .collect(),
    ));
}
// Stable UTC calendar representation, independent of platform locale/calendar.
fn utc_timestamp(seconds: u64) -> String {
    if seconds > 253_402_300_799 {
        return t("modelSettings.invalidTime");
    }
    let days = (seconds / 86_400) as i64 + 719_468;
    let era = days / 146_097;
    let day_of_era = days % 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = month_index + if month_index < 10 { 3 } else { -9 };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    let within = seconds % 86_400;
    format!(
        "{year:04}-{month:02}-{day:02} {:02}:{:02}:{:02}",
        within / 3600,
        (within % 3600) / 60,
        within % 60
    )
}

fn project_facts(
    ui: &AgentSettingsUi,
    facts: &CatalogueFacts,
    channel: Option<&ModelChannel>,
    model: &str,
) {
    ui.set_context_capacity(
        channel
            .and_then(|c| c.context_overrides.get(model))
            .map_or_else(String::new, u32::to_string)
            .into(),
    );
    project_fact_labels(ui, facts, channel, model);
}

fn project_fact_labels(
    ui: &AgentSettingsUi,
    facts: &CatalogueFacts,
    channel: Option<&ModelChannel>,
    model: &str,
) {
    let now = now_seconds();
    ui.set_catalogue_status(
        facts
            .directory
            .as_ref()
            .map_or_else(
                || t("modelSettings.catalogueNone"),
                |directory| {
                    let kind = match directory.source {
                        CatalogueSource::Official if directory.is_fresh(now) => {
                            "modelSettings.catalogueOfficial"
                        }
                        CatalogueSource::Official => "modelSettings.catalogueCached",
                        CatalogueSource::PublicRecommended => "modelSettings.catalogueRecommended",
                        CatalogueSource::Manual => "modelSettings.catalogueManual",
                    };
                    let count = directory.models.len();
                    let age = now.saturating_sub(directory.fetched_at) / 60;
                    format!(
                        "{}\n{}\n{}",
                        t(kind),
                        tf("modelSettings.catalogueCountAge", &[&count, &age]),
                        directory.source_url
                    )
                },
            )
            .into(),
    );
    ui.set_refresh_status(
        facts
            .refresh
            .as_ref()
            .map_or_else(
                || t("modelSettings.refreshNever"),
                |refresh| {
                    let outcome = refresh.error_code.as_ref().map_or_else(
                        || t("modelSettings.refreshComplete"),
                        |code| tf("modelSettings.refreshFailed", &[&t(code)]),
                    );
                    tf(
                        "modelSettings.factAt",
                        &[&outcome, &utc_timestamp(refresh.attempted_at)],
                    )
                },
            )
            .into(),
    );
    let verification = |capability| {
        let prefix = t(if capability == VerificationCapability::Inference {
            "modelSettings.inference"
        } else {
            "modelSettings.tools"
        });
        let Some(channel) = channel else {
            return tf(
                "modelSettings.verificationState",
                &[&prefix, &t("modelSettings.notVerified")],
            );
        };
        let summary = facts.verification_summary(channel, model, capability);
        let current = summary.latest_current.map_or_else(
            || t("modelSettings.notVerified"),
            |record| {
                let result = if record.succeeded {
                    t("modelSettings.verified")
                } else {
                    record
                        .error_code
                        .as_ref()
                        .map_or_else(|| t("modelSettings.verificationFailed"), |code| t(code))
                };
                tf(
                    "modelSettings.factAt",
                    &[&result, &utc_timestamp(record.checked_at)],
                )
            },
        );
        let mut state = tf("modelSettings.verificationState", &[&prefix, &current]);
        if let Some(success) = summary.last_success
            && summary.latest_current != Some(success)
        {
            let key = if success.connection_version == channel.version
                && success.credential_version == channel.credential_version
            {
                "modelSettings.lastSuccess"
            } else {
                "modelSettings.historicalSuccess"
            };
            state.push('\n');
            state.push_str(&tf(key, &[&utc_timestamp(success.checked_at)]));
        }
        state
    };
    ui.set_inference_status(verification(VerificationCapability::Inference).into());
    ui.set_tools_status(verification(VerificationCapability::Tools).into());
    ui.set_context_status(
        channel
            .and_then(|channel| {
                concat_agent::configuration::catalogue::resolve_context(channel, model, facts)
            })
            .map(|(capacity, source)| {
                // The stable cache prefix predates custom endpoints. Present
                // the actual provenance without claiming vendor endorsement.
                let source = match source.strip_prefix("official-directory:") {
                    Some(address) => {
                        format!("{} ({address})", t("modelSettings.catalogueOfficial"))
                    }
                    None => source,
                };
                tf("modelSettings.contextKnown", &[&capacity, &source])
            })
            .unwrap_or_else(|| t("modelSettings.contextUnknown"))
            .into(),
    );
}
/// Labels-only projection. In particular, do not set busy/open/can_modify,
/// provider/model indices, the draft, filter results, or the capacity input:
/// any of those can replace a mounted form or overwrite an unfinished edit.
fn retranslate(pane: &AgentSettingsPane, ui: &AgentSettingsUi) {
    let provider = usize::try_from(ui.get_provider_index())
        .ok()
        .and_then(|index| PROVIDERS.get(index))
        .map_or("custom", |provider| provider.id);
    project_provider_labels(ui, provider);
    let mut labels = vec![t("modelSettings.newConnection")];
    if let Some(snapshot) = &pane.snapshot {
        labels.extend(snapshot.channels().iter().map(|channel| {
            format!(
                "{} · {}",
                provider_label(&channel.provider_id),
                channel.label
            )
        }));
        ui.set_active_name(
            snapshot
                .active()
                .map_or_else(
                    || t("modelSettings.noDefault"),
                    |channel| format!("{} · {}", channel.label, channel.model),
                )
                .into(),
        );
    } else {
        ui.set_active_name(t("modelSettings.configurationUnavailable").into());
    }
    ui.set_channel_labels(string_model(labels));
    project_choice_details(&pane.visible_choices, ui);
    let channel = pane
        .snapshot
        .as_ref()
        .and_then(|snapshot| pane.selected.as_ref().and_then(|id| snapshot.channel(id)));
    project_fact_labels(ui, &pane.facts, channel, &ui.get_draft().model);
    pane.project_messages(ui);
}

/// Called after the explicit native language change. Reusing the displayed
/// snapshot avoids credential reads, directory requests and task reconfiguration.
pub(crate) fn retranslate_from_ui() {
    Shell::with(|shell, app| {
        retranslate(
            &shell.agent_settings.borrow(),
            &app.global::<AgentSettingsUi>(),
        );
    });
}

fn publish(
    shell: &Shell,
    app: &App,
    request: u64,
    inspection: Inspection,
    outcome: Result<String, String>,
) {
    {
        let mut pane = shell.agent_settings.borrow_mut();
        if pane.request != request {
            return;
        }
        pane.busy = false;
        pane.cancellation = None;
        let ui = app.global::<AgentSettingsUi>();
        ui.set_busy(false);
        ui.set_cancellable(false);
        let (message, error) = match outcome {
            Ok(message) => (message, String::new()),
            Err(error) => (String::new(), error),
        };
        pane.message = message;
        pane.error = error;
        match inspection.snapshot {
            Ok(snapshot) => {
                pane.selected = inspection.selected;
                let selected = pane.selected.as_ref().and_then(|id| snapshot.channel(id));
                let mut labels = vec![t("modelSettings.newConnection")];
                labels.extend(
                    snapshot
                        .channels()
                        .iter()
                        .map(|c| format!("{} · {}", provider_label(&c.provider_id), c.label)),
                );
                ui.set_channel_labels(string_model(labels));
                ui.set_channel_index(
                    pane.selected
                        .as_ref()
                        .and_then(|id| snapshot.channels().iter().position(|c| &c.id == id))
                        .map_or(0, |i| i as i32 + 1),
                );
                ui.set_has_saved_channel(selected.is_some());
                let mut draft = draft_to_ui(selected);
                if let Some(model) = inspection.model_override.as_ref() {
                    draft.model = model.as_str().into();
                }
                let displayed_model = draft.model.to_string();
                ui.set_draft(draft);
                project_provider(&ui, selected.map_or("custom", |c| c.provider_id.as_str()));
                ui.set_can_modify(true);
                ui.set_configuration_version(snapshot.version().to_string().into());
                pane.credential_status = match inspection.credential {
                    Some(Ok(CredentialStatus::Present)) => "modelSettings.credentialPresent".into(),
                    Some(Ok(CredentialStatus::Missing)) => "modelSettings.credentialMissing".into(),
                    Some(Err(error)) => credential_error(error),
                    None => "modelSettings.credentialNew".into(),
                };
                ui.set_active_name(
                    snapshot
                        .active()
                        .map_or_else(
                            || t("modelSettings.noDefault"),
                            |c| format!("{} · {}", c.label, c.model),
                        )
                        .into(),
                );
                pane.facts = inspection.facts.unwrap_or_else(|error| {
                    pane.error = error;
                    CatalogueFacts::default()
                });
                pane.choices = inspection.choices.unwrap_or_else(|error| {
                    pane.error = error;
                    Vec::new()
                });
                project_facts(&ui, &pane.facts, selected, &displayed_model);
                project_choices(&mut pane, &ui, "");
                pane.snapshot = Some(snapshot);
            }
            Err(error) => {
                pane.snapshot = None;
                pane.selected = None;
                pane.choices.clear();
                ui.set_can_modify(false);
                ui.set_has_saved_channel(false);
                pane.error = error;
                ui.set_active_name(t("modelSettings.configurationUnavailable").into());
                pane.credential_status = "modelSettings.credentialNotRead".into();
            }
        }
        pane.project_messages(&ui);
    }
    // Application defaults never replace the conversation's next-task selection.
    crate::panes::agent::refresh_model_choices();
    if !app.global::<AgentSettingsUi>().get_open() {
        let _ = slint::invoke_from_event_loop(
            crate::embedded::resume_assistant_connection_after_settings,
        );
    }
}
fn fail_ui(message: String) {
    Shell::with(|shell, app| {
        shell
            .agent_settings
            .borrow_mut()
            .show_error(&app.global::<AgentSettingsUi>(), message);
    });
}
fn refresh(wanted: Option<ChannelId>, choose_existing: bool, open: bool) {
    Shell::with(|shell, app| {
        let mut pane = shell.agent_settings.borrow_mut();
        if pane.busy {
            return;
        }
        pane.request += 1;
        let request = pane.request;
        pane.busy = true;
        let ui = app.global::<AgentSettingsUi>();
        if open {
            ui.set_open(true);
        }
        ui.set_busy(true);
        ui.set_cancellable(false);
        pane.show_error(&ui, String::new());
        let store = configuration_store(&shell.studio.borrow().host.dirs);
        host::spawn_detached(move || {
            let inspection = inspect(&store, wanted, choose_existing);
            let stale = open
                && inspection
                    .credential
                    .as_ref()
                    .is_some_and(|status| matches!(status, Ok(CredentialStatus::Present)))
                && inspection.facts.as_ref().is_ok_and(|facts| {
                    facts
                        .directory
                        .as_ref()
                        .is_none_or(|d| !d.is_fresh(now_seconds()))
                });
            let _ = slint::invoke_from_event_loop(move || {
                Shell::with(|shell, app| {
                    publish(
                        shell,
                        &app,
                        request,
                        inspection,
                        Ok("modelSettings.localRead".to_owned()),
                    )
                });
                if stale {
                    configuration_request(None);
                }
            });
        });
    });
}

enum Mutation {
    Save(ChannelDraft),
    SaveConnection {
        draft: ChannelDraft,
        provider: String,
        region: String,
        secret: Option<Secret>,
        load: bool,
    },
    SelectModel(String),
    ContextOverride(String, Option<u32>),
    Delete,
    WriteCredential(Secret),
    DeleteCredential,
}
fn write_credential(
    store: &ConfigurationStore,
    expected: &ConfigurationSnapshot,
    id: &ChannelId,
    credentials: &dyn CredentialStore,
    secret: &Secret,
) -> Result<ConfigurationSnapshot, String> {
    let (next, result) = store
        .credential_transaction(expected, id, |channel| {
            let bound = scope(channel)?;
            credentials
                .write(&bound, secret)
                .map_err(credential_error)?;
            let read = credentials
                .read(&bound)
                .map_err(credential_error)?
                .ok_or_else(|| "modelSettings.credentialReadback".to_owned())?;
            if read.expose_secret() != secret.expose_secret() {
                return Err("modelSettings.credentialReadback".to_owned());
            }
            Ok(())
        })
        .map_err(config_error)?;
    result?;
    Ok(next)
}
fn apply_mutation(
    store: &ConfigurationStore,
    expected: &ConfigurationSnapshot,
    selected: Option<&ChannelId>,
    mutation: Mutation,
    credentials: Result<Arc<dyn CredentialStore>, CredentialError>,
) -> Result<(ConfigurationSnapshot, Option<ChannelId>, String), String> {
    store.assert_current(expected).map_err(config_error)?;
    match mutation {
        Mutation::Save(draft) => {
            let next = store
                .save_channel(expected, selected, draft)
                .map_err(config_error)?;
            let selected = selected
                .cloned()
                .or_else(|| next.channels().last().map(|c| c.id.clone()));
            Ok((
                next,
                selected,
                "modelSettings.configurationSaved".to_owned(),
            ))
        }
        Mutation::SaveConnection {
            draft,
            provider,
            region,
            secret,
            ..
        } => {
            let mut next = store
                .save_connection(expected, selected, draft, &provider, &region)
                .map_err(config_error)?;
            let id = selected
                .cloned()
                .or_else(|| next.channels().last().map(|c| c.id.clone()))
                .ok_or_else(|| "modelSettings.missingChannel".to_owned())?;
            if let Some(secret) = secret {
                let credentials = credentials.as_deref().map_err(|e| credential_error(*e))?;
                next = write_credential(store, &next, &id, credentials, &secret)?;
            }
            Ok((
                next,
                Some(id),
                "modelSettings.configurationSaved".to_owned(),
            ))
        }
        Mutation::SelectModel(model) => {
            let id = selected.ok_or_else(|| "modelSettings.saveFirst".to_owned())?;
            let next = store
                .select_model(expected, id, &model)
                .map_err(config_error)?;
            Ok((
                next,
                Some(id.clone()),
                "modelSettings.defaultSaved".to_owned(),
            ))
        }
        Mutation::ContextOverride(model, capacity) => {
            let id = selected.ok_or_else(|| "modelSettings.saveFirst".to_owned())?;
            let next = store
                .set_context_override(expected, id, &model, capacity)
                .map_err(config_error)?;
            Ok((
                next,
                Some(id.clone()),
                "modelSettings.capacitySaved".to_owned(),
            ))
        }
        Mutation::Delete => {
            let credentials = credentials.as_deref().map_err(|e| credential_error(*e))?;
            let id = selected.ok_or_else(|| "modelSettings.saveFirst".to_owned())?;
            let (next, result) = store
                .delete_with_credential(expected, id, |channel| {
                    let bound = scope(channel)?;
                    credentials.delete(&bound).map_err(credential_error)?;
                    if credentials.status(&bound).map_err(credential_error)?
                        != CredentialStatus::Missing
                    {
                        return Err("modelSettings.credentialStillPresent".to_owned());
                    }
                    Ok(())
                })
                .map_err(config_error)?;
            result?;
            Ok((next, None, "modelSettings.connectionDeleted".to_owned()))
        }
        Mutation::WriteCredential(secret) => {
            let credentials = credentials.as_deref().map_err(|e| credential_error(*e))?;
            let id = selected.ok_or_else(|| "modelSettings.saveFirst".to_owned())?;
            let next = write_credential(store, expected, id, credentials, &secret)?;
            Ok((
                next,
                Some(id.clone()),
                "modelSettings.credentialSaved".to_owned(),
            ))
        }
        Mutation::DeleteCredential => {
            let credentials = credentials.as_deref().map_err(|e| credential_error(*e))?;
            let id = selected.ok_or_else(|| "modelSettings.saveFirst".to_owned())?;
            let (next, result) = store
                .credential_transaction(expected, id, |channel| {
                    let bound = scope(channel)?;
                    credentials.delete(&bound).map_err(credential_error)?;
                    if credentials.status(&bound).map_err(credential_error)?
                        != CredentialStatus::Missing
                    {
                        return Err("modelSettings.credentialStillPresent".to_owned());
                    }
                    Ok(())
                })
                .map_err(config_error)?;
            result?;
            Ok((
                next,
                Some(id.clone()),
                "modelSettings.credentialDeleted".to_owned(),
            ))
        }
    }
}
/// Only changing the connection captured by an executing task retires its host.
fn maintenance_owner(
    dirs: &AppDirs,
    bridge: Option<Arc<AgentBridge>>,
    runtime: Option<Arc<EmbeddedRuntime>>,
) -> Result<SessionOwner, String> {
    if let Some(runtime) = runtime {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let released = bridge
                .as_ref()
                .map(|b| b.status())
                .transpose()?
                .is_none_or(|s| s.phase == Phase::Released);
            if runtime.view().closed && released {
                break;
            }
            if Instant::now() >= deadline {
                return Err("modelSettings.runtimeNotClosed".to_owned());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    } else if let Some(bridge) = bridge {
        bridge.finish_handoff(Duration::from_secs(10))?;
    }
    let storage = dirs.data.join("bk-cat").join("agent-session");
    std::fs::create_dir_all(&storage).map_err(|_| "modelSettings.sessionStorage".to_owned())?;
    SessionOwner::acquire(storage).map_err(|_| "modelSettings.sessionBusy".to_owned())
}
fn mutate(mutation: Mutation, supplied: Option<ModelChannelDraftData>) {
    Shell::with(|shell, app| {
        let mut pane = shell.agent_settings.borrow_mut();
        let ui = app.global::<AgentSettingsUi>();
        if pane.busy {
            return;
        }
        let Some(expected) = pane.snapshot.clone() else {
            pane.show_error(&ui, "modelSettings.readFirst".to_owned());
            return;
        };
        let selected = pane.selected.clone();
        if let Some(supplied) = supplied {
            let allow_model = matches!(
                &mutation,
                Mutation::SelectModel(_) | Mutation::ContextOverride(_, _)
            );
            if let Err(error) =
                checked_connection(&expected, selected.as_ref(), &supplied, allow_model)
            {
                pane.show_error(&ui, error);
                return;
            }
        }
        if !matches!(
            &mutation,
            Mutation::Save(_) | Mutation::SaveConnection { .. }
        ) && selected.is_none()
        {
            pane.show_error(&ui, "modelSettings.saveFirst".to_owned());
            return;
        }
        let load = matches!(&mutation, Mutation::SaveConnection { load: true, .. });
        let new_connection = selected.is_none()
            && matches!(
                &mutation,
                Mutation::Save(_) | Mutation::SaveConnection { .. }
            );
        let configuration_changes = match &mutation {
            Mutation::SelectModel(_) => false,
            Mutation::Save(draft) | Mutation::SaveConnection { draft, .. } => {
                let saved = selected.as_ref().and_then(|id| expected.channel(id));
                saved.is_some_and(|saved| {
                    saved.label != draft.label
                        || saved.endpoint != draft.endpoint
                        || saved.protocol != draft.protocol
                        || saved.request_timeout_ms != draft.request_timeout_ms
                        || saved.max_output_tokens != draft.max_output_tokens
                }) || matches!(
                    &mutation,
                    Mutation::SaveConnection {
                        secret: Some(_),
                        ..
                    }
                ) || matches!(&mutation, Mutation::SaveConnection { provider, region, .. }
                    if saved.is_some_and(|saved| &saved.provider_id != provider || &saved.region != region))
            }
            _ => true,
        };
        let affects_task = configuration_changes
            && selected.as_ref().is_some_and(|id| {
                shell.agent_pane.borrow().task_connection_id() == Some(id.as_str())
            });
        let dirs = shell.studio.borrow().host.dirs.clone();
        let store = configuration_store(&dirs);
        let bridge = affects_task.then(|| shell.agent.borrow().clone()).flatten();
        let runtime = affects_task
            .then(|| shell.embedded.borrow().clone())
            .flatten();
        if let Some(bridge) = &bridge {
            bridge.stop();
        }
        if let Some(runtime) = &runtime {
            runtime.stop();
        }
        if affects_task {
            shell.agent_pane.borrow_mut().readiness.calls_authorized = false;
        }
        pane.request += 1;
        let request = pane.request;
        pane.busy = true;
        app.global::<AgentSettingsUi>().set_busy(true);
        app.global::<AgentSettingsUi>().set_cancellable(false);
        pane.show_error(&ui, String::new());
        host::spawn_detached(move || {
            let outcome = (|| {
                let owner = if affects_task {
                    Some(maintenance_owner(&dirs, bridge, runtime)?)
                } else {
                    None
                };
                let result = apply_mutation(
                    &store,
                    &expected,
                    selected.as_ref(),
                    mutation,
                    platform_store(),
                );
                if let Some(owner) = owner {
                    owner
                        .finish_handoff(Duration::ZERO)
                        .map_err(|_| "modelSettings.sessionRelease".to_owned())?;
                }
                result
            })();
            let chosen = outcome
                .as_ref()
                .ok()
                .and_then(|(_, id, _)| id.clone())
                .or_else(|| selected.clone())
                .or_else(|| {
                    new_connection
                        .then(|| {
                            store
                                .load()
                                .ok()
                                .and_then(|s| s.channels().last().map(|c| c.id.clone()))
                        })
                        .flatten()
                });
            let succeeded = outcome.is_ok();
            let inspection = inspect(&store, chosen, false);
            let message = outcome.map(|(_, _, message)| message);
            let _ = slint::invoke_from_event_loop(move || {
                Shell::with(|shell, app| publish(shell, &app, request, inspection, message));
                if succeeded && load {
                    configuration_request(None);
                }
            });
        });
    });
}
fn configuration_request(capability: Option<VerificationCapability>) {
    Shell::with(|shell, app| {
        let mut pane = shell.agent_settings.borrow_mut();
        if pane.busy {
            return;
        }
        let ui = app.global::<AgentSettingsUi>();
        let Some(snapshot) = pane.snapshot.clone() else {
            pane.show_error(&ui, "modelSettings.readFirst".to_owned());
            return;
        };
        let selected = pane.selected.clone();
        let channel = match checked_connection(&snapshot, selected.as_ref(), &ui.get_draft(), true)
        {
            Ok(channel) => channel,
            Err(error) => {
                pane.show_error(&ui, error);
                return;
            }
        };
        if PROVIDERS
            .get(ui.get_provider_index() as usize)
            .is_none_or(|p| p.id != channel.provider_id)
        {
            pane.show_error(&ui, "modelSettings.saveChangesFirst".to_owned());
            return;
        }
        if capability.is_some() && channel.model.is_empty() {
            pane.show_error(&ui, "modelSettings.invalidModel".to_owned());
            return;
        }
        pane.request += 1;
        let request = pane.request;
        pane.busy = true;
        let cancellation = Arc::new(ConfigurationCancellation::default());
        pane.cancellation = Some(Arc::clone(&cancellation));
        ui.set_busy(true);
        ui.set_cancellable(true);
        pane.show_error(&ui, String::new());
        let store = configuration_store(&shell.studio.borrow().host.dirs);
        host::spawn_detached(move || {
            let outcome = (|| {
                let credentials = platform_store().map_err(|_| CatalogueError::Credential);
                if let Some(capability) = capability {
                    let result = credentials
                        .as_ref()
                        .map_err(|e| *e)
                        .and_then(|credentials| {
                            verify_model(
                                &store,
                                &channel,
                                capability,
                                credentials.as_ref(),
                                &cancellation,
                            )
                        });
                    store
                        .record_verification(
                            &channel,
                            VerificationRecord {
                                connection_version: channel.version,
                                credential_version: channel.credential_version,
                                model: channel.model.clone(),
                                capability,
                                checked_at: now_seconds(),
                                succeeded: result.is_ok(),
                                error_code: result.as_ref().err().map(|e| e.code().into()),
                            },
                        )
                        .map_err(config_error)?;
                    result.map_err(|e| e.code().to_owned())?;
                    Ok("modelSettings.verificationComplete".to_owned())
                } else {
                    credentials
                        .as_ref()
                        .map_err(|e| *e)
                        .and_then(|credentials| {
                            refresh_directory(&store, &channel, credentials.as_ref(), &cancellation)
                        })
                        .map_err(|e| e.code().to_owned())?;
                    Ok("modelSettings.directoryComplete".to_owned())
                }
            })();
            let mut inspection = inspect(&store, selected, false);
            inspection.model_override = Some(channel.model.clone());
            let _ = slint::invoke_from_event_loop(move || {
                Shell::with(|shell, app| publish(shell, &app, request, inspection, outcome))
            });
        });
    });
}
fn save(draft: ModelChannelDraftData, text: Option<SharedString>, load: bool) {
    let draft = match draft_from_ui(&draft) {
        Ok(draft) => draft,
        Err(error) => {
            fail_ui(error);
            return;
        }
    };
    let secret = match text.filter(|s| !s.is_empty()) {
        Some(text) => match Secret::new(text.to_string()) {
            Ok(secret) => Some(secret),
            Err(error) => {
                fail_ui(credential_error(error));
                return;
            }
        },
        None => None,
    };
    let mut provider = None;
    Shell::with(|_, app| {
        provider = PROVIDERS.get(app.global::<AgentSettingsUi>().get_provider_index() as usize);
    });
    let Some(provider) = provider else {
        fail_ui("modelSettings.unsupportedProtocol".to_owned());
        return;
    };
    mutate(
        Mutation::SaveConnection {
            draft,
            provider: provider.id.into(),
            region: provider.region.into(),
            secret,
            load,
        },
        None,
    );
}

/// Called only when a user opens the conversation's model picker. This does
/// configuration discovery, never project reads, inference or task admission.
pub(crate) fn refresh_directory_for_picker(connection_id: &str) {
    let Ok(id) = ChannelId::parse(connection_id) else {
        return;
    };
    Shell::with(|shell, _| {
        let store = configuration_store(&shell.studio.borrow().host.dirs);
        host::spawn_detached(move || {
            let outcome = (|| {
                let snapshot = store.load().map_err(config_error)?;
                let channel = snapshot
                    .channel(&id)
                    .ok_or_else(|| "modelSettings.missingChannel".to_owned())?;
                let facts = store.catalogue(channel).map_err(config_error)?;
                if facts
                    .directory
                    .as_ref()
                    .is_some_and(|directory| directory.is_fresh(now_seconds()))
                {
                    return Ok(());
                }
                let credentials = platform_store().map_err(credential_error)?;
                let result =
                    refresh_directory(&store, channel, credentials.as_ref(), &Arc::default());
                // Another visible refresh already owns the connection; do not
                // overwrite its latest result with an unattempted duplicate.
                if result.as_ref().err() == Some(&CatalogueError::Busy) {
                    return Ok(());
                }
                result.map_err(|error| error.code().to_owned())?;
                Ok::<_, String>(())
            })();
            let _ = slint::invoke_from_event_loop(move || {
                if let Err(error) = outcome {
                    fail_ui(error);
                }
                crate::panes::agent::refresh_model_choices();
            });
        });
    });
}

pub(crate) fn install(app: &App) {
    let ui = app.global::<AgentSettingsUi>();
    ui.on_open_requested(|| refresh(None, true, true));
    ui.on_close_requested(|| {
        Shell::with(|shell, app| {
            let pane = shell.agent_settings.borrow();
            if let Some(cancel) = &pane.cancellation {
                cancel.cancel();
            }
            if !pane.busy || pane.cancellation.is_some() {
                app.global::<AgentSettingsUi>().set_open(false);
            }
        });
        crate::embedded::resume_assistant_connection_after_settings();
    });
    ui.on_cancel_requested(|| {
        Shell::with(|shell, _| {
            if let Some(cancel) = &shell.agent_settings.borrow().cancellation {
                cancel.cancel();
            }
        })
    });
    ui.on_reload_requested(|| refresh(None, true, false));
    ui.on_channel_chosen(|index| {
        let mut selected = None;
        Shell::with(|shell, _| {
            if index > 0 {
                selected = shell
                    .agent_settings
                    .borrow()
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.channels().get((index - 1) as usize))
                    .map(|c| c.id.clone());
            }
        });
        refresh(selected, false, true);
    });
    ui.on_provider_chosen(|index| {
        let Some(provider) = usize::try_from(index).ok().and_then(|i| PROVIDERS.get(i)) else {
            return;
        };
        Shell::with(|_, app| {
            let ui = app.global::<AgentSettingsUi>();
            let mut draft = ui.get_draft();
            project_provider(&ui, provider.id);
            draft.protocol = protocol_index(provider.protocol);
            draft.endpoint = provider.endpoint.into();
            draft.model = "".into();
            if draft.label.is_empty() {
                draft.label = provider_label(provider.id).into();
            }
            ui.set_draft(draft);
            ui.set_model_index(-1);
        });
    });
    ui.on_model_search_changed(|text| {
        Shell::with(|shell, app| {
            project_choices(
                &mut shell.agent_settings.borrow_mut(),
                &app.global::<AgentSettingsUi>(),
                &text,
            )
        })
    });
    ui.on_manual_model_changed(|model| {
        Shell::with(|shell, app| {
            let pane = shell.agent_settings.borrow();
            let ui = app.global::<AgentSettingsUi>();
            let mut draft = ui.get_draft();
            draft.model = model.clone();
            ui.set_draft(draft);
            let channel = pane
                .snapshot
                .as_ref()
                .and_then(|s| pane.selected.as_ref().and_then(|id| s.channel(id)));
            project_facts(&ui, &pane.facts, channel, &model);
        })
    });
    ui.on_model_chosen(|index| {
        Shell::with(|shell, app| {
            let pane = shell.agent_settings.borrow();
            let Some(choice) = usize::try_from(index)
                .ok()
                .and_then(|i| pane.visible_choices.get(i))
            else {
                return;
            };
            let ui = app.global::<AgentSettingsUi>();
            let mut draft = ui.get_draft();
            draft.model = choice.model.as_str().into();
            ui.set_draft(draft);
            let channel = pane
                .snapshot
                .as_ref()
                .and_then(|s| pane.selected.as_ref().and_then(|id| s.channel(id)));
            project_facts(&ui, &pane.facts, channel, &choice.model);
        })
    });
    ui.on_save_requested(|draft| save(draft, None, false));
    ui.on_save_load_requested(|draft, secret| save(draft, Some(secret), true));
    ui.on_refresh_models_requested(|_| configuration_request(None));
    ui.on_activate_requested(|draft| {
        mutate(Mutation::SelectModel(draft.model.to_string()), Some(draft))
    });
    ui.on_delete_requested(|draft| mutate(Mutation::Delete, Some(draft)));
    ui.on_credential_save_requested(|draft, text| match Secret::new(text.to_string()) {
        Ok(secret) => mutate(Mutation::WriteCredential(secret), Some(draft)),
        Err(error) => fail_ui(credential_error(error)),
    });
    ui.on_credential_delete_requested(|draft| mutate(Mutation::DeleteCredential, Some(draft)));
    ui.on_context_save_requested(|draft, value| {
        let capacity = if value.is_empty() {
            None
        } else {
            match value.parse::<u32>() {
                Ok(capacity) if (1..=100_000_000).contains(&capacity) => Some(capacity),
                _ => {
                    fail_ui("modelSettings.invalidCapacity".to_owned());
                    return;
                }
            }
        };
        mutate(
            Mutation::ContextOverride(draft.model.to_string(), capacity),
            Some(draft),
        );
    });
    ui.on_verify_requested(|_, tools| {
        configuration_request(Some(if tools {
            VerificationCapability::Tools
        } else {
            VerificationCapability::Inference
        }))
    });
    refresh(None, true, false);
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Read/write the actual mounted password item, not a duplicate form model.
    fn password_text(app: &App, set: Option<&str>) -> String {
        use slint::private_unstable_api::re_exports::{
            AccessibleStringProperty, TextInput, WindowInner,
        };
        app.window().take_snapshot().unwrap();
        let root = WindowInner::from_pub(app.window())
            .window_item_rc()
            .unwrap();
        let label = t("modelSettings.credential");
        let mut values = Vec::new();
        root.visit_descendants::<()>(|item| {
            if item.is_visible()
                && item
                    .accessible_string_property(AccessibleStringProperty::Label)
                    .is_some_and(|value| value == label)
                && let Some(input) = item.downcast::<TextInput>()
            {
                let text = TextInput::FIELD_OFFSETS
                    .text()
                    .apply_pin(input.as_pin_ref());
                if let Some(value) = set {
                    text.set(value.into());
                }
                values.push(text.get().to_string());
            }
            std::ops::ControlFlow::Continue(())
        });
        assert_eq!(values.len(), 1, "one current production password field");
        values.remove(0)
    }

    fn click_settings_button(app: &App, label: &str) {
        use slint::platform::{PointerEventButton, WindowEvent};
        use slint::private_unstable_api::re_exports::{
            AccessibleRole, AccessibleStringProperty, WindowInner,
        };
        app.window().take_snapshot().unwrap();
        let root = WindowInner::from_pub(app.window())
            .window_item_rc()
            .unwrap();
        let mut points = Vec::new();
        root.visit_descendants::<()>(|item| {
            if item.is_visible()
                && item.accessible_role() == AccessibleRole::Button
                && item
                    .accessible_string_property(AccessibleStringProperty::Label)
                    .is_some_and(|value| value == label)
            {
                let point = item.map_to_window(item.geometry().center());
                points.push(slint::LogicalPosition::new(point.x, point.y));
            }
            std::ops::ControlFlow::Continue(())
        });
        assert_eq!(points.len(), 1, "one current settings button: {label}");
        let position = points[0];
        let viewport = app.window().size();
        let scale = app.window().scale_factor();
        assert!(
            position.x >= 0.
                && position.y >= 0.
                && position.x < viewport.width as f32 / scale
                && position.y < viewport.height as f32 / scale
        );
        app.window()
            .dispatch_event(WindowEvent::PointerMoved { position });
        app.window().dispatch_event(WindowEvent::PointerPressed {
            position,
            button: PointerEventButton::Left,
        });
        app.window().dispatch_event(WindowEvent::PointerReleased {
            position,
            button: PointerEventButton::Left,
        });
    }

    #[test]
    fn language_reprojection_preserves_mounted_secret_draft_filter_and_inflight_selection() {
        std::thread::Builder::new()
            .stack_size(32 << 20)
            .spawn(|| {
                use slint::Model;
                use slint::platform::software_renderer::{
                    MinimalSoftwareWindow, RepaintBufferType,
                };
                let _language = crate::i18n::language_lock();
                let (directory, store, snapshot, id) = fixture();
                let dirs = AppDirs::under(&directory.0.join("ui-only"));
                struct RestoreLanguage(String, AppDirs);
                impl Drop for RestoreLanguage {
                    fn drop(&mut self) {
                        crate::i18n::select(&self.0, &self.1);
                    }
                }
                let _restore = RestoreLanguage(crate::i18n::current(), dirs.clone());
                crate::i18n::select("en", &dirs);
                slint::platform::set_platform(Box::new(
                    crate::agent::window::tests::NativeTestPlatform(MinimalSoftwareWindow::new(
                        RepaintBufferType::NewBuffer,
                    )),
                ))
                .unwrap();
                let app = App::new().unwrap();
                crate::i18n::install(&app);
                app.set_phone(false);
                app.set_on_start(false);
                app.window().set_size(slint::LogicalSize::new(1400., 2400.));
                let ui = app.global::<AgentSettingsUi>();
                ui.set_open(true);
                ui.set_can_modify(true);
                ui.set_has_saved_channel(true);
                project_provider(&ui, "custom");
                ui.set_channel_index(1);
                let mut draft = draft_to_ui(snapshot.channel(&id));
                draft.label = "User label 用户内容".into();
                draft.endpoint = "https://unsaved.example.test/custom/v1".into();
                draft.timeout_seconds = "12.".into();
                ui.set_draft(draft.clone());
                ui.set_context_capacity("42.".into());
                let mut pane = AgentSettingsPane {
                    snapshot: Some(snapshot.clone()),
                    selected: Some(id.clone()),
                    request: 73,
                    choices: vec![
                        ModelChoice {
                            connection: id.clone(),
                            model: "test-model".into(),
                            label: "user label".into(),
                            source: Some(CatalogueSource::Manual),
                            stale: true,
                            listed_in_directory: None,
                        },
                        ModelChoice {
                            connection: id.clone(),
                            model: "other-model".into(),
                            label: "another label".into(),
                            source: Some(CatalogueSource::Manual),
                            stale: true,
                            listed_in_directory: None,
                        },
                    ],
                    error: "agent.errorAuthentication".into(),
                    message: "modelSettings.configurationSaved".into(),
                    credential_status: "modelSettings.credentialPresent".into(),
                    ..Default::default()
                };
                project_choices(&mut pane, &ui, "test-model");
                let selected = (
                    ui.get_channel_index(),
                    ui.get_provider_index(),
                    ui.get_model_index(),
                );
                let filtered = pane.visible_choices.clone();
                let choices = pane.choices.clone();
                let facts = pane.facts.clone();
                retranslate(&pane, &ui);
                let english = (
                    ui.get_channel_labels().row_data(0).unwrap(),
                    ui.get_error(),
                    ui.get_message(),
                    ui.get_credential_status(),
                    ui.get_catalogue_status(),
                    ui.get_refresh_status(),
                    ui.get_inference_status(),
                    ui.get_active_name(),
                    ui.get_model_details().row_data(0).unwrap(),
                );
                let configuration_before = std::fs::read(
                    directory
                        .0
                        .join(concat_agent::configuration::CONFIGURATION_FILE),
                )
                .unwrap();
                let delivered = Rc::new(std::cell::RefCell::new(Vec::new()));
                let received = Rc::clone(&delivered);
                ui.on_save_load_requested(move |draft, secret| {
                    received.borrow_mut().push((draft, secret))
                });
                let callbacks = Rc::new(std::cell::Cell::new(0));
                let changed = Rc::clone(&callbacks);
                ui.on_provider_chosen(move |_| changed.set(changed.get() + 1));
                let changed = Rc::clone(&callbacks);
                ui.on_model_chosen(move |_| changed.set(changed.get() + 1));
                app.show().unwrap();
                const TYPED: &str = "isolated-unsaved-password-not-a-real-key";
                assert_eq!(password_text(&app, Some(TYPED)), TYPED);

                crate::i18n::select("zh-Hans", &dirs);
                app.global::<crate::ui::I18n>().set_lang("zh-Hans".into());
                retranslate(&pane, &ui);
                assert_eq!(
                    password_text(&app, None),
                    TYPED,
                    "language change must not recreate the password control"
                );
                assert_eq!(ui.get_draft(), draft);
                assert_eq!(ui.get_context_capacity(), "42.");
                assert_eq!(
                    (
                        ui.get_channel_index(),
                        ui.get_provider_index(),
                        ui.get_model_index()
                    ),
                    selected
                );
                assert_eq!(pane.visible_choices, filtered);
                assert_eq!(pane.choices, choices);
                assert_eq!(pane.facts, facts);
                assert_eq!(pane.snapshot.as_ref(), Some(&snapshot));
                assert_eq!(pane.selected.as_ref(), Some(&id));
                assert_eq!(pane.request, 73);
                assert!(!pane.busy && !ui.get_busy() && ui.get_open() && ui.get_can_modify());
                assert_eq!(
                    callbacks.get(),
                    0,
                    "translated rows must not invoke a model/provider selection callback"
                );
                let chinese = (
                    ui.get_channel_labels().row_data(0).unwrap(),
                    ui.get_error(),
                    ui.get_message(),
                    ui.get_credential_status(),
                    ui.get_catalogue_status(),
                    ui.get_refresh_status(),
                    ui.get_inference_status(),
                    ui.get_active_name(),
                    ui.get_model_details().row_data(0).unwrap(),
                );
                assert_ne!(english.0, chinese.0);
                assert_ne!(english.1, chinese.1);
                assert_ne!(english.2, chinese.2);
                assert_ne!(english.3, chinese.3);
                assert_ne!(english.4, chinese.4);
                assert_ne!(english.5, chinese.5);
                assert_ne!(english.6, chinese.6);
                assert_ne!(english.7, chinese.7);
                assert_ne!(english.8, chinese.8);
                assert_eq!(ui.get_error(), t("agent.errorAuthentication"));
                click_settings_button(&app, &t("modelSettings.saveLoad"));
                assert_eq!(delivered.borrow().len(), 1);
                assert_eq!(
                    delivered.borrow()[0],
                    (draft.clone(), SharedString::from(TYPED))
                );

                let cancellation = Arc::new(ConfigurationCancellation::default());
                pane.busy = true;
                pane.cancellation = Some(Arc::clone(&cancellation));
                ui.set_busy(true);
                ui.set_cancellable(true);
                crate::i18n::select("en", &dirs);
                app.global::<crate::ui::I18n>().set_lang("en".into());
                retranslate(&pane, &ui);
                assert!(pane.busy && ui.get_busy() && ui.get_cancellable());
                assert_eq!(pane.request, 73);
                assert!(Arc::ptr_eq(
                    pane.cancellation.as_ref().unwrap(),
                    &cancellation
                ));
                assert!(!cancellation.cancelled());
                assert_eq!(ui.get_draft(), draft);
                assert_eq!(
                    (
                        ui.get_channel_index(),
                        ui.get_provider_index(),
                        ui.get_model_index()
                    ),
                    selected
                );
                assert_eq!(store.load().unwrap(), snapshot);
                assert_eq!(
                    std::fs::read(
                        directory
                            .0
                            .join(concat_agent::configuration::CONFIGURATION_FILE)
                    )
                    .unwrap(),
                    configuration_before
                );
                app.hide().unwrap();
            })
            .unwrap()
            .join()
            .unwrap();
    }

    #[test]
    fn verification_times_have_unambiguous_calendar_dates() {
        assert_eq!(utc_timestamp(0), "1970-01-01 00:00:00");
        assert_eq!(utc_timestamp(951_827_696), "2000-02-29 12:34:56");
        assert_eq!(utc_timestamp(1_798_113_600), "2026-12-24 12:00:00");
        assert_eq!(utc_timestamp(253_402_300_799), "9999-12-31 23:59:59");
    }

    struct Directory(std::path::PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (
        Directory,
        ConfigurationStore,
        ConfigurationSnapshot,
        ChannelId,
    ) {
        let directory = Directory(std::env::temp_dir().join(format!(
            "bk-cat-native-settings-test-{}",
            crate::embedded::random_capability().unwrap()
        )));
        std::fs::create_dir(&directory.0).unwrap();
        let store = ConfigurationStore::new(&directory.0);
        let snapshot = store
            .save_channel(
                &store.load().unwrap(),
                None,
                ChannelDraft {
                    label: "Isolated native test".into(),
                    protocol: ModelProtocol::OpenAiResponses,
                    endpoint: "https://models.example.test/team/v1".into(),
                    model: "test-model".into(),
                    request_timeout_ms: 60_000,
                    max_output_tokens: 4096,
                },
            )
            .unwrap();
        let id = snapshot.channels()[0].id.clone();
        (directory, store, snapshot, id)
    }

    #[derive(Default)]
    struct RefusingCredentials {
        writes: AtomicUsize,
        deletes: AtomicUsize,
    }
    impl CredentialStore for RefusingCredentials {
        fn read(&self, _: &CredentialScope) -> Result<Option<Secret>, CredentialError> {
            Err(CredentialError::Unavailable)
        }
        fn write(&self, _: &CredentialScope, _: &Secret) -> Result<(), CredentialError> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            Err(CredentialError::Unavailable)
        }
        fn delete(&self, _: &CredentialScope) -> Result<(), CredentialError> {
            self.deletes.fetch_add(1, Ordering::Relaxed);
            Err(CredentialError::Unavailable)
        }
    }

    #[test]
    fn saved_millisecond_timeout_roundtrips_through_the_native_seconds_form() {
        let (_directory, _store, snapshot, id) = fixture();
        let mut channel = snapshot.channel(&id).unwrap().clone();
        for milliseconds in [1000, 1001, 1050, 1250, 59_999, 599_999, 600_000] {
            channel.request_timeout_ms = milliseconds;
            let form = draft_to_ui(Some(&channel));
            assert_eq!(draft_from_ui(&form).unwrap(), channel.draft());
        }
        for invalid in [
            "",
            "1.",
            ".5",
            "1.0001",
            "1e2",
            "-1",
            "1..2",
            " 1",
            "999999999999",
        ] {
            assert!(timeout_from_ui(invalid).is_err());
        }
        for outside in ["0.999", "600.001"] {
            let mut form = draft_to_ui(Some(&channel));
            form.timeout_seconds = outside.into();
            assert!(draft_from_ui(&form).is_err());
        }
    }

    #[test]
    fn unsaved_destination_or_model_cannot_mutate_the_previous_credential_scope() {
        let (_directory, _store, snapshot, id) = fixture();
        let saved = snapshot.channel(&id).unwrap();
        let exact = draft_to_ui(Some(saved));
        assert!(check_saved_draft(&snapshot, Some(&id), &exact).is_ok());
        let mut edited = exact.clone();
        edited.endpoint = "https://models.example.test/other/v1".into();
        assert!(check_saved_draft(&snapshot, Some(&id), &edited).is_err());
        let mut edited = exact;
        edited.model = "different-model".into();
        assert!(check_saved_draft(&snapshot, Some(&id), &edited).is_err());
    }

    #[test]
    fn stale_native_settings_reject_a_secret_before_the_backend_is_touched() {
        let (_directory, store, snapshot, id) = fixture();
        store.credential_changed(&snapshot, &id).unwrap();
        let backend = Arc::new(RefusingCredentials::default());
        let result = apply_mutation(
            &store,
            &snapshot,
            Some(&id),
            Mutation::WriteCredential(Secret::new("isolated-not-real".into()).unwrap()),
            Ok(backend.clone()),
        );
        assert!(result.is_err());
        assert_eq!(backend.writes.load(Ordering::Relaxed), 0);
        assert_eq!(backend.deletes.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn secure_delete_failure_keeps_the_channel_available_for_an_explicit_retry() {
        let (_directory, store, snapshot, id) = fixture();
        let backend = Arc::new(RefusingCredentials::default());
        assert!(
            apply_mutation(
                &store,
                &snapshot,
                Some(&id),
                Mutation::Delete,
                Ok(backend.clone())
            )
            .is_err()
        );
        assert_eq!(backend.deletes.load(Ordering::Relaxed), 1);
        let retained = store.load().unwrap();
        assert!(retained.channel(&id).is_some());
        assert!(retained.version() > snapshot.version());
        assert!(store.assert_current(&snapshot).is_err());
    }

    #[test]
    fn failed_secure_write_invalidates_old_configuration_but_persists_no_secret() {
        let (directory, store, snapshot, id) = fixture();
        let backend = Arc::new(RefusingCredentials::default());
        let value = "isolated-never-serialize-this-value";
        assert!(
            apply_mutation(
                &store,
                &snapshot,
                Some(&id),
                Mutation::WriteCredential(Secret::new(value.into()).unwrap()),
                Ok(backend.clone())
            )
            .is_err()
        );
        assert_eq!(backend.writes.load(Ordering::Relaxed), 1);
        assert!(store.assert_current(&snapshot).is_err());
        let bytes = std::fs::read(
            directory
                .0
                .join(concat_agent::configuration::CONFIGURATION_FILE),
        )
        .unwrap();
        assert!(!String::from_utf8(bytes).unwrap().contains(value));
    }

    #[test]
    fn non_secret_settings_remain_writable_when_secure_storage_is_unavailable() {
        let (_directory, store, snapshot, id) = fixture();
        let mut draft = snapshot.channel(&id).unwrap().draft();
        draft.label = "Changed name".into();
        let (next, _, _) = apply_mutation(
            &store,
            &snapshot,
            Some(&id),
            Mutation::Save(draft),
            Err(CredentialError::Unsupported),
        )
        .unwrap();
        assert_eq!(next.channel(&id).unwrap().label, "Changed name");
        assert!(next.active().is_none());
    }
}
