// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native per-submission authority. Pi owns conversations, tasks and scheduling;
//! this module only binds one explicit UI action to its native project and limits.

use std::path::Path;
use std::sync::{Arc, Mutex};

use concat_agent::configuration::{ConfigurationSnapshot, ConfigurationStore, ModelChannel};
use concat_agent::conversations::{
    ConversationBindings, inspect_latest, inspect_project_conversations,
};
use concat_agent::credentials::{CredentialStore, platform_store};
use concat_agent::network::{ModelNetwork, ModelRequest, runtime_configuration_for};
use concat_agent::operations::Invocation;
use concat_agent::scripts::{ScriptStore, VersionBinding};
use serde_json::{Value, json};

use super::AgentBridge;
use crate::embedded::random_capability;
use crate::live::CurrentProject;
use crate::panes::script::GenerationRequest;
use crate::studio::Studio;

#[derive(Clone, Copy, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum TaskMode {
    Chat,
    Script,
    Edit,
}
impl TaskMode {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Chat => "chat",
            Self::Script => "script",
            Self::Edit => "edit",
        }
    }

    /// Keep model-facing instructions aligned with the native, fail-closed
    /// tool gate. A permitted edit is not a read-only conversation.
    fn authority_instructions(self) -> &'static str {
        match self {
            Self::Chat => {
                "对话（只读）：仅答疑和分析，不能修改工程；如需执行修改，请用户选择剪辑模式"
            }
            Self::Script => "文案（只读）：仅生成候选文案，不得直接修改工程或时间线",
            Self::Edit => {
                "剪辑（受控写入）：仅按用户明确要求修改当前工程；写入必须经过edit_apply_guarded、原生审批与工程修订校验，禁止绕过权限保护"
            }
        }
    }
}

/// A direct edit must never invoke the script-based full-video template.
/// Inspect the entire command tree so a nested batch cannot bypass the gate.
fn contains_main_template(value: &Value) -> bool {
    match value {
        Value::Object(object) => {
            object.get("op").and_then(Value::as_str) == Some("applySingleMainTemplate")
                || object.values().any(contains_main_template)
        }
        Value::Array(items) => items.iter().any(contains_main_template),
        _ => false,
    }
}

/// Immutable per-task language policy; independent from permissions and models.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub(crate) struct TaskLanguages {
    pub(crate) interface_language: String,
    pub(crate) reply_language: String,
    pub(crate) content_language: String,
}

impl TaskLanguages {
    fn validate(&self) -> Result<(), String> {
        let valid = |value: &str| crate::i18n::BUILT_IN.iter().any(|(code, _)| *code == value);
        if !valid(&self.interface_language)
            || !valid(&self.content_language)
            || (self.reply_language != "auto" && !valid(&self.reply_language))
        {
            return Err("agent.errorRecoverySnapshotInvalid".into());
        }
        Ok(())
    }
    pub(crate) fn capture(content: Option<&str>) -> Self {
        let interface = crate::i18n::current();
        let valid = |value: &str| crate::i18n::BUILT_IN.iter().any(|(code, _)| *code == value);
        let interface = if valid(&interface) {
            interface
        } else {
            "en".into()
        };
        Self {
            reply_language: "auto".into(),
            content_language: content
                .filter(|value| valid(value))
                .unwrap_or(&interface)
                .to_owned(),
            interface_language: interface,
        }
    }
}

impl Default for TaskLanguages {
    fn default() -> Self {
        Self {
            interface_language: "zh-Hans".into(),
            reply_language: "zh-Hans".into(),
            content_language: "zh-Hans".into(),
        }
    }
}

/// Captured on the native UI thread before any asynchronous preparation.
#[derive(Clone)]
pub(crate) struct SubmissionIntent {
    pub languages: TaskLanguages,
    /// Native-captured next-task selection, independent of the app default.
    pub model: Option<ModelChannel>,
    pub id: String,
    pub project: CurrentProject,
    pub mode: TaskMode,
    pub input: String,
    /// Original editor draft and reference snapshot, for clearing only after acceptance.
    pub ui_draft: String,
    pub reference_keys: Vec<String>,
    pub selected: Option<VersionBinding>,
    pub generation: Option<GenerationRequest>,
}

impl SubmissionIntent {
    /// Display text is captured before native context is added. It is metadata
    /// on the same original Pi input entry, never a second executable request.
    fn runtime_payload(&self, input: &str, permit: &str, conversation: Option<u64>) -> Value {
        let mut payload = json!({
            "method": "runtime.submit", "taskId": self.id, "networkPermit": permit,
            "input": input, "mode": self.mode.as_str(), "languages": self.languages,
        });
        // Internal callers predating display metadata have an empty UI draft.
        // Their complete model input remains the compatible display fallback.
        if !self.ui_draft.trim().is_empty() {
            payload["displayText"] = json!(self.ui_draft);
        }
        if let Some(conversation) = conversation {
            payload["conversationId"] = json!(conversation);
        }
        payload
    }
}

/// Only local input bounds are recoverable. Ownership, project, storage and
/// transport failures retain the existing fail-closed lifecycle.
#[derive(Debug)]
pub(crate) enum SubmissionError {
    RejectedInput(&'static str),
    Failed(String),
}

impl std::fmt::Display for SubmissionError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(match self {
            Self::RejectedInput(message) => message,
            Self::Failed(message) => message,
        })
    }
}

impl From<String> for SubmissionError {
    fn from(message: String) -> Self {
        Self::Failed(message)
    }
}

impl From<&str> for SubmissionError {
    fn from(message: &str) -> Self {
        Self::Failed(message.to_owned())
    }
}

struct PreparedInput {
    project: CurrentProject,
    snapshot: ConfigurationSnapshot,
    input: String,
    conversation: Option<u64>,
}

/// Non-secret choices captured once before original Pi admission.
#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DurableTaskMetadata {
    version: u32,
    mode: TaskMode,
    languages: TaskLanguages,
    model: ModelChannel,
    model_configuration: Value,
    input: String,
    ui_draft: String,
    selected: Option<VersionBinding>,
    generation_snapshot: Option<Value>,
}
impl DurableTaskMetadata {
    fn validate(&self) -> Result<(), String> {
        self.languages.validate()?;
        let model = &self.model_configuration;
        if self.version != 1
            || self.input.trim().is_empty()
            || self.input.len() > 512 * 1024
            || self.ui_draft.len() > 256 * 1024
            || (self.mode == TaskMode::Script) != self.generation_snapshot.is_some()
            || model["id"].as_str() != Some(self.model.id.as_str())
            || model["version"].as_str() != Some(self.model.version.to_string().as_str())
            || model["providerId"].as_str() != Some(self.model.provider_id.as_str())
            || model["protocol"].as_str() != Some(self.model.protocol.as_str())
            || model["endpoint"].as_str() != Some(self.model.endpoint.as_str())
            || model["model"].as_str() != Some(self.model.model.as_str())
            || model["requestTimeoutMs"].as_u64() != Some(u64::from(self.model.request_timeout_ms))
            || model["maxOutputTokens"].as_u64() != Some(u64::from(self.model.max_output_tokens))
        {
            return Err("agent.errorRecoverySnapshotInvalid".into());
        }
        Ok(())
    }
}

/// Native recovery scope; the original durable submission is never replaced.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct RecoveryTaskScope {
    pub(crate) snapshot: concat_agent::task_snapshots::TaskSnapshot,
    pub(crate) submission_id: u64,
    pub(crate) task_ids: Vec<u64>,
    invocations: Vec<concat_agent::operations::Invocation>,
    metadata: DurableTaskMetadata,
}
impl RecoveryTaskScope {
    pub(crate) fn value(&self) -> Value {
        json!({"taskId":self.snapshot.request_id, "conversationId":self.snapshot.conversation_id,
            "submissionId":self.submission_id, "taskIds":self.task_ids, "mode":self.metadata.mode.as_str(),
            "languages":self.metadata.languages, "modelConfiguration":self.metadata.model_configuration})
    }
}

/// UI restoration uses this native object, never event-supplied configuration.
#[derive(Clone)]
pub(crate) struct RecoveredTask {
    pub(crate) id: String,
    pub(crate) project: CurrentProject,
    pub(crate) mode: TaskMode,
    pub(crate) languages: TaskLanguages,
    pub(crate) model: ModelChannel,
    pub(crate) input: String,
    pub(crate) ui_draft: String,
    pub(crate) selected: Option<VersionBinding>,
    pub(crate) generation_snapshot: Option<Value>,
    pub(crate) conversation_id: u64,
    pub(crate) submission_id: u64,
}

/// A previous edit counts only when its original tool identity belongs to this
/// same submitted input and the native journal proves a changed, saved version
/// whose exact document still occupies the current project file.
fn recovered_saved_change(
    scope: &RecoveryTaskScope,
    review: &concat_agent::operations::RecoveryReview,
) -> bool {
    use concat_agent::operations::RecoveryState;
    review.project_path == scope.snapshot.project_path
        && review.operations.iter().any(|operation| {
            operation.current_project
                && operation.project_path == scope.snapshot.project_path
                && operation.invocation.conversation_id == scope.snapshot.conversation_id
                && scope.invocations.contains(&operation.invocation)
                && matches!(
                    operation.state,
                    RecoveryState::SavedNotNotified | RecoveryState::Notified
                )
                && operation.receipt.is_some()
                && operation.saved.is_some()
                && operation.current_file_matches_save
                && operation
                    .result_revision
                    .is_some_and(|revision| revision > operation.expected_revision)
        })
}

#[derive(Clone)]
struct AuthorizedTask {
    id: String,
    permit: String,
    project: CurrentProject,
    mode: TaskMode,
    selected: Option<VersionBinding>,
    conversation: Option<u64>,
    expected_conversation: Option<u64>,
    changed_by_native_tool: bool,
    metadata: Option<DurableTaskMetadata>,
    recovered_submission: Option<u64>,
    network: Arc<ModelNetwork>,
}

pub(crate) struct NativeTasks {
    bridge: Arc<AgentBridge>,
    store: ConfigurationStore,
    startup: Mutex<Option<ConfigurationSnapshot>>,
    task: Mutex<Option<AuthorizedTask>>,
    // Stop must not wait for a tool call or a blocked network read holding
    // NativeSession's serial database state. This slot holds no slow work.
    network: Mutex<Option<Arc<ModelNetwork>>>,
}

impl NativeTasks {
    /// Read original task identity and its immutable native choices. This
    /// cannot initialize files, open the vault, or grant model/tool authority.
    pub(crate) fn recovery_task_scope(
        &self,
        project: &CurrentProject,
    ) -> Result<Option<RecoveryTaskScope>, String> {
        let bindings = self.recovery_conversations(project)?;
        let work =
            concat_agent::sqlite::inspect_interrupted_work(&self.bridge.owner, &self.bridge.token)
                .map_err(|_| "agent.errorRecoverySnapshotInvalid")?;
        if work.tasks.is_empty() && work.submissions.is_empty() {
            return Ok(None);
        }
        if work.submissions.len() > 1 {
            return Err("agent.errorRecoveryScope".into());
        }
        let submission = work
            .submissions
            .first()
            .or(work.draining_submission.as_ref())
            .ok_or("agent.errorRecoveryScope")?;
        if submission["type"].as_str() != Some("input")
            || !matches!(
                submission["status"].as_str(),
                Some("queued" | "placed" | "done" | "unanswered")
            )
        {
            return Err("agent.errorRecoveryScope".into());
        }
        let submission_id = submission["id"]
            .as_u64()
            .ok_or("agent.errorRecoveryScope")?;
        let conversation = submission["conversationId"]
            .as_u64()
            .ok_or("agent.errorRecoveryScope")?;
        let request = submission["requestId"]
            .as_str()
            .ok_or("agent.errorRecoverySnapshotMissing")?;
        let snapshot =
            concat_agent::task_snapshots::inspect(&self.bridge.owner, &self.bridge.token, request)?
                .ok_or("agent.errorRecoverySnapshotMissing")?;
        if snapshot.conversation_id != conversation
            || snapshot.project_id != project.project_id
            || snapshot.project_path != project.path
            || !bindings.contains(&conversation)
        {
            return Err("agent.errorRecoveryScope".into());
        }
        let metadata: DurableTaskMetadata = serde_json::from_value(snapshot.metadata.clone())
            .map_err(|_| "agent.errorRecoverySnapshotInvalid")?;
        metadata.validate()?;
        if let Some(generation) = &metadata.generation_snapshot {
            let ticket = &generation["ticket"];
            let valid_text = |value: &Value| {
                value
                    .as_str()
                    .is_some_and(|text| !text.is_empty() && text.len() <= 4096)
            };
            if generation["schema"] != 1
                || generation
                    .as_object()
                    .is_none_or(|object| object.len() != 4)
                || generation["requestId"].as_str() != Some(request)
                || generation["prompt"]
                    .as_str()
                    .is_none_or(|prompt| prompt.is_empty() || prompt.len() > 512 * 1024)
                || ticket.as_object().is_none_or(|object| object.len() != 5)
                || ticket["projectId"].as_str() != Some(project.project_id.as_str())
                || !valid_text(&ticket["documentId"])
                || ticket["revision"].as_u64().is_none()
                || ticket["editRevision"].as_u64().is_none()
                || ticket["sha256"].as_str().is_none_or(|hash| {
                    hash.len() != 64
                        || !hash
                            .bytes()
                            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
                })
            {
                return Err("agent.errorRecoverySnapshotInvalid".into());
            }
            let scripts = ScriptStore::open_read_only(
                &self
                    .bridge
                    .dirs
                    .data
                    .join("bk-cat")
                    .join("script-workspaces"),
            )
            .map_err(|_| "agent.errorRecoverySnapshotInvalid")?;
            let current = scripts
                .load(&project.project_id)
                .map_err(|_| "agent.errorRecoverySnapshotInvalid")?
                .ok_or("agent.errorRecoverySnapshotMissing")?;
            if ticket["documentId"].as_str() != Some(current.document_id.as_str()) {
                return Err("agent.errorRecoverySnapshotInvalid".into());
            }
        }
        self.store
            .assert_connection_current(&metadata.model)
            .map_err(|_| "agent.errorRecoveryConnectionChanged")?;
        if let Some(selected) = &metadata.selected {
            if selected.project_id != project.project_id {
                return Err("agent.errorRecoveryScope".into());
            }
            let scripts = ScriptStore::open_read_only(
                &self
                    .bridge
                    .dirs
                    .data
                    .join("bk-cat")
                    .join("script-workspaces"),
            )
            .map_err(|error| error.to_string())?;
            // Preserve the selected old version; a newer hand-edited draft is
            // not substituted into the original task.
            scripts
                .resolve_binding(selected)
                .map_err(|error| error.to_string())?;
        }
        let mut task_ids = Vec::new();
        for task in &work.tasks {
            if task["conversationId"].as_u64() != Some(conversation)
                || task["abortRequested"] == true
                || task["background"] == true
            {
                return Err("agent.errorRecoveryScope".into());
            }
            let task_id = task["id"].as_u64().ok_or("agent.errorRecoveryScope")?;
            if task_id > 9_007_199_254_740_991 {
                return Err("agent.errorRecoveryScope".into());
            }
            task_ids.push(task_id);
        }
        if submission_id > 9_007_199_254_740_991 {
            return Err("agent.errorRecoveryScope".into());
        }
        let invocations = concat_agent::sqlite::inspect_submission_invocations(
            &self.bridge.owner,
            &self.bridge.token,
            conversation,
            submission_id,
        )
        .map_err(|_| "agent.errorRecoverySnapshotInvalid")?;
        Ok(Some(RecoveryTaskScope {
            snapshot,
            submission_id,
            task_ids,
            invocations,
            metadata,
        }))
    }

    /// Final native admission after original Pi's local open/review. No
    /// request is sent until the runtime subsequently calls original resume.
    pub(crate) fn prepare_recovery(
        &self,
        expected: &RecoveryTaskScope,
        project: &CurrentProject,
        review: &concat_agent::operations::RecoveryReview,
    ) -> Result<Value, String> {
        if self.recovery_task_scope(project)?.as_ref() != Some(expected) {
            return Err("agent.errorRecoveryChanged".into());
        }
        if self
            .task
            .lock()
            .map_err(|_| "agent.sessionUnavailable")?
            .is_some()
        {
            return Err("agent.errorRecoveryChanged".into());
        }
        let snapshot = self.store.load().map_err(|error| error.to_string())?;
        let network = Arc::new(ModelNetwork::authorize_selected(
            Arc::clone(&self.bridge.owner),
            self.bridge.token.clone(),
            self.store.clone(),
            snapshot,
            platform_store().map_err(|error| error.to_string())?,
            expected.metadata.model.clone(),
        )?);
        let permit = random_capability()?;
        let metadata = &expected.metadata;
        let context = json!({"project": {
            "path":project.path,"projectId":project.project_id,
            "expectedSession":project.session_id,"expectedRevision":project.revision},
            "operationRecovery":review.facts(),
            "oldPlans":"Previous write calls must not be replayed. Read the current native project before creating a new plan. Unknown outcomes are not success; an applied receipt alone is not a saved project."
        }).to_string();
        let payload = json!({"taskId":expected.snapshot.request_id, "conversationId":expected.snapshot.conversation_id,
            "submissionId":expected.submission_id, "networkPermit":permit,
            "input":metadata.input, "mode":metadata.mode.as_str(), "languages":metadata.languages,
            "modelConfiguration":metadata.model_configuration, "nativeContext":context});
        let mut task = self.task.lock().map_err(|_| "agent.sessionUnavailable")?;
        if task.is_some() {
            return Err("agent.errorRecoveryChanged".into());
        }
        *self
            .network
            .lock()
            .map_err(|_| "agent.sessionUnavailable")? = Some(Arc::clone(&network));
        *task = Some(AuthorizedTask {
            id: expected.snapshot.request_id.clone(),
            permit,
            project: project.clone(),
            mode: metadata.mode,
            selected: metadata.selected.clone(),
            expected_conversation: Some(expected.snapshot.conversation_id),
            conversation: Some(expected.snapshot.conversation_id),
            changed_by_native_tool: recovered_saved_change(expected, review),
            metadata: Some(metadata.clone()),
            recovered_submission: Some(expected.submission_id),
            network,
        });
        Ok(payload)
    }

    /// A short-lived native projection for the matching runtime.resumed event.
    pub(crate) fn recovered_task(&self) -> Option<RecoveredTask> {
        let task = self.task.lock().ok()?;
        let task = task.as_ref()?;
        let metadata = task.metadata.as_ref()?;
        Some(RecoveredTask {
            id: task.id.clone(),
            project: task.project.clone(),
            mode: task.mode,
            languages: metadata.languages.clone(),
            model: metadata.model.clone(),
            input: metadata.input.clone(),
            ui_draft: metadata.ui_draft.clone(),
            selected: metadata.selected.clone(),
            generation_snapshot: metadata.generation_snapshot.clone(),
            conversation_id: task.conversation?,
            submission_id: task.recovered_submission?,
        })
    }

    pub(crate) fn new(bridge: Arc<AgentBridge>) -> Self {
        Self {
            store: crate::panes::agent_settings::configuration_store(&bridge.dirs),
            bridge,
            startup: Mutex::new(None),
            task: Mutex::new(None),
            network: Mutex::new(None),
        }
    }

    pub(crate) fn model_configuration(&self) -> Result<Value, String> {
        let snapshot = self.store.load().map_err(|error| error.to_string())?;
        let configuration = snapshot
            .active()
            .map(|channel| runtime_configuration_for(&self.store, &snapshot, channel))
            .unwrap_or(Value::Null);
        *self
            .startup
            .lock()
            .map_err(|_| "modelSettings.configurationUnavailable")? = Some(snapshot);
        Ok(configuration)
    }

    /// Worker-only. No authority is derived from a page's proposed model or
    /// script version. The caller has already consumed the native fee action.
    pub(crate) fn prepare<T>(
        &self,
        intent: &SubmissionIntent,
        encode: impl FnOnce(Value) -> Result<T, SubmissionError>,
    ) -> Result<T, SubmissionError> {
        if intent.input.trim().is_empty()
            || intent.input.len() > 256 * 1024
            || intent.ui_draft.len() > 256 * 1024
        {
            // Admission is unchanged; the display distinguishes missing text
            // from the bounded transport error without deriving authority from it.
            return Err(SubmissionError::RejectedInput(
                if intent.input.trim().is_empty() {
                    "agent.enterRequest"
                } else {
                    "runtime.inputTooLarge"
                },
            ));
        }
        if self
            .task
            .lock()
            .map_err(|_| "agent.sessionUnavailable")?
            .is_some()
        {
            return Err("runtime.taskBusy".into());
        }
        let snapshot = self.store.load().map_err(|error| error.to_string())?;
        let selected = intent
            .model
            .as_ref()
            .or_else(|| snapshot.active())
            .ok_or("agent.errorModelRequired")?;
        self.store
            .assert_connection_current(selected)
            .map_err(|error| error.to_string())?;
        let current = self
            .bridge
            .live
            .current_project()
            .map_err(|error| error.to_string())?
            .ok_or("agent.openProject")?;
        if current != intent.project {
            return Err("runtime.stateChanged".into());
        }
        if concat_host::project_identity::read_persisted(Path::new(&current.path))?.as_deref()
            != Some(current.project_id.as_str())
        {
            return Err("agent.reopenIdentity".into());
        }
        // History and selected scripts share the existing native location
        // registry. A copied manifest must not inherit another live folder's
        // conversations merely because it carries the same persistent id.
        let (input, existing) = self.at_project_location(&current, || {
            let mut input = intent.input.clone();
            if intent.mode == TaskMode::Edit {
                if let Some(selected) = intent.selected.as_ref() {
                    if selected.project_id != current.project_id {
                        return Err("script.errorVersion".into());
                    }
                    let scripts = ScriptStore::open_read_only(
                        &self
                            .bridge
                            .dirs
                            .data
                            .join("bk-cat")
                            .join("script-workspaces"),
                    )
                    .map_err(|error| error.to_string())?;
                    if scripts
                        .bind_selected(&current.project_id)
                        .map_err(|error| error.to_string())?
                        != *selected
                    {
                        return Err("script.errorStale".into());
                    }
                    let version = scripts
                        .resolve_binding(selected)
                        .map_err(|error| error.to_string())?;
                    input = format!(
                        "原生捕获的本次剪辑依据（不可改写版本声明）：{}\n文案正文：\n{}\n\n用户要求：\n{}\n\n先读取当前工程；只能使用已实际导入且可用的素材。空时间线使用顶层applySingleMainTemplate类型化计划，由原生一次创建主视频、字幕、贴片文字、音乐和配音角色轨；已存在片段时不能使用该模板替换它们，应使用现有轨道的精确编辑。必须按真实时长生成一个精确批次；素材或轨道不具备时明确说明缺口，禁止虚构。媒体新增仅使用addMediaClipExact或受限applySingleMainTemplate，禁止自动寻找空隙、嵌套模板及通用建轨。调用一次edit_apply_guarded提交完整批次。",
                        serde_json::to_string(selected).map_err(|_| "runtime.invalidRequest")?,
                        version.content,
                        input
                    );
                } else {
                    // A direct timeline edit has no script dependency. A full
                    // one-track template still requires a selected version.
                    input = format!(
                        "本次直接编辑原生当前工程，没有已选择的文案版本。先读取project_get或timeline_read，再按用户要求精确修改已有轨道和已导入素材。禁止使用applySingleMainTemplate整片模板；如用户要按文案自动生成整片，请提示先在文案工作区保存并选定版本。媒体新增仅使用addMediaClipExact，轨道、起止时间和素材必须真实存在；不得猜测素材或自动新建轨道。将写入组织成一次edit_apply_guarded批次，由原生审批、撤销和保存规则保护。\n用户要求：\n{}",
                        input
                    );
                }
            }
            if let Some(generation) = &intent.generation {
                if intent.mode != TaskMode::Script
                    || generation.project_id() != current.project_id
                    || generation.session_id() != current.session_id
                    || generation.project_revision() != current.revision
                {
                    return Err("runtime.stateChanged".into());
                }
                input = format!(
                    "本次只生成独立文案候选，不得修改原生工程。原生文案源摘要：{}。只返回候选正文，不包裹代码块。\n{}",
                    generation.ticket().sha256,
                    input
                );
            }
            input = format!(
                "原生当前工程上下文：{}\n任务权限：{}。\n{}",
                json!({
                    "path":current.path,"projectId":current.project_id,
                    "expectedSession":current.session_id,"expectedRevision":current.revision,
                }),
                intent.mode.authority_instructions(),
                input
            );
            let existing = if intent.mode == TaskMode::Script {
                None
            } else {
                inspect_latest(
                    &self.bridge.owner,
                    &self.bridge.token,
                    &current.project_id,
                    intent.mode.as_str(),
                )
                .map_err(|error| error.to_string())?
            };
            Ok((input, existing))
        })?;
        // The location lock covers local identity/history work only. Encode
        // the complete transport frame before opening a vault or issuing any
        // model authority, and send those exact admitted bytes later.
        self.admit_prepared(
            intent,
            PreparedInput {
                project: current,
                snapshot,
                input,
                conversation: existing,
            },
            encode,
            || platform_store().map_err(|error| error.to_string()),
        )
    }

    fn admit_prepared<T>(
        &self,
        intent: &SubmissionIntent,
        prepared: PreparedInput,
        encode: impl FnOnce(Value) -> Result<T, SubmissionError>,
        credentials: impl FnOnce() -> Result<Arc<dyn CredentialStore>, String>,
    ) -> Result<T, SubmissionError> {
        if prepared.input.len() > 512 * 1024 {
            return Err(SubmissionError::RejectedInput("runtime.inputTooLarge"));
        }
        let permit = random_capability()?;
        let selected = intent
            .model
            .as_ref()
            .or_else(|| prepared.snapshot.active())
            .cloned()
            .ok_or("agent.errorModelRequired")?;
        self.store
            .assert_connection_current(&selected)
            .map_err(|error| error.to_string())?;
        let mut payload = intent.runtime_payload(&prepared.input, &permit, prepared.conversation);
        payload["modelConfiguration"] =
            runtime_configuration_for(&self.store, &prepared.snapshot, &selected);
        let metadata = DurableTaskMetadata {
            version: 1,
            mode: intent.mode,
            languages: intent.languages.clone(),
            model: selected.clone(),
            model_configuration: payload["modelConfiguration"].clone(),
            input: intent.input.clone(),
            ui_draft: intent.ui_draft.clone(),
            selected: intent.selected.clone(),
            generation_snapshot: intent
                .generation
                .as_ref()
                .map(GenerationRequest::recovery_snapshot),
        };
        metadata.validate()?;
        let encoded = encode(payload)?;
        let network = Arc::new(ModelNetwork::authorize_selected(
            Arc::clone(&self.bridge.owner),
            self.bridge.token.clone(),
            self.store.clone(),
            prepared.snapshot,
            credentials()?,
            selected,
        )?);
        let mut task = self.task.lock().map_err(|_| "agent.sessionUnavailable")?;
        if task.is_some() {
            return Err("runtime.taskBusy".into());
        }
        *self
            .network
            .lock()
            .map_err(|_| "agent.sessionUnavailable")? = Some(Arc::clone(&network));
        *task = Some(AuthorizedTask {
            id: intent.id.clone(),
            permit,
            project: prepared.project,
            mode: intent.mode,
            selected: intent.selected.clone(),
            expected_conversation: prepared.conversation,
            conversation: None,
            changed_by_native_tool: false,
            metadata: Some(metadata),
            recovered_submission: None,
            network,
        });
        Ok(encoded)
    }

    /// The original runtime binds a real Pi conversation before submit can resume
    /// its scheduler. The native task id was issued by the explicit UI action.
    pub(crate) fn bind(&self, id: &str, conversation: u64) -> Result<(), String> {
        if conversation > 9_007_199_254_740_991 {
            return Err("runtime.invalidRequest".into());
        }
        let captured = self
            .task
            .lock()
            .map_err(|_| "agent.sessionUnavailable")?
            .as_ref()
            .filter(|task| task.id == id)
            .cloned()
            .ok_or("permission.taskScope")?;
        let task = &captured;
        task.network.check_current()?;
        if task
            .expected_conversation
            .is_some_and(|value| value != conversation)
            || task.conversation.is_some_and(|value| value != conversation)
        {
            return Err("permission.taskScope".into());
        }
        let bindings = ConversationBindings::open(&self.bridge.owner, &self.bridge.token)
            .map_err(|error| error.to_string())?;
        bindings
            .bind(&task.project.project_id, task.mode.as_str(), conversation)
            .map_err(|error| error.to_string())?;
        task.network.check_current()?;
        if let Some(metadata) = &task.metadata {
            concat_agent::task_snapshots::persist(
                &self.bridge.owner,
                &self.bridge.token,
                &concat_agent::task_snapshots::TaskSnapshot {
                    schema_version: 1,
                    request_id: task.id.clone(),
                    conversation_id: conversation,
                    project_id: task.project.project_id.clone(),
                    project_path: task.project.path.clone(),
                    metadata: serde_json::to_value(metadata)
                        .map_err(|_| "agent.errorRecoverySnapshotInvalid")?,
                },
            )?;
        }
        task.network.check_current()?;
        let mut current = self.task.lock().map_err(|_| "agent.sessionUnavailable")?;
        let task = current
            .as_mut()
            .filter(|task| task.id == captured.id && task.permit == captured.permit)
            .ok_or("permission.taskScope")?;
        task.conversation = Some(conversation);
        Ok(())
    }

    pub(crate) fn network(&self, permit: &str) -> Result<Arc<ModelNetwork>, String> {
        let task = self.task.lock().map_err(|_| "agent.sessionUnavailable")?;
        let task = task
            .as_ref()
            .filter(|task| task.permit == permit && task.conversation.is_some())
            .ok_or("permission.taskScope")?;
        Ok(Arc::clone(&task.network))
    }

    pub(crate) fn open(&self, permit: &str, request: ModelRequest) -> Result<Value, String> {
        let expected = self
            .task
            .lock()
            .map_err(|_| "agent.sessionUnavailable")?
            .as_ref()
            .map(|task| task.project.clone())
            .ok_or("permission.taskScope")?;
        let current = self
            .bridge
            .live
            .current_project()
            .map_err(|error| error.to_string())?;
        if !current.is_some_and(|value| {
            value.project_id == expected.project_id
                && value.session_id == expected.session_id
                && value.path == expected.path
        }) {
            self.cancel();
            return Err("permission.taskScope".into());
        }
        self.network(permit)?.open(request)
    }

    pub(crate) fn authorize_tool(
        &self,
        message: &mut Value,
        invocation: Option<&Invocation>,
    ) -> Result<(), String> {
        if message["method"] != "tools/call" {
            return Ok(());
        }
        let task = self
            .task
            .lock()
            .map_err(|_| "agent.sessionUnavailable")?
            .as_ref()
            .cloned()
            .ok_or("permission.taskScope")?;
        let invocation = invocation.ok_or("permission.missingInvocation")?;
        if task.conversation != Some(invocation.conversation_id) {
            return Err("permission.taskScope".into());
        }
        task.network.check_current()?;
        let name = message
            .pointer("/params/name")
            .and_then(Value::as_str)
            .ok_or("permission.invalidArguments")?
            .to_owned();
        if name == "concat_version" {
            return Ok(());
        }
        if matches!(name.as_str(), "effects_list" | "template_list") {
            // Metadata queries have no project path. Refuse injected arguments
            // here, before the native MCP schema and backend are reached.
            let args = message
                .pointer("/params/arguments")
                .and_then(Value::as_object)
                .ok_or("permission.invalidArguments")?;
            if (name == "template_list" && !args.is_empty())
                || (name == "effects_list"
                    && (args.keys().any(|key| key != "kind")
                        || args.get("kind").is_some_and(|kind| !kind.is_string())))
            {
                return Err("permission.invalidArguments".into());
            }
            return Ok(());
        }
        let args = message
            .pointer_mut("/params/arguments")
            .and_then(Value::as_object_mut)
            .ok_or("permission.invalidArguments")?;
        if args.get("path").and_then(Value::as_str) != Some(task.project.path.as_str()) {
            return Err("permission.taskScope".into());
        }
        if name == "edit_apply_guarded" {
            if task.mode != TaskMode::Edit {
                return Err("permission.readOnlyTask".into());
            }
            // The user may directly edit the current timeline without first
            // choosing a script. Full-video template generation is different:
            // it remains bound to a verified, explicitly selected version.
            if task.selected.is_none() && args.get("command").is_none_or(contains_main_template) {
                return Err("script.errorSelection".into());
            }
            if args.get("expectedSession").and_then(Value::as_u64) != Some(task.project.session_id)
                || args.get("expectedRevision").and_then(Value::as_u64)
                    != Some(task.project.revision)
            {
                return Err("permission.editContextChanged".into());
            }
        } else if !matches!(name.as_str(), "project_get" | "timeline_read") {
            return Err("permission.taskScope".into());
        }
        Ok(())
    }

    /// Called inside Live's existing owner gate. It must not re-enter any owner
    /// lock, perform disk I/O or borrow Studio from the global Shell again.
    pub(crate) fn validate_edit(
        &self,
        studio: &Studio,
        session: u64,
        revision: u64,
    ) -> Result<(), String> {
        let task = self.task.lock().map_err(|_| "agent.sessionUnavailable")?;
        let task = task.as_ref().ok_or("permission.taskScope")?;
        let current = studio.session.as_ref().ok_or("agent.openProject")?;
        if task.mode != TaskMode::Edit
            || task.project.session_id != session
            || task.project.revision != revision
            || current.session_id() != session
            || current.revision() != revision
            || current.project_identity() != task.project.project_id
        {
            return Err("permission.editContextChanged".into());
        }
        if let Some(selected) = task.selected.as_ref()
            && (!studio.script.selection_settled()
                || studio.script.selected_binding().as_ref() != Some(selected))
        {
            return Err("permission.editContextChanged".into());
        }
        Ok(())
    }

    /// Only a successful, original Pi-bound guarded tool response backed by
    /// the durable native editor receipt can count as an actual edit. Model
    /// prose, a read-only tool, a rejected call and a no-op cannot set this.
    pub(crate) fn record_edit_result(
        &self,
        invocation: &Invocation,
        response: &Value,
    ) -> Result<(), String> {
        if response.pointer("/result/isError") != Some(&Value::Bool(false)) {
            return Ok(());
        }
        let reply = response
            .pointer("/result/structuredContent")
            .filter(|value| value.is_object())
            .ok_or("runtime.editReceiptUnconfirmed")?;
        let task = self
            .task
            .lock()
            .map_err(|_| "runtime.editReceiptUnconfirmed")?
            .as_ref()
            .filter(|task| {
                task.mode == TaskMode::Edit && task.conversation == Some(invocation.conversation_id)
            })
            .cloned()
            .ok_or("runtime.editReceiptUnconfirmed")?;
        let journal = self
            .bridge
            .journal
            .lock()
            .map_err(|_| "runtime.editReceiptUnconfirmed")?
            .clone()
            .ok_or("runtime.editReceiptUnconfirmed")?;
        let native_receipt = journal
            .changed_receipt(
                invocation,
                &task.project.path,
                task.project.session_id,
                task.project.revision,
            )
            .map_err(|_| "runtime.editReceiptUnconfirmed")?;
        let Some(native_receipt) = native_receipt else {
            if reply["revision"]
                .as_u64()
                .is_some_and(|revision| revision > task.project.revision)
            {
                return Err("runtime.editReceiptUnconfirmed".into());
            }
            return Ok(());
        };
        // The MCP adapter deliberately returns a compact editor view, whereas
        // the journal stores the full native editor response. Compare the
        // precise adapter projection, not the two different wire shapes.
        if concat_mcp::summary::view(&native_receipt) != *reply {
            return Err("runtime.editReceiptUnconfirmed".into());
        }
        let mut current = self
            .task
            .lock()
            .map_err(|_| "runtime.editReceiptUnconfirmed")?;
        let current = current
            .as_mut()
            .filter(|current| {
                current.id == task.id
                    && current.permit == task.permit
                    && current.conversation == task.conversation
            })
            .ok_or("runtime.editReceiptUnconfirmed")?;
        current.changed_by_native_tool = true;
        Ok(())
    }

    pub(crate) fn finish(&self, id: &str, conversation: u64) -> Result<bool, String> {
        let mut task = self.task.lock().map_err(|_| "agent.sessionUnavailable")?;
        if !task
            .as_ref()
            .is_some_and(|task| task.id == id && task.conversation == Some(conversation))
        {
            return Err("permission.taskScope".into());
        }
        let edited = task
            .as_ref()
            .is_some_and(|task| task.changed_by_native_tool);
        if let Some(task) = task.take() {
            task.network.cancel();
        }
        self.network
            .lock()
            .map_err(|_| "agent.sessionUnavailable")?
            .take();
        Ok(edited)
    }

    pub(crate) fn cancel(&self) {
        if let Ok(network) = self.network.lock()
            && let Some(network) = network.as_ref()
        {
            network.cancel();
        }
    }

    pub(crate) fn abandon(&self) {
        self.cancel();
        if let Ok(mut task) = self.task.lock() {
            task.take();
        }
        if let Ok(mut network) = self.network.lock() {
            network.take();
        }
    }

    pub(crate) fn latest(&self) -> Result<Option<u64>, String> {
        let project = self
            .bridge
            .live
            .current_project()
            .map_err(|error| error.to_string())?;
        let Some(project) = project else {
            return Ok(None);
        };
        self.latest_at(&project)
    }

    /// Recovery-only: inspect every historical binding after the same native
    /// identity/copy check used for document writes, without creating or moving
    /// a registry record. No model authority or scheduler is opened here.
    pub(crate) fn recovery_conversations(
        &self,
        project: &CurrentProject,
    ) -> Result<Vec<u64>, String> {
        if concat_host::project_identity::read_persisted(Path::new(&project.path))?.as_deref()
            != Some(project.project_id.as_str())
        {
            return Err("agent.errorRecoveryChanged".into());
        }
        let root = self
            .bridge
            .dirs
            .data
            .join("bk-cat")
            .join("script-workspaces");
        concat_agent::script_locations::with_read_only_project_location(
            &root,
            &project.project_id,
            Path::new(&project.path),
            concat_host::project_identity::read_persisted,
            || {
                inspect_project_conversations(
                    &self.bridge.owner,
                    &self.bridge.token,
                    &project.project_id,
                )
                .map_err(|error| error.to_string())
            },
        )
    }

    fn latest_at(&self, project: &CurrentProject) -> Result<Option<u64>, String> {
        self.at_project_location(project, || {
            let read = |mode| {
                inspect_latest(
                    &self.bridge.owner,
                    &self.bridge.token,
                    &project.project_id,
                    mode,
                )
                .map_err(|error| error.to_string())
            };
            Ok(read("chat")?.or(read("edit")?))
        })
    }

    /// Worker-only. Reuse the script workspace's location registry so model
    /// history and document candidates cannot disagree about a copied project.
    /// The action must be local and must not borrow the native editing thread.
    fn at_project_location<T>(
        &self,
        project: &CurrentProject,
        action: impl FnOnce() -> Result<T, String>,
    ) -> Result<T, String> {
        let root = self
            .bridge
            .dirs
            .data
            .join("bk-cat")
            .join("script-workspaces");
        std::fs::create_dir_all(&root).map_err(|_| "agent.sessionUnavailable".to_owned())?;
        concat_agent::script_locations::with_project_location(
            &root,
            &project.project_id,
            Path::new(&project.path),
            concat_host::project_identity::read_persisted,
            action,
        )
    }

    pub(crate) fn may_present(&self, conversation: u64, observed: Option<u64>) -> bool {
        self.task.lock().is_ok_and(|task| match task.as_ref() {
            Some(task) => task.conversation == Some(conversation),
            None => observed == Some(conversation),
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use concat_agent::configuration::{ChannelDraft, ModelProtocol};
    use concat_agent::credentials::{CredentialError, CredentialScope, CredentialStore, Secret};
    use concat_agent::{Reconciliation, RecoveryDecision};
    use concat_api::{AppDirs, OpenProjects};
    use concat_host::export::Exporter;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PROJECT: &str = "bkcat-project:0123456789abcdef0123456789abcdef";
    const OTHER_PROJECT: &str = "bkcat-project:fedcba9876543210fedcba9876543210";
    const CONVERSATION: u64 = 71;
    const TASK: &str = "isolated-native-task";
    const PERMIT: &str = "isolated-native-permit";

    #[test]
    fn mode_instructions_keep_read_only_and_native_edit_authority_distinct() {
        assert!(TaskMode::Chat.authority_instructions().contains("只读"));
        assert!(TaskMode::Script.authority_instructions().contains("只读"));
        let edit = TaskMode::Edit.authority_instructions();
        assert!(edit.contains("受控写入"));
        assert!(edit.contains("edit_apply_guarded"));
        assert!(edit.contains("原生审批"));
        assert!(!edit.contains("只读"));
        assert!(
            !TaskMode::Chat
                .authority_instructions()
                .contains("edit_apply_guarded")
        );
        assert!(
            !TaskMode::Script
                .authority_instructions()
                .contains("edit_apply_guarded")
        );
    }

    #[test]
    fn user_display_metadata_keeps_the_complete_native_model_input_and_task_binding() {
        let display = "  请保留换行\n用户要求：这是我输入的正文。  ";
        let wrapped = format!(
            "原生当前工程上下文：{{\"projectId\":\"{PROJECT}\",\"expectedSession\":17,\"expectedRevision\":3}}\n任务权限：只读。\n{display}"
        );
        for mode in [TaskMode::Chat, TaskMode::Script, TaskMode::Edit] {
            let mut intent = SubmissionIntent {
                languages: crate::agent::tasks::TaskLanguages::default(),
                model: None,
                id: TASK.into(),
                project: CurrentProject {
                    project_id: PROJECT.into(),
                    path: "isolated-project".into(),
                    session_id: 17,
                    revision: 3,
                },
                mode,
                input: display.into(),
                ui_draft: display.into(),
                reference_keys: vec![],
                selected: None,
                generation: None,
            };
            let payload = intent.runtime_payload(&wrapped, PERMIT, Some(CONVERSATION));
            assert_eq!(payload["input"], wrapped);
            assert_eq!(payload["displayText"], display);
            assert_eq!(payload["taskId"], TASK);
            assert_eq!(payload["networkPermit"], PERMIT);
            assert_eq!(payload["mode"], mode.as_str());
            assert_eq!(payload["conversationId"], CONVERSATION);
            for legacy_draft in ["", " \n\t"] {
                intent.ui_draft = legacy_draft.into();
                let legacy = intent.runtime_payload(&wrapped, PERMIT, None);
                assert!(legacy.get("displayText").is_none());
                assert!(legacy.get("conversationId").is_none());
                assert_eq!(legacy["input"], wrapped);
            }
        }
    }

    #[test]
    fn oversized_user_display_is_rejected_before_another_task_or_credential_access() {
        let fixture = Fixture::new(TaskMode::Chat, None);
        let intent = SubmissionIntent {
            languages: crate::agent::tasks::TaskLanguages::default(),
            model: None,
            id: "not-admitted".into(),
            project: fixture.project(),
            mode: TaskMode::Chat,
            input: "valid short model input".into(),
            ui_draft: "字".repeat(256 * 1024 / 3 + 1),
            reference_keys: vec![],
            selected: None,
            generation: None,
        };
        let reads = fixture.credentials.reads.load(Ordering::SeqCst);
        assert_eq!(
            fixture.tasks.prepare(&intent, Ok).unwrap_err().to_string(),
            "runtime.inputTooLarge"
        );
        assert_eq!(fixture.credentials.reads.load(Ordering::SeqCst), reads);
        assert_eq!(
            fixture.tasks.task.lock().unwrap().as_ref().unwrap().id,
            TASK
        );
        fixture.network.check_current().unwrap();
    }

    #[test]
    fn empty_input_and_busy_task_keep_distinct_codes_without_admitting_work() {
        let fixture = Fixture::new(TaskMode::Chat, None);
        let reads = fixture.credentials.reads.load(Ordering::SeqCst);
        let mut intent = SubmissionIntent {
            languages: TaskLanguages::default(),
            model: None,
            id: "not-admitted".into(),
            project: fixture.project(),
            mode: TaskMode::Chat,
            input: " \n\t".into(),
            ui_draft: " \n\t".into(),
            reference_keys: vec![],
            selected: None,
            generation: None,
        };
        for language in ["en", "zh-Hans", "ja"] {
            intent.languages.interface_language = language.into();
            assert_eq!(
                fixture.tasks.prepare(&intent, Ok).unwrap_err().to_string(),
                "agent.enterRequest"
            );
        }
        intent.input = "A new valid request".into();
        intent.ui_draft = intent.input.clone();
        assert_eq!(
            fixture.tasks.prepare(&intent, Ok).unwrap_err().to_string(),
            "runtime.taskBusy"
        );
        assert_eq!(fixture.credentials.reads.load(Ordering::SeqCst), reads);
        assert_eq!(
            fixture.tasks.task.lock().unwrap().as_ref().unwrap().id,
            TASK
        );
        fixture.network.check_current().unwrap();
    }

    #[test]
    fn complete_frame_budget_precedes_vault_authority_and_allows_valid_retry() {
        let fixture = Fixture::new(TaskMode::Chat, None);
        let project = fixture.project();
        let snapshot = fixture.tasks.startup.lock().unwrap().clone().unwrap();
        fixture.tasks.abandon();
        let original_reads = fixture.credentials.reads.load(Ordering::SeqCst);
        let vault_opens = AtomicUsize::new(0);
        let draft = "\u{1}".repeat(200_000);
        let mut intent = SubmissionIntent {
            languages: crate::agent::tasks::TaskLanguages::default(),
            model: None,
            id: "bounded-native-input".into(),
            project: project.clone(),
            mode: TaskMode::Chat,
            input: draft.clone(),
            ui_draft: draft,
            reference_keys: vec![],
            selected: None,
            generation: None,
        };
        let encode = |payload| {
            crate::embedded::encode_frame(
                "actual-component-with-complete-envelope",
                json!({"id":format!("native:{}", u64::MAX),"payload":payload}),
            )
            .map_err(SubmissionError::from)
        };
        let Err(error) = fixture.tasks.admit_prepared(
            &intent,
            PreparedInput {
                project: project.clone(),
                snapshot: snapshot.clone(),
                input: format!("原生完整上下文\n{}", intent.input),
                conversation: None,
            },
            |payload| {
                // This bounded user/native input fits the larger current
                // bridge budget. Exercise a caller-owned complete-envelope
                // failure with explicit test padding, never a larger draft.
                let valid = encode(payload.clone())?;
                let limit = concat_agent::transport::MAX_BRIDGE_FRAME_BYTES;
                assert!(valid.len() < limit);
                crate::embedded::encode_frame(
                    "actual-component-with-complete-envelope",
                    json!({
                        "id": format!("native:{}", u64::MAX),
                        "payload": payload,
                        "fixtureEnvelopePadding": " ".repeat(limit),
                    }),
                )
                .map_err(SubmissionError::from)
            },
            || {
                vault_opens.fetch_add(1, Ordering::SeqCst);
                Ok(fixture.credentials.clone())
            },
        ) else {
            panic!("the oversized complete frame must be rejected before authority");
        };
        assert!(matches!(
            error,
            SubmissionError::RejectedInput("runtime.inputTooLarge")
        ));
        assert_eq!(vault_opens.load(Ordering::SeqCst), 0);
        assert_eq!(
            fixture.credentials.reads.load(Ordering::SeqCst),
            original_reads
        );
        assert!(fixture.tasks.task.lock().unwrap().is_none());
        assert!(fixture.tasks.network.lock().unwrap().is_none());
        assert_eq!(
            fixture.bridge.status().unwrap().phase,
            concat_agent::Phase::Ready
        );

        intent.input = "请解释当前工程，并保留我的换行。\n第二行。".into();
        intent.ui_draft = intent.input.clone();
        let full_input = format!("原生完整上下文\n{}", intent.input);
        let encoded = fixture
            .tasks
            .admit_prepared(
                &intent,
                PreparedInput {
                    project,
                    snapshot,
                    input: full_input.clone(),
                    conversation: None,
                },
                encode,
                || {
                    vault_opens.fetch_add(1, Ordering::SeqCst);
                    Ok(fixture.credentials.clone())
                },
            )
            .unwrap();
        let frame: Value = serde_json::from_str(&encoded).unwrap();
        assert_eq!(frame["payload"]["input"], full_input);
        assert_eq!(frame["payload"]["displayText"], intent.ui_draft);
        assert_eq!(frame["id"], format!("native:{}", u64::MAX));
        assert_eq!(vault_opens.load(Ordering::SeqCst), 1);
        assert_eq!(
            fixture.credentials.reads.load(Ordering::SeqCst),
            original_reads + 1
        );
        assert_eq!(
            fixture.tasks.task.lock().unwrap().as_ref().unwrap().id,
            intent.id
        );
        fixture
            .tasks
            .network
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .check_current()
            .unwrap();
        fixture.tasks.abandon();
    }

    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    // This is an isolated fake vault, never the platform credential store.
    // Its random test-only value is held in the zeroing Secret container and
    // never sent to a socket, written to the project or printed in assertions.
    struct Credentials {
        scope: CredentialScope,
        secret: Secret,
        reads: AtomicUsize,
    }
    impl CredentialStore for Credentials {
        fn read(&self, scope: &CredentialScope) -> Result<Option<Secret>, CredentialError> {
            if scope != &self.scope {
                return Err(CredentialError::InvalidScope);
            }
            self.reads.fetch_add(1, Ordering::SeqCst);
            Secret::new(self.secret.expose_secret().to_owned()).map(Some)
        }
        fn write(&self, _: &CredentialScope, _: &Secret) -> Result<(), CredentialError> {
            Err(CredentialError::Unavailable)
        }
        fn delete(&self, _: &CredentialScope) -> Result<(), CredentialError> {
            Err(CredentialError::Unavailable)
        }
    }

    struct Fixture {
        tasks: NativeTasks,
        network: Arc<ModelNetwork>,
        credentials: Arc<Credentials>,
        bridge: Arc<AgentBridge>,
        // Last field: all real SQLite/owner handles close before deletion.
        _directory: Directory,
    }

    // Reuse the existing isolated vault/network/task setup for runtime
    // completion tests. No platform credential store or model socket is used.
    fn seed_task(
        tasks: &NativeTasks,
        project: CurrentProject,
        mode: TaskMode,
        expected_conversation: Option<u64>,
    ) -> (Arc<ModelNetwork>, Arc<Credentials>) {
        let saved = tasks
            .store
            .save_channel(
                &tasks.store.load().unwrap(),
                None,
                ChannelDraft {
                    label: "Isolated task-authority fixture".into(),
                    protocol: ModelProtocol::OpenAiCompletions,
                    endpoint: "https://native-task.invalid/explicit/v1".into(),
                    model: "fixture-only".into(),
                    request_timeout_ms: 1_000,
                    max_output_tokens: 128,
                },
            )
            .unwrap();
        let id = saved.channels()[0].id.clone();
        let snapshot = tasks.store.select(&saved, Some(&id)).unwrap();
        let channel = snapshot.active().unwrap();
        let credentials = Arc::new(Credentials {
            scope: CredentialScope::new(channel.id.as_str(), &channel.service_origin().unwrap())
                .unwrap(),
            secret: Secret::new(random_capability().unwrap()).unwrap(),
            reads: AtomicUsize::new(0),
        });
        let network = Arc::new(
            ModelNetwork::authorize(
                Arc::clone(&tasks.bridge.owner),
                tasks.bridge.token.clone(),
                tasks.store.clone(),
                snapshot.clone(),
                credentials.clone(),
            )
            .unwrap(),
        );
        *tasks.startup.lock().unwrap() = Some(snapshot);
        *tasks.network.lock().unwrap() = Some(Arc::clone(&network));
        *tasks.task.lock().unwrap() = Some(AuthorizedTask {
            id: TASK.into(),
            permit: PERMIT.into(),
            project,
            mode,
            selected: (mode == TaskMode::Edit).then(|| VersionBinding {
                project_id: PROJECT.into(),
                document_id: "fixture-document".into(),
                version: 2,
                sha256: "a".repeat(64),
            }),
            expected_conversation,
            conversation: None,
            changed_by_native_tool: false,
            metadata: None,
            recovered_submission: None,
            network: Arc::clone(&network),
        });
        (network, credentials)
    }

    pub(crate) fn install_bound_edit_task(
        tasks: &NativeTasks,
        project: CurrentProject,
        id: &str,
        conversation: u64,
    ) {
        let _ = seed_task(tasks, project, TaskMode::Edit, Some(conversation));
        let mut slot = tasks.task.lock().unwrap();
        let task = slot.as_mut().unwrap();
        task.id = id.to_owned();
        task.conversation = Some(conversation);
        task.selected = None;
    }

    impl Fixture {
        fn new(mode: TaskMode, expected_conversation: Option<u64>) -> Self {
            let directory = Directory(std::env::temp_dir().join(format!(
                "bk-cat-task-authority-{}",
                random_capability().unwrap()
            )));
            std::fs::create_dir(&directory.0).unwrap();
            let dirs = AppDirs::under(&directory.0);
            let bridge = Arc::new(
                AgentBridge::acquire(&dirs, OpenProjects::default(), Exporter::new()).unwrap(),
            );
            bridge
                .confirm_recovery(
                    Reconciliation {
                        project_identity: PROJECT.into(),
                        project_revision: "3".into(),
                        session_revision: "1".into(),
                    },
                    RecoveryDecision::DiscardInterrupted,
                )
                .unwrap();
            let tasks = NativeTasks::new(Arc::clone(&bridge));
            let (network, credentials) = seed_task(
                &tasks,
                CurrentProject {
                    project_id: PROJECT.into(),
                    path: directory.0.join("project").to_string_lossy().into_owned(),
                    session_id: 17,
                    revision: 3,
                },
                mode,
                expected_conversation,
            );
            Self {
                tasks,
                network,
                credentials,
                bridge,
                _directory: directory,
            }
        }

        fn invocation(&self, conversation: u64) -> Invocation {
            Invocation {
                conversation_id: conversation,
                task_id: 89,
                call_id: "fixture-call".into(),
            }
        }

        fn message(&self, tool: &str) -> Value {
            let task = self.tasks.task.lock().unwrap();
            let task = task.as_ref().unwrap();
            json!({"jsonrpc":"2.0","id":1,"method":"tools/call","params":{
                "name":tool,"arguments":{"path":task.project.path,
                    "expectedSession":task.project.session_id,"expectedRevision":task.project.revision,
                    "command":{"op":"batch","commands":[]}}
            }})
        }

        fn authorize(&self, message: &mut Value, conversation: u64) -> Result<(), String> {
            self.tasks
                .authorize_tool(message, Some(&self.invocation(conversation)))
        }

        fn project(&self) -> CurrentProject {
            self.tasks
                .task
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .project
                .clone()
        }
    }

    fn write_native_identity(path: &Path, project_id: &str) {
        std::fs::create_dir_all(path).unwrap();
        std::fs::write(
            path.join("concat.json"),
            serde_json::to_vec(&json!({(concat_host::project_identity::FIELD): project_id}))
                .unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn recovery_completion_requires_this_inputs_native_receipt_and_exact_saved_document() {
        use concat_agent::operations::{Admission, inspect_recovery};
        let fixture = Fixture::new(TaskMode::Chat, None);
        let project = fixture.project();
        write_native_identity(Path::new(&project.path), PROJECT);
        let original = fixture.tasks.startup.lock().unwrap().clone().unwrap();
        let channel = original.active().unwrap().clone();
        let metadata = DurableTaskMetadata {
            version: 1,
            mode: TaskMode::Edit,
            languages: TaskLanguages::default(),
            model: channel.clone(),
            model_configuration: runtime_configuration_for(
                &fixture.tasks.store,
                &original,
                &channel,
            ),
            input: "original edit".into(),
            ui_draft: "original edit".into(),
            selected: None,
            generation_snapshot: None,
        };
        let invocation = fixture.invocation(CONVERSATION);
        let mut scope = RecoveryTaskScope {
            snapshot: concat_agent::task_snapshots::TaskSnapshot {
                schema_version: 1,
                request_id: TASK.into(),
                conversation_id: CONVERSATION,
                project_id: PROJECT.into(),
                project_path: project.path.clone(),
                metadata: serde_json::to_value(&metadata).unwrap(),
            },
            submission_id: 22,
            task_ids: vec![89],
            invocations: vec![invocation.clone()],
            metadata,
        };
        let journal = fixture.bridge.journal.lock().unwrap().clone().unwrap();
        let request = json!({"method":"edit.applyGuarded","path":project.path,"expectedSession":17,"expectedRevision":3});
        let Admission::New(id) = journal
            .prepare(&invocation, &request, &project.path, 17, 3)
            .unwrap()
        else {
            panic!("new intent")
        };
        let inspect = || {
            inspect_recovery(&fixture.bridge.owner, &fixture.bridge.token, &project.path).unwrap()
        };
        assert!(
            !recovered_saved_change(&scope, &inspect()),
            "unknown intent is never success"
        );
        journal
            .record_receipt(id, 17, 4, &json!({"sessionId":17,"revision":4}))
            .unwrap();
        assert!(
            !recovered_saved_change(&scope, &inspect()),
            "applied without save is not recovered saved work"
        );
        let document: Value = serde_json::from_slice(
            &std::fs::read(Path::new(&project.path).join("concat.json")).unwrap(),
        )
        .unwrap();
        assert_eq!(
            journal
                .record_saved_document(&project.path, 17, 4, &document)
                .unwrap(),
            1
        );
        assert!(recovered_saved_change(&scope, &inspect()));
        scope.invocations[0].call_id = "another-input-call".into();
        assert!(
            !recovered_saved_change(&scope, &inspect()),
            "same conversation does not imply same original input"
        );
        scope.invocations[0] = invocation;
        std::fs::write(
            Path::new(&project.path).join("concat.json"),
            b"{\"changed-after-save\":true}",
        )
        .unwrap();
        assert!(
            !recovered_saved_change(&scope, &inspect()),
            "an overwritten document is not the proven saved output"
        );
        let early = Invocation {
            conversation_id: CONVERSATION,
            task_id: 90,
            call_id: "save-before-receipt".into(),
        };
        let Admission::New(_id)=journal.prepare(&early,&json!({"method":"edit.applyGuarded","path":project.path,"expectedSession":17,"expectedRevision":4}),&project.path,17,4).unwrap() else {panic!("early intent")};
        let document = json!({"changed-after-save":true});
        assert_eq!(
            journal
                .record_saved_document(&project.path, 17, 5, &document)
                .unwrap(),
            0
        );
        scope.invocations = vec![early];
        assert!(
            !recovered_saved_change(&scope, &inspect()),
            "an early save alone does not prove the operation applied"
        );
    }

    #[test]
    fn a_project_copy_cannot_restore_history_or_enter_task_preparation() {
        let fixture = Fixture::new(TaskMode::Chat, None);
        let original = fixture.project();
        write_native_identity(Path::new(&original.path), PROJECT);
        fixture.tasks.bind(TASK, CONVERSATION).unwrap();

        // First register through the pre-existing script-workspace entry, not
        // a second conversation-only identity table.
        let root = fixture
            .bridge
            .dirs
            .data
            .join("bk-cat")
            .join("script-workspaces");
        std::fs::create_dir_all(&root).unwrap();
        concat_agent::script_locations::with_project_location(
            &root,
            PROJECT,
            Path::new(&original.path),
            concat_host::project_identity::read_persisted,
            || Ok(()),
        )
        .unwrap();
        assert_eq!(
            fixture.tasks.latest_at(&original).unwrap(),
            Some(CONVERSATION)
        );

        let copy_path = fixture._directory.0.join("copy");
        std::fs::create_dir(&copy_path).unwrap();
        std::fs::copy(
            Path::new(&original.path).join("concat.json"),
            copy_path.join("concat.json"),
        )
        .unwrap();
        let copy = CurrentProject {
            path: copy_path.to_str().unwrap().into(),
            session_id: 18,
            ..original.clone()
        };
        assert!(fixture.tasks.latest_at(&copy).is_err());
        let admitted = std::cell::Cell::new(false);
        assert!(
            fixture
                .tasks
                .at_project_location(&copy, || {
                    admitted.set(true);
                    Ok(())
                })
                .is_err()
        );
        assert!(
            !admitted.get(),
            "the same preparation gate refuses before reading a saved script or granting network access"
        );
        assert_eq!(
            fixture.tasks.latest_at(&original).unwrap(),
            Some(CONVERSATION)
        );
        assert_eq!(fixture.credentials.reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn moving_the_native_project_keeps_its_conversation_without_rebinding() {
        let fixture = Fixture::new(TaskMode::Chat, None);
        let original = fixture.project();
        write_native_identity(Path::new(&original.path), PROJECT);
        fixture.tasks.bind(TASK, CONVERSATION).unwrap();
        assert_eq!(
            fixture.tasks.latest_at(&original).unwrap(),
            Some(CONVERSATION)
        );
        let moved_path = fixture._directory.0.join("moved");
        std::fs::rename(&original.path, &moved_path).unwrap();
        let moved = CurrentProject {
            path: moved_path.to_str().unwrap().into(),
            session_id: 18,
            ..original.clone()
        };
        assert_eq!(fixture.tasks.latest_at(&moved).unwrap(), Some(CONVERSATION));
        assert!(fixture.tasks.latest_at(&original).is_err());
        assert_eq!(
            inspect_latest(
                &fixture.bridge.owner,
                &fixture.bridge.token,
                PROJECT,
                "chat"
            )
            .unwrap(),
            Some(CONVERSATION)
        );
    }

    #[test]
    fn reusing_an_old_folder_for_another_project_does_not_reuse_its_messages() {
        let fixture = Fixture::new(TaskMode::Chat, None);
        let original = fixture.project();
        write_native_identity(Path::new(&original.path), PROJECT);
        fixture.tasks.bind(TASK, CONVERSATION).unwrap();
        assert_eq!(
            fixture.tasks.latest_at(&original).unwrap(),
            Some(CONVERSATION)
        );
        let moved_path = fixture._directory.0.join("moved");
        std::fs::rename(&original.path, &moved_path).unwrap();
        write_native_identity(Path::new(&original.path), OTHER_PROJECT);
        let replacement = CurrentProject {
            project_id: OTHER_PROJECT.into(),
            session_id: 18,
            ..original.clone()
        };
        assert_eq!(fixture.tasks.latest_at(&replacement).unwrap(), None);
        let moved = CurrentProject {
            path: moved_path.to_str().unwrap().into(),
            session_id: 19,
            ..original
        };
        assert_eq!(fixture.tasks.latest_at(&moved).unwrap(), Some(CONVERSATION));
        assert_eq!(fixture.tasks.latest_at(&replacement).unwrap(), None);
    }

    #[test]
    fn missing_or_replaced_native_identity_refuses_history_and_preparation() {
        let fixture = Fixture::new(TaskMode::Chat, None);
        let current = fixture.project();
        let path = Path::new(&current.path);
        std::fs::create_dir_all(path).unwrap();
        fixture.tasks.bind(TASK, CONVERSATION).unwrap();
        for document in [
            json!({"name":"legacy without persistent identity"}),
            json!({(concat_host::project_identity::FIELD):OTHER_PROJECT}),
        ] {
            std::fs::write(
                path.join("concat.json"),
                serde_json::to_vec(&document).unwrap(),
            )
            .unwrap();
            assert!(fixture.tasks.latest_at(&current).is_err());
            let admitted = std::cell::Cell::new(false);
            assert!(
                fixture
                    .tasks
                    .at_project_location(&current, || {
                        admitted.set(true);
                        Ok(())
                    })
                    .is_err()
            );
            assert!(!admitted.get());
            assert_eq!(
                serde_json::from_slice::<Value>(&std::fs::read(path.join("concat.json")).unwrap())
                    .unwrap(),
                document
            );
        }
        assert_eq!(fixture.credentials.reads.load(Ordering::SeqCst), 1);
        assert_eq!(
            inspect_latest(
                &fixture.bridge.owner,
                &fixture.bridge.token,
                PROJECT,
                "chat"
            )
            .unwrap(),
            Some(CONVERSATION)
        );
    }

    #[test]
    fn reused_conversation_is_not_authority_until_native_bind_completes() {
        let fixture = Fixture::new(TaskMode::Chat, Some(CONVERSATION));
        assert!(fixture.tasks.network(PERMIT).is_err());
        for tool in [
            "concat_version",
            "project_get",
            "timeline_read",
            "edit_apply_guarded",
        ] {
            assert!(
                fixture
                    .authorize(&mut fixture.message(tool), CONVERSATION)
                    .is_err()
            );
        }
        assert!(!fixture.tasks.may_present(CONVERSATION, None));
        assert!(fixture.tasks.bind(TASK, CONVERSATION + 1).is_err());
        assert!(fixture.tasks.bind("other-task", CONVERSATION).is_err());
        assert!(fixture.tasks.network(PERMIT).is_err());

        fixture.tasks.bind(TASK, CONVERSATION).unwrap();
        fixture.tasks.bind(TASK, CONVERSATION).unwrap();
        assert!(fixture.tasks.network(PERMIT).is_ok());
        assert!(fixture.tasks.network("another-permit").is_err());
        assert!(fixture.tasks.may_present(CONVERSATION, None));
        assert_eq!(
            inspect_latest(
                &fixture.bridge.owner,
                &fixture.bridge.token,
                PROJECT,
                "chat"
            )
            .unwrap(),
            Some(CONVERSATION)
        );
        assert_eq!(fixture.credentials.reads.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn native_binding_refuses_cross_project_cross_mode_and_lossy_original_ids() {
        for (project, mode) in [(OTHER_PROJECT, "chat"), (PROJECT, "edit")] {
            let fixture = Fixture::new(TaskMode::Chat, None);
            let bindings =
                ConversationBindings::open(&fixture.bridge.owner, &fixture.bridge.token).unwrap();
            bindings.bind(project, mode, CONVERSATION).unwrap();
            drop(bindings);
            assert!(fixture.tasks.bind(TASK, CONVERSATION).is_err());
            assert_eq!(
                fixture.tasks.bind(TASK, 9_007_199_254_740_992).unwrap_err(),
                "runtime.invalidRequest"
            );
            assert!(fixture.tasks.network(PERMIT).is_err());
            assert_eq!(
                inspect_latest(&fixture.bridge.owner, &fixture.bridge.token, project, mode)
                    .unwrap(),
                Some(CONVERSATION)
            );
        }
    }

    #[test]
    fn chat_and_script_tasks_cannot_turn_read_tools_into_project_edits() {
        for mode in [TaskMode::Chat, TaskMode::Script] {
            let fixture = Fixture::new(mode, None);
            fixture.tasks.bind(TASK, CONVERSATION).unwrap();
            for tool in [
                "concat_version",
                "project_get",
                "timeline_read",
                "effects_list",
                "template_list",
            ] {
                let mut message = fixture.message(tool);
                if matches!(tool, "effects_list" | "template_list") {
                    message["params"]["arguments"] = json!({});
                }
                fixture.authorize(&mut message, CONVERSATION).unwrap();
                assert_eq!(
                    fixture
                        .authorize(&mut message, CONVERSATION + 1)
                        .unwrap_err(),
                    "permission.taskScope"
                );
                assert_eq!(
                    fixture
                        .tasks
                        .authorize_tool(&mut message, None)
                        .unwrap_err(),
                    "permission.missingInvocation"
                );
            }
            let mut filtered_effects = fixture.message("effects_list");
            filtered_effects["params"]["arguments"] = json!({"kind":"effect"});
            fixture
                .authorize(&mut filtered_effects, CONVERSATION)
                .unwrap();
            for tool in ["effects_list", "template_list"] {
                // A model cannot smuggle another project's path or arbitrary
                // input through globally read-only metadata tools.
                assert_eq!(
                    fixture
                        .authorize(&mut fixture.message(tool), CONVERSATION)
                        .unwrap_err(),
                    "permission.invalidArguments"
                );
            }
            for tool in ["edit_apply_guarded", "project_delete", "media_upload"] {
                assert_eq!(
                    fixture
                        .authorize(&mut fixture.message(tool), CONVERSATION)
                        .unwrap_err(),
                    if tool == "edit_apply_guarded" {
                        "permission.readOnlyTask"
                    } else {
                        "permission.taskScope"
                    }
                );
            }
            for tool in ["project_get", "timeline_read"] {
                let mut message = fixture.message(tool);
                message["params"]["arguments"]["path"] = json!("another-project");
                assert_eq!(
                    fixture.authorize(&mut message, CONVERSATION).unwrap_err(),
                    "permission.taskScope"
                );
            }
        }
    }

    #[test]
    fn direct_edit_needs_exact_project_but_only_template_needs_selected_script() {
        let fixture = Fixture::new(TaskMode::Edit, None);
        fixture.tasks.bind(TASK, CONVERSATION).unwrap();
        let valid = fixture.message("edit_apply_guarded");
        let mut admitted = valid.clone();
        fixture.authorize(&mut admitted, CONVERSATION).unwrap();
        assert_eq!(
            admitted, valid,
            "native admission never rewrites a stale plan into a current one"
        );
        for (field, wrong) in [
            ("path", json!("another-project")),
            ("expectedSession", json!(18)),
            ("expectedRevision", json!(4)),
            ("expectedSession", json!("17")),
        ] {
            let mut message = valid.clone();
            message["params"]["arguments"][field] = wrong;
            assert_eq!(
                fixture.authorize(&mut message, CONVERSATION).unwrap_err(),
                if field == "path" {
                    "permission.taskScope"
                } else {
                    "permission.editContextChanged"
                }
            );
        }
        // A selected version permits the full-video template path.
        let mut selected_template = valid.clone();
        selected_template["params"]["arguments"]["command"] =
            json!({"op":"applySingleMainTemplate","plan":{}});
        fixture
            .authorize(&mut selected_template, CONVERSATION)
            .unwrap();
        // Without a selected version, ordinary exact edits still work.
        fixture
            .tasks
            .task
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .selected = None;
        fixture.authorize(&mut valid.clone(), CONVERSATION).unwrap();
        // A model cannot hide a full-video template inside a direct edit.
        assert!(
            fixture
                .authorize(&mut selected_template, CONVERSATION)
                .is_err()
        );
        let mut nested_template = valid;
        nested_template["params"]["arguments"]["command"] = json!({
            "op":"batch", "commands":[
                {"op":"addCaption", "text":"safe"},
                {"op":"applySingleMainTemplate", "plan":{}}
            ]
        });
        assert!(
            fixture
                .authorize(&mut nested_template, CONVERSATION)
                .is_err()
        );
    }

    #[test]
    fn stop_owner_retirement_and_configuration_change_invalidate_bound_task() {
        for reason in ["stop", "owner", "configuration"] {
            let fixture = Fixture::new(TaskMode::Edit, None);
            fixture.tasks.bind(TASK, CONVERSATION).unwrap();
            match reason {
                "stop" => fixture.tasks.cancel(),
                "owner" => fixture.bridge.stop(),
                _ => {
                    let prior = fixture.tasks.store.load().unwrap();
                    let bound = prior.active().unwrap().clone();
                    let unselected = fixture.tasks.store.select(&prior, None).unwrap();
                    assert!(unselected.active().is_none());
                    // The application default is independent of the captured
                    // task. Only a change to its actual connection revokes it.
                    fixture.network.check_current().unwrap();
                    fixture.tasks.bind(TASK, CONVERSATION).unwrap();
                    fixture
                        .authorize(&mut fixture.message("project_get"), CONVERSATION)
                        .unwrap();
                    let changed = fixture
                        .tasks
                        .store
                        .save_channel(
                            &unselected,
                            Some(&bound.id),
                            ChannelDraft {
                                label: bound.label.clone(),
                                protocol: bound.protocol,
                                endpoint: bound.endpoint.clone(),
                                model: bound.model.clone(),
                                request_timeout_ms: bound.request_timeout_ms + 1,
                                max_output_tokens: bound.max_output_tokens,
                            },
                        )
                        .unwrap();
                    assert_ne!(changed.channel(&bound.id).unwrap().version, bound.version);
                }
            }
            assert!(fixture.network.check_current().is_err());
            assert!(fixture.tasks.bind(TASK, CONVERSATION).is_err());
            for tool in ["concat_version", "project_get", "edit_apply_guarded"] {
                assert!(
                    fixture
                        .authorize(&mut fixture.message(tool), CONVERSATION)
                        .is_err()
                );
            }
            // Capturing a network object for cleanup grants no new admission.
            assert!(
                fixture
                    .tasks
                    .network(PERMIT)
                    .unwrap()
                    .check_current()
                    .is_err()
            );
            assert_eq!(fixture.credentials.reads.load(Ordering::SeqCst), 1);
        }
    }

    #[test]
    fn completion_is_correlated_once_and_revokes_existing_network_handles() {
        let fixture = Fixture::new(TaskMode::Script, None);
        fixture.tasks.bind(TASK, CONVERSATION).unwrap();
        assert!(fixture.tasks.finish("another-task", CONVERSATION).is_err());
        assert!(fixture.tasks.finish(TASK, CONVERSATION + 1).is_err());
        fixture.network.check_current().unwrap();
        assert!(!fixture.tasks.finish(TASK, CONVERSATION).unwrap());
        assert!(fixture.network.check_current().is_err());
        assert!(fixture.tasks.network(PERMIT).is_err());
        assert!(fixture.tasks.bind(TASK, CONVERSATION).is_err());
        assert!(fixture.tasks.finish(TASK, CONVERSATION).is_err());
        assert!(!fixture.tasks.may_present(CONVERSATION, None));
        assert!(fixture.tasks.may_present(CONVERSATION, Some(CONVERSATION)));
    }

    #[test]
    fn model_prose_and_unrecorded_tool_replies_do_not_count_as_native_edits() {
        let fixture = Fixture::new(TaskMode::Edit, None);
        fixture.tasks.bind(TASK, CONVERSATION).unwrap();
        let invocation = fixture.invocation(CONVERSATION);
        let reply = json!({"sessionId":17,"revision":4,"applied":true});
        let response = json!({"jsonrpc":"2.0","id":1,"result":{
            "isError":false,"structuredContent":reply,
        }});
        assert_eq!(
            fixture
                .tasks
                .record_edit_result(&invocation, &response)
                .unwrap_err(),
            "runtime.editReceiptUnconfirmed",
            "a fabricated positive result cannot bypass the operation journal"
        );
        assert_eq!(
            fixture
                .tasks
                .record_edit_result(&invocation, &json!({"result":{"isError":false}}))
                .unwrap_err(),
            "runtime.editReceiptUnconfirmed"
        );
        fixture
            .tasks
            .record_edit_result(
                &invocation,
                &json!({"result":{"isError":true,"structuredContent":reply}}),
            )
            .unwrap();
        fixture
            .tasks
            .record_edit_result(
                &invocation,
                &json!({"result":{"isError":false,"structuredContent":{
                    "sessionId":17,"revision":3
                }}}),
            )
            .unwrap();
        assert!(!fixture.tasks.finish(TASK, CONVERSATION).unwrap());
    }

    #[test]
    fn only_an_original_bound_and_persisted_changed_edit_marks_completion() {
        let fixture = Fixture::new(TaskMode::Edit, None);
        fixture.tasks.bind(TASK, CONVERSATION).unwrap();
        let invocation = fixture.invocation(CONVERSATION);
        let project = fixture.project();
        let journal = fixture.bridge.journal.lock().unwrap().clone().unwrap();
        let request = json!({"method":"edit.applyGuarded","path":project.path,
            "expectedSession":project.session_id,"expectedRevision":project.revision,
            "command":{"op":"batch","commands":[]}});
        let operation = match journal
            .prepare(
                &invocation,
                &request,
                &project.path,
                project.session_id,
                project.revision,
            )
            .unwrap()
        {
            concat_agent::operations::Admission::New(id) => id,
            other => panic!("new original Pi call expected: {other:?}"),
        };
        let native_reply = json!({
            "sessionId":project.session_id, "revision":project.revision + 1,
            "canUndo":true, "canRedo":false,
            "settings":{"name":"reviewed-project","width":1920,"height":1080,
                "rateNum":30,"rateDen":1},
            "project":{"activeTimelineId":"main","media":[],"timelines":[{
                "id":"main","name":"Main","tracks":[],"clips":[]
            }]}
        });
        let summary = concat_mcp::summary::view(&native_reply);
        assert_ne!(
            native_reply, summary,
            "real native receipts are not MCP summaries"
        );
        let response = json!({"result":{"isError":false,"structuredContent":summary}});
        assert_eq!(
            fixture
                .tasks
                .record_edit_result(&invocation, &response)
                .unwrap_err(),
            "runtime.editReceiptUnconfirmed"
        );
        journal
            .record_receipt(
                operation,
                project.session_id,
                project.revision + 1,
                &native_reply,
            )
            .unwrap();
        let forged = Invocation {
            call_id: "not-the-original-call".into(),
            ..invocation.clone()
        };
        assert_eq!(
            fixture
                .tasks
                .record_edit_result(&forged, &response)
                .unwrap_err(),
            "runtime.editReceiptUnconfirmed"
        );
        let cross_conversation = Invocation {
            conversation_id: CONVERSATION + 1,
            ..invocation.clone()
        };
        assert_eq!(
            fixture
                .tasks
                .record_edit_result(&cross_conversation, &response)
                .unwrap_err(),
            "runtime.editReceiptUnconfirmed"
        );
        let mut inaccurate = response.clone();
        inaccurate["result"]["structuredContent"]["timeline"]["id"] = json!("forged");
        assert_eq!(
            fixture
                .tasks
                .record_edit_result(&invocation, &inaccurate)
                .unwrap_err(),
            "runtime.editReceiptUnconfirmed"
        );
        fixture
            .tasks
            .record_edit_result(&invocation, &response)
            .unwrap();
        assert!(fixture.tasks.finish(TASK, CONVERSATION).unwrap());
    }
}

#[cfg(test)]
mod reply_language_tests {
    use super::TaskLanguages;

    #[test]
    fn automatic_reply_policy_accepts_legacy_recovery_metadata_without_locale_injection() {
        let current = TaskLanguages::capture(Some("ja"));
        assert_eq!(current.reply_language, "auto");
        assert_eq!(current.content_language, "ja");
        assert!(current.validate().is_ok());
        let mut legacy = current.clone();
        legacy.reply_language = "de".into();
        assert!(legacy.validate().is_ok());
        legacy.reply_language = "arbitrary prompt".into();
        assert!(legacy.validate().is_err());
        let mut invalid_content = current;
        invalid_content.content_language = "auto".into();
        assert!(invalid_content.validate().is_err());
    }
}
