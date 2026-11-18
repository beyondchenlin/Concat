// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native readiness, recovery and task presentation for the embedded runtime.
//! This owns no model client, scheduler, persistence or editable project.
//! Visible messages are bounded projections of the original persisted session.

use concat_agent::{OwnerStatus, Phase};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::rc::Rc;

use crate::host::Shell;
use crate::ui::{
    AgentCallUi, AgentMessageData, AgentStatusData, AgentSuggestion, AgentUi, App, ScriptUi,
};

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
    /// Explicit authorization for the selected channel, without a task-wide budget.
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
    references: Vec<MediaReference>,
    pending_attachments: Vec<PendingAttachment>,
    completion: Option<CompletionContext>,
}

#[derive(Clone)]
struct CompletionContext {
    draft: String,
    start: usize,
    end: usize,
    marker: char,
    cursor: usize,
}

impl CompletionContext {
    fn matches(&self, draft: &str, cursor: usize) -> bool {
        self.draft == draft && self.cursor == cursor
    }
}

struct PendingAttachment {
    path: String,
    session: u64,
    started: std::time::Instant,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct MediaReference {
    id: String,
    name: String,
    session: u64,
    project_id: String,
    project_path: String,
}

impl MediaReference {
    fn selected(session: &concat_host::session::Session, id: &str, name: &str) -> Self {
        Self {
            id: id.into(),
            name: name.replace(['\r', '\n'], " "),
            session: session.session_id(),
            project_id: session.project_identity().into(),
            project_path: session.path().into(),
        }
    }

    // Only an explicit selection renews a chip after reopening its project.
    fn add_to(self, references: &mut Vec<Self>) {
        if let Some(previous) = references.iter_mut().find(|previous| {
            previous.id == self.id
                && previous.project_id == self.project_id
                && previous.project_path == self.project_path
        }) {
            *previous = self;
        } else {
            references.push(self);
        }
    }
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
            needs_model_setup: self.readiness.provider.trim().is_empty()
                || self.readiness.model.trim().is_empty()
                || !self.readiness.credentials_ready,
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
        let call = app.global::<AgentCallUi>();
        call.set_allowed(self.readiness.calls_authorized);
        // Pressing Send now grants the current model request. The separate
        // pre-send permission button is unnecessary; explicit revoke remains.
        call.set_can_authorize(false);
        let ui = app.global::<AgentUi>();
        if ui.get_script_picker() {
            let count = app.global::<ScriptUi>().get_versions().row_count();
            if usize::try_from(ui.get_script_index()).map_or(true, |index| index >= count) {
                ui.set_script_index(0);
            }
        }
        ui.set_status(self.data(owner));
        if !ui.get_messages().iter().eq(self.messages.iter().cloned()) {
            ui.set_messages(ModelRc::from(Rc::new(VecModel::from(
                self.messages.clone(),
            ))));
        }
    }
}

/// Locate the token at a UTF-8 caret; preserve text after that token.
fn completion_token(text: &str, cursor: usize) -> Option<(usize, usize, char, &str)> {
    if cursor > text.len() || !text.is_char_boundary(cursor) {
        return None;
    }
    let prefix = &text[..cursor];
    let tail = prefix
        .char_indices()
        .rev()
        .find(|(_, c)| c.is_whitespace())
        .map_or(0, |(i, c)| i + c.len_utf8());
    let (offset, marker) = prefix[tail..]
        .char_indices()
        .rev()
        .find(|(_, c)| *c == '@' || *c == '/')?;
    let start = tail + offset;
    if marker == '@'
        && text[..start]
            .chars()
            .next_back()
            .is_some_and(|c| c.is_ascii_alphanumeric())
    {
        return None;
    }
    let end = text[cursor..]
        .char_indices()
        .find(|(_, c)| c.is_whitespace())
        .map_or(text.len(), |(i, _)| cursor + i);
    Some((start, end, marker, &text[start + marker.len_utf8()..cursor]))
}

fn refresh_suggestions(text: &str, cursor: usize) {
    Shell::with(|shell, app| {
        let ui = app.global::<AgentUi>();
        let Some((start, end, marker, query)) = completion_token(text, cursor) else {
            shell.agent_pane.borrow_mut().completion = None;
            ui.set_suggestions_open(false);
            return;
        };
        shell.agent_pane.borrow_mut().completion = Some(CompletionContext {
            draft: text.to_owned(),
            start,
            end,
            marker,
            cursor,
        });
        let query = query.to_lowercase();
        let mut rows = Vec::new();
        let status;
        if marker == '@' {
            let studio = shell.studio.borrow();
            if studio.session.is_some() {
                for item in studio
                    .project()
                    .media
                    .iter()
                    .filter(|item| !item.placeholder && item.name.to_lowercase().contains(&query))
                    .take(8)
                {
                    rows.push(AgentSuggestion {
                        identity: format!(
                            "media:{}:{}",
                            studio
                                .session
                                .as_ref()
                                .expect("session checked")
                                .session_id(),
                            item.id
                        )
                        .into(),
                        label: item.name.as_str().into(),
                        detail: "工程素材".into(),
                        thumbnail: studio
                            .media
                            .thumbs
                            .get(&item.id)
                            .cloned()
                            .unwrap_or_default(),
                        kind: match item.kind {
                            concat_project::model::MediaKind::Video => "视频",
                            concat_project::model::MediaKind::Audio => "音频",
                            _ => "图片",
                        }
                        .into(),
                    });
                }
                status = if rows.is_empty() {
                    "没有匹配的工程素材；可用 + 导入。"
                } else {
                    "工程素材 · 选择要引用的素材"
                };
                // This is a connection-status row, not a fabricated result.
                rows.push(AgentSuggestion {
                    identity: "server:unavailable".into(),
                    label: "素材服务".into(),
                    detail: "未连接".into(),
                    thumbnail: slint::Image::default(),
                    kind: "服务".into(),
                });
            } else {
                status = "请先打开工程。";
            }
        } else {
            if "文案".contains(&query) || "script".contains(&query) {
                rows.push(AgentSuggestion {
                    identity: "command:script".into(),
                    label: "/文案".into(),
                    detail: "打开文案并选择版本".into(),
                    thumbnail: slint::Image::default(),
                    kind: "命令".into(),
                });
            }
            if "素材".contains(&query) || "media".contains(&query) {
                rows.push(AgentSuggestion {
                    identity: "command:media".into(),
                    label: "/素材".into(),
                    detail: "搜索工程素材".into(),
                    thumbnail: slint::Image::default(),
                    kind: "命令".into(),
                });
            }
            if "模型".contains(&query) || "model".contains(&query) {
                rows.push(AgentSuggestion {
                    identity: "command:model".into(),
                    label: "/模型".into(),
                    detail: "打开模型设置".into(),
                    thumbnail: slint::Image::default(),
                    kind: "命令".into(),
                });
            }
            status = if rows.is_empty() {
                "没有匹配的命令。"
            } else {
                "命令"
            };
        }
        ui.set_suggestion_index(0);
        ui.set_suggestions(ModelRc::from(Rc::new(VecModel::from(rows))));
        ui.set_suggestion_status(status.into());
        ui.set_suggestions_open(true);
    });
}

impl AgentPane {
    pub(crate) fn queue_attachments(
        &mut self,
        session: u64,
        paths: &[std::path::PathBuf],
        app: &App,
    ) {
        self.pending_attachments
            .extend(paths.iter().map(|path| PendingAttachment {
                path: path.to_string_lossy().into_owned(),
                session,
                started: std::time::Instant::now(),
            }));
        app.global::<AgentUi>().set_attachment_pending(true);
        app.global::<AgentUi>()
            .set_attachment_status("正在导入并引用素材…".into());
        crate::agent::window::schedule_attachment_watch();
    }

    pub(crate) fn collect_attachments(&mut self, studio: &crate::studio::Studio, app: &App) {
        if self.pending_attachments.is_empty() {
            return;
        }
        let current = studio.session.as_ref();
        let mut remaining = Vec::new();
        let mut failed = false;
        for pending in self.pending_attachments.drain(..) {
            let Some(session) = current.filter(|s| s.session_id() == pending.session) else {
                failed = true;
                continue;
            };
            if let Some(item) = studio
                .project()
                .media
                .iter()
                .find(|item| item.path == pending.path && !item.placeholder)
            {
                MediaReference::selected(session, &item.id, &item.name)
                    .add_to(&mut self.references);
            } else if pending.started.elapsed() > std::time::Duration::from_secs(120) {
                failed = true;
            } else {
                remaining.push(pending);
            }
        }
        self.pending_attachments = remaining;
        app.global::<AgentUi>()
            .set_attachment_pending(!self.pending_attachments.is_empty());
        self.publish_references(app);
        app.global::<AgentUi>().set_attachment_status(if failed {
            "部分素材未能引用或工程已切换，请检查素材库。".into()
        } else if self.pending_attachments.is_empty() {
            "".into()
        } else {
            "正在导入并引用素材…".into()
        });
    }

    fn publish_references(&self, app: &App) {
        let rows = self
            .references
            .iter()
            .map(|reference| AgentSuggestion {
                identity: format!("{}:{}", reference.session, reference.id).into(),
                label: reference.name.chars().take(14).collect::<String>().into(),
                detail: "工程素材".into(),
                thumbnail: slint::Image::default(),
                kind: "素材".into(),
            })
            .collect::<Vec<_>>();
        app.global::<AgentUi>()
            .set_references(ModelRc::from(Rc::new(VecModel::from(rows))));
    }

    pub(crate) fn resolve_references(
        &self,
        studio: &crate::studio::Studio,
    ) -> Result<(Vec<String>, String), String> {
        if !self.pending_attachments.is_empty() {
            return Err("素材仍在导入，请等待完成后发送。".into());
        }
        if self.references.is_empty() {
            return Ok((Vec::new(), String::new()));
        }
        let current = studio.session.as_ref().ok_or("请先打开工程。")?;
        let mut keys = Vec::new();
        let mut values = Vec::new();
        for reference in &self.references {
            if current.session_id() != reference.session
                || !studio.project().media.iter().any(|item| {
                    item.id == reference.id
                        && !item.placeholder
                        && item.name.replace(['\r', '\n'], " ") == reference.name
                })
            {
                return Err("素材引用属于旧工程或已失效，请移除后重新选择。".into());
            }
            keys.push(format!("{}:{}", reference.session, reference.id));
            values.push(serde_json::json!({"mediaId": reference.id, "name": reference.name}));
        }
        Ok((
            keys,
            format!(
                "\n\n用户引用的当前工程素材：{}",
                serde_json::Value::Array(values)
            ),
        ))
    }

    fn submission_unchanged(&self, current_draft: &str, draft: &str, keys: &[String]) -> bool {
        current_draft == draft
            && self.pending_attachments.is_empty()
            && self
                .references
                .iter()
                .map(|r| format!("{}:{}", r.session, r.id))
                .eq(keys.iter().cloned())
    }

    pub(crate) fn clear_submission_if_unchanged(
        &mut self,
        draft: &str,
        keys: &[String],
        app: &App,
    ) {
        let ui = app.global::<AgentUi>();
        if self.submission_unchanged(ui.get_draft().as_str(), draft, keys) {
            // Clear the accepted draft and references as one snapshot. A new
            // attachment or edited reference must retain the next draft too.
            self.references.clear();
            self.completion = None;
            ui.set_suggestions_open(false);
            ui.set_draft("".into());
            self.publish_references(app);
        }
    }
}

pub(crate) fn install_composer(app: &App) {
    let ui = app.global::<AgentUi>();
    ui.on_cursor_moved(|cursor| {
        Shell::with(|_, app| {
            let ui = app.global::<AgentUi>();
            if ui.get_draft_cursor() != cursor {
                ui.set_suggestions_open(false);
            }
            // Do not borrow AgentPane here: programmatic draft changes can
            // synchronously emit this callback while that pane is borrowed.
            ui.set_draft_cursor(cursor);
        });
    });
    ui.on_draft_edited(|text, cursor| {
        let cursor = usize::try_from(cursor).unwrap_or(usize::MAX);
        Shell::with(|_, app| {
            let ui = app.global::<AgentUi>();
            ui.set_script_picker(false);
            ui.set_draft_cursor(cursor.min(i32::MAX as usize) as i32);
        });
        refresh_suggestions(text.as_str(), cursor);
        // Start the desktop agent only after the user actually types a
        // request. This has no idle polling and does not run on Android.
        #[cfg(all(target_os = "windows", not(feature = "bk-cat-native-probe")))]
        if !text.trim().is_empty() {
            crate::embedded::activate_from_user_intent();
        }
    });
    ui.on_trigger_requested(|marker| {
        if marker != "@" && marker != "/" {
            return;
        }
        Shell::with(|_, app| {
            let ui = app.global::<AgentUi>();
            ui.set_script_picker(false);
            let mut draft = ui.get_draft().to_string();
            let cursor = usize::try_from(ui.get_draft_cursor())
                .ok()
                .filter(|c| *c <= draft.len() && draft.is_char_boundary(*c))
                .unwrap_or(draft.len());
            if draft[cursor..].starts_with(|c: char| !c.is_whitespace()) {
                draft.insert(cursor, ' ');
            }
            draft.insert_str(cursor, marker.as_str());
            ui.set_draft(draft.as_str().into());
            ui.set_draft_cursor((cursor + 1) as i32);
            refresh_suggestions(&draft, cursor + 1);
        });
    });
    ui.on_cancel_attachments(|| {
        Shell::with(|shell, app| {
            shell.agent_pane.borrow_mut().pending_attachments.clear();
            let ui = app.global::<AgentUi>();
            ui.set_attachment_pending(false);
            ui.set_attachment_status("已取消引用；素材导入可能仍在继续。".into());
        });
    });
    ui.on_attach_requested(|| {
        let mut selected_session = None;
        Shell::with(|shell, _| {
            selected_session = shell
                .studio
                .borrow()
                .session
                .as_ref()
                .map(|session| session.session_id())
        });
        let Some(selected_session) = selected_session else {
            return;
        };
        crate::host::pick_files_then(
            "添加素材".into(),
            Some((
                "视频、音频和图片".into(),
                &[
                    "mp4", "mov", "mkv", "webm", "avi", "m4v", "mp3", "wav", "aac", "m4a", "flac",
                    "ogg", "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "svg",
                ],
            )),
            move |studio, app, _, paths| {
                if !studio
                    .session
                    .as_ref()
                    .is_some_and(|session| session.session_id() == selected_session)
                {
                    app.global::<AgentUi>()
                        .set_attachment_status("工程已切换，请重新选择素材。".into());
                    return;
                }
                Shell::with(|shell, _| {
                    shell
                        .agent_pane
                        .borrow_mut()
                        .queue_attachments(selected_session, &paths, app)
                });
                studio.handle(crate::panes::Msg::Media(
                    crate::panes::media_bin::MediaMsg::Import(paths),
                ));
            },
        );
    });
    ui.on_reference_removed(|identity| {
        Shell::with(|shell, app| {
            let mut pane = shell.agent_pane.borrow_mut();
            pane.references.retain(|reference| {
                format!("{}:{}", reference.session, reference.id) != identity.as_str()
            });
            pane.publish_references(&app);
        });
    });
    ui.on_suggestion_chosen(|identity| {
        Shell::with(|shell, app| {
            let ui = app.global::<AgentUi>();
            if identity == "server:unavailable" {
                ui.set_suggestion_status("素材服务尚未接通，暂时不能搜索。".into());
                return;
            }
            let draft = ui.get_draft().to_string();
            let Some(context) = shell.agent_pane.borrow().completion.clone().filter(|c| {
                usize::try_from(ui.get_draft_cursor()).is_ok_and(|cursor| c.matches(&draft, cursor))
            }) else {
                ui.set_suggestions_open(false);
                return;
            };
            let CompletionContext {
                start, end, marker, ..
            } = context;
            if marker == '/' && identity == "command:script" {
                ui.set_draft(format!("{}{}", &draft[..start], &draft[end..]).into());
                ui.set_draft_cursor(start as i32);
                ui.set_suggestions_open(false);
                ui.set_script_index(0);
                ui.set_script_picker(true);
                app.global::<ScriptUi>().invoke_open_requested();
            } else if marker == '/' && identity == "command:media" {
                let next = format!("{}@{}", &draft[..start], &draft[end..]);
                ui.set_draft(next.as_str().into());
                ui.set_draft_cursor((start + 1) as i32);
                refresh_suggestions(&next, start + 1);
            } else if marker == '/' && identity == "command:model" {
                ui.set_draft(format!("{}{}", &draft[..start], &draft[end..]).into());
                ui.set_draft_cursor(start as i32);
                ui.set_suggestions_open(false);
                app.global::<crate::ui::AgentSettingsUi>()
                    .invoke_open_requested();
            } else if marker == '@'
                && let Some((session, id)) = identity
                    .strip_prefix("media:")
                    .and_then(|value| value.split_once(':'))
            {
                let studio = shell.studio.borrow();
                if let Some(current) = studio.session.as_ref()
                    && session.parse::<u64>() == Ok(current.session_id())
                    && let Some(item) = studio
                        .project()
                        .media
                        .iter()
                        .find(|item| item.id == id && !item.placeholder)
                {
                    let mut pane = shell.agent_pane.borrow_mut();
                    MediaReference::selected(current, &item.id, &item.name)
                        .add_to(&mut pane.references);
                    pane.publish_references(&app);
                    ui.set_draft(format!("{}{}", &draft[..start], &draft[end..]).into());
                    ui.set_draft_cursor(start as i32);
                    ui.set_suggestions_open(false);
                } else {
                    ui.set_suggestion_status("素材已不存在，请重新选择。".into());
                }
            }
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn model_authorization_survives_a_running_and_completed_task_but_not_owner_loss() {
        let mut pane = AgentPane::default();
        pane.readiness.calls_authorized = true;
        let mut runtime = crate::embedded::RuntimeView {
            phase: "ready".into(),
            task_running: true,
            ..Default::default()
        };
        pane.apply_runtime(&runtime);
        assert!(pane.readiness.calls_authorized);
        runtime.task_running = false;
        pane.apply_runtime(&runtime);
        assert!(pane.readiness.calls_authorized);
        runtime.closed = true;
        pane.apply_runtime(&runtime);
        assert!(!pane.readiness.calls_authorized);
    }

    #[test]
    fn moving_caret_invalidates_an_unchanged_completion_draft() {
        let draft = "use @video after";
        let cursor = "use @video".len();
        let (start, end, marker, _) = completion_token(draft, cursor).unwrap();
        let context = CompletionContext {
            draft: draft.into(),
            start,
            end,
            marker,
            cursor,
        };
        assert!(context.matches(draft, cursor));
        assert!(!context.matches(draft, draft.len()));
        assert!(!context.matches(draft, start));
        assert!(!context.matches("new draft", cursor));
    }

    #[test]
    fn media_command_completion_uses_the_replacement_caret() {
        let draft = "use /media after";
        let (start, end, _, _) = completion_token(draft, "use /media".len()).unwrap();
        let replacement = format!("{}@{}", &draft[..start], &draft[end..]);
        let cursor = start + 1;
        let (start, end, marker, query) = completion_token(&replacement, cursor).unwrap();
        assert_eq!(replacement, "use @ after");
        assert_eq!(query, "");
        let context = CompletionContext {
            draft: replacement.clone(),
            start,
            end,
            marker,
            cursor,
        };
        assert!(context.matches(&replacement, cursor));
        assert!(!context.matches(&replacement, "use /media".len()));
    }

    #[test]
    fn submission_receipt_preserves_draft_when_references_or_imports_change() {
        let reference = MediaReference {
            id: "m1".into(),
            name: "a.mp4".into(),
            session: 1,
            project_id: "p".into(),
            project_path: "folder".into(),
        };
        let mut pane = AgentPane::default();
        pane.references.push(reference.clone());
        let keys = vec!["1:m1".to_owned()];
        assert!(pane.submission_unchanged("edit", "edit", &keys));
        assert!(!pane.submission_unchanged("new edit", "edit", &keys));
        pane.references.clear();
        assert!(!pane.submission_unchanged("edit", "edit", &keys));
        pane.references.push(reference.clone());
        pane.references.push(MediaReference {
            id: "m2".into(),
            ..reference
        });
        assert!(!pane.submission_unchanged("edit", "edit", &keys));
        pane.references.pop();
        pane.pending_attachments.push(PendingAttachment {
            path: "new.mp4".into(),
            session: 1,
            started: std::time::Instant::now(),
        });
        assert!(!pane.submission_unchanged("edit", "edit", &keys));
    }

    #[test]
    fn reselecting_media_in_a_reopened_project_renews_just_its_chip() {
        let first = MediaReference {
            id: "m1".into(),
            name: "bars.mp4".into(),
            session: 1,
            project_id: "original".into(),
            project_path: "original-folder".into(),
        };
        let copied = MediaReference {
            project_path: "copied-folder".into(),
            session: 2,
            ..first.clone()
        };
        let reopened = MediaReference {
            session: 3,
            ..first.clone()
        };
        let mut chips = vec![first, copied.clone()];
        reopened.clone().add_to(&mut chips);
        reopened.clone().add_to(&mut chips);
        assert_eq!(chips, [reopened, copied]);
    }

    #[test]
    fn completion_uses_utf8_caret_and_preserves_suffix() {
        let text = "请参考 @海边 后续";
        let caret = "请参考 @海".len();
        let (start, end, marker, query) = completion_token(text, caret).unwrap();
        assert_eq!(&text[..start], "请参考 ");
        assert_eq!(&text[end..], " 后续");
        assert_eq!((marker, query), ('@', "海"));
        assert!(completion_token("hello@example.com", 17).is_none());
        assert!(completion_token("字@片", 1).is_none());
    }

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
        // Sending a prompt is the explicit model-call authorization.
        // The preceding UI should not require a separate permission click.
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
    fn send_button_needs_no_separate_model_consent_click() {
        let mut pane = configured();
        pane.readiness.calls_authorized = false;
        assert!(pane.data(owner(Phase::Ready)).can_send);
        assert!(
            pane.validate_submission(owner(Phase::Ready), "在当前工程添加字幕")
                .is_ok()
        );
        // Native submit_from_ui must grant and recheck model authority before
        // queueing a task. This rendering-only check is never its substitute.
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
