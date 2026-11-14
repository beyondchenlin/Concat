// SPDX-License-Identifier: AGPL-3.0-or-later

//! One native component's private message ports. Platform code verifies the
//! bundled page and component capability before passing a payload here.
//! Recovery reads never open writable storage; only a one-shot native UI
//! approval followed by a fresh worker-side reconciliation can do that.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Read;
use std::path::Path;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use concat_agent::conversations::CONVERSATIONS_FILE_NAME;
use concat_agent::operations::{
    Invocation, JournalInspection, OPERATIONS_FILE_NAME, inspect_operations,
};
use concat_agent::sqlite::{
    DATABASE_FILE_NAME, NativeSqlite, PI_SCHEMA_VERSION, SqliteMethod, SqliteRequest,
    StorageInspection, StorageState, inspect_storage,
};
use concat_agent::{Phase, Reconciliation, RecoveryDecision};
use serde::Deserialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::{AgentBridge, Ordering};
use crate::embedded::random_capability;
use crate::live::CurrentProject;

/// A logical application store, never a caller-selected directory.
pub(crate) const STORAGE_ID: &str = "bk-cat-agent-session-v1";

type ProjectReader = Arc<dyn Fn() -> Result<Option<CurrentProject>, String> + Send + Sync>;

#[derive(Clone, Debug, Eq, PartialEq)]
struct Evidence {
    project: Option<CurrentProject>,
    storage: StorageInspection,
    journal: JournalInspection,
    session_revision: String,
    project_fingerprint: String,
    bindings_fingerprint: String,
    discard_conversations: Vec<u64>,
    discard_scope_error: Option<String>,
}

impl Evidence {
    fn unfinished_pi(&self) -> bool {
        self.storage.counts.as_ref().is_some_and(|counts| {
            counts
                .tasks
                .iter()
                .any(|(status, count)| *count > 0 && status != "terminal")
                || counts.submissions.iter().any(|(status, count)| {
                    *count > 0 && !matches!(status.as_str(), "done" | "unanswered")
                })
        })
    }

    fn unsettled_edits(&self) -> bool {
        self.journal.needs_project_reconciliation > 0
            || self.journal.applied_not_saved > 0
            || self.journal.saved_not_notified > 0
    }

    fn interrupted(&self) -> bool {
        self.unfinished_pi() || self.unsettled_edits()
    }

    fn common_blocked_reason(&self) -> Option<&'static str> {
        if self.project.is_none() {
            return Some("请先打开原生工程，再确认会话恢复");
        }
        if self.storage.state == StorageState::Existing && !self.storage.compatible {
            return Some("会话数据库版本或结构不兼容，已保持只读；不能自动迁移或覆盖");
        }
        if self.unsettled_edits() {
            return Some(
                "原生编辑或保存记录尚未核对，不能放弃任务或恢复会话；现有工程与日志保持不变",
            );
        }
        None
    }

    fn blocked_reason(&self) -> Option<&str> {
        if let Some(reason) = self.common_blocked_reason() {
            return Some(reason);
        }
        if self.unfinished_pi() {
            self.discard_scope_error.as_deref().or(Some(
                "检测到未完成任务。可明确放弃当前工程已登记的旧任务后打开会话；不会恢复模型调用，已有消息保留。",
            ))
        } else {
            None
        }
    }

    fn discard_blocked_reason(&self) -> Option<&str> {
        if let Some(reason) = self.common_blocked_reason() {
            return Some(reason);
        }
        if !self.unfinished_pi() {
            Some("当前没有待放弃的未完成任务，请使用确认继续。")
        } else if let Some(error) = self.discard_scope_error.as_deref() {
            Some(error)
        } else if self.discard_conversations.is_empty() {
            Some("当前工程没有可核对的历史会话归属，未完成任务保持停止。")
        } else {
            None
        }
    }

    fn decision_blocked_reason(&self, decision: &str) -> Option<&str> {
        if decision == "discard" {
            self.discard_blocked_reason()
        } else {
            self.blocked_reason()
        }
    }
}

#[derive(Clone)]
struct Snapshot {
    id: String,
    evidence: Evidence,
}

impl Snapshot {
    fn value(&self) -> Value {
        let project = self.evidence.project.as_ref();
        let schema = if self.evidence.storage.state == StorageState::Empty {
            json!("empty")
        } else {
            json!(self.evidence.storage.schema_version.unwrap_or(0))
        };
        json!({
            "snapshotId": self.id,
            "projectSession": project.map(|p| p.session_id.to_string()).unwrap_or_else(|| "none".into()),
            "projectRevision": project.map(|p| p.revision.to_string()).unwrap_or_else(|| "none".into()),
            "sessionRevision": self.evidence.session_revision,
            "schemaVersion": schema,
            "hasInterruptedWork": self.evidence.interrupted(),
            "canDiscardInterrupted": self.evidence.discard_blocked_reason().is_none(),
            "projectPath": project.map(|p| p.path.as_str()),
            "blockedReason": self.evidence.blocked_reason(),
        })
    }
}

#[derive(Default)]
struct ApprovalState {
    snapshot: Option<Snapshot>,
    pending: Option<Value>,
}

#[derive(Default)]
struct State {
    acquired: bool,
    granted: bool,
    initialize_empty: bool,
    database: Option<NativeSqlite>,
    closed_database: Option<String>,
    closed: bool,
    awaiting_delivery: BTreeSet<u64>,
    discard: Option<DiscardVerification>,
}

#[derive(Clone)]
struct DiscardVerification {
    project: CurrentProject,
    project_fingerprint: String,
    bindings_fingerprint: String,
    conversations: Vec<u64>,
    attempted: bool,
    verified: bool,
}

impl State {
    fn execution_ready(&self) -> bool {
        self.granted && !self.closed && self.discard.as_ref().is_none_or(|scope| scope.verified)
    }
}

/// A single component bound to an already-held cross-process session owner.
/// Slow work is serialised on the private worker. The UI approval lock is
/// separate and short-lived: no UI action waits for the editor or disk.
pub(crate) struct NativeSession {
    pub(crate) tasks: super::tasks::NativeTasks,
    bridge: Arc<AgentBridge>,
    handle: String,
    generation: String,
    project_reader: ProjectReader,
    state: Mutex<State>,
    approval: Mutex<ApprovalState>,
}

impl NativeSession {
    pub(crate) fn new(bridge: Arc<AgentBridge>, component_id: String) -> Result<Self, String> {
        let live = Arc::clone(&bridge.live);
        Self::with_reader(
            bridge,
            component_id,
            Arc::new(move || live.current_project().map_err(|error| error.to_string())),
        )
    }

    fn with_reader(
        bridge: Arc<AgentBridge>,
        component_id: String,
        project_reader: ProjectReader,
    ) -> Result<Self, String> {
        if component_id.is_empty()
            || component_id.len() > 256
            || component_id.chars().any(char::is_control)
        {
            return Err("native component identity is invalid".into());
        }
        let handle = format!("{component_id}.{}", random_capability()?);
        if bridge.status()?.phase != Phase::RecoveryRequired {
            return Err("a new component requires a fresh recovery-only native generation".into());
        }
        bridge
            .owner
            .with_owned(&bridge.token, || ())
            .map_err(|error| error.to_string())?;
        bridge
            .component_claimed
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .map_err(|_| {
                "this native generation already belongs to a runtime component".to_owned()
            })?;
        Ok(Self {
            tasks: super::tasks::NativeTasks::new(Arc::clone(&bridge)),
            generation: bridge.token.generation().to_string(),
            bridge,
            handle,
            project_reader,
            state: Mutex::new(State::default()),
            approval: Mutex::new(ApprovalState::default()),
        })
    }

    fn state(&self) -> Result<MutexGuard<'_, State>, String> {
        self.state
            .lock()
            .map_err(|_| "native component state failed".into())
    }

    fn approval(&self) -> Result<MutexGuard<'_, ApprovalState>, String> {
        self.approval
            .lock()
            .map_err(|_| "native recovery approval state failed".into())
    }

    /// A page's ready event cannot bypass the native completion of an explicit
    /// discard. Writable recovery storage alone never admits a new model task.
    pub(crate) fn prepare_submission(
        &self,
        intent: &super::tasks::SubmissionIntent,
    ) -> Result<Value, String> {
        if !self.state()?.execution_ready() {
            return Err("原生会话恢复核验尚未完成，未签发模型任务许可。".into());
        }
        self.tasks.prepare(intent)
    }

    fn check_bound(&self, payload: &Value) -> Result<(), String> {
        if payload.get("ownerHandle").and_then(Value::as_str) != Some(self.handle.as_str())
            || payload.get("generation").and_then(Value::as_str) != Some(self.generation.as_str())
        {
            return Err("native owner handle or generation does not match this component".into());
        }
        Ok(())
    }

    fn check_owned(&self) -> Result<(), String> {
        self.bridge
            .owner
            .with_owned(&self.bridge.token, || ())
            .map_err(|error| error.to_string())
    }

    /// Worker-only: routes the exact native-wire/sqlite-port contract.
    pub(crate) fn handle(&self, payload: Value) -> Result<Value, String> {
        let method = text_field(&payload, "method")?.to_owned();
        let mut state = self.state()?;
        if method == "ownership.acquire" {
            allow_keys(&payload, &["method", "storageId"])?;
            if text_field(&payload, "storageId")? != STORAGE_ID {
                return Err("only the application's fixed session store is available".into());
            }
            self.check_owned()?;
            if state.acquired || state.closed {
                return Err("this component cannot acquire a second host or restart".into());
            }
            state.acquired = true;
            return Ok(
                json!({"storageId": STORAGE_ID, "handle": self.handle, "generation": self.generation}),
            );
        }
        if method == "request.cancel" {
            allow_keys(
                &payload,
                &["method", "requestId", "ownerHandle", "generation"],
            )?;
            text_field(&payload, "requestId")?;
            if payload.get("ownerHandle").is_some() || payload.get("generation").is_some() {
                self.check_bound(&payload)?;
            }
            // The outer component capability was already checked. Cancelling
            // even an acquire revokes this component only; it grants nothing.
            self.bridge.stop();
            self.approval()?.pending = None;
            return Ok(Value::Null);
        }
        if !state.acquired {
            return Err("native ownership has not been acquired by this component".into());
        }
        self.check_bound(&payload)?;
        if method == "ownership.revoke" {
            allow_bound_keys(&payload, &[])?;
            self.bridge.stop();
            self.approval()?.pending = None;
            return Ok(Value::Null);
        }
        if method == "ownership.drainAndRelease" {
            allow_bound_keys(&payload, &[])?;
            self.bridge.stop();
            self.close_resources(&mut state)?;
            // A page cannot attest to its own destruction. Final OS-lock
            // release occurs only in shutdown after the platform closed event.
            return Ok(json!({"released": false, "awaitingComponentClose": true}));
        }
        if method.starts_with("sqlite.") {
            return self.sqlite(&mut state, payload);
        }
        self.check_owned()?;
        if state.closed {
            return Err("native component is closed".into());
        }
        match method.as_str() {
            "task.bind" => {
                allow_bound_keys(&payload, &["taskId", "conversationId"])?;
                if !state.execution_ready() {
                    return Err("任务绑定尚未完成原生恢复确认。".into());
                }
                let conversation = payload["conversationId"]
                    .as_u64()
                    .ok_or("原版会话编号无效。")?;
                self.tasks
                    .bind(text_field(&payload, "taskId")?, conversation)?;
                Ok(Value::Null)
            }
            "network.open" => {
                allow_bound_keys(
                    &payload,
                    &["networkPermit", "url", "methodName", "headers", "body"],
                )?;
                let request = serde_json::from_value(json!({"url":payload["url"],"methodName":payload["methodName"],"headers":payload["headers"],"body":payload["body"]}))
                    .map_err(|_| "原生模型请求格式无效。")?;
                self.tasks
                    .open(text_field(&payload, "networkPermit")?, request)
            }
            "network.read" => {
                allow_bound_keys(&payload, &["networkPermit", "streamId"])?;
                self.tasks
                    .network(text_field(&payload, "networkPermit")?)?
                    .read(text_field(&payload, "streamId")?)
            }
            "network.cancel" => {
                allow_bound_keys(&payload, &["networkPermit", "streamId"])?;
                self.tasks
                    .network(text_field(&payload, "networkPermit")?)?
                    .cancel_stream(text_field(&payload, "streamId")?)?;
                Ok(Value::Null)
            }
            "recovery.read" => {
                allow_bound_keys(&payload, &[])?;
                self.snapshot_locked(&state)
            }
            "recovery.discardVerified" => {
                allow_bound_keys(&payload, &[])?;
                self.verify_discard(&mut state)
            }
            "ownership.confirm" => {
                allow_bound_keys(&payload, &["snapshot", "approval"])?;
                self.confirm(&mut state, &payload)
            }
            "operation.acknowledge" => {
                allow_bound_keys(&payload, &["operationId"])?;
                if !state.granted {
                    return Err("save acknowledgements are unavailable during recovery".into());
                }
                let text = text_field(&payload, "operationId")?;
                let operation_id: u64 = text
                    .parse()
                    .map_err(|_| "invalid native operation identity".to_owned())?;
                if operation_id.to_string() != text
                    || !state.awaiting_delivery.contains(&operation_id)
                {
                    return Err("this component has not received that save notification".into());
                }
                let journal = self
                    .bridge
                    .journal
                    .lock()
                    .map_err(|_| "native journal state failed".to_owned())?
                    .clone()
                    .ok_or_else(|| "native journal is closed".to_owned())?;
                journal
                    .record_notified(operation_id)
                    .map_err(|error| error.to_string())?;
                state.awaiting_delivery.remove(&operation_id);
                Ok(Value::Null)
            }
            "mcp.send" => {
                allow_bound_keys(&payload, &["message", "invocation"])?;
                let mut message = payload
                    .get("message")
                    .filter(|value| value.is_object())
                    .ok_or_else(|| "one internal protocol object is required".to_owned())?
                    .clone();
                let invocation = payload
                    .get("invocation")
                    .map(parse_invocation)
                    .transpose()?;
                if message.get("method").and_then(Value::as_str) == Some("initialize")
                    && message
                        .pointer("/params/protocolVersion")
                        .and_then(Value::as_str)
                        != Some("2025-11-25")
                {
                    return Err(
                        "the internal protocol version does not match the pinned runtime".into(),
                    );
                }
                if message.get("method").and_then(Value::as_str) == Some("tools/call")
                    && message.pointer("/params/name").and_then(Value::as_str)
                        == Some("edit_apply_guarded")
                    && (!state.granted || invocation.is_none())
                {
                    return Err(
                        "guarded edits require native recovery approval and original Pi invocation"
                            .into(),
                    );
                }
                let guarded_edit = message.get("method").and_then(Value::as_str)
                    == Some("tools/call")
                    && message.pointer("/params/name").and_then(Value::as_str)
                        == Some("edit_apply_guarded");
                self.tasks
                    .authorize_tool(&mut message, invocation.as_ref())?;
                let response = self
                    .bridge
                    .handle_with_invocation(message, invocation.clone())?;
                if guarded_edit {
                    let reply = response
                        .as_ref()
                        .ok_or("原生剪辑工具没有返回结果，需核对当前工程。")?;
                    self.tasks.record_edit_result(
                        invocation.as_ref().ok_or("原生工具调用身份缺失。")?,
                        reply,
                    )?;
                }
                Ok(response.unwrap_or(Value::Null))
            }
            _ => Err("unreviewed native port method".into()),
        }
    }

    /// A network reader can be cancelled without waiting behind its own read.
    pub(crate) fn cancel_network_frame(&self, payload: &Value) -> Result<Value, String> {
        self.check_bound(payload)?;
        allow_bound_keys(payload, &["networkPermit", "streamId"])?;
        self.tasks
            .network(text_field(payload, "networkPermit")?)?
            .cancel_stream(text_field(payload, "streamId")?)?;
        Ok(Value::Null)
    }

    /// Worker-only: gather current evidence without creating a writable store.
    #[cfg(any(test, feature = "bk-cat-native-probe"))]
    pub(crate) fn snapshot(&self) -> Result<Value, String> {
        let state = self.state()?;
        if !state.acquired || state.closed {
            return Err("native recovery is not open".into());
        }
        self.snapshot_locked(&state)
    }

    /// Worker-only: deliver only saves created in this active generation.
    /// Dequeuing is not proof of receipt; the bound page must acknowledge each
    /// operation separately. Recovery never drains a prior generation's queue.
    pub(crate) fn take_saved_notifications(&self) -> Result<Vec<Value>, String> {
        let mut state = self.state()?;
        if !state.acquired || !state.granted || state.closed {
            return Ok(Vec::new());
        }
        self.check_owned()?;
        let journal = self
            .bridge
            .journal
            .lock()
            .map_err(|_| "native journal state failed".to_owned())?
            .clone()
            .ok_or_else(|| "native journal is closed".to_owned())?;
        let saved = journal
            .take_saved_notifications()
            .map_err(|error| error.to_string())?;
        let mut result = Vec::with_capacity(saved.len());
        for notification in saved {
            state.awaiting_delivery.insert(notification.operation_id);
            result.push(json!({"operationId":notification.operation_id.to_string(),
                "ownerHandle":self.handle,"generation":self.generation,"saved":notification.saved}));
        }
        Ok(result)
    }

    fn snapshot_locked(&self, state: &State) -> Result<Value, String> {
        if state.granted {
            return Err("this generation has already completed recovery".into());
        }
        let evidence = self.read_evidence()?;
        let mut approval = self.approval()?;
        if approval
            .snapshot
            .as_ref()
            .is_none_or(|snapshot| snapshot.evidence != evidence)
        {
            approval.snapshot = Some(Snapshot {
                id: random_capability()?,
                evidence,
            });
            approval.pending = None;
        }
        Ok(approval
            .snapshot
            .as_ref()
            .expect("snapshot was just installed")
            .value())
    }

    /// UI-only intent. It takes only the short approval lock; the worker will
    /// re-read every authority-bearing fact before consuming the one-shot id.
    pub(crate) fn approve_from_ui(
        &self,
        snapshot_id: &str,
        decision: &str,
    ) -> Result<Value, String> {
        if !matches!(decision, "continue" | "discard") {
            return Err("the recovery decision is invalid".into());
        }
        let mut state = self.approval()?;
        let snapshot = state
            .snapshot
            .as_ref()
            .ok_or_else(|| "read native recovery evidence first".to_owned())?;
        if snapshot.id != snapshot_id {
            return Err("the displayed recovery snapshot is stale; inspect again".into());
        }
        if let Some(reason) = snapshot.evidence.decision_blocked_reason(decision) {
            return Err(reason.to_owned());
        }
        if state.pending.is_some() {
            return Err("this native recovery decision is already awaiting consumption".into());
        }
        let approval = json!({"snapshotId": snapshot_id, "decision": decision, "approvalId": random_capability()?});
        state.pending = Some(approval.clone());
        Ok(approval)
    }

    fn confirm(&self, state: &mut State, payload: &Value) -> Result<Value, String> {
        if state.granted {
            return Err("recovery was already confirmed for this native generation".into());
        }
        let supplied = payload
            .get("approval")
            .ok_or_else(|| "native approval is missing".to_owned())?;
        allow_keys(supplied, &["snapshotId", "decision", "approvalId"])?;
        let snapshot = {
            let mut approvals = self.approval()?;
            let pending = approvals.pending.take().ok_or_else(|| {
                "a native user approval is required; the page cannot approve itself".to_owned()
            })?;
            if &pending != supplied {
                return Err(
                    "native recovery approval does not match the pending user decision".into(),
                );
            }
            let snapshot = approvals
                .snapshot
                .clone()
                .ok_or_else(|| "native recovery evidence is missing".to_owned())?;
            if payload.get("snapshot") != Some(&snapshot.value()) {
                return Err("the page changed the native recovery evidence".into());
            }
            snapshot
        };
        // Compare full current evidence, not page-supplied counts or a cache.
        let current = self.read_evidence()?;
        if current != snapshot.evidence {
            return Err("工程、会话或存储已变化；请重新核对后再确认".into());
        }
        let decision = text_field(supplied, "decision")?;
        if let Some(reason) = current.decision_blocked_reason(decision) {
            return Err(reason.to_owned());
        }
        let project = current
            .project
            .as_ref()
            .ok_or_else(|| "请先打开原生工程，再确认会话恢复".to_owned())?;
        let decision = match decision {
            "continue" => RecoveryDecision::ContinueAfterRevalidation,
            "discard" => RecoveryDecision::DiscardInterrupted,
            _ => return Err("invalid native recovery decision".into()),
        };
        self.bridge.confirm_recovery(
            Reconciliation {
                project_identity: format!("{}#{}", project.path, project.session_id),
                project_revision: project.revision.to_string(),
                session_revision: current.session_revision,
            },
            decision,
        )?;
        state.granted = true;
        state.initialize_empty = current.storage.state == StorageState::Empty;
        let mut grant = json!({
            "generation": self.generation,
            "snapshotId": snapshot.id,
            "allowInitializeEmptyStore": state.initialize_empty,
        });
        if matches!(decision, RecoveryDecision::DiscardInterrupted) {
            // This scope permits original Pi cancellation only. There is no
            // model grant, task authority, edit permission or automatic resume.
            state.discard = Some(DiscardVerification {
                project: project.clone(),
                project_fingerprint: current.project_fingerprint,
                bindings_fingerprint: current.bindings_fingerprint,
                conversations: current.discard_conversations.clone(),
                attempted: false,
                verified: false,
            });
            grant["discardConversationIds"] = json!(current.discard_conversations);
        }
        Ok(grant)
    }

    /// One final native fact check, never a new authorization. A failed first
    /// check keeps admission closed and cannot be retried in this component.
    /// A duplicate after success is refused without undoing that verified state.
    fn verify_discard(&self, state: &mut State) -> Result<Value, String> {
        if !state.granted || state.closed {
            return Err("当前组件没有已确认的放弃操作。".into());
        }
        let scope = state
            .discard
            .as_mut()
            .ok_or("本代没有明确放弃任务的原生决定。")?;
        if scope.attempted {
            return Err("本代放弃核验已使用，不能重复或重试。".into());
        }
        scope.attempted = true;
        let expected = scope.clone();
        let current = self.read_evidence()?;
        if current.storage.state != StorageState::Existing
            || !current.storage.compatible
            || current.storage.counts.is_none()
            || current.interrupted()
            || current.project.as_ref() != Some(&expected.project)
            || current.project_fingerprint != expected.project_fingerprint
            || current.bindings_fingerprint != expected.bindings_fingerprint
        {
            return Err(
                "放弃后仍有未完成记录，或工程身份、会话、版本和文件已改变；会话未恢复就绪。".into(),
            );
        }
        if self.tasks.recovery_conversations(&expected.project)? != expected.conversations
            || (self.project_reader)()?.as_ref() != Some(&expected.project)
        {
            return Err("放弃期间当前工程的真实位置或历史会话归属已改变，会话未恢复就绪。".into());
        }
        self.check_owned()?;
        state
            .discard
            .as_mut()
            .expect("discard review retained")
            .verified = true;
        Ok(json!({"verified":true}))
    }

    fn read_evidence(&self) -> Result<Evidence, String> {
        self.check_owned()?;
        let project = (self.project_reader)()?;
        let storage = inspect_storage(&self.bridge.owner, &self.bridge.token)
            .map_err(|error| error.to_string())?;
        let journal = inspect_operations(&self.bridge.owner, &self.bridge.token)
            .map_err(|error| error.to_string())?;
        // The owner remains held, but read-only file I/O never holds its gate.
        // Stop can revoke immediately, and every block plus the final result
        // rechecks the original generation before approval can be consumed.
        let mut digest = Sha256::new();
        let mut bindings_fingerprint = String::new();
        for name in [
            DATABASE_FILE_NAME,
            OPERATIONS_FILE_NAME,
            CONVERSATIONS_FILE_NAME,
        ] {
            let mut file_digest = Sha256::new();
            for suffix in ["", "-wal", "-journal"] {
                fingerprint(
                    &self
                        .bridge
                        .owner
                        .storage_root()
                        .join(format!("{name}{suffix}")),
                    &mut file_digest,
                    || self.check_owned(),
                )?;
            }
            let file_fingerprint = format!("{:x}", file_digest.finalize());
            digest.update(file_fingerprint.as_bytes());
            if name == CONVERSATIONS_FILE_NAME {
                bindings_fingerprint = file_fingerprint;
            }
        }
        let mut project_digest = Sha256::new();
        if let Some(project) = &project {
            fingerprint(
                &concat_host::projects::manifest_path(Path::new(&project.path)),
                &mut project_digest,
                || self.check_owned(),
            )?;
        }
        let project_fingerprint = format!("{:x}", project_digest.finalize());
        digest.update(project_fingerprint.as_bytes());
        let revision = format!("{:x}", digest.finalize());
        if (self.project_reader)()? != project {
            return Err(
                "the current native project changed while reading recovery evidence".into(),
            );
        }
        self.check_owned()?;
        let mut evidence = Evidence {
            project,
            storage,
            journal,
            session_revision: revision,
            project_fingerprint,
            bindings_fingerprint,
            discard_conversations: Vec::new(),
            discard_scope_error: None,
        };
        if evidence.unfinished_pi() && evidence.common_blocked_reason().is_none() {
            let project = evidence.project.as_ref().expect("checked native project");
            match self.tasks.recovery_conversations(project) {
                Ok(conversations) if !conversations.is_empty() => {
                    evidence.discard_conversations = conversations;
                }
                Ok(_) => {
                    evidence.discard_scope_error =
                        Some("当前工程没有可核对的历史会话归属，未完成任务保持停止。".into());
                }
                Err(error) => evidence.discard_scope_error = Some(error),
            }
        }
        if (self.project_reader)()? != evidence.project {
            return Err("核对恢复范围时工程已变化，请重新核对。".into());
        }
        self.check_owned()?;
        Ok(evidence)
    }

    fn sqlite(&self, state: &mut State, payload: Value) -> Result<Value, String> {
        let request: SqliteRequest = serde_json::from_value(payload)
            .map_err(|_| "invalid native SQLite request".to_owned())?;
        if request.method == SqliteMethod::Open {
            self.check_owned()?;
            if request.expected_schema_version != Some(PI_SCHEMA_VERSION) {
                return Err("the requested database schema is not the pinned Pi version".into());
            }
            if !state.granted || state.closed {
                return Err("writable storage requires a consumed native recovery approval".into());
            }
            if state.database.is_some() || state.closed_database.is_some() {
                return Err("this component already opened its one database connection".into());
            }
            let database = NativeSqlite::open_request(
                &self.bridge.owner,
                &self.bridge.token,
                &self.handle,
                state.initialize_empty,
                &request,
            )
            .map_err(|error| error.to_string())?;
            let result = json!({"databaseId": database.database_id()});
            state.database = Some(database);
            return Ok(result);
        }
        if request.method == SqliteMethod::Close
            && state.database.is_none()
            && request.database_id == state.closed_database
            && state.closed_database.is_some()
            && request.transaction_id.is_none()
            && request.sql.is_none()
            && request.params.is_none()
            && request.expected_schema_version.is_none()
            && request.allow_initialize_empty_store.is_none()
        {
            return Ok(Value::Null);
        }
        let database = state
            .database
            .as_mut()
            .ok_or_else(|| "the component has no matching open database".to_owned())?;
        let result = database
            .handle(&request)
            .map_err(|error| error.to_string())?;
        if request.method == SqliteMethod::Close {
            state.closed_database = Some(database.database_id().to_owned());
            state.database.take();
        }
        Ok(result)
    }

    fn close_resources(&self, state: &mut State) -> Result<(), String> {
        if let Some(database) = state.database.as_mut() {
            database.close().map_err(|error| error.to_string())?;
            state.closed_database = Some(database.database_id().to_owned());
            state.database.take();
        }
        self.bridge.close_journal()?;
        state.awaiting_delivery.clear();
        self.approval()?.pending = None;
        Ok(())
    }

    /// Called only by native glue after its actual platform closed event.
    /// A failed close/drain retains the revoked lock; no newer component wins.
    pub(crate) fn shutdown(&self) -> Result<(), String> {
        self.bridge.stop();
        self.tasks.abandon();
        {
            let mut state = self.state()?;
            self.close_resources(&mut state)?;
            state.closed = true;
        }
        self.bridge.finish_handoff(Duration::from_secs(3))
    }
}

impl Drop for NativeSession {
    fn drop(&mut self) {
        self.bridge.stop();
        self.tasks.abandon();
        // This is only a fail-closed fallback. Explicit shutdown owns the
        // platform-destruction acknowledgement and final handoff.
        let state = self
            .state
            .get_mut()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(database) = state.database.as_mut() {
            let _ = database.close();
        }
        let _ = self.bridge.close_journal();
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct WireInvocation {
    conversation_id: u64,
    task_id: u64,
    call_id: String,
}

fn parse_invocation(value: &Value) -> Result<Invocation, String> {
    let wire: WireInvocation = serde_json::from_value(value.clone())
        .map_err(|_| "the original Pi invocation identity is invalid".to_owned())?;
    if wire.conversation_id > 9_007_199_254_740_991
        || wire.task_id > 9_007_199_254_740_991
        || wire.call_id.is_empty()
        || wire.call_id.len() > 512
        || wire.call_id.chars().any(char::is_control)
    {
        return Err("the original Pi invocation identity is invalid".into());
    }
    Ok(Invocation {
        conversation_id: wire.conversation_id,
        task_id: wire.task_id,
        call_id: wire.call_id,
    })
}

fn text_field<'a>(value: &'a Value, field: &str) -> Result<&'a str, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| format!("native message requires {field}"))
}

fn allow_keys(value: &Value, allowed: &[&str]) -> Result<(), String> {
    let object = value
        .as_object()
        .ok_or_else(|| "native payload must be an object".to_owned())?;
    if object.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err("native message contains an unreviewed field".into());
    }
    Ok(())
}

fn allow_bound_keys(value: &Value, extra: &[&str]) -> Result<(), String> {
    let mut allowed = vec!["method", "ownerHandle", "generation"];
    allowed.extend_from_slice(extra);
    allow_keys(value, &allowed)
}

/// Hash content, not counts alone: edits preserving row counts still invalidate
/// a displayed recovery approval. Only native-owned/selected paths reach here.
fn fingerprint(
    path: &Path,
    digest: &mut Sha256,
    check: impl Fn() -> Result<(), String>,
) -> Result<(), String> {
    check()?;
    digest.update(path.to_string_lossy().as_bytes());
    match fs::symlink_metadata(path) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            digest.update([0]);
            Ok(())
        }
        Err(error) => Err(format!("recovery evidence could not be read: {error}")),
        Ok(metadata) => {
            if metadata.file_type().is_symlink() || !metadata.is_file() {
                return Err("recovery evidence is not an ordinary file".into());
            }
            #[cfg(windows)]
            {
                use std::os::windows::fs::MetadataExt;
                if metadata.file_attributes() & 0x400 != 0 {
                    return Err("recovery evidence cannot be a reparse point".into());
                }
            }
            digest.update([1]);
            digest.update(metadata.len().to_le_bytes());
            let mut file =
                File::open(path).map_err(|error| format!("read recovery evidence: {error}"))?;
            let mut read_total = 0_u64;
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                check()?;
                let read = file
                    .read(&mut buffer)
                    .map_err(|error| format!("read recovery evidence: {error}"))?;
                if read == 0 {
                    break;
                }
                read_total += read as u64;
                digest.update(&buffer[..read]);
            }
            if read_total != metadata.len() {
                return Err("recovery evidence changed while it was being read".into());
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use concat_api::{AppDirs, OpenProjects};
    use concat_host::export::Exporter;
    use std::path::PathBuf;

    struct Directory(PathBuf);
    impl Drop for Directory {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    struct Fixture {
        native: NativeSession,
        bridge: Arc<AgentBridge>,
        project: Arc<Mutex<Option<CurrentProject>>>,
        directory: Directory,
    }

    impl Fixture {
        fn new() -> Self {
            let root = std::env::temp_dir()
                .join(format!("bk-cat-native-{}", random_capability().unwrap()));
            fs::create_dir(&root).unwrap();
            let project_path = root.join("project");
            fs::create_dir(&project_path).unwrap();
            fs::write(
                project_path.join("concat.json"),
                b"{\"project\":\"isolated\"}",
            )
            .unwrap();
            let project = Arc::new(Mutex::new(Some(CurrentProject {
                project_id: "native-test-project".into(),
                path: project_path.to_str().unwrap().to_owned(),
                session_id: 7,
                revision: 1,
            })));
            let bridge = Arc::new(
                AgentBridge::acquire(
                    &AppDirs::under(&root),
                    OpenProjects::default(),
                    Exporter::new(),
                )
                .unwrap(),
            );
            let project_reader = Arc::clone(&project);
            let native = NativeSession::with_reader(
                Arc::clone(&bridge),
                random_capability().unwrap(),
                Arc::new(move || Ok(project_reader.lock().unwrap().clone())),
            )
            .unwrap();
            Self {
                native,
                bridge,
                project,
                directory: Directory(root),
            }
        }

        fn acquire(&self) -> Value {
            self.native
                .handle(json!({"method":"ownership.acquire","storageId":STORAGE_ID}))
                .unwrap()
        }

        fn approve(&self, lease: &Value) -> Value {
            let snapshot = self.native.snapshot().unwrap();
            let approval = self
                .native
                .approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "continue")
                .unwrap();
            self.native
                .handle(bound(
                    lease,
                    "ownership.confirm",
                    json!({"snapshot":snapshot,"approval":approval}),
                ))
                .unwrap()
        }

        fn open_database(&self, lease: &Value) -> String {
            self.approve(lease);
            self.native
                .handle(bound(
                    lease,
                    "sqlite.open",
                    json!({
                        "expectedSchemaVersion":PI_SCHEMA_VERSION,"allowInitializeEmptyStore":true,
                    }),
                ))
                .unwrap()["databaseId"]
                .as_str()
                .unwrap()
                .to_owned()
        }

        // Real files and fixed Pi schema, with only a native inspection row.
        // The original runtime's cancellation execution is covered separately
        // by its integration suite; this helper does not simulate a scheduler.
        fn with_bound_interrupted_task() -> Self {
            let fixture = Self::new();
            let current = {
                let mut project = fixture.project.lock().unwrap();
                let current = project.as_mut().unwrap();
                current.project_id = "bkcat-project:33333333333333333333333333333333".into();
                current.clone()
            };
            fs::write(
                Path::new(&current.path).join("concat.json"),
                serde_json::to_vec(
                    &json!({"project":"isolated", "bkCatProjectId":current.project_id}),
                )
                .unwrap(),
            )
            .unwrap();
            let root = fixture
                .bridge
                .dirs
                .data
                .join("bk-cat")
                .join("script-workspaces");
            fs::create_dir_all(&root).unwrap();
            concat_agent::script_locations::with_project_location(
                &root,
                &current.project_id,
                Path::new(&current.path),
                concat_host::project_identity::read_persisted,
                || Ok(()),
            )
            .unwrap();
            let lease = fixture.acquire();
            let database = fixture.open_database(&lease);
            fixture.native.handle(bound(&lease, "sqlite.exec", json!({
                "databaseId":database,
                "sql":include_str!("../../../concat-agent/tests/fixtures/pi-schema-1.5.2.sql"),
            }))).unwrap();
            fixture.native.handle(bound(&lease, "sqlite.run", json!({
                "databaseId":database,
                "sql":"INSERT INTO tasks (id,conversation_id,kind,status,abort_requested,background,record) VALUES (2,1,'agent','pending',0,0,'{}')",
                "params":[],
            }))).unwrap();
            let bindings = concat_agent::conversations::ConversationBindings::open(
                &fixture.bridge.owner,
                &fixture.bridge.token,
            )
            .unwrap();
            for (mode, id) in [("chat", 1), ("script", 7), ("edit", 9)] {
                bindings.bind(&current.project_id, mode, id).unwrap();
            }
            bindings
                .bind("bkcat-project:44444444444444444444444444444444", "chat", 4)
                .unwrap();
            drop(bindings);
            fixture
        }

        fn reopen(&self) -> (Arc<AgentBridge>, NativeSession, Value) {
            let bridge = Arc::new(
                AgentBridge::acquire(
                    &AppDirs::under(&self.directory.0),
                    OpenProjects::default(),
                    Exporter::new(),
                )
                .unwrap(),
            );
            let project = Arc::clone(&self.project);
            let next = NativeSession::with_reader(
                Arc::clone(&bridge),
                random_capability().unwrap(),
                Arc::new(move || Ok(project.lock().unwrap().clone())),
            )
            .unwrap();
            let lease = next
                .handle(json!({"method":"ownership.acquire","storageId":STORAGE_ID}))
                .unwrap();
            (bridge, next, lease)
        }
    }

    fn bound(lease: &Value, method: &str, extra: Value) -> Value {
        let mut payload =
            json!({"method":method,"ownerHandle":lease["handle"],"generation":lease["generation"]});
        payload
            .as_object_mut()
            .unwrap()
            .extend(extra.as_object().unwrap().clone());
        payload
    }

    #[test]
    fn recovery_reads_do_not_open_storage_or_grant_a_second_component() {
        let fixture = Fixture::new();
        assert!(
            fixture
                .native
                .handle(json!({"method":"ownership.acquire","storageId":"C:/arbitrary"}))
                .is_err()
        );
        assert!(fixture.native.handle(json!({"method":"ownership.acquire","storageId":STORAGE_ID,"path":"C:/arbitrary"})).is_err());
        let lease = fixture.acquire();
        let snapshot = fixture
            .native
            .handle(bound(&lease, "recovery.read", json!({})))
            .unwrap();
        assert_eq!(snapshot["schemaVersion"], "empty");
        assert_eq!(snapshot["hasInterruptedWork"], false);
        assert_eq!(snapshot["projectSession"], "7");
        assert_eq!(snapshot["blockedReason"], Value::Null);
        assert!(
            !fixture
                .bridge
                .owner
                .storage_root()
                .join(DATABASE_FILE_NAME)
                .exists()
        );
        assert!(
            !fixture
                .bridge
                .owner
                .storage_root()
                .join(OPERATIONS_FILE_NAME)
                .exists()
        );
        assert_eq!(
            fixture.bridge.status().unwrap().phase,
            Phase::RecoveryRequired
        );
        assert!(
            NativeSession::with_reader(
                Arc::clone(&fixture.bridge),
                "second".into(),
                Arc::new(|| Ok(None))
            )
            .is_err()
        );
        assert!(
            fixture
                .native
                .handle(json!({"method":"ownership.acquire","storageId":STORAGE_ID}))
                .is_err()
        );
        fixture.native.shutdown().unwrap();
    }

    #[test]
    fn a_page_cannot_approve_recovery_or_change_native_evidence() {
        let fixture = Fixture::new();
        let lease = fixture.acquire();
        let snapshot = fixture.native.snapshot().unwrap();
        let fabricated = json!({"snapshotId":snapshot["snapshotId"],"decision":"continue","approvalId":"made-up"});
        assert!(
            fixture
                .native
                .handle(bound(
                    &lease,
                    "ownership.confirm",
                    json!({"snapshot":snapshot,"approval":fabricated})
                ))
                .is_err()
        );
        assert!(
            fixture
                .native
                .handle(bound(
                    &lease,
                    "sqlite.open",
                    json!({"expectedSchemaVersion":2,"allowInitializeEmptyStore":true})
                ))
                .is_err()
        );
        let approval = fixture
            .native
            .approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "continue")
            .unwrap();
        let mut forged = snapshot.clone();
        forged["projectRevision"] = json!("999");
        assert!(
            fixture
                .native
                .handle(bound(
                    &lease,
                    "ownership.confirm",
                    json!({"snapshot":forged,"approval":approval})
                ))
                .is_err()
        );
        assert_eq!(
            fixture.bridge.status().unwrap().phase,
            Phase::RecoveryRequired
        );
        assert!(
            !fixture
                .bridge
                .owner
                .storage_root()
                .join(DATABASE_FILE_NAME)
                .exists()
        );
        // The mismatched attempt consumed the one-shot intent.
        assert!(
            fixture
                .native
                .handle(bound(
                    &lease,
                    "ownership.confirm",
                    json!({"snapshot":snapshot,"approval":approval})
                ))
                .is_err()
        );
        fixture.native.shutdown().unwrap();
    }

    #[test]
    fn a_native_approval_rechecks_project_and_file_contents_before_activation() {
        let fixture = Fixture::new();
        let lease = fixture.acquire();
        let snapshot = fixture.native.snapshot().unwrap();
        let approval = fixture
            .native
            .approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "continue")
            .unwrap();
        fixture.project.lock().unwrap().as_mut().unwrap().revision += 1;
        assert!(
            fixture
                .native
                .handle(bound(
                    &lease,
                    "ownership.confirm",
                    json!({"snapshot":snapshot,"approval":approval})
                ))
                .is_err()
        );
        assert_eq!(
            fixture.bridge.status().unwrap().phase,
            Phase::RecoveryRequired
        );

        let snapshot = fixture.native.snapshot().unwrap();
        let approval = fixture
            .native
            .approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "continue")
            .unwrap();
        let path = fixture
            .project
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .path
            .clone();
        // Same native revision, different saved document: counts or path alone
        // cannot validate the previously displayed snapshot.
        fs::write(
            Path::new(&path).join("concat.json"),
            b"{\"project\":\"changed\"}",
        )
        .unwrap();
        assert!(
            fixture
                .native
                .handle(bound(
                    &lease,
                    "ownership.confirm",
                    json!({"snapshot":snapshot,"approval":approval})
                ))
                .is_err()
        );
        assert_eq!(
            fixture.bridge.status().unwrap().phase,
            Phase::RecoveryRequired
        );
        fixture.native.shutdown().unwrap();
    }

    #[test]
    fn no_open_project_is_readable_but_cannot_enable_execution() {
        let fixture = Fixture::new();
        fixture.project.lock().unwrap().take();
        let lease = fixture.acquire();
        let snapshot = fixture.native.snapshot().unwrap();
        assert_eq!(snapshot["projectSession"], "none");
        assert_eq!(snapshot["projectPath"], Value::Null);
        assert!(
            snapshot["blockedReason"]
                .as_str()
                .unwrap()
                .contains("原生工程")
        );
        assert!(
            fixture
                .native
                .approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "continue")
                .is_err()
        );
        assert!(
            fixture
                .native
                .handle(bound(
                    &lease,
                    "sqlite.open",
                    json!({"expectedSchemaVersion":2,"allowInitializeEmptyStore":true})
                ))
                .is_err()
        );
        fixture.native.shutdown().unwrap();
    }

    #[test]
    fn revoked_database_can_rollback_and_close_but_page_cannot_release_os_owner() {
        let fixture = Fixture::new();
        let lease = fixture.acquire();
        let database = fixture.open_database(&lease);
        let tx = fixture
            .native
            .handle(bound(
                &lease,
                "sqlite.begin",
                json!({"databaseId":database}),
            ))
            .unwrap();
        fixture.bridge.stop();
        assert!(fixture.native.handle(bound(&lease, "sqlite.exec", json!({
            "databaseId":database,"transactionId":tx["transactionId"],"sql":"CREATE TABLE late (value TEXT)",
        }))).is_err());
        fixture
            .native
            .handle(bound(
                &lease,
                "sqlite.rollback",
                json!({"databaseId":database,"transactionId":tx["transactionId"]}),
            ))
            .unwrap();
        fixture
            .native
            .handle(bound(
                &lease,
                "sqlite.close",
                json!({"databaseId":database}),
            ))
            .unwrap();
        let result = fixture
            .native
            .handle(bound(&lease, "ownership.drainAndRelease", json!({})))
            .unwrap();
        assert_eq!(result["released"], false);
        assert!(
            AgentBridge::acquire(
                &AppDirs::under(&fixture.directory.0),
                OpenProjects::default(),
                Exporter::new()
            )
            .is_err()
        );
        fixture.native.shutdown().unwrap();
        let next = AgentBridge::acquire(
            &AppDirs::under(&fixture.directory.0),
            OpenProjects::default(),
            Exporter::new(),
        )
        .unwrap();
        assert_eq!(next.status().unwrap().phase, Phase::RecoveryRequired);
        assert!(next.status().unwrap().generation > fixture.bridge.status().unwrap().generation);
        next.finish_handoff(Duration::ZERO).unwrap();
    }

    #[test]
    fn interrupted_tasks_without_native_project_bindings_block_both_decisions() {
        let fixture = Fixture::new();
        let lease = fixture.acquire();
        let database = fixture.open_database(&lease);
        fixture
            .native
            .handle(bound(
                &lease,
                "sqlite.exec",
                json!({
                    "databaseId":database,
                    "sql":include_str!("../../../concat-agent/tests/fixtures/pi-schema-1.5.2.sql"),
                }),
            ))
            .unwrap();
        fixture.native.handle(bound(&lease, "sqlite.run", json!({
            "databaseId":database,
            "sql":"INSERT INTO tasks (id,conversation_id,kind,status,abort_requested,background,record) VALUES (1,1,'agent','pending',0,0,'{}')",
            "params":[],
        }))).unwrap();
        fixture.native.shutdown().unwrap();

        let bridge = Arc::new(
            AgentBridge::acquire(
                &AppDirs::under(&fixture.directory.0),
                OpenProjects::default(),
                Exporter::new(),
            )
            .unwrap(),
        );
        let project = Arc::clone(&fixture.project);
        let next = NativeSession::with_reader(
            Arc::clone(&bridge),
            random_capability().unwrap(),
            Arc::new(move || Ok(project.lock().unwrap().clone())),
        )
        .unwrap();
        next.handle(json!({"method":"ownership.acquire","storageId":STORAGE_ID}))
            .unwrap();
        let snapshot = next.snapshot().unwrap();
        assert_eq!(snapshot["schemaVersion"], 2);
        assert_eq!(snapshot["hasInterruptedWork"], true);
        assert_eq!(snapshot["canDiscardInterrupted"], false);
        assert!(
            next.approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "continue")
                .is_err()
        );
        assert!(
            next.approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "discard")
                .is_err()
        );
        assert_eq!(bridge.status().unwrap().phase, Phase::RecoveryRequired);
        assert!(next.state().unwrap().database.is_none());
        next.shutdown().unwrap();
    }

    #[test]
    fn explicit_discard_is_bound_to_all_project_conversations_and_grants_no_model_task() {
        let fixture = Fixture::with_bound_interrupted_task();
        fixture.native.shutdown().unwrap();
        let (bridge, next, lease) = fixture.reopen();
        let database = bridge.owner.storage_root().join(DATABASE_FILE_NAME);
        let bindings = bridge.owner.storage_root().join(CONVERSATIONS_FILE_NAME);
        let before_database = fs::read(&database).unwrap();
        let before_bindings = fs::read(&bindings).unwrap();
        let snapshot = next.snapshot().unwrap();
        assert_eq!(snapshot["hasInterruptedWork"], true);
        assert_eq!(snapshot["canDiscardInterrupted"], true);
        assert!(
            next.approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "continue")
                .is_err()
        );
        assert!(next.handle(bound(&lease, "ownership.confirm", json!({
            "snapshot":snapshot,"approval":{"snapshotId":snapshot["snapshotId"],"decision":"discard","approvalId":"fabricated"},
        }))).is_err());
        assert_eq!(bridge.status().unwrap().phase, Phase::RecoveryRequired);
        assert!(next.state().unwrap().database.is_none());
        let approval = next
            .approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "discard")
            .unwrap();
        assert!(
            next.approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "discard")
                .is_err()
        );
        let grant = next
            .handle(bound(
                &lease,
                "ownership.confirm",
                json!({"snapshot":snapshot,"approval":approval}),
            ))
            .unwrap();
        assert_eq!(grant["discardConversationIds"], json!([1, 7, 9]));
        assert_eq!(grant["allowInitializeEmptyStore"], false);
        assert!(grant.get("networkPermit").is_none());
        assert!(next.tasks.network("no-native-task").is_err());
        assert!(
            next.tasks
                .authorize_tool(
                    &mut json!({"method":"tools/call","params":{"name":"concat_version"}}),
                    None
                )
                .is_err()
        );
        assert!(next.state().unwrap().database.is_none());
        assert_eq!(fs::read(database).unwrap(), before_database);
        assert_eq!(fs::read(bindings).unwrap(), before_bindings);
        assert!(
            next.handle(bound(
                &lease,
                "ownership.confirm",
                json!({"snapshot":snapshot,"approval":approval})
            ))
            .is_err()
        );
        next.shutdown().unwrap();
    }

    #[test]
    fn discard_rechecks_project_and_mapping_fingerprints_after_the_native_click() {
        for mutation in ["revision", "manifest", "bindings"] {
            let fixture = Fixture::with_bound_interrupted_task();
            fixture.native.shutdown().unwrap();
            let (bridge, next, lease) = fixture.reopen();
            let snapshot = next.snapshot().unwrap();
            let approval = next
                .approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "discard")
                .unwrap();
            match mutation {
                "revision" => fixture.project.lock().unwrap().as_mut().unwrap().revision += 1,
                "manifest" => {
                    let path = fixture
                        .project
                        .lock()
                        .unwrap()
                        .as_ref()
                        .unwrap()
                        .path
                        .clone();
                    fs::write(
                        Path::new(&path).join("concat.json"),
                        b"{\"project\":\"replaced\"}",
                    )
                    .unwrap();
                }
                "bindings" => {
                    use std::io::Write;
                    // Even a change outside parsed row counts invalidates the
                    // exact file fingerprint captured by the displayed review.
                    fs::OpenOptions::new()
                        .append(true)
                        .open(bridge.owner.storage_root().join(CONVERSATIONS_FILE_NAME))
                        .unwrap()
                        .write_all(&[0])
                        .unwrap();
                }
                _ => unreachable!(),
            }
            assert!(
                next.handle(bound(
                    &lease,
                    "ownership.confirm",
                    json!({"snapshot":snapshot,"approval":approval})
                ))
                .is_err(),
                "{mutation}"
            );
            assert_eq!(bridge.status().unwrap().phase, Phase::RecoveryRequired);
            assert!(next.state().unwrap().database.is_none());
            next.shutdown().unwrap();
        }
    }

    #[test]
    fn discard_review_hashes_the_actual_legacy_manifest_with_unchanged_identity() {
        let fixture = Fixture::with_bound_interrupted_task();
        let project = fixture.project.lock().unwrap().as_ref().unwrap().clone();
        let legacy = Path::new(&project.path).join("wolfcut.json");
        fs::rename(Path::new(&project.path).join("concat.json"), &legacy).unwrap();
        fixture.native.shutdown().unwrap();
        let (bridge, next, lease) = fixture.reopen();
        let snapshot = next.snapshot().unwrap();
        assert_eq!(snapshot["canDiscardInterrupted"], true);
        let approval = next
            .approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "discard")
            .unwrap();
        let mut document: Value = serde_json::from_slice(&fs::read(&legacy).unwrap()).unwrap();
        document["project"] = json!("changed after the native review");
        fs::write(&legacy, serde_json::to_vec(&document).unwrap()).unwrap();
        assert_eq!(
            concat_host::project_identity::read_persisted(Path::new(&project.path))
                .unwrap()
                .as_deref(),
            Some(project.project_id.as_str())
        );
        assert!(
            next.handle(bound(
                &lease,
                "ownership.confirm",
                json!({"snapshot":snapshot,"approval":approval})
            ))
            .is_err()
        );
        assert_eq!(bridge.status().unwrap().phase, Phase::RecoveryRequired);
        assert!(next.state().unwrap().database.is_none());
        next.shutdown().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn redirected_current_project_path_cannot_receive_a_discard_scope() {
        let fixture = Fixture::with_bound_interrupted_task();
        fixture.native.shutdown().unwrap();
        let original = fixture
            .project
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .path
            .clone();
        let link = fixture.directory.0.join("linked-project");
        std::os::unix::fs::symlink(&original, &link).unwrap();
        fixture.project.lock().unwrap().as_mut().unwrap().path = link.to_str().unwrap().into();
        let (_, next, _) = fixture.reopen();
        let snapshot = next.snapshot().unwrap();
        assert_eq!(snapshot["canDiscardInterrupted"], false);
        assert!(
            next.approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "discard")
                .is_err()
        );
        next.shutdown().unwrap();
    }

    #[test]
    fn discard_final_verification_is_single_use_and_requires_terminal_unchanged_native_facts() {
        for mutation in [
            "none",
            "unfinished",
            "revision",
            "manifest",
            "bindings",
            "journal",
        ] {
            let fixture = Fixture::with_bound_interrupted_task();
            fixture.native.shutdown().unwrap();
            let (bridge, next, lease) = fixture.reopen();
            let snapshot = next.snapshot().unwrap();
            let approval = next
                .approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "discard")
                .unwrap();
            next.handle(bound(
                &lease,
                "ownership.confirm",
                json!({"snapshot":snapshot,"approval":approval}),
            ))
            .unwrap();
            let project = fixture.project.lock().unwrap().as_ref().unwrap().clone();
            assert!(!next.state().unwrap().execution_ready());
            let intent = crate::agent::tasks::SubmissionIntent {
                id: "must-not-start".into(),
                project: project.clone(),
                mode: crate::agent::tasks::TaskMode::Chat,
                input: "do not call a model".into(),
                selected: None,
                generation: None,
            };
            assert!(
                next.prepare_submission(&intent)
                    .unwrap_err()
                    .contains("恢复核验")
            );
            let database = next
                .handle(bound(
                    &lease,
                    "sqlite.open",
                    json!({
                        "expectedSchemaVersion":PI_SCHEMA_VERSION,"allowInitializeEmptyStore":false,
                    }),
                ))
                .unwrap()["databaseId"]
                .as_str()
                .unwrap()
                .to_owned();
            // Exercise the final native verifier against real on-disk facts.
            // This is a fixture-only state update, not evidence of original Pi
            // cancellation; the runtime integration executes that actual path.
            if mutation != "unfinished" {
                next.handle(bound(&lease, "sqlite.run", json!({
                    "databaseId":database,"sql":"UPDATE tasks SET status='terminal', abort_requested=1 WHERE id=2", "params":[],
                }))).unwrap();
            }
            match mutation {
                "revision" => fixture.project.lock().unwrap().as_mut().unwrap().revision += 1,
                "manifest" => {
                    let path = concat_host::projects::manifest_path(Path::new(&project.path));
                    let mut document: Value =
                        serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
                    document["project"] = json!("edited during discard");
                    fs::write(path, serde_json::to_vec(&document).unwrap()).unwrap();
                }
                "bindings" => {
                    use std::io::Write;
                    fs::OpenOptions::new()
                        .append(true)
                        .open(bridge.owner.storage_root().join(CONVERSATIONS_FILE_NAME))
                        .unwrap()
                        .write_all(&[0])
                        .unwrap();
                }
                "journal" => {
                    let journal = bridge.journal.lock().unwrap().clone().unwrap();
                    let invocation = Invocation {
                        conversation_id: 1,
                        task_id: 2,
                        call_id: "unsettled-during-discard".into(),
                    };
                    let request = json!({"method":"edit.applyGuarded","path":project.path,"expectedSession":7,"expectedRevision":1});
                    journal
                        .prepare(&invocation, &request, &project.path, 7, 1)
                        .unwrap();
                }
                _ => {}
            }
            let result = next.handle(bound(&lease, "recovery.discardVerified", json!({})));
            if mutation == "none" {
                assert_eq!(result.unwrap(), json!({"verified":true}));
                assert!(next.state().unwrap().execution_ready());
            } else {
                assert!(result.is_err(), "{mutation}");
                assert!(!next.state().unwrap().execution_ready());
                assert!(
                    next.prepare_submission(&intent)
                        .unwrap_err()
                        .contains("恢复核验")
                );
            }
            assert!(
                next.handle(bound(&lease, "recovery.discardVerified", json!({})))
                    .is_err()
            );
            assert!(next.tasks.network("no-model-authorization").is_err());
            next.shutdown().unwrap();
        }
    }

    #[test]
    fn a_live_project_copy_cannot_receive_a_discard_scope() {
        let fixture = Fixture::with_bound_interrupted_task();
        fixture.native.shutdown().unwrap();
        let original = fixture
            .project
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .path
            .clone();
        let copy = fixture.directory.0.join("copy");
        fs::create_dir(&copy).unwrap();
        fs::copy(
            Path::new(&original).join("concat.json"),
            copy.join("concat.json"),
        )
        .unwrap();
        fixture.project.lock().unwrap().as_mut().unwrap().path = copy.to_str().unwrap().into();
        let (_, next, _) = fixture.reopen();
        let snapshot = next.snapshot().unwrap();
        assert_eq!(snapshot["canDiscardInterrupted"], false);
        assert!(snapshot["blockedReason"].as_str().unwrap().contains("副本"));
        assert!(
            next.approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "discard")
                .is_err()
        );
        next.shutdown().unwrap();
    }

    #[test]
    fn each_unsettled_native_edit_stage_keeps_discard_closed() {
        for stage in ["intent", "applied", "saved"] {
            let fixture = Fixture::with_bound_interrupted_task();
            let project = fixture.project.lock().unwrap().as_ref().unwrap().clone();
            {
                let journal = fixture.bridge.journal.lock().unwrap().clone().unwrap();
                let invocation = Invocation {
                    conversation_id: 1,
                    task_id: 2,
                    call_id: format!("discard-{stage}"),
                };
                let request = json!({"method":"edit.applyGuarded","path":project.path,"expectedSession":7,"expectedRevision":1});
                let concat_agent::operations::Admission::New(id) = journal
                    .prepare(&invocation, &request, &project.path, 7, 1)
                    .unwrap()
                else {
                    panic!("new intent")
                };
                if stage != "intent" {
                    journal
                        .record_receipt(id, 7, 2, &json!({"sessionId":7,"revision":2}))
                        .unwrap();
                }
                if stage == "saved" {
                    let document: Value = serde_json::from_slice(
                        &fs::read(Path::new(&project.path).join("concat.json")).unwrap(),
                    )
                    .unwrap();
                    assert_eq!(
                        fixture
                            .bridge
                            .record_saved(&project.path, 7, 2, &document)
                            .unwrap(),
                        1
                    );
                }
            }
            let counts = inspect_operations(&fixture.bridge.owner, &fixture.bridge.token).unwrap();
            assert_eq!(
                (
                    counts.needs_project_reconciliation,
                    counts.applied_not_saved,
                    counts.saved_not_notified
                ),
                match stage {
                    "intent" => (1, 0, 0),
                    "applied" => (0, 1, 0),
                    "saved" => (0, 0, 1),
                    _ => unreachable!(),
                },
            );
            fixture.native.shutdown().unwrap();
            let (bridge, next, _) = fixture.reopen();
            let snapshot = next.snapshot().unwrap();
            assert_eq!(snapshot["canDiscardInterrupted"], false, "{stage}");
            assert!(
                snapshot["blockedReason"]
                    .as_str()
                    .unwrap()
                    .contains("编辑或保存记录")
            );
            assert!(
                next.approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "discard")
                    .is_err()
            );
            assert!(
                next.approve_from_ui(snapshot["snapshotId"].as_str().unwrap(), "continue")
                    .is_err()
            );
            assert_eq!(bridge.status().unwrap().phase, Phase::RecoveryRequired);
            next.shutdown().unwrap();
        }
    }

    #[test]
    fn real_protocol_handshake_does_not_grant_edit_authority() {
        let fixture = Fixture::new();
        let lease = fixture.acquire();
        assert!(
            fixture
                .native
                .handle(bound(
                    &lease,
                    "mcp.send",
                    json!({"message":{
                        "jsonrpc":"2.0","id":0,"method":"initialize",
                        "params":{"protocolVersion":"unreviewed","capabilities":{},
                            "clientInfo":{"name":"native-port-test","version":"1"}},
                    }})
                ))
                .is_err()
        );
        let reply = fixture
            .native
            .handle(bound(
                &lease,
                "mcp.send",
                json!({"message":{
                    "jsonrpc":"2.0","id":1,"method":"initialize",
                    "params":{"protocolVersion":"2025-11-25","capabilities":{},
                        "clientInfo":{"name":"native-port-test","version":"1"}},
                }}),
            ))
            .unwrap();
        assert_eq!(reply["result"]["protocolVersion"], "2025-11-25");
        fixture
            .native
            .handle(bound(
                &lease,
                "mcp.send",
                json!({"message":{
                    "jsonrpc":"2.0","method":"notifications/initialized",
                }}),
            ))
            .unwrap();
        assert_eq!(
            fixture.bridge.status().unwrap().phase,
            Phase::RecoveryRequired
        );
        assert!(
            fixture
                .native
                .handle(bound(
                    &lease,
                    "mcp.send",
                    json!({"message":{
                        "jsonrpc":"2.0","id":2,"method":"tools/call",
                        "params":{"name":"edit_apply_guarded","arguments":{}},
                    }})
                ))
                .is_err()
        );
        fixture.approve(&lease);
        assert!(
            fixture
                .native
                .handle(bound(&lease, "recovery.discardVerified", json!({})))
                .is_err()
        );
        // Recovery alone cannot substitute model arguments for original Pi
        // tool identity. This is refused before an edit intent exists.
        assert!(fixture.native.handle(bound(&lease, "mcp.send", json!({"message":{
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"edit_apply_guarded","arguments":{},"_meta":{"callId":"forged"}},
        }}))).is_err());
        assert_eq!(
            inspect_operations(&fixture.bridge.owner, &fixture.bridge.token)
                .unwrap()
                .needs_project_reconciliation,
            0
        );
        fixture.native.shutdown().unwrap();
    }

    #[test]
    fn saved_completion_requires_an_actual_bound_page_acknowledgement() {
        for revoke_before_ack in [false, true] {
            let fixture = Fixture::new();
            let lease = fixture.acquire();
            assert!(
                fixture
                    .native
                    .take_saved_notifications()
                    .unwrap()
                    .is_empty()
            );
            fixture.approve(&lease);
            let id = {
                let journal = fixture.bridge.journal.lock().unwrap().clone().unwrap();
                let path = fixture
                    .project
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .path
                    .clone();
                let invocation = Invocation {
                    conversation_id: 1,
                    task_id: 2,
                    call_id: "save-delivery".into(),
                };
                let request = json!({"method":"edit.applyGuarded","path":path,"expectedSession":7,"expectedRevision":1});
                let concat_agent::operations::Admission::New(id) =
                    journal.prepare(&invocation, &request, &path, 7, 1).unwrap()
                else {
                    panic!("new intent");
                };
                journal
                    .record_receipt(id, 7, 2, &json!({"sessionId":7,"revision":2}))
                    .unwrap();
                let document = json!({"project":"isolated"});
                assert_eq!(
                    fixture.bridge.record_saved(&path, 7, 2, &document).unwrap(),
                    1
                );
                id
            };
            let acknowledge = bound(
                &lease,
                "operation.acknowledge",
                json!({"operationId":id.to_string()}),
            );
            assert!(
                fixture.native.handle(acknowledge.clone()).is_err(),
                "guessing an id before delivery is refused"
            );
            let notifications = fixture.native.take_saved_notifications().unwrap();
            assert_eq!(notifications.len(), 1);
            assert_eq!(notifications[0]["operationId"], id.to_string());
            assert_eq!(notifications[0]["ownerHandle"], lease["handle"]);
            assert_eq!(notifications[0]["generation"], lease["generation"]);
            assert_eq!(
                inspect_operations(&fixture.bridge.owner, &fixture.bridge.token)
                    .unwrap()
                    .notified,
                0
            );
            let mut forged = acknowledge.clone();
            forged["generation"] = json!("0");
            assert!(fixture.native.handle(forged).is_err());
            if revoke_before_ack {
                fixture.bridge.stop();
                assert!(fixture.native.handle(acknowledge).is_err());
                assert!(fixture.native.take_saved_notifications().is_err());
            } else {
                fixture.native.handle(acknowledge.clone()).unwrap();
                assert_eq!(
                    inspect_operations(&fixture.bridge.owner, &fixture.bridge.token)
                        .unwrap()
                        .notified,
                    1
                );
                assert!(fixture.native.handle(acknowledge).is_err());
                assert!(
                    fixture
                        .native
                        .take_saved_notifications()
                        .unwrap()
                        .is_empty()
                );
            }
            fixture.native.shutdown().unwrap();
        }
    }

    #[test]
    fn read_only_fingerprinting_observes_revocation_between_blocks() {
        let fixture = Fixture::new();
        let path = fixture.directory.0.join("large-evidence");
        fs::write(&path, vec![7_u8; 256 * 1024]).unwrap();
        let checks = std::cell::Cell::new(0);
        let result = fingerprint(&path, &mut Sha256::new(), || {
            checks.set(checks.get() + 1);
            if checks.get() == 3 {
                fixture.bridge.stop();
            }
            fixture.native.check_owned()
        });
        assert!(result.is_err());
        assert_eq!(checks.get(), 3);
        assert_eq!(fixture.bridge.status().unwrap().phase, Phase::Draining);
        fixture.native.shutdown().unwrap();
    }
}
