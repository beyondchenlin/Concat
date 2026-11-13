// SPDX-License-Identifier: AGPL-3.0-or-later
//! Settings integration in a new portable directory. This module is reachable
//! only through the opt-in probe feature, never the ordinary product entry.

use std::ops::ControlFlow;

use concat_agent::configuration::{CONFIGURATION_FILE, ConfigurationSnapshot};
use concat_agent::credentials::{
    CredentialScope, CredentialStatus, CredentialStore, Secret, platform_store,
};
use slint::private_unstable_api::re_exports::{
    InputType, LogicalPoint, LogicalRect, Text, TextInput, TouchArea, WindowInner,
};

use super::*;
use crate::panes::agent_settings::configuration_store;
use crate::ui::{AgentSettingsUi, ModelChannelDraftData};

struct CreatedCredentials {
    store: Arc<dyn CredentialStore>,
    scopes: Vec<CredentialScope>,
}

impl CreatedCredentials {
    fn cleanup(&self) -> Result<(), String> {
        let mut failure = None;
        for scope in &self.scopes {
            let result = self
                .store
                .delete(scope)
                .and_then(|()| self.store.status(scope));
            let result = result
                .map_err(|error| error.to_string())
                .and_then(|status| {
                    ensure(
                        status == CredentialStatus::Missing,
                        "isolated credential cleanup did not confirm absence",
                    )
                });
            if let Err(error) = result {
                failure.get_or_insert(error);
            }
        }
        failure.map_or(Ok(()), Err)
    }
}

impl Drop for CreatedCredentials {
    fn drop(&mut self) {
        let _ = self.cleanup();
    }
}

fn draft() -> ModelChannelDraftData {
    ModelChannelDraftData {
        label: "本次隔离设置验证".into(),
        protocol: 2,
        endpoint: "https://model-settings-probe.invalid/team-a/v1".into(),
        model: "isolated-no-network-model".into(),
        timeout_seconds: "60".into(),
        max_output_tokens: "1024".into(),
    }
}

fn idle() -> Result<(), String> {
    wait_for("native settings worker", || {
        on_ui(|_, app| {
            let ui = app.global::<AgentSettingsUi>();
            if ui.get_busy() {
                return Ok(None);
            }
            ensure(
                ui.get_error().is_empty(),
                &format!("native settings rejected action: {}", ui.get_error()),
            )?;
            ensure(
                ui.get_can_modify(),
                "native settings did not load a valid configuration",
            )?;
            Ok(Some(()))
        })
    })
}

fn saved(configuration: &Configuration) -> Result<ConfigurationSnapshot, String> {
    configuration_store(&configuration.dirs)
        .load()
        .map_err(|error| error.to_string())
}

fn no_calls() -> Result<(), String> {
    on_ui(|shell, _| {
        ensure(
            !shell.agent_pane.borrow().readiness.calls_authorized,
            "settings granted model-call authority",
        )
    })
}

fn actual_runtime() -> Result<(Arc<crate::embedded::EmbeddedRuntime>, Arc<AgentBridge>), String> {
    on_ui(|shell, _| {
        Ok((
            shell
                .embedded
                .borrow()
                .clone()
                .ok_or("settings runtime absent")?,
            shell
                .agent
                .borrow()
                .clone()
                .ok_or("settings owner absent")?,
        ))
    })
}

fn recovery(
    runtime: &crate::embedded::EmbeddedRuntime,
    bridge: &AgentBridge,
) -> Result<(), String> {
    wait_for("settings page recovery", || {
        let view = runtime.view();
        ensure(view.error.is_empty(), &view.error)?;
        Ok((view.loaded && view.phase == "recovery" && view.snapshot.is_some()).then_some(()))
    })?;
    ensure(
        bridge.status()?.phase == Phase::RecoveryRequired,
        "settings page automatically approved execution",
    )
}

/// Operates the actual local password item and the actual Slint button callback.
/// No production credential setter or test authorization route is added. The
/// generated shared status and this report never receive the credential value.
fn enter_and_submit_secret(secret: &Secret) -> Result<(), String> {
    let entered = secret.expose_secret().to_owned();
    on_ui(move |_, app| {
        let root = WindowInner::from_pub(app.window())
            .window_item_rc()
            .ok_or("native item tree absent")?;
        let mut inputs = Vec::new();
        root.visit_descendants::<()>(|item| {
            if item.is_visible() {
                if let Some(input) = item.downcast::<TextInput>() {
                    if input.as_pin_ref().input_type.get() == InputType::Password {
                        inputs.push(input);
                    }
                }
            }
            ControlFlow::Continue(())
        });
        ensure(
            inputs.len() == 1,
            "settings password input was not uniquely identified",
        )?;
        let input = inputs.pop().unwrap();
        ensure(
            input.as_pin_ref().text.get().is_empty(),
            "password entry retained previous content",
        )?;
        input.as_pin_ref().text.set(entered.into());

        let mut labels = Vec::new();
        root.visit_descendants::<()>(|item| {
            if item.is_visible() {
                if let Some(text) = item.downcast::<Text>() {
                    if text.as_pin_ref().text.get() == "保存或替换系统凭据" {
                        let size = item.geometry().size;
                        labels.push(
                            item.map_to_window(LogicalPoint::new(
                                size.width / 2.,
                                size.height / 2.,
                            )),
                        );
                    }
                }
            }
            ControlFlow::Continue(())
        });
        ensure(
            labels.len() == 1,
            "credential save button label was not unique",
        )?;
        let point = labels[0];
        let mut candidates = Vec::new();
        root.visit_descendants::<()>(|item| {
            if item.is_visible() {
                if let Some(touch) = item.downcast::<TouchArea>() {
                    let bounds = LogicalRect::new(
                        item.map_to_window(LogicalPoint::zero()),
                        item.geometry().size,
                    );
                    if touch.as_pin_ref().enabled.get() && bounds.contains(point) {
                        candidates.push((bounds.size.width * bounds.size.height, touch));
                    }
                }
            }
            ControlFlow::Continue(())
        });
        candidates.sort_by(|left, right| left.0.total_cmp(&right.0));
        let (_, button) = candidates
            .first()
            .ok_or("credential save button has no enabled native touch item")?;
        // The smallest observed touch item covering this unique label is its
        // real button, not the sheet-wide event barrier. This is a direct native
        // callback invocation, not a claim of mouse/keyboard interaction.
        button.as_pin_ref().clicked.call(&());
        ensure(
            input.as_pin_ref().text.get().is_empty(),
            "password was not cleared before the native callback returned",
        )?;
        ensure(
            app.global::<AgentSettingsUi>().get_busy(),
            "credential button did not enter the real native worker",
        )
    })
}

fn password_is_empty() -> Result<(), String> {
    on_ui(|_, app| {
        let root = WindowInner::from_pub(app.window())
            .window_item_rc()
            .ok_or("native item tree absent")?;
        let mut count = 0;
        let mut empty = true;
        root.visit_descendants::<()>(|item| {
            if item.is_visible() {
                if let Some(input) = item.downcast::<TextInput>() {
                    if input.as_pin_ref().input_type.get() == InputType::Password {
                        count += 1;
                        empty &= input.as_pin_ref().text.get().is_empty();
                    }
                }
            }
            ControlFlow::Continue(())
        });
        ensure(
            count == 1 && empty,
            "reopened password input was absent, ambiguous or nonempty",
        )
    })
}

fn snapshot_evidence(configuration: &Configuration, steps: &mut Vec<Value>, name: &str) {
    match window_snapshot(configuration, name) {
        Ok(()) => step(
            steps,
            "native_settings_window_snapshot",
            json!({"file":name,"completeVisualAcceptance":false}),
        ),
        Err(error) => steps.push(
            json!({"name":"native_settings_window_snapshot","passed":false,
            "evidenceOnly":true,"error":error,"completeVisualAcceptance":false}),
        ),
    }
}

pub(super) fn run(configuration: &Configuration, steps: &mut Vec<Value>) -> Result<(), String> {
    let backend = platform_store().map_err(|error| error.to_string())?;
    let mut created = CreatedCredentials {
        store: backend,
        scopes: Vec::new(),
    };
    let result = run_steps(configuration, steps, &mut created);
    let cleaned = created.cleanup();
    if cleaned.is_ok() {
        step(
            steps,
            "only_this_run_created_credentials_removed",
            json!({"scopeCount":created.scopes.len(),"absenceVerified":true}),
        );
    }
    result.and(cleaned)
}

fn run_steps(
    configuration: &Configuration,
    steps: &mut Vec<Value>,
    created: &mut CreatedCredentials,
) -> Result<(), String> {
    let (first_runtime, first_bridge) = actual_runtime()?;
    recovery(&first_runtime, &first_bridge)?;
    idle()?;
    ensure(
        saved(configuration)?.channels().is_empty(),
        "settings probe adopted existing configuration",
    )?;
    let first_generation = first_bridge.status()?.generation;
    on_ui(|_, app| {
        app.global::<AgentSettingsUi>().invoke_open_requested();
        Ok(())
    })?;
    idle()?;
    let initial = draft();
    let proposed = initial.clone();
    on_ui(move |_, app| {
        let ui = app.global::<AgentSettingsUi>();
        ui.set_draft(proposed.clone());
        ui.invoke_save_requested(proposed);
        Ok(())
    })?;
    idle()?;
    let first = saved(configuration)?;
    ensure(
        first.channels().len() == 1 && first.active().is_none(),
        "save implicitly selected a channel",
    )?;
    ensure(
        first_runtime.view().closed && first_bridge.status()?.phase == Phase::Released,
        "configuration save completed without real platform destruction and owner release",
    )?;
    let channel = &first.channels()[0];
    let id = channel.id.clone();
    let origin_a = channel
        .service_origin()
        .map_err(|error| error.to_string())?;
    let scope_a =
        CredentialScope::new(id.as_str(), &origin_a).map_err(|error| error.to_string())?;
    ensure(
        created
            .store
            .status(&scope_a)
            .map_err(|error| error.to_string())?
            == CredentialStatus::Missing,
        "new random channel unexpectedly had a credential",
    )?;
    created.scopes.push(scope_a);
    step(
        steps,
        "native_save_readback_after_old_runtime_closed",
        json!({"configurationVersion":first.version(),
        "channelId":id.as_str(),"oldGeneration":first_generation,"actualClosed":true,"ownerReleased":true,
        "scope":"real final states plus production maintenance dependency; not an independent timestamp ordering trace"}),
    );

    on_ui(|_, app| {
        app.global::<AgentSettingsUi>().invoke_reload_requested();
        Ok(())
    })?;
    idle()?;
    on_ui(move |_, app| {
        let ui = app.global::<AgentSettingsUi>();
        ensure(
            ui.get_draft() == initial,
            "native form reload did not match the saved values",
        )?;
        ui.invoke_activate_requested(ui.get_draft());
        Ok(())
    })?;
    idle()?;
    ensure(
        saved(configuration)?
            .active()
            .is_some_and(|channel| channel.id == id),
        "manual channel selection was not persisted",
    )?;
    no_calls()?;
    step(
        steps,
        "explicit_channel_selection_persisted_without_fee_authority",
        json!({"configurationVersion":saved(configuration)?.version()}),
    );

    let secret =
        Secret::new(crate::embedded::random_capability()?).map_err(|error| error.to_string())?;
    enter_and_submit_secret(&secret)?;
    idle()?;
    let stored = created
        .store
        .read(&created.scopes[0])
        .map_err(|error| error.to_string())?
        .ok_or("actual system credential missing after UI save")?;
    ensure(
        stored.expose_secret() == secret.expose_secret(),
        "actual system credential differs from native form value",
    )?;
    let bytes = fs::read(
        configuration
            .dirs
            .config
            .join("bk-cat/model-settings")
            .join(CONFIGURATION_FILE),
    )
    .map_err(|error| error.to_string())?;
    ensure(
        !String::from_utf8_lossy(&bytes).contains(secret.expose_secret()),
        "ordinary configuration contains the credential",
    )?;
    on_ui(|shell, app| {
        ensure(
            shell.agent_pane.borrow().readiness.credentials_ready,
            "active credential status was not published from actual secure storage",
        )?;
        app.global::<AgentSettingsUi>().invoke_close_requested();
        Ok(())
    })?;
    on_ui(|_, app| {
        ensure(
            !app.global::<AgentSettingsUi>().get_open(),
            "settings did not close",
        )?;
        app.global::<AgentSettingsUi>().invoke_open_requested();
        Ok(())
    })?;
    idle()?;
    password_is_empty()?;
    no_calls()?;
    snapshot_evidence(configuration, steps, "settings-saved-window.png");
    step(
        steps,
        "real_system_credential_and_actual_password_callback_clear",
        json!({
        "systemReadbackEqual":true,"sharedConfigurationHasNoSecret":true,"passwordClearedBeforeNativeCallbackReturn":true,
        "reopenedPasswordEmpty":true,"interaction":"native item property plus actual button callback; not mouse or keyboard acceptance"}),
    );

    on_ui(|_, _| crate::embedded::connect_from_ui())?;
    let (replacement, second_bridge) = actual_runtime()?;
    recovery(&replacement, &second_bridge)?;
    ensure(
        second_bridge.status()?.generation > first_generation,
        "settings reconnect reused its retired generation",
    )?;
    std::thread::sleep(Duration::from_millis(500));
    recovery(&replacement, &second_bridge)?;
    no_calls()?;
    let current = ui_view()?;
    ensure(
        clips(&current)?.is_empty(),
        "settings or reconnect changed the isolated project",
    )?;
    step(
        steps,
        "explicit_reconnect_remains_in_recovery_without_network_authority",
        json!({"generation":second_bridge.status()?.generation,
        "phase":replacement.view().phase,"originalProjectUnchanged":true}),
    );

    on_ui(|_, app| {
        let ui = app.global::<AgentSettingsUi>();
        let mut changed = ui.get_draft();
        changed.endpoint = "https://model-settings-probe.invalid/team-b/v1".into();
        ui.set_draft(changed.clone());
        ui.invoke_save_requested(changed);
        Ok(())
    })?;
    idle()?;
    let changed = saved(configuration)?;
    ensure(
        changed.channels().len() == 1 && changed.channels()[0].id == id,
        "destination edit replaced the stable channel identity",
    )?;
    ensure(
        replacement.view().closed && second_bridge.status()?.phase == Phase::Released,
        "changed service destination left its previous runtime active",
    )?;
    let origin_b = changed
        .channel(&id)
        .unwrap()
        .service_origin()
        .map_err(|error| error.to_string())?;
    let scope_b =
        CredentialScope::new(id.as_str(), &origin_b).map_err(|error| error.to_string())?;
    ensure(
        scope_b.key_id() != created.scopes[0].key_id(),
        "different base paths reused a credential identity",
    )?;
    ensure(
        created
            .store
            .status(&scope_b)
            .map_err(|error| error.to_string())?
            == CredentialStatus::Missing,
        "new base path reused the old system credential",
    )?;
    ensure(
        created
            .store
            .status(&created.scopes[0])
            .map_err(|error| error.to_string())?
            == CredentialStatus::Present,
        "destination change modified the original credential",
    )?;
    created.scopes.push(scope_b);
    on_ui(|shell, _| {
        ensure(
            !shell.agent_pane.borrow().readiness.credentials_ready,
            "new destination displayed the old credential as ready",
        )
    })?;
    no_calls()?;
    step(
        steps,
        "changed_base_path_has_distinct_actual_missing_credential",
        json!({"sameChannelId":true,"differentScope":true,
        "originalCredentialStillPresent":true,"newCredentialMissing":true,"oldRuntimeClosed":true}),
    );

    let second_secret =
        Secret::new(crate::embedded::random_capability()?).map_err(|error| error.to_string())?;
    enter_and_submit_secret(&second_secret)?;
    idle()?;
    let second_stored = created
        .store
        .read(&created.scopes[1])
        .map_err(|error| error.to_string())?
        .ok_or("second isolated credential missing")?;
    ensure(
        second_stored.expose_secret() == second_secret.expose_secret(),
        "second scoped credential readback differs",
    )?;
    on_ui(|_, app| {
        let ui = app.global::<AgentSettingsUi>();
        ui.invoke_credential_delete_requested(ui.get_draft());
        Ok(())
    })?;
    idle()?;
    ensure(
        created
            .store
            .status(&created.scopes[1])
            .map_err(|error| error.to_string())?
            == CredentialStatus::Missing,
        "native credential delete did not remove its own scope",
    )?;
    ensure(
        created
            .store
            .status(&created.scopes[0])
            .map_err(|error| error.to_string())?
            == CredentialStatus::Present,
        "native credential delete touched the other base path",
    )?;
    on_ui(|_, app| {
        let ui = app.global::<AgentSettingsUi>();
        let mut original = ui.get_draft();
        original.endpoint = "https://model-settings-probe.invalid/team-a/v1".into();
        ui.set_draft(original.clone());
        ui.invoke_save_requested(original);
        Ok(())
    })?;
    idle()?;
    on_ui(|_, app| {
        let ui = app.global::<AgentSettingsUi>();
        ui.invoke_delete_requested(ui.get_draft());
        Ok(())
    })?;
    idle()?;
    let empty = saved(configuration)?;
    ensure(
        empty.channels().is_empty() && empty.active().is_none(),
        "native channel delete retained its selection",
    )?;
    ensure(
        created
            .store
            .status(&created.scopes[0])
            .map_err(|error| error.to_string())?
            == CredentialStatus::Missing,
        "native channel delete left its corresponding credential",
    )?;
    no_calls()?;
    snapshot_evidence(configuration, steps, "settings-cleaned-window.png");
    step(
        steps,
        "native_credential_and_channel_deletion_verified",
        json!({"configurationVersion":empty.version(),
        "remainingChannels":0,"activeChannel":null,"modelCalls":false}),
    );
    on_ui(|_, app| {
        app.global::<AgentSettingsUi>().invoke_close_requested();
        Ok(())
    })?;
    Ok(())
}
