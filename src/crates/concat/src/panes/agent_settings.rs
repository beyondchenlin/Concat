// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native settings and secure-credential state, independent of any model call.
//! Ordinary settings, projects, Pi storage and logs never receive a secret.

use std::rc::Rc;
use std::sync::Arc;
use std::time::{Duration, Instant};

use concat_agent::configuration::{
    ChannelDraft, ChannelId, ConfigurationSnapshot, ConfigurationStore, ModelChannel,
    ModelProtocol, validate_draft,
};
use concat_agent::credentials::{
    CredentialError, CredentialScope, CredentialStatus, CredentialStore, Secret, platform_store,
};
use concat_agent::{Phase, SessionOwner};
use concat_api::AppDirs;
use slint::{ComponentHandle, ModelRc, SharedString, VecModel};

use crate::agent::AgentBridge;
use crate::embedded::EmbeddedRuntime;
use crate::host::{self, Shell};
use crate::ui::{AgentSettingsUi, App, ModelChannelDraftData};

/// Non-secret native form state; credentials only cross a bounded action.
#[derive(Default)]
pub struct AgentSettingsPane {
    snapshot: Option<ConfigurationSnapshot>,
    selected: Option<ChannelId>,
    request: u64,
    busy: bool,
}

/// The next network integration must re-read and assert the snapshot before
/// admission. Nothing in this directory represents fee or network authority.
pub(crate) fn configuration_store(dirs: &AppDirs) -> ConfigurationStore {
    ConfigurationStore::new(dirs.config.join("bk-cat").join("model-settings"))
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

fn timeout_from_ui(value: &str) -> Result<u32, String> {
    let invalid = || "请求超时须为1至600秒，最多三位小数。".to_owned();
    let (whole, fraction) = value.split_once('.').unwrap_or((value, ""));
    if whole.is_empty()
        || !whole.bytes().all(|byte| byte.is_ascii_digit())
        || fraction.len() > 3
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
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

fn draft_from_ui(draft: &ModelChannelDraftData) -> Result<ChannelDraft, String> {
    let protocol = match draft.protocol {
        1 => ModelProtocol::OpenAiCompletions,
        2 => ModelProtocol::OpenAiResponses,
        3 => ModelProtocol::AnthropicMessages,
        _ => return Err("请选择明确的模型协议。".into()),
    };
    let timeout_ms = timeout_from_ui(&draft.timeout_seconds)?;
    let output = draft
        .max_output_tokens
        .parse::<u32>()
        .map_err(|_| "输出上限须为正整数。")?;
    validate_draft(ChannelDraft {
        label: draft.label.to_string(),
        protocol,
        endpoint: draft.endpoint.to_string(),
        model: draft.model.to_string(),
        request_timeout_ms: timeout_ms,
        max_output_tokens: output,
    })
    .map_err(|error| error.to_string())
}

fn draft_to_ui(channel: Option<&ModelChannel>) -> ModelChannelDraftData {
    match channel {
        Some(channel) => ModelChannelDraftData {
            label: channel.label.as_str().into(),
            protocol: match channel.protocol {
                ModelProtocol::OpenAiCompletions => 1,
                ModelProtocol::OpenAiResponses => 2,
                ModelProtocol::AnthropicMessages => 3,
            },
            endpoint: channel.endpoint.as_str().into(),
            model: channel.model.as_str().into(),
            timeout_seconds: timeout_to_ui(channel.request_timeout_ms).into(),
            max_output_tokens: channel.max_output_tokens.to_string().into(),
        },
        None => ModelChannelDraftData {
            label: "".into(),
            protocol: 0,
            endpoint: "".into(),
            model: "".into(),
            timeout_seconds: "60".into(),
            max_output_tokens: "4096".into(),
        },
    }
}

fn check_saved_draft(
    snapshot: &ConfigurationSnapshot,
    selected: Option<&ChannelId>,
    draft: &ModelChannelDraftData,
) -> Result<(), String> {
    let channel = selected
        .and_then(|id| snapshot.channel(id))
        .ok_or("请先保存渠道和服务地址。")?;
    if draft_from_ui(draft)? != channel.draft() {
        return Err("请先保存当前修改，再操作已保存的渠道或凭据。".into());
    }
    Ok(())
}

struct Inspection {
    snapshot: Result<ConfigurationSnapshot, String>,
    selected: Option<ChannelId>,
    selected_credential: Option<Result<CredentialStatus, CredentialError>>,
    active_credential: Option<Result<CredentialStatus, CredentialError>>,
}

fn inspect(
    store: &ConfigurationStore,
    wanted: Option<ChannelId>,
    choose_existing: bool,
) -> Inspection {
    let snapshot = store.load().map_err(|error| error.to_string());
    let mut selected = None;
    let mut selected_credential = None;
    let mut active_credential = None;
    if let Ok(snapshot) = &snapshot {
        selected = wanted
            .filter(|id| snapshot.channel(id).is_some())
            .or_else(|| {
                choose_existing
                    .then(|| {
                        snapshot
                            .active()
                            .or_else(|| snapshot.channels().first())
                            .map(|channel| channel.id.clone())
                    })
                    .flatten()
            });
        let credentials = platform_store();
        let status = |channel: &ModelChannel| -> Result<CredentialStatus, CredentialError> {
            let origin = channel
                .service_origin()
                .map_err(|_| CredentialError::InvalidScope)?;
            let bound = CredentialScope::new(channel.id.as_str(), &origin)?;
            credentials.as_ref().map_err(|error| *error)?.status(&bound)
        };
        selected_credential = selected
            .as_ref()
            .and_then(|id| snapshot.channel(id))
            .map(&status);
        active_credential = snapshot.active().map(status);
    }
    Inspection {
        snapshot,
        selected,
        selected_credential,
        active_credential,
    }
}

fn publish(
    shell: &Shell,
    app: &App,
    request: u64,
    inspection: Inspection,
    outcome: Result<String, String>,
) {
    let mut pane = shell.agent_settings.borrow_mut();
    if pane.request != request {
        return;
    }
    pane.busy = false;
    let ui = app.global::<AgentSettingsUi>();
    ui.set_busy(false);
    ui.set_error(outcome.as_ref().err().map_or("", String::as_str).into());
    ui.set_message(outcome.as_ref().ok().map_or("", String::as_str).into());
    let mut readiness = shell.agent_pane.borrow_mut();
    readiness.readiness.calls_authorized = false;
    readiness.readiness.credentials_ready = false;
    readiness.readiness.provider.clear();
    readiness.readiness.model.clear();
    readiness.configuration_error.clear();
    match inspection.snapshot {
        Ok(snapshot) => {
            pane.selected = inspection.selected;
            let selected = pane.selected.as_ref().and_then(|id| snapshot.channel(id));
            let mut labels: Vec<SharedString> = vec!["新建渠道".into()];
            labels.extend(
                snapshot
                    .channels()
                    .iter()
                    .map(|channel| channel.label.as_str().into()),
            );
            ui.set_channel_labels(ModelRc::from(Rc::new(VecModel::from(labels))));
            ui.set_channel_index(
                pane.selected
                    .as_ref()
                    .and_then(|id| {
                        snapshot
                            .channels()
                            .iter()
                            .position(|channel| &channel.id == id)
                    })
                    .map_or(0, |index| index as i32 + 1),
            );
            ui.set_has_saved_channel(selected.is_some());
            ui.set_draft(draft_to_ui(selected));
            ui.set_can_modify(true);
            ui.set_configuration_version(snapshot.version().to_string().into());
            ui.set_credential_status(match inspection.selected_credential {
                Some(Ok(CredentialStatus::Present)) => {
                    "系统凭据已存在并可读取；联网调用仍未授权。".into()
                }
                Some(Ok(CredentialStatus::Missing)) => "当前渠道及服务地址没有系统凭据。".into(),
                Some(Err(error)) => error.to_string().into(),
                None => "先保存渠道及服务地址，再存入系统凭据库。".into(),
            });
            if let Some(active) = snapshot.active() {
                ui.set_active_name(active.label.as_str().into());
                readiness.readiness.provider = active.label.clone();
                readiness.readiness.model = active.model.clone();
                match inspection.active_credential {
                    Some(Ok(CredentialStatus::Present)) => {
                        readiness.readiness.credentials_ready = true
                    }
                    Some(Err(error)) => readiness.configuration_error = error.to_string(),
                    _ => {}
                }
            } else {
                ui.set_active_name("尚未选择渠道".into());
            }
            pane.snapshot = Some(snapshot);
        }
        Err(error) => {
            pane.snapshot = None;
            pane.selected = None;
            ui.set_can_modify(false);
            ui.set_has_saved_channel(false);
            ui.set_error(error.as_str().into());
            ui.set_active_name("模型配置不可用".into());
            ui.set_credential_status("配置未读取成功，未推定凭据缺失。".into());
            readiness.configuration_error = error;
        }
    }
    readiness.publish(
        app,
        shell
            .agent
            .borrow()
            .as_ref()
            .ok_or_else(|| "会话尚未连接。".to_owned())
            .and_then(|bridge| bridge.status()),
    );
}

fn fail_ui(message: String) {
    Shell::with(|_, app| app.global::<AgentSettingsUi>().set_error(message.into()));
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
        ui.set_error("".into());
        let store = configuration_store(&shell.studio.borrow().host.dirs);
        host::spawn_detached(move || {
            let inspection = inspect(&store, wanted, choose_existing);
            let _ = slint::invoke_from_event_loop(move || {
                Shell::with(|shell, app| {
                    publish(
                        shell,
                        &app,
                        request,
                        inspection,
                        Ok("已从本机读取配置与系统凭据状态；没有发起模型请求。".into()),
                    );
                })
            });
        });
    });
}

enum Mutation {
    Save(ChannelDraft),
    Select,
    Delete,
    WriteCredential(Secret),
    DeleteCredential,
}

fn apply_mutation(
    store: &ConfigurationStore,
    expected: &ConfigurationSnapshot,
    selected: Option<&ChannelId>,
    mutation: Mutation,
    credentials: Result<Arc<dyn CredentialStore>, CredentialError>,
) -> Result<(ConfigurationSnapshot, Option<ChannelId>, String), String> {
    // A stale draft may never delete or replace even a correctly-scoped secret.
    store
        .assert_current(expected)
        .map_err(|error| error.to_string())?;
    match mutation {
        Mutation::Save(draft) => {
            let next = store
                .save_channel(expected, selected, draft)
                .map_err(|error| error.to_string())?;
            let selected = selected
                .cloned()
                .or_else(|| next.channels().last().map(|channel| channel.id.clone()));
            Ok((
                next,
                selected,
                "渠道配置已落盘；原运行组件已退役，联网调用仍未授权。".into(),
            ))
        }
        Mutation::Select => {
            let id = selected.ok_or("请先保存要选择的渠道。")?;
            let next = store
                .select(expected, Some(id))
                .map_err(|error| error.to_string())?;
            Ok((
                next,
                Some(id.clone()),
                "当前渠道已保存；联网调用仍未授权。".into(),
            ))
        }
        Mutation::Delete => {
            let credentials = credentials.as_deref().map_err(|error| error.to_string())?;
            let id = selected.ok_or("请先选择已保存的渠道。")?;
            let channel = expected.channel(id).ok_or("所选渠道已不存在。")?;
            let bound = scope(channel)?;
            // Refuse a backend failure and keep the channel available to retry.
            credentials
                .delete(&bound)
                .map_err(|error| error.to_string())?;
            if credentials
                .status(&bound)
                .map_err(|error| error.to_string())?
                != CredentialStatus::Missing
            {
                return Err("系统凭据仍存在，渠道删除未完成。".into());
            }
            let next = store
                .delete(expected, id)
                .map_err(|_| "凭据已移除，但渠道配置删除未确认；请重新读取。".to_owned())?;
            Ok((
                next,
                None,
                "渠道与对应系统凭据已删除；未自动选择其他渠道。".into(),
            ))
        }
        Mutation::WriteCredential(secret) => {
            let credentials = credentials.as_deref().map_err(|error| error.to_string())?;
            let id = selected.ok_or("请先保存渠道和服务地址。")?;
            let bound = scope(expected.channel(id).ok_or("所选渠道已不存在。")?)?;
            let next = store
                .credential_changed(expected, id)
                .map_err(|error| error.to_string())?;
            credentials
                .write(&bound, &secret)
                .map_err(|error| error.to_string())?;
            let read = credentials
                .read(&bound)
                .map_err(|error| error.to_string())?
                .ok_or("系统凭据写入后无法读取。")?;
            if read.expose_secret() != secret.expose_secret() {
                return Err("系统凭据写入后核验失败。".into());
            }
            Ok((
                next,
                Some(id.clone()),
                "系统凭据已写入并重新读取核验；联网调用仍未授权。".into(),
            ))
        }
        Mutation::DeleteCredential => {
            let credentials = credentials.as_deref().map_err(|error| error.to_string())?;
            let id = selected.ok_or("请先选择已保存的渠道。")?;
            let bound = scope(expected.channel(id).ok_or("所选渠道已不存在。")?)?;
            let next = store
                .credential_changed(expected, id)
                .map_err(|error| error.to_string())?;
            credentials
                .delete(&bound)
                .map_err(|error| error.to_string())?;
            if credentials
                .status(&bound)
                .map_err(|error| error.to_string())?
                != CredentialStatus::Missing
            {
                return Err("系统凭据删除后仍可读取，未确认删除。".into());
            }
            Ok((
                next,
                Some(id.clone()),
                "系统凭据已删除；旧配置版本和运行资格已失效。".into(),
            ))
        }
    }
}

/// Stop first on the UI thread; wait for real destruction and release only on
/// a worker. A separate temporary recovery-only owner holds the same OS lock
/// while settings are changed. It opens no Pi database and admits no queue.
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
                .map(|bridge| bridge.status())
                .transpose()?
                .is_none_or(|status| status.phase == Phase::Released);
            if runtime.view().closed && released {
                break;
            }
            if Instant::now() >= deadline {
                return Err("旧运行组件尚未安全关闭，设置未修改；请稍后重试。".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
    } else if let Some(bridge) = bridge {
        bridge.finish_handoff(Duration::from_secs(10))?;
    }
    let storage = dirs.data.join("bk-cat").join("agent-session");
    std::fs::create_dir_all(&storage).map_err(|_| "无法打开本产品会话目录，设置未修改。")?;
    SessionOwner::acquire(storage)
        .map_err(|_| "无法取得会话独占资格，设置未修改；请确认其他实例已安全退出。".to_owned())
}

fn mutate(mutation: Mutation, supplied: Option<ModelChannelDraftData>) {
    Shell::with(|shell, app| {
        let mut pane = shell.agent_settings.borrow_mut();
        if pane.busy {
            return;
        }
        let Some(expected) = pane.snapshot.clone() else {
            fail_ui("请先成功读取模型配置。".into());
            return;
        };
        let selected = pane.selected.clone();
        if let Some(supplied) = supplied
            && let Err(error) = check_saved_draft(&expected, selected.as_ref(), &supplied)
        {
            fail_ui(error);
            return;
        }
        if !matches!(&mutation, Mutation::Save(_)) && selected.is_none() {
            fail_ui("请先选择已保存的渠道。".into());
            return;
        }
        let dirs = shell.studio.borrow().host.dirs.clone();
        let store = configuration_store(&dirs);
        let bridge = shell.agent.borrow().clone();
        let runtime = shell.embedded.borrow().clone();
        // Revoke before even queueing disk or credential work. No late callback
        // may send another request under the configuration being replaced.
        if let Some(bridge) = &bridge {
            bridge.stop();
        }
        if let Some(runtime) = &runtime {
            runtime.stop();
        }
        shell.agent_pane.borrow_mut().readiness.calls_authorized = false;
        pane.request += 1;
        let request = pane.request;
        pane.busy = true;
        app.global::<AgentSettingsUi>().set_busy(true);
        app.global::<AgentSettingsUi>().set_error("".into());
        host::spawn_detached(move || {
            let outcome = (|| {
                let owner = maintenance_owner(&dirs, bridge, runtime)?;
                let result = apply_mutation(
                    &store,
                    &expected,
                    selected.as_ref(),
                    mutation,
                    platform_store(),
                );
                owner
                    .finish_handoff(Duration::ZERO)
                    .map_err(|_| "设置操作后独占资格释放未确认，请重新检查。".to_owned())?;
                result
            })();
            let chosen = outcome
                .as_ref()
                .ok()
                .and_then(|(_, selected, _)| selected.clone())
                .or(selected);
            let inspection = inspect(&store, chosen, false);
            let message = outcome.map(|(_, _, message)| message);
            let _ = slint::invoke_from_event_loop(move || {
                Shell::with(|shell, app| publish(shell, &app, request, inspection, message))
            });
        });
    });
}

pub(crate) fn install(app: &App) {
    let ui = app.global::<AgentSettingsUi>();
    ui.on_open_requested(|| refresh(None, true, true));
    ui.on_close_requested(|| {
        Shell::with(|shell, app| {
            if !shell.agent_settings.borrow().busy {
                app.global::<AgentSettingsUi>().set_open(false);
            }
        })
    });
    ui.on_reload_requested(|| refresh(None, true, false));
    ui.on_channel_chosen(|index| {
        let mut selected = None;
        Shell::with(|shell, _| {
            let pane = shell.agent_settings.borrow();
            if index > 0 {
                selected = pane
                    .snapshot
                    .as_ref()
                    .and_then(|snapshot| snapshot.channels().get((index - 1) as usize))
                    .map(|channel| channel.id.clone());
            }
        });
        refresh(selected, false, false);
    });
    ui.on_save_requested(|draft| match draft_from_ui(&draft) {
        Ok(draft) => mutate(Mutation::Save(draft), None),
        Err(error) => fail_ui(error),
    });
    ui.on_activate_requested(|draft| mutate(Mutation::Select, Some(draft)));
    ui.on_delete_requested(|draft| mutate(Mutation::Delete, Some(draft)));
    ui.on_credential_save_requested(|draft, text| match Secret::new(text.to_string()) {
        Ok(secret) => mutate(Mutation::WriteCredential(secret), Some(draft)),
        Err(error) => fail_ui(error.to_string()),
    });
    ui.on_credential_delete_requested(|draft| mutate(Mutation::DeleteCredential, Some(draft)));
    refresh(None, true, false);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

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
        assert_eq!(store.load().unwrap(), snapshot);
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
