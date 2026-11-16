// SPDX-License-Identifier: AGPL-3.0-or-later

//! Pi callbacks attached to the native editor window.

use crate::{
    embedded,
    host::Shell,
    panes,
    ui::{self, AgentUi, App},
};
use slint::ComponentHandle;

pub(crate) fn install(app: &App) -> slint::Timer {
    panes::agent_settings::install(app);

    // State comes from the same native owner as protocol admission. A rendered
    // enabled button is never authority: the send callback reads it again.
    panes::script::install(app);
    panes::agent::install_composer(app);
    let agent_ui = app.global::<AgentUi>();
    agent_ui.on_send_requested(|draft| {
        embedded::show_ui_error(embedded::submit_chat_from_ui(draft.to_string(), false));
    });
    agent_ui.on_edit_requested(|draft| {
        embedded::show_ui_error(embedded::submit_chat_from_ui(draft.to_string(), true));
    });
    agent_ui.on_stop_requested(embedded::stop_from_ui);
    app.global::<ui::AgentCallUi>()
        .on_authorize_requested(embedded::authorize_call_from_ui);
    agent_ui.on_review_requested(|| {
        let mut result = Err("聊天运行组件尚未连接。".to_owned());
        Shell::with(|shell, _| {
            if let Some(runtime) = shell.embedded.borrow().as_ref() {
                result = runtime.inspect();
            }
        });
        embedded::show_ui_error(result);
    });
    agent_ui.on_continue_requested(|| {
        let mut result = Err("聊天运行组件尚未连接。".to_owned());
        Shell::with(|shell, _| {
            if let Some(runtime) = shell.embedded.borrow().as_ref() {
                result = runtime.confirm();
            }
        });
        embedded::show_ui_error(result);
    });
    agent_ui.on_discard_requested(|| {
        let mut result = Err("聊天运行组件尚未连接。".to_owned());
        Shell::with(|shell, _| {
            if let Some(runtime) = shell.embedded.borrow().as_ref() {
                result = runtime.discard_interrupted();
            }
        });
        embedded::show_ui_error(result);
    });
    agent_ui.on_connect_requested(|| embedded::show_ui_error(embedded::connect_from_ui()));
    let agent_status_timer = slint::Timer::default();
    agent_status_timer.start(
        slint::TimerMode::Repeated,
        std::time::Duration::from_millis(250),
        || {
            Shell::with(|shell, app| {
                if let Some(runtime) = shell.embedded.borrow().as_ref() {
                    runtime.poll_saved();
                }
                shell
                    .agent_pane
                    .borrow_mut()
                    .collect_attachments(&shell.studio.borrow(), &app);
                shell.agent_pane.borrow().publish(
                    &app,
                    shell
                        .agent
                        .borrow()
                        .as_ref()
                        .ok_or_else(|| "会话暂不可用，仍可使用手动编辑。".to_owned())
                        .and_then(|bridge| bridge.status()),
                );
            });
        },
    );
    agent_status_timer
}

/// Revocation only: never wait on the UI thread for callbacks it must drain.
pub(crate) fn stop() {
    Shell::with(|shell, _| {
        if let Some(agent) = shell.agent.borrow().as_ref() {
            agent.stop();
        }
        if let Some(runtime) = shell.embedded.borrow().as_ref() {
            runtime.stop();
        }
    });
}
