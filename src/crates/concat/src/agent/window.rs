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
    app.global::<AgentUi>().on_tool_mode_requested(|index| {
        embedded::show_ui_error(embedded::set_tool_mode_from_ui(index));
    });
    app.global::<AgentUi>()
        .on_tool_decision_requested(|approve| {
            embedded::show_ui_error(embedded::decide_tool_from_ui(approve));
        });
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
    // Runtime messages, permission decisions and view switches are pushed to
    // the native event loop. Ordinary editing starts NO periodic agent timer.
    slint::Timer::default()
}

/// Dispatched by a real native tool-approval event, never by a frame timer.
/// A pending request is always made visible even in manual editing.
pub(crate) fn refresh_tool_permission() {
    Shell::with(|shell, app| {
        let ui = app.global::<AgentUi>();
        let runtime = shell.embedded.borrow().clone();
        let Some(runtime) = runtime else {
            ui.set_tool_connected(false);
            ui.set_tool_pending(false);
            ui.set_tool_summary("".into());
            return;
        };
        let Some(ready) = runtime.try_ready() else {
            slint::Timer::single_shot(
                std::time::Duration::from_millis(40),
                refresh_tool_permission,
            );
            return;
        };
        let Ok(permission) = runtime.tool_permission_view() else {
            ui.set_tool_connected(false);
            ui.set_tool_pending(false);
            ui.set_tool_summary("".into());
            return;
        };
        let pending = permission.pending_summary.is_some();
        ui.set_tool_mode(permission.mode);
        ui.set_tool_connected(ready);
        ui.set_tool_pending(pending);
        let mut summary = permission.pending_summary.unwrap_or_default();
        if pending && !permission.pending_clip_ids.is_empty() {
            summary.push_str("\n当前工程中已有片段（批准前核对）：");
            let studio = shell.studio.borrow();
            for id in &permission.pending_clip_ids {
                if let Some(clip) = studio.clip(id) {
                    let name: String = clip
                        .name
                        .chars()
                        .filter(|ch| !ch.is_control())
                        .take(48)
                        .collect();
                    summary.push_str(&format!(
                        "\n· 片段 {}「{}」：当前轨道 {}，当前时间 {:.3}～{:.3} 秒",
                        id,
                        name,
                        clip.track_id,
                        clip.start,
                        clip.start + clip.duration
                    ));
                } else {
                    summary.push_str(&format!(
                        "\n· 片段 {}：当前工程中找不到，编辑器将拒绝无效目标",
                        id
                    ));
                }
            }
        }
        ui.set_tool_summary(summary.into());
        if pending && ready {
            ui.set_view(ui::AgentView::Chat);
            ui.set_sidebar_open(true);
        }
    });
}

/// Temporary attachment import is the only reason to schedule another check.
/// When imports finish, no timer or background UI work remains.
pub(crate) fn schedule_attachment_watch() {
    slint::Timer::single_shot(std::time::Duration::from_millis(250), || {
        let mut pending = false;
        Shell::with(|shell, app| {
            shell
                .agent_pane
                .borrow_mut()
                .collect_attachments(&shell.studio.borrow(), &app);
            pending = app.global::<AgentUi>().get_attachment_pending();
        });
        if pending {
            schedule_attachment_watch();
        }
    });
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
