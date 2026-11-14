// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native readiness, recovery and task presentation for the embedded runtime.
//! This owns no model client, scheduler, persistence or editable project.
//! Visible messages are bounded projections of the original persisted session.

use concat_agent::{OwnerStatus, Phase};
use slint::{ComponentHandle, ModelRc, VecModel};
use std::rc::Rc;

use crate::ui::{AgentCallUi, AgentMessageData, AgentStatusData, AgentUi, App};

/// Facts supplied by the native platform host. Never store credentials in
/// this presenter or in a Slint property; only their availability belongs here.
#[derive(Clone, Debug, Default)]
pub struct AgentReadiness {
    /// The controlled platform runtime has completed its startup handshake.
    pub runtime_ready: bool,
    /// The explicitly selected model channel, without its secret.
    pub provider: String,
    /// The exact configured model identifier.
    pub model: String,
    /// Native credential storage has an available credential.
    pub credentials_ready: bool,
    /// Explicit authorization for this configured channel and its cost limit.
    pub calls_authorized: bool,
}

/// A projection of an actual running task, not a progress simulation.
#[derive(Clone, Debug, Default)]
pub struct AgentTask {
    /// A task that the real runtime reports as running.
    pub running: bool,
    /// The task's user-facing title.
    pub title: String,
    /// The task's current detail, without fabricated progress.
    pub detail: String,
    /// A real fraction supplied by a task, or None for indeterminate work.
    pub progress: Option<f32>,
}

#[derive(Default)]
/// The conversation's native presentation state, with no execution authority.
pub struct AgentPane {
    /// Native facts required before submitting a new request.
    pub readiness: AgentReadiness,
    /// The currently observed task, if any.
    pub task: AgentTask,
    /// A user-facing failure reported by the real runtime or native bridge.
    pub error: String,
    /// Actual configuration or secure-store failure; never a secret value.
    pub configuration_error: String,
    /// Read-only recovery evidence from the current, authenticated component.
    pub recovery_summary: String,
    pub can_recover: bool,
    /// Explicit cancellation of reviewed interrupted work, never fee consent.
    pub can_discard: bool,
    pub can_review: bool,
    pub runtime_loading: bool,
    pub runtime_loaded: bool,
    messages: Vec<AgentMessageData>,
    call_limits: String,
}

impl AgentPane {
    pub(crate) fn apply_runtime(&mut self, runtime: &crate::embedded::RuntimeView) {
        self.readiness.runtime_ready = runtime.phase == "ready" && !runtime.closed;
        if !self.readiness.runtime_ready {
            self.readiness.calls_authorized = false;
        }
        self.task.running = runtime.task_running;
        self.task.title = runtime.task_title.clone();
        self.task.detail = if runtime.task_running {
            "正在接收真实模型与原生工具执行结果。".into()
        } else {
            String::new()
        };
        if let Some(configuration) = &runtime.configuration {
            self.call_limits =
                configuration["maxOutputTokens"]
                    .as_u64()
                    .map_or_else(String::new, |limit| {
                        format!(
                            "本次最多4次模型请求，每次输出最多{limit}个词元；费用按服务商计费。"
                        )
                    });
        }
        self.messages.clear();
        if let Some(presentation) = &runtime.presentation {
            if let Some(messages) = presentation["messages"].as_array() {
                self.messages.extend(messages.iter().filter_map(|message| {
                    let author = match message["role"].as_str()? {
                        "user" => "我",
                        "assistant" => "助手",
                        "tool" => "执行结果",
                        _ => return None,
                    };
                    Some(AgentMessageData {
                        author: author.into(),
                        body: message["text"].as_str()?.into(),
                        pending: false,
                    })
                }));
            }
            if let Some(partial) = presentation["partialText"]
                .as_str()
                .filter(|text| !text.is_empty())
            {
                self.messages.push(AgentMessageData {
                    author: "助手".into(),
                    body: partial.into(),
                    pending: true,
                });
            }
        }
        self.runtime_loading = matches!(
            runtime.phase.as_str(),
            "loading" | "acquiring" | "activating"
        );
        self.runtime_loaded = runtime.loaded && !runtime.closed;
        self.can_review = runtime.phase == "recovery";
        self.can_recover = false;
        self.can_discard = false;
        self.error = runtime.error.clone();
        self.recovery_summary.clear();
        if let Some(snapshot) = &runtime.snapshot {
            self.can_discard = self.can_review
                && snapshot["snapshotId"].is_string()
                && snapshot["projectPath"].is_string()
                && snapshot["hasInterruptedWork"] == true
                && snapshot["canDiscardInterrupted"] == true;
            if let Some(reason) = snapshot["blockedReason"].as_str() {
                self.recovery_summary = reason.to_owned();
            } else if self.can_review {
                self.can_recover = snapshot["snapshotId"].is_string()
                    && snapshot["projectPath"].is_string()
                    && snapshot["hasInterruptedWork"] == false;
                self.recovery_summary = if snapshot["schemaVersion"] == "empty" {
                    "当前工程已核对。确认后初始化空会话；不会调用模型或自动编辑。"
                } else {
                    "当前工程与已存会话已核对。确认后打开会话；不会自动继续旧任务。"
                }
                .to_owned();
            }
        }
    }

    fn missing(&self) -> String {
        let mut missing = Vec::new();
        if self.readiness.provider.trim().is_empty() {
            missing.push("模型渠道");
        }
        if self.readiness.model.trim().is_empty() {
            missing.push("模型名称");
        }
        if !self.readiness.credentials_ready {
            missing.push("调用凭据");
        }
        if !self.readiness.calls_authorized {
            missing.push("调用授权与费用限额");
        }
        if !self.readiness.runtime_ready {
            missing.push("聊天功能就绪");
        }
        if missing.is_empty() {
            String::new()
        } else {
            format!("待完成：{}。", missing.join("、"))
        }
    }

    /// Recompute from the bridge's current, read-only ownership status.
    /// This never confirms recovery, acquires an execution permit or resumes
    /// interrupted work. UI enablement is not an execution credential.
    pub fn data(&self, owner: Result<OwnerStatus, String>) -> AgentStatusData {
        let missing = self.missing();
        let model = if self.readiness.provider.trim().is_empty()
            || self.readiness.model.trim().is_empty()
        {
            "未配置模型".to_owned()
        } else {
            format!("{} / {}", self.readiness.provider, self.readiness.model)
        };
        let mut data = AgentStatusData {
            missing: missing.clone().into(),
            error: if self.error.is_empty() {
                self.configuration_error.as_str()
            } else {
                self.error.as_str()
            }
            .into(),
            model: model.into(),
            can_review: self.can_review,
            can_continue: self.can_recover,
            can_discard: self.can_discard,
            recovery_summary: self.recovery_summary.as_str().into(),
            ..AgentStatusData::default()
        };
        let owner = match owner {
            Ok(owner) => owner,
            Err(error) => {
                data.title = "会话暂不可用".into();
                data.detail = "智能任务没有启动，仍可使用手动编辑。".into();
                data.error = error.into();
                data.can_connect = true;
                data.can_review = false;
                data.can_continue = false;
                data.can_discard = false;
                return data;
            }
        };
        data.can_connect = owner.phase == Phase::Released
            || (owner.phase == Phase::RecoveryRequired
                && !self.runtime_loaded
                && !self.runtime_loading);
        data.can_stop = matches!(owner.phase, Phase::RecoveryRequired | Phase::Ready)
            && (self.task.running
                || self.readiness.runtime_ready
                || self.runtime_loaded
                || self.runtime_loading
                || owner.active_operations > 0);
        if owner.phase != Phase::RecoveryRequired {
            data.can_review = false;
            data.can_continue = false;
            data.can_discard = false;
        }
        match owner.phase {
            Phase::RecoveryRequired => {
                data.title = "等待恢复核对".into();
                data.detail = "核对当前工程和会话后，由你决定是否继续。".into();
            }
            Phase::Draining => {
                data.busy = self.task.running || owner.active_operations > 0;
                data.title = if data.busy {
                    "正在停止"
                } else {
                    "已停止"
                }
                .into();
                data.detail = "已阻止新的智能操作。再次使用前需要重新核对。".into();
            }
            Phase::Released => {
                data.title = "会话已关闭".into();
                data.detail = "再次使用前需要重新核对当前工程和会话。".into();
            }
            Phase::Ready if !self.error.is_empty() || !self.configuration_error.is_empty() => {
                data.title = "任务失败".into();
                data.detail = "处理错误后再提交，草稿保留在输入框中。".into();
            }
            // A database connection holds an operation permit for its entire
            // lifetime. That permit blocks handoff, not new conversation input.
            Phase::Ready if self.task.running => {
                data.busy = true;
                data.title = if self.task.title.is_empty() {
                    "正在处理".into()
                } else {
                    self.task.title.as_str().into()
                };
                data.detail = self.task.detail.as_str().into();
                if self.task.running
                    && let Some(progress) = self.task.progress
                    && progress.is_finite()
                    && (0.0..=1.0).contains(&progress)
                {
                    data.has_progress = true;
                    data.progress = progress;
                }
            }
            Phase::Ready if !missing.is_empty() => {
                data.title = "聊天未就绪".into();
                data.detail = "完成下列条件后才能发送。".into();
            }
            Phase::Ready => {
                data.title = "等待输入".into();
                data.detail = "描述需要修改的内容，所有编辑使用当前工程。".into();
                data.can_send = true;
            }
        }
        data
    }

    /// Called by the native send callback with a fresh bridge status, not
    /// with the last rendered can-send flag. The caller must still use the
    /// native execution and provider gates and a persistent request id when
    /// submitting to Pi. Only an accepted submission may clear the UI draft.
    pub fn validate_submission(
        &self,
        owner: Result<OwnerStatus, String>,
        draft: &str,
    ) -> Result<String, String> {
        let text = draft.trim();
        if text.is_empty() {
            return Err("请输入剪辑要求。".to_owned());
        }
        let data = self.data(owner);
        if !data.can_send {
            let reason = if !data.error.is_empty() {
                data.error
            } else if !data.missing.is_empty() {
                data.missing
            } else {
                data.detail
            };
            return Err(reason.to_string());
        }
        Ok(text.to_owned())
    }

    /// Native UI-thread publication only. It does not touch conversation
    /// storage or the current project and is safe in read-only recovery.
    pub fn publish(&self, app: &App, owner: Result<OwnerStatus, String>) {
        let can_authorize = owner
            .as_ref()
            .is_ok_and(|status| status.phase == Phase::Ready)
            && self.readiness.runtime_ready
            && self.readiness.credentials_ready
            && !self.task.running
            && !self.readiness.provider.is_empty()
            && !self.readiness.model.is_empty();
        let call = app.global::<AgentCallUi>();
        call.set_allowed(self.readiness.calls_authorized);
        call.set_can_authorize(can_authorize);
        call.set_limits(if self.call_limits.is_empty() {
            "每次任务最多4次模型请求；请先配置模型输出上限。".into()
        } else {
            self.call_limits.as_str().into()
        });
        let ui = app.global::<AgentUi>();
        ui.set_status(self.data(owner));
        ui.set_messages(ModelRc::from(Rc::new(VecModel::from(
            self.messages.clone(),
        ))));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owner(phase: Phase) -> Result<OwnerStatus, String> {
        Ok(OwnerStatus {
            generation: 7,
            phase,
            active_operations: 0,
        })
    }

    fn configured() -> AgentPane {
        AgentPane {
            readiness: AgentReadiness {
                runtime_ready: true,
                provider: "explicit-test-channel".to_owned(),
                model: "explicit-test-model".to_owned(),
                credentials_ready: true,
                calls_authorized: true,
            },
            ..AgentPane::default()
        }
    }

    #[test]
    fn every_missing_condition_blocks_native_submission() {
        let mut absent = Vec::new();
        let mut pane = configured();
        pane.readiness.runtime_ready = false;
        absent.push(pane);
        let mut pane = configured();
        pane.readiness.provider.clear();
        absent.push(pane);
        let mut pane = configured();
        pane.readiness.model = "  ".to_owned();
        absent.push(pane);
        let mut pane = configured();
        pane.readiness.credentials_ready = false;
        absent.push(pane);
        let mut pane = configured();
        pane.readiness.calls_authorized = false;
        absent.push(pane);
        absent.push(AgentPane::default());
        for pane in absent {
            let data = pane.data(owner(Phase::Ready));
            assert!(!data.can_send);
            assert!(!data.missing.is_empty());
            assert!(
                pane.validate_submission(owner(Phase::Ready), "添加两段字幕")
                    .is_err()
            );
        }
        assert_eq!(
            configured().validate_submission(owner(Phase::Ready), "  添加两段字幕  "),
            Ok("添加两段字幕".to_owned())
        );
    }

    #[test]
    fn a_stale_enabled_button_cannot_submit_after_ownership_is_lost() {
        let pane = configured();
        assert!(pane.data(owner(Phase::Ready)).can_send);
        for phase in [Phase::RecoveryRequired, Phase::Draining, Phase::Released] {
            assert!(pane.validate_submission(owner(phase), "添加字幕").is_err());
        }
        assert!(
            pane.validate_submission(Err("会话由另一实例使用".into()), "添加字幕")
                .is_err()
        );
        assert!(!pane.data(owner(Phase::Draining)).can_stop);
        assert!(!pane.data(owner(Phase::Released)).can_stop);
    }

    #[test]
    fn explicit_discard_is_a_native_recovery_action_without_send_or_fee_authority() {
        let mut pane = configured();
        let mut runtime = crate::embedded::RuntimeView {
            loaded: true,
            phase: "recovery".into(),
            snapshot: Some(serde_json::json!({
                "snapshotId":"native-review", "projectPath":"native-project",
                "schemaVersion":2, "hasInterruptedWork":true,
                "canDiscardInterrupted":true,
                "blockedReason":"存在未完成任务；只允许明确放弃本工程旧任务。",
            })),
            ..crate::embedded::RuntimeView::default()
        };
        pane.apply_runtime(&runtime);
        let recovery = pane.data(owner(Phase::RecoveryRequired));
        assert!(recovery.can_discard);
        assert!(recovery.can_review);
        assert!(!recovery.can_continue);
        assert!(!recovery.can_send);
        assert!(!pane.readiness.calls_authorized);
        assert!(!pane.data(owner(Phase::Ready)).can_discard);
        assert!(!pane.data(owner(Phase::Draining)).can_discard);
        assert!(!pane.data(owner(Phase::Released)).can_discard);
        assert!(!pane.data(Err("会话所有权已失效。".into())).can_discard);
        runtime.snapshot.as_mut().unwrap()["canDiscardInterrupted"] = serde_json::json!(false);
        pane.apply_runtime(&runtime);
        assert!(!pane.data(owner(Phase::RecoveryRequired)).can_discard);
        runtime.phase = "activating".into();
        runtime.snapshot.as_mut().unwrap()["canDiscardInterrupted"] = serde_json::json!(true);
        pane.apply_runtime(&runtime);
        assert!(!pane.data(owner(Phase::RecoveryRequired)).can_discard);
    }

    #[test]
    fn an_idle_open_database_does_not_look_like_a_running_task() {
        let pane = configured();
        let data = pane.data(Ok(OwnerStatus {
            generation: 7,
            phase: Phase::Ready,
            active_operations: 1,
        }));
        assert!(data.can_send);
        assert!(!data.busy);
        assert!(!data.has_progress);
        assert!(data.can_stop);
        let draining = pane.data(Ok(OwnerStatus {
            generation: 7,
            phase: Phase::Draining,
            active_operations: 1,
        }));
        assert!(draining.busy);
        assert!(!draining.can_send);
    }

    #[test]
    fn running_or_failed_work_disables_send_but_keeps_stop_available() {
        let mut pane = configured();
        pane.task.running = true;
        assert!(pane.data(owner(Phase::Ready)).can_stop);
        assert!(
            pane.validate_submission(owner(Phase::Ready), "第二次请求")
                .is_err()
        );
        pane.task.running = false;
        pane.error = "服务返回错误".to_owned();
        assert!(!pane.data(owner(Phase::Ready)).can_send);
        assert!(pane.data(owner(Phase::Ready)).can_stop);
        assert!(
            pane.validate_submission(owner(Phase::Ready), "再次请求")
                .is_err()
        );
    }

    #[test]
    fn only_real_bounded_task_progress_is_displayed() {
        let mut pane = configured();
        pane.task.running = true;
        for progress in [
            None,
            Some(f32::NAN),
            Some(f32::INFINITY),
            Some(-0.1),
            Some(1.1),
        ] {
            pane.task.progress = progress;
            assert!(!pane.data(owner(Phase::Ready)).has_progress);
        }
        pane.task.progress = Some(0.25);
        let data = pane.data(owner(Phase::Ready));
        assert!(data.has_progress);
        assert_eq!(data.progress, 0.25);
        pane.task.running = false;
        assert!(!pane.data(owner(Phase::Ready)).has_progress);
        assert!(
            !pane
                .data(Ok(OwnerStatus {
                    generation: 7,
                    phase: Phase::Ready,
                    active_operations: 1,
                }))
                .has_progress
        );
    }
}
