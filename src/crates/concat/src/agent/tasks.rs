// SPDX-License-Identifier: AGPL-3.0-or-later
//! Native per-submission authority. Pi owns conversations, tasks and scheduling;
//! this module only binds one explicit UI action to its native project and limits.

use std::path::Path;
use std::sync::{Arc, Mutex};

use concat_agent::configuration::{ConfigurationSnapshot, ConfigurationStore};
use concat_agent::conversations::{ConversationBindings, inspect_latest};
use concat_agent::credentials::platform_store;
use concat_agent::network::{
    MAX_REQUESTS_PER_TASK, ModelNetwork, ModelRequest, runtime_configuration,
};
use concat_agent::operations::Invocation;
use concat_agent::scripts::{ScriptStore, VersionBinding};
use serde_json::{Value, json};

use super::AgentBridge;
use crate::embedded::random_capability;
use crate::live::CurrentProject;
use crate::panes::script::GenerationRequest;
use crate::studio::Studio;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
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
}

/// Captured on the native UI thread before any asynchronous preparation.
#[derive(Clone)]
pub(crate) struct SubmissionIntent {
    pub id: String,
    pub project: CurrentProject,
    pub mode: TaskMode,
    pub input: String,
    pub selected: Option<VersionBinding>,
    pub generation: Option<GenerationRequest>,
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
        let configuration = runtime_configuration(&snapshot).unwrap_or(Value::Null);
        *self.startup.lock().map_err(|_| "模型配置状态不可用。")? = Some(snapshot);
        Ok(configuration)
    }

    /// Worker-only. No authority is derived from a page's proposed model or
    /// script version. The caller has already consumed the native fee action.
    pub(crate) fn prepare(&self, intent: &SubmissionIntent) -> Result<Value, String> {
        if intent.input.trim().is_empty() || intent.input.len() > 256 * 1024 {
            return Err("请输入有效内容，每次请求最多256千字节。".into());
        }
        if self.task.lock().map_err(|_| "任务状态不可用。")?.is_some() {
            return Err("已有任务正在执行，请等待完成或停止后重新连接。".into());
        }
        let snapshot = self
            .startup
            .lock()
            .map_err(|_| "模型配置状态不可用。")?
            .clone()
            .ok_or("请先连接聊天运行组件。")?;
        self.store
            .assert_current(&snapshot)
            .map_err(|error| error.to_string())?;
        let current = self
            .bridge
            .live
            .current_project()
            .map_err(|error| error.to_string())?
            .ok_or("当前没有打开的工程。")?;
        if current != intent.project {
            return Err("工程已切换或修改，本次请求未发送；请重新确认。".into());
        }
        if concat_host::project_identity::read_persisted(Path::new(&current.path))?.as_deref()
            != Some(current.project_id.as_str())
        {
            return Err("请先保存当前原生工程的持久身份，再发送模型任务。".into());
        }
        // History and selected scripts share the existing native location
        // registry. A copied manifest must not inherit another live folder's
        // conversations merely because it carries the same persistent id.
        let (input, existing) = self.at_project_location(&current, || {
            let mut input = intent.input.clone();
            if intent.mode == TaskMode::Edit {
                let selected = intent
                    .selected
                    .as_ref()
                    .ok_or("请先在文案工作区明确选择本次剪辑版本。")?;
                if selected.project_id != current.project_id {
                    return Err("所选文案不属于当前工程。".into());
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
                    return Err("本次剪辑选版已改变，请重新发送。".into());
                }
                let version = scripts
                    .resolve_binding(selected)
                    .map_err(|error| error.to_string())?;
                input = format!(
                    "原生捕获的本次剪辑依据（不可改写版本声明）：{}\n文案正文：\n{}\n\n用户要求：\n{}\n\n先读取当前工程；只能使用已实际导入且可用的素材和现有轨道。短片模板使用一条主视频轨，字幕、贴片文字、音乐和配音分别保持可编辑。必须按真实时长生成一个精确批次；素材或轨道不具备时明确说明缺口，禁止虚构。媒体新增只能使用addMediaClipExact，禁止自动寻找空隙。调用一次edit_apply_guarded提交完整批次。",
                    serde_json::to_string(selected).map_err(|_| "文案绑定无法编码。")?,
                    version.content,
                    input
                );
            }
            if let Some(generation) = &intent.generation {
                if intent.mode != TaskMode::Script
                    || generation.project_id() != current.project_id
                    || generation.session_id() != current.session_id
                    || generation.project_revision() != current.revision
                {
                    return Err("文案生成上下文已失效，未发送模型任务。".into());
                }
                input = format!(
                    "本次只生成独立文案候选，不得修改原生工程。原生文案源摘要：{}。只返回候选正文，不包裹代码块。\n{}",
                    generation.ticket().sha256,
                    input
                );
            }
            input = format!(
                "原生当前工程上下文：{}\n任务权限：{}。只读任务不能编辑工程。\n{}",
                json!({
                    "path":current.path,"projectId":current.project_id,
                    "expectedSession":current.session_id,"expectedRevision":current.revision,
                }),
                intent.mode.as_str(),
                input
            );
            if input.len() > 512 * 1024 {
                return Err("所选文案与请求合计超过本次模型输入上限。".into());
            }
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
        // The location lock covers local identity/history work only. Do not
        // hold it while accessing a system vault or issuing network authority.
        let network = Arc::new(ModelNetwork::authorize(
            Arc::clone(&self.bridge.owner),
            self.bridge.token.clone(),
            self.store.clone(),
            snapshot,
            platform_store().map_err(|error| error.to_string())?,
            MAX_REQUESTS_PER_TASK,
        )?);
        let permit = random_capability()?;
        let payload = json!({"method":"runtime.submit","taskId":intent.id,"networkPermit":permit,
            "input":input,"mode":intent.mode.as_str(),
        });
        let mut payload = payload;
        if let Some(conversation) = existing {
            payload["conversationId"] = json!(conversation);
        }
        let mut task = self.task.lock().map_err(|_| "任务状态不可用。")?;
        if task.is_some() {
            return Err("已有任务进入执行，本次请求未发送。".into());
        }
        *self.network.lock().map_err(|_| "模型接收状态不可用。")? = Some(Arc::clone(&network));
        *task = Some(AuthorizedTask {
            id: intent.id.clone(),
            permit,
            project: current,
            mode: intent.mode,
            selected: intent.selected.clone(),
            expected_conversation: existing,
            conversation: None,
            network,
        });
        Ok(payload)
    }

    /// The original runtime binds a real Pi conversation before submit can resume
    /// its scheduler. The native task id was issued by the explicit UI action.
    pub(crate) fn bind(&self, id: &str, conversation: u64) -> Result<(), String> {
        if conversation > 9_007_199_254_740_991 {
            return Err("会话编号超出原版运行组件范围。".into());
        }
        let captured = self
            .task
            .lock()
            .map_err(|_| "任务状态不可用。")?
            .as_ref()
            .filter(|task| task.id == id)
            .cloned()
            .ok_or("本次任务未获原生授权。")?;
        let task = &captured;
        task.network.check_current()?;
        if task
            .expected_conversation
            .is_some_and(|value| value != conversation)
            || task.conversation.is_some_and(|value| value != conversation)
        {
            return Err("原版会话不属于本次任务绑定。".into());
        }
        let bindings = ConversationBindings::open(&self.bridge.owner, &self.bridge.token)
            .map_err(|error| error.to_string())?;
        bindings
            .bind(&task.project.project_id, task.mode.as_str(), conversation)
            .map_err(|error| error.to_string())?;
        task.network.check_current()?;
        let mut current = self.task.lock().map_err(|_| "任务状态不可用。")?;
        let task = current
            .as_mut()
            .filter(|task| task.id == captured.id && task.permit == captured.permit)
            .ok_or("会话绑定期间原生任务已退役。")?;
        task.conversation = Some(conversation);
        Ok(())
    }

    pub(crate) fn network(&self, permit: &str) -> Result<Arc<ModelNetwork>, String> {
        let task = self.task.lock().map_err(|_| "任务状态不可用。")?;
        let task = task
            .as_ref()
            .filter(|task| task.permit == permit && task.conversation.is_some())
            .ok_or("模型请求许可已失效或没有绑定原版会话。")?;
        Ok(Arc::clone(&task.network))
    }

    pub(crate) fn open(&self, permit: &str, request: ModelRequest) -> Result<Value, String> {
        let expected = self
            .task
            .lock()
            .map_err(|_| "任务状态不可用。")?
            .as_ref()
            .map(|task| task.project.clone())
            .ok_or("没有已授权任务。")?;
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
            return Err("工程已切换，模型请求许可失效。".into());
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
            .map_err(|_| "任务状态不可用。")?
            .as_ref()
            .cloned()
            .ok_or("工具调用没有本次原生任务授权。")?;
        let invocation = invocation.ok_or("工具调用缺少原版会话身份。")?;
        if task.conversation != Some(invocation.conversation_id) {
            return Err("工具调用不属于当前已绑定会话。".into());
        }
        task.network.check_current()?;
        let name = message
            .pointer("/params/name")
            .and_then(Value::as_str)
            .ok_or("工具名称缺失。")?
            .to_owned();
        if name == "concat_version" {
            return Ok(());
        }
        let args = message
            .pointer_mut("/params/arguments")
            .and_then(Value::as_object_mut)
            .ok_or("工具参数无效。")?;
        if args.get("path").and_then(Value::as_str) != Some(task.project.path.as_str()) {
            return Err("工具仅可访问本次原生任务绑定的当前工程。".into());
        }
        if name == "edit_apply_guarded" {
            if task.mode != TaskMode::Edit || task.selected.is_none() {
                return Err("本次为只读任务，原生权限层拒绝修改工程。".into());
            }
            if args.get("expectedSession").and_then(Value::as_u64) != Some(task.project.session_id)
                || args.get("expectedRevision").and_then(Value::as_u64)
                    != Some(task.project.revision)
            {
                return Err("剪辑计划没有使用本次原生捕获的工程会话与版本。".into());
            }
        } else if !matches!(name.as_str(), "project_get" | "timeline_read") {
            return Err("此工具不在本次原生任务权限内。".into());
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
        let task = self.task.lock().map_err(|_| "任务状态不可用。")?;
        let task = task.as_ref().ok_or("本次剪辑授权已结束。")?;
        let current = studio.session.as_ref().ok_or("当前工程已关闭。")?;
        if task.mode != TaskMode::Edit
            || task.project.session_id != session
            || task.project.revision != revision
            || current.session_id() != session
            || current.revision() != revision
            || current.project_identity() != task.project.project_id
        {
            return Err("工程已变化或本次任务没有剪辑权限，旧计划被拒绝。".into());
        }
        if !studio.script.selection_settled() || studio.script.selected_binding() != task.selected {
            return Err("文案选版已改变或正在保存，本次旧计划被拒绝。".into());
        }
        Ok(())
    }

    pub(crate) fn finish(&self, id: &str, conversation: u64) -> Result<(), String> {
        let mut task = self.task.lock().map_err(|_| "任务状态不可用。")?;
        if !task
            .as_ref()
            .is_some_and(|task| task.id == id && task.conversation == Some(conversation))
        {
            return Err("拒绝了不属于当前任务的完成回调。".into());
        }
        if let Some(task) = task.take() {
            task.network.cancel();
        }
        self.network
            .lock()
            .map_err(|_| "模型接收状态不可用。")?
            .take();
        Ok(())
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
        std::fs::create_dir_all(&root)
            .map_err(|_| "无法核对工程持久归属，未读取会话或发送模型任务。".to_owned())?;
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
mod tests {
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
                scope: CredentialScope::new(
                    channel.id.as_str(),
                    &channel.service_origin().unwrap(),
                )
                .unwrap(),
                secret: Secret::new(random_capability().unwrap()).unwrap(),
                reads: AtomicUsize::new(0),
            });
            let network = Arc::new(
                ModelNetwork::authorize(
                    Arc::clone(&bridge.owner),
                    bridge.token.clone(),
                    tasks.store.clone(),
                    snapshot.clone(),
                    credentials.clone(),
                    MAX_REQUESTS_PER_TASK,
                )
                .unwrap(),
            );
            *tasks.startup.lock().unwrap() = Some(snapshot);
            *tasks.network.lock().unwrap() = Some(Arc::clone(&network));
            *tasks.task.lock().unwrap() = Some(AuthorizedTask {
                id: TASK.into(),
                permit: PERMIT.into(),
                project: CurrentProject {
                    project_id: PROJECT.into(),
                    path: directory.0.join("project").to_string_lossy().into_owned(),
                    session_id: 17,
                    revision: 3,
                },
                mode,
                selected: (mode == TaskMode::Edit).then(|| VersionBinding {
                    project_id: PROJECT.into(),
                    document_id: "fixture-document".into(),
                    version: 2,
                    sha256: "a".repeat(64),
                }),
                expected_conversation,
                conversation: None,
                network: Arc::clone(&network),
            });
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
            assert!(fixture.tasks.bind(TASK, 9_007_199_254_740_992).is_err());
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
            for tool in ["concat_version", "project_get", "timeline_read"] {
                let mut message = fixture.message(tool);
                fixture.authorize(&mut message, CONVERSATION).unwrap();
                assert!(fixture.authorize(&mut message, CONVERSATION + 1).is_err());
                assert!(fixture.tasks.authorize_tool(&mut message, None).is_err());
            }
            for tool in ["edit_apply_guarded", "project_delete", "media_upload"] {
                assert!(
                    fixture
                        .authorize(&mut fixture.message(tool), CONVERSATION)
                        .is_err()
                );
            }
            for tool in ["project_get", "timeline_read"] {
                let mut message = fixture.message(tool);
                message["params"]["arguments"]["path"] = json!("another-project");
                assert!(fixture.authorize(&mut message, CONVERSATION).is_err());
            }
        }
    }

    #[test]
    fn editable_task_requires_selected_script_and_exact_native_session_revision() {
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
            assert!(fixture.authorize(&mut message, CONVERSATION).is_err());
        }
        fixture
            .tasks
            .task
            .lock()
            .unwrap()
            .as_mut()
            .unwrap()
            .selected = None;
        assert!(fixture.authorize(&mut valid.clone(), CONVERSATION).is_err());
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
                    fixture.tasks.store.select(&prior, None).unwrap();
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
        fixture.tasks.finish(TASK, CONVERSATION).unwrap();
        assert!(fixture.network.check_current().is_err());
        assert!(fixture.tasks.network(PERMIT).is_err());
        assert!(fixture.tasks.bind(TASK, CONVERSATION).is_err());
        assert!(fixture.tasks.finish(TASK, CONVERSATION).is_err());
        assert!(!fixture.tasks.may_present(CONVERSATION, None));
        assert!(fixture.tasks.may_present(CONVERSATION, Some(CONVERSATION)));
    }
}
