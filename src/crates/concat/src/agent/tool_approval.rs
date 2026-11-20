// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native-only, per-tool consent for the embedded Pi runtime.
//! This is deliberately not an MCP method: model/page JSON cannot select a
//! permission mode, answer a prompt, or mint an approval token.

use std::collections::BTreeSet;
use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use concat_agent::operations::Invocation;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::embedded::random_capability;

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(180);
// Once a human has decided, the approved operation must be dispatched promptly.
// This does not extend either the human deadline or the MCP execution timeout.
const TICKET_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_APPROVAL_IDENTITIES: usize = 65_536;

// A tool worker pushes changes to the native editor instead of waking an
// unconditional 250ms status timer during manual editing or playback.
fn notify_native_ui() {
    let _ = slint::invoke_from_event_loop(crate::agent::window::refresh_tool_permission);
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Mode {
    #[default]
    Ask,
    Assist,
    Full,
}

impl Mode {
    fn index(self) -> i32 {
        match self {
            Self::Ask => 0,
            Self::Assist => 1,
            Self::Full => 2,
        }
    }

    fn from_index(index: i32) -> Result<Self, String> {
        match index {
            0 => Ok(Self::Ask),
            1 => Ok(Self::Assist),
            2 => Ok(Self::Full),
            _ => Err("未知的智能体操作权限设置。".into()),
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct PermissionView {
    pub mode: i32,
    /// Native-issued identity of the approval actually presented by the UI.
    pub pending_id: Option<String>,
    pub pending_summary: Option<String>,
    /// Existing clip identities, resolved only by the native project at render time.
    pub pending_clip_ids: Vec<String>,
}

#[derive(Debug)]
struct Pending {
    request_id: String,
    fingerprint: String,
    invocation: Invocation,
    summary: String,
    affected_clip_ids: Vec<String>,
    deadline: Instant,
    decided: Option<bool>,
}

#[derive(Debug)]
struct Grant {
    ticket: String,
    request_id: String,
    fingerprint: String,
    invocation: Invocation,
    deadline: Instant,
}

#[derive(Default)]
struct State {
    mode: Mode,
    pending: Option<Pending>,
    grant: Option<Grant>,
    // Never evict and re-admit an old Pi invocation in this component. The
    // bounded set fails closed once full; a fresh native generation starts anew.
    seen: BTreeSet<(u64, u64, String)>,
    // Diagnostic only, cleared after native admission of each new user task.
    // It cannot grant permission or cause an edit to be reported as completed.
    last_refusal: Option<(Invocation, String)>,
    // Once an execution permit was consumed, a later refusal cannot prove the
    // entire task wrote nothing. Only the durable editor receipt may settle it.
    execution_permit_consumed: bool,
    stopped: bool,
}

/// A separate approval worker waits here, before any timed MCP call starts.
/// Its short lock never holds the native session or editing engine lock.
#[derive(Default)]
pub(crate) struct NativeToolPermissions {
    state: Mutex<State>,
    wake: Condvar,
}

impl NativeToolPermissions {
    /// Clear task-scoped diagnostics only once the native submission owner has
    /// verified that no earlier user task is still active.
    pub(crate) fn begin_task(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| "工具授权状态不可用。")?;
        if state.stopped {
            return Err("当前会话已停止，不能开始新的剪辑审批。".into());
        }
        expire_grant(&mut state);
        if state.pending.is_some() || state.grant.is_some() {
            return Err("请先完成当前剪辑审批及执行，再开始新任务。".into());
        }
        state.last_refusal = None;
        state.execution_permit_consumed = false;
        Ok(())
    }

    /// A later write attempt has its own outcome, including failures before
    /// authorization. Do not explain those failures using an older refusal.
    pub(crate) fn begin_approval_attempt(&self) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| "工具授权状态不可用。")?;
        if state.stopped {
            return Err("当前会话已停止，不能开始新的剪辑审批。".into());
        }
        state.last_refusal = None;
        Ok(())
    }

    /// Native explanation of an intentional no-write result, never model prose.
    pub(crate) fn last_refusal(&self, conversation: u64) -> Option<String> {
        let state = self.state.lock().ok()?;
        if state.stopped
            || state.execution_permit_consumed
            || state.pending.is_some()
            || state.grant.is_some()
        {
            return None;
        }
        state
            .last_refusal
            .as_ref()
            .filter(|(invocation, _)| invocation.conversation_id == conversation)
            .map(|(_, reason)| reason.clone())
    }

    pub(crate) fn view(&self) -> Result<PermissionView, String> {
        let state = self.state.lock().map_err(|_| "工具授权状态不可用。")?;
        Ok(PermissionView {
            mode: state.mode.index(),
            pending_id: state
                .pending
                .as_ref()
                .map(|request| request.request_id.clone()),
            pending_summary: state
                .pending
                .as_ref()
                .map(|request| request.summary.clone()),
            pending_clip_ids: state
                .pending
                .as_ref()
                .map(|request| request.affected_clip_ids.clone())
                .unwrap_or_default(),
        })
    }

    /// Called exclusively by the Slint native window callback; no web/native
    /// wire method or MCP argument can reach this setter.
    pub(crate) fn set_mode_from_ui(&self, index: i32) -> Result<(), String> {
        let new_mode = Mode::from_index(index)?;
        let mut state = self.state.lock().map_err(|_| "工具授权状态不可用。")?;
        if state.stopped {
            return Err("当前会话已停止，不能修改权限。".into());
        }
        expire_grant(&mut state);
        if state.pending.is_some() || state.grant.is_some() {
            return Err("请先完成当前剪辑审批及执行，再切换权限。".into());
        }
        let previous = state.mode;
        state.mode = new_mode;
        notify_native_ui();
        log::info!(
            "bk-cat-tool-audit event=permission_changed previous={} current={}",
            previous.index(),
            new_mode.index()
        );
        Ok(())
    }

    /// The ID comes from the rendered native prompt, never from model JSON.
    /// A queued click for a previous prompt cannot decide a subsequent request.
    pub(crate) fn decide_from_ui(&self, request_id: &str, approve: bool) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| "工具授权状态不可用。")?;
        if state.stopped {
            return Err("当前会话已停止。".into());
        }
        let pending = state.pending.as_mut().ok_or("没有待批准的剪辑操作。")?;
        if pending.request_id != request_id {
            return Err("该审批已失效，当前剪辑操作未获批准。".into());
        }
        if pending.deadline <= Instant::now() {
            self.wake.notify_all();
            return Err("本次剪辑审批已过期，工程未修改。".into());
        }
        if pending.decided.is_some() {
            return Err("本次剪辑操作已经作出决定。".into());
        }
        pending.decided = Some(approve);
        notify_native_ui();
        log::info!(
            "bk-cat-tool-audit event=user_decision approval_id={} fingerprint={} conversation={} task={} call_id={} granted={}",
            pending.request_id,
            pending.fingerprint,
            pending.invocation.conversation_id,
            pending.invocation.task_id,
            pending.invocation.call_id,
            approve
        );
        self.wake.notify_all();
        Ok(())
    }

    pub(crate) fn cancel(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
            state.last_refusal = None;
            if let Some(pending) = state.pending.take() {
                log::info!(
                    "bk-cat-tool-audit event=approval_cancelled approval_id={} conversation={} task={} call_id={}",
                    pending.request_id,
                    pending.invocation.conversation_id,
                    pending.invocation.task_id,
                    pending.invocation.call_id
                );
            }
            if let Some(grant) = state.grant.take() {
                log::info!(
                    "bk-cat-tool-audit event=approval_revoked approval_id={} conversation={} task={} call_id={}",
                    grant.request_id,
                    grant.invocation.conversation_id,
                    grant.invocation.task_id,
                    grant.invocation.call_id
                );
            }
            self.wake.notify_all();
            notify_native_ui();
        }
    }

    /// Called only after per-task, project and original Pi identity checks.
    /// This never edits the project: it creates a single-use, exact-operation
    /// grant. Native task and project checks still run again at actual dispatch.
    pub(crate) fn authorize(
        &self,
        message: &Value,
        invocation: Option<&Invocation>,
    ) -> Result<Option<String>, String> {
        self.authorize_with_timeout(message, invocation, APPROVAL_TIMEOUT)
    }

    fn authorize_with_timeout(
        &self,
        message: &Value,
        invocation: Option<&Invocation>,
        timeout: Duration,
    ) -> Result<Option<String>, String> {
        if !is_guarded_edit(message) {
            return Ok(None);
        }
        self.begin_approval_attempt()?;
        let invocation = validated_invocation(invocation)?;
        let fingerprint = operation_fingerprint(message, invocation)?;
        let request_id = random_capability()?;
        let mut state = self.state.lock().map_err(|_| "工具授权状态不可用。")?;
        if state.stopped {
            return Err("剪辑任务已停止。".into());
        }
        expire_grant(&mut state);
        if state.pending.is_some() || state.grant.is_some() {
            return Err("已有剪辑操作等待批准或执行。".into());
        }
        let key = (
            invocation.conversation_id,
            invocation.task_id,
            invocation.call_id.clone(),
        );
        if state.seen.len() >= MAX_APPROVAL_IDENTITIES || !state.seen.insert(key) {
            return Err("该工具调用已申请过审批或达到本会话审批上限，拒绝重复执行。".into());
        }
        // Identity, ownership and project checks precede this preview. A
        // preview rejection has issued no ticket and cannot reach the editor;
        // retain that native fact across subsequent read-only model calls.
        // Do not classify admission, replay, revocation or execution failures
        // as a known no-write result.
        let (safe_append_only, description) = match describe_edit(message) {
            Ok(preview) => preview,
            Err(reason) => {
                log::info!(
                    "bk-cat-tool-audit event=approval_preflight_rejected approval_id={} fingerprint={} conversation={} task={} call_id={}",
                    request_id,
                    fingerprint,
                    invocation.conversation_id,
                    invocation.task_id,
                    invocation.call_id
                );
                state.last_refusal = Some((invocation.clone(), reason.clone()));
                return Err(reason);
            }
        };
        let needs_prompt = match state.mode {
            Mode::Ask => true,
            Mode::Assist => !safe_append_only,
            Mode::Full => false,
        };
        if !needs_prompt {
            log::info!(
                "bk-cat-tool-audit event=auto_granted mode={} approval_id={} fingerprint={} conversation={} task={} call_id={}",
                state.mode.index(),
                request_id,
                fingerprint,
                invocation.conversation_id,
                invocation.task_id,
                invocation.call_id
            );
            return mint_grant(&mut state, request_id, fingerprint, invocation);
        }
        let deadline = Instant::now() + timeout;
        let summary = format!("当前工程：{}（编号 {}）", description, &request_id[..12]);
        state.pending = Some(Pending {
            request_id: request_id.clone(),
            fingerprint: fingerprint.clone(),
            invocation: invocation.clone(),
            summary,
            affected_clip_ids: affected_existing_clip_ids(message),
            deadline,
            decided: None,
        });
        notify_native_ui();
        log::info!(
            "bk-cat-tool-audit event=approval_requested mode={} approval_id={} conversation={} task={} call_id={}",
            state.mode.index(),
            request_id,
            invocation.conversation_id,
            invocation.task_id,
            invocation.call_id
        );
        loop {
            if state.stopped {
                state.pending = None;
                notify_native_ui();
                return Err("用户已停止剪辑工具操作。".into());
            }
            if let Some(granted) = state.pending.as_ref().and_then(|pending| pending.decided) {
                state.pending = None;
                notify_native_ui();
                return if granted {
                    mint_grant(&mut state, request_id, fingerprint, invocation)
                } else {
                    let reason = "用户拒绝了本次剪辑操作，工程未修改。".to_owned();
                    state.last_refusal = Some((invocation.clone(), reason.clone()));
                    Err(reason)
                };
            }
            let remaining = state
                .pending
                .as_ref()
                .ok_or("剪辑审批记录已失效。")?
                .deadline
                .saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.pending = None;
                notify_native_ui();
                log::info!(
                    "bk-cat-tool-audit event=approval_timeout approval_id={} conversation={} task={} call_id={}",
                    request_id,
                    invocation.conversation_id,
                    invocation.task_id,
                    invocation.call_id
                );
                let reason = "等待剪辑批准超时，工程未修改。".to_owned();
                state.last_refusal = Some((invocation.clone(), reason.clone()));
                return Err(reason);
            }
            let (next, _) = self
                .wake
                .wait_timeout(state, remaining)
                .map_err(|_| "等待剪辑批准时状态不可用。")?;
            state = next;
        }
    }

    /// The timed MCP dispatch consumes the exact grant once, without waiting.
    /// Mismatched, expired and replayed tickets cannot acquire authorization.
    pub(crate) fn consume(
        &self,
        message: &Value,
        invocation: Option<&Invocation>,
        ticket: &str,
    ) -> Result<(), String> {
        if !is_guarded_edit(message) {
            return Err("审批凭证只能用于受保护剪辑工具。".into());
        }
        let invocation = validated_invocation(invocation)?;
        let fingerprint = operation_fingerprint(message, invocation)?;
        let mut state = self.state.lock().map_err(|_| "工具授权状态不可用。")?;
        if state.stopped {
            return Err("剪辑任务已停止。".into());
        }
        // Burn a presented grant even on a mismatch. Changing parameters and
        // trying again must require a fresh user task, never reuse old consent.
        let grant = state.grant.take().ok_or("剪辑审批凭证不存在或已使用。")?;
        let valid = grant.ticket == ticket
            && grant.fingerprint == fingerprint
            && grant.invocation == *invocation
            && grant.deadline > Instant::now();
        log::info!(
            "bk-cat-tool-audit event=approval_consumed approval_id={} fingerprint={} conversation={} task={} call_id={} granted={}",
            grant.request_id,
            grant.fingerprint,
            grant.invocation.conversation_id,
            grant.invocation.task_id,
            grant.invocation.call_id,
            valid
        );
        if !valid {
            return Err("剪辑审批已失效或与当前任务、工程、命令不符，工程未修改。".into());
        }
        state.execution_permit_consumed = true;
        state.last_refusal = None;
        Ok(())
    }

    pub(crate) fn record_result(
        &self,
        name: &str,
        response: &Value,
        invocation: Option<&Invocation>,
    ) {
        let outcome = if response.pointer("/result/isError") == Some(&Value::Bool(true))
            || response.get("error").is_some()
        {
            "refused"
        } else {
            "completed"
        };
        if let Some(invocation) = invocation {
            log::info!(
                "bk-cat-tool-audit event=tool_result name={name} outcome={outcome} conversation={} task={} call_id={}",
                invocation.conversation_id,
                invocation.task_id,
                invocation.call_id
            );
        } else {
            log::info!("bk-cat-tool-audit event=tool_result name={name} outcome={outcome}");
        }
    }
}

fn is_guarded_edit(message: &Value) -> bool {
    message.get("method").and_then(Value::as_str) == Some("tools/call")
        && message.pointer("/params/name").and_then(Value::as_str) == Some("edit_apply_guarded")
}

fn validated_invocation(invocation: Option<&Invocation>) -> Result<&Invocation, String> {
    let invocation = invocation.ok_or("剪辑工具缺少原版智能体调用身份。")?;
    if invocation.conversation_id > 9_007_199_254_740_991
        || invocation.task_id > 9_007_199_254_740_991
        || invocation.call_id.is_empty()
        || invocation.call_id.len() > 512
        || invocation.call_id.chars().any(char::is_control)
    {
        return Err("剪辑工具原版调用身份无效。".into());
    }
    Ok(invocation)
}

fn canonical_json(value: &Value) -> Value {
    match value {
        Value::Object(object) => {
            let mut keys: Vec<_> = object.keys().collect();
            keys.sort();
            Value::Object(
                keys.into_iter()
                    .map(|key| (key.clone(), canonical_json(&object[key])))
                    .collect(),
            )
        }
        Value::Array(items) => Value::Array(items.iter().map(canonical_json).collect()),
        value => value.clone(),
    }
}

fn operation_fingerprint(message: &Value, invocation: &Invocation) -> Result<String, String> {
    let arguments = message
        .pointer("/params/arguments")
        .filter(|value| value.is_object())
        .ok_or("剪辑工具参数无法核对。")?;
    // JSON-RPC request IDs are allocated only after approval. The exact native
    // path/session/revision and command arguments remain part of this digest.
    let bound = json!({"name":"edit_apply_guarded", "arguments":canonical_json(arguments),
        "conversationId":invocation.conversation_id,"taskId":invocation.task_id,"callId":invocation.call_id});
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&bound).map_err(|_| "剪辑工具参数无法核对。")?)
    ))
}

fn expire_grant(state: &mut State) {
    if state
        .grant
        .as_ref()
        .is_some_and(|grant| grant.deadline <= Instant::now())
        && let Some(grant) = state.grant.take()
    {
        log::info!(
            "bk-cat-tool-audit event=approval_ticket_expired approval_id={} conversation={} task={} call_id={}",
            grant.request_id,
            grant.invocation.conversation_id,
            grant.invocation.task_id,
            grant.invocation.call_id
        );
    }
}

fn mint_grant(
    state: &mut State,
    request_id: String,
    fingerprint: String,
    invocation: &Invocation,
) -> Result<Option<String>, String> {
    let ticket = random_capability()?;
    log::info!(
        "bk-cat-tool-audit event=approval_granted approval_id={} fingerprint={} conversation={} task={} call_id={}",
        request_id,
        fingerprint,
        invocation.conversation_id,
        invocation.task_id,
        invocation.call_id
    );
    state.grant = Some(Grant {
        ticket: ticket.clone(),
        request_id,
        fingerprint,
        invocation: invocation.clone(),
        deadline: Instant::now() + TICKET_TIMEOUT,
    });
    Ok(Some(ticket))
}

/// Permission prompts must show exact native command targets, not a generic
/// count or unchecked prose from a model. Reject undisplayable identities.
fn approval_id(value: &Value, field: &str) -> Result<String, String> {
    let id = value
        .get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| format!("剪辑审批缺少 {field}，拒绝执行。"))?;
    if id.is_empty() || id.chars().count() > 96 || id.chars().any(char::is_control) {
        return Err(format!("剪辑审批的 {field} 无法安全显示，拒绝执行。"));
    }
    Ok(id.to_owned())
}

fn approval_time(value: &Value, field: &str) -> Result<f64, String> {
    let seconds = value
        .get(field)
        .and_then(Value::as_f64)
        .ok_or_else(|| format!("剪辑审批缺少 {field} 时间，拒绝执行。"))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(format!("剪辑审批的 {field} 时间无效，拒绝执行。"));
    }
    Ok(seconds)
}

fn approval_text(value: &Value) -> Result<String, String> {
    let Some(body) = value.pointer("/style/content").and_then(Value::as_str) else {
        return Ok("使用默认文字".to_owned());
    };
    if body.chars().count() > 512 {
        return Err("待添加的文字超过512字，无法在批准前完整预览；请拆分片段。".into());
    }
    if body
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err("待添加的文字包含不能安全显示的控制字符，拒绝执行。".into());
    }
    let displayed = body
        .replace("\r\n", "↵")
        .replace(['\n', '\r'], "↵")
        .replace('\t', " ");
    Ok(format!("文字「{displayed}」"))
}

fn describe_one_edit(item: &Value) -> Result<Vec<String>, String> {
    let op = item
        .get("op")
        .and_then(Value::as_str)
        .ok_or("剪辑命令没有有效操作类型。")?;
    match op {
        "addMediaClipExact" => {
            let media = approval_id(item, "mediaId")?;
            let track = approval_id(item, "trackId")?;
            let start = approval_time(item, "start")?;
            let source_start = approval_time(item, "sourceStart")?;
            let source_end = approval_time(item, "sourceEnd")?;
            if source_end <= source_start {
                return Err("素材来源时间范围无效，拒绝执行。".into());
            }
            Ok(vec![format!(
                "添加素材 {media} → 轨道 {track}，时间 {start:.3}～{end:.3} 秒；素材取 {source_start:.3}～{source_end:.3} 秒",
                end = start + source_end - source_start
            )])
        }
        "addTextClip" => {
            let track = approval_id(item, "trackId")?;
            let start = approval_time(item, "start")?;
            let duration = approval_time(item, "duration")?;
            if duration <= 0.0 {
                return Err("文字片段时长无效，拒绝执行。".into());
            }
            Ok(vec![format!(
                "添加文字 → 轨道 {track}，时间 {start:.3}～{end:.3} 秒；{}",
                approval_text(item)?,
                end = start + duration
            )])
        }
        "moveClips" => {
            let moves = item
                .get("moves")
                .and_then(Value::as_array)
                .ok_or("缺少片段移动列表。")?;
            if moves.is_empty() {
                return Err("片段移动列表为空，拒绝执行。".into());
            }
            moves
                .iter()
                .map(|wanted| {
                    let clip = approval_id(wanted, "clipId")?;
                    let track = approval_id(wanted, "trackId")?;
                    let start = approval_time(wanted, "start")?;
                    Ok(format!(
                        "移动片段 {clip} → 轨道 {track}，新起点 {start:.3} 秒"
                    ))
                })
                .collect()
        }
        "removeClips" => {
            if item.get("ripple").and_then(Value::as_bool).unwrap_or(false) {
                return Err("自动剪辑不允许波纹删除，拒绝执行。".into());
            }
            let clips = item
                .get("clipIds")
                .and_then(Value::as_array)
                .ok_or("缺少删除片段列表。")?;
            if clips.is_empty() {
                return Err("删除片段列表为空，拒绝执行。".into());
            }
            clips
                .iter()
                .map(|clip| {
                    let id = clip.as_str().ok_or("删除片段编号无效。")?;
                    if id.is_empty() || id.chars().count() > 96 || id.chars().any(char::is_control)
                    {
                        return Err("删除片段编号无法安全显示，拒绝执行。".into());
                    }
                    Ok(format!(
                        "删除片段 {id}（位置按当前工程核验；不移动其他片段）"
                    ))
                })
                .collect()
        }
        "updateClip" => {
            let clip = approval_id(item, "clipId")?;
            let patch = item
                .get("patch")
                .and_then(Value::as_object)
                .ok_or("缺少片段属性修改内容。")?;
            if patch.is_empty() {
                return Err("片段属性修改为空，拒绝执行。".into());
            }
            let details =
                serde_json::to_string(patch).map_err(|_| "片段属性无法显示，拒绝执行。")?;
            if details.chars().count() > 2048 {
                return Err("片段属性变更过长，无法在批准前完整预览；请拆分操作。".into());
            }
            Ok(vec![format!(
                "修改片段 {clip}（轨道与起点不变），具体属性：{details}"
            )])
        }
        "applySingleMainTemplate" => {
            let plan = item.get("plan").ok_or("缺少主轨模板计划。")?;
            let base = approval_time(plan, "start")?;
            let mut rows = vec![format!(
                "在空时间线应用主轨模板，起点 {base:.3} 秒；按非空角色新建主视频、字幕、贴片、音乐、配音轨道"
            )];
            for (field, role, is_text) in [
                ("videos", "主视频", false),
                ("subtitles", "字幕", true),
                ("overlays", "贴片文字", true),
                ("music", "背景音乐", false),
                ("voice", "配音", false),
            ] {
                let entries = plan
                    .get(field)
                    .and_then(Value::as_array)
                    .ok_or_else(|| format!("模板缺少 {field} 列表。"))?;
                for entry in entries {
                    let start = base + approval_time(entry, "start")?;
                    if is_text {
                        let duration = approval_time(entry, "duration")?;
                        if duration <= 0.0 || !(start + duration).is_finite() {
                            return Err("模板文字片段时间范围无效，拒绝执行。".into());
                        }
                        rows.push(format!(
                            "{role} → 新轨道，时间 {start:.3}～{end:.3} 秒；{}",
                            approval_text(entry)?,
                            end = start + duration
                        ));
                    } else {
                        let media = approval_id(entry, "mediaId")?;
                        let from = approval_time(entry, "sourceStart")?;
                        let to = approval_time(entry, "sourceEnd")?;
                        if to <= from || !(start + to - from).is_finite() {
                            return Err("模板素材时间范围无效，拒绝执行。".into());
                        }
                        rows.push(format!(
                            "{role}素材 {media} → 新轨道，时间 {start:.3}～{end:.3} 秒；取素材 {from:.3}～{to:.3} 秒",
                            end = start + to - from
                        ));
                    }
                }
            }
            Ok(rows)
        }
        _ => Err("操作不在可安全预览的原生剪辑范围内，已拒绝授权。".into()),
    }
}

/// Only exact append operations are auto-admitted by the assisted setting.
/// Changes to existing clips, unknown operations, templates and mixed batches
/// always ask. Neither class bypasses the native edit schema/ownership checks.
fn describe_edit(message: &Value) -> Result<(bool, String), String> {
    let command = message
        .pointer("/params/arguments/command")
        .and_then(Value::as_object)
        .ok_or("缺少结构化剪辑命令，未申请执行权限。")?;
    let op = command
        .get("op")
        .and_then(Value::as_str)
        .ok_or("剪辑命令没有有效操作类型。")?;
    let commands: Vec<&Value> = if op == "batch" {
        let items = command
            .get("commands")
            .and_then(Value::as_array)
            .ok_or("批量剪辑命令无效。")?;
        if items.is_empty() || items.len() > 64 {
            return Err("批量剪辑命令数量必须在1到64项之间。".into());
        }
        items.iter().collect()
    } else {
        vec![&message["params"]["arguments"]["command"]]
    };
    let mut safe = true;
    let mut preview = Vec::new();
    for item in &commands {
        let kind = item
            .get("op")
            .and_then(Value::as_str)
            .ok_or("剪辑批次中有缺少操作类型的命令。")?;
        if kind == "batch" {
            return Err("不允许嵌套批量剪辑命令。".into());
        }
        if !matches!(kind, "addMediaClipExact" | "addTextClip") {
            safe = false;
        }
        preview.extend(describe_one_edit(item)?);
        // An approval is meaningful only when the whole intended edit can be
        // inspected. Never silently omit operations from a destructive plan.
        if preview.len() > 128 {
            return Err("待批准的片段太多，无法完整预览；请拆分剪辑批次。".into());
        }
    }
    Ok((
        safe,
        format!(
            "本次共 {} 项原生剪辑操作，完整目标如下：\n{}\n执行前仍将核对当前工程、版本和轨道锁。",
            commands.len(),
            preview
                .iter()
                .enumerate()
                .map(|(n, line)| format!("{}. {line}", n + 1))
                .collect::<Vec<_>>()
                .join("\n")
        ),
    ))
}

// Extract existing identities from the validated command so the native window can
// display their *current* track/time, not only the model-supplied destination.
fn affected_existing_clip_ids(message: &Value) -> Vec<String> {
    let Some(command) = message.pointer("/params/arguments/command") else {
        return Vec::new();
    };
    let commands: Vec<&Value> = if command.get("op").and_then(Value::as_str) == Some("batch") {
        command
            .get("commands")
            .and_then(Value::as_array)
            .map(|items| items.iter().collect())
            .unwrap_or_default()
    } else {
        vec![command]
    };
    let mut ids = Vec::new();
    for item in commands {
        match item.get("op").and_then(Value::as_str) {
            Some("removeClips") => {
                if let Some(clips) = item.get("clipIds").and_then(Value::as_array) {
                    ids.extend(clips.iter().filter_map(Value::as_str).map(str::to_owned));
                }
            }
            Some("moveClips") => {
                if let Some(moves) = item.get("moves").and_then(Value::as_array) {
                    ids.extend(
                        moves
                            .iter()
                            .filter_map(|mv| mv.get("clipId"))
                            .filter_map(Value::as_str)
                            .map(str::to_owned),
                    );
                }
            }
            Some("updateClip") => {
                if let Some(id) = item.get("clipId").and_then(Value::as_str) {
                    ids.push(id.to_owned());
                }
            }
            _ => {}
        }
    }
    ids.sort();
    ids.dedup();
    ids
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::thread;

    fn frame(op: &str) -> Value {
        serde_json::json!({
            "method": "tools/call",
            "params": {
                "name": "edit_apply_guarded",
                "arguments": {"path":"selected","expectedSession":7,"expectedRevision":1,
                    "command":{"op":op,"trackId":"t1","start":0.0,"duration":1.0,
                        "mediaId":"m1","kind":"video","sourceStart":0.0,"sourceEnd":1.0,
                        "clipIds":["c1"],"clipId":"c1","patch":{"volume":0.5}}}
            }
        })
    }

    fn original() -> Invocation {
        Invocation {
            conversation_id: 9,
            task_id: 4,
            call_id: "original-pi-call".into(),
        }
    }

    fn invocation(call: &str) -> Invocation {
        Invocation {
            call_id: call.into(),
            ..original()
        }
    }

    fn wait_pending(gate: &NativeToolPermissions) -> String {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if let Some(id) = gate.view().unwrap().pending_id {
                return id;
            }
            assert!(Instant::now() < deadline, "approval did not become visible");
            thread::yield_now();
        }
    }

    fn expire_pending(gate: &NativeToolPermissions) {
        gate.state
            .lock()
            .unwrap()
            .pending
            .as_mut()
            .unwrap()
            .deadline = Instant::now();
        gate.wake.notify_all();
    }

    #[test]
    fn default_asks_per_operation_and_denial_blocks_execution() {
        let gate = Arc::new(NativeToolPermissions::default());
        let worker = Arc::clone(&gate);
        let handle =
            thread::spawn(move || worker.authorize(&frame("addTextClip"), Some(&original())));
        let approval = wait_pending(&gate);
        assert_eq!(gate.view().unwrap().mode, 0);
        assert!(gate.set_mode_from_ui(2).is_err());
        assert!(
            gate.consume(&frame("addTextClip"), Some(&original()), "forged")
                .is_err()
        );
        gate.decide_from_ui(&approval, false).unwrap();
        assert!(handle.join().unwrap().unwrap_err().contains("拒绝"));
        assert!(gate.view().unwrap().pending_summary.is_none());
        assert!(
            gate.consume(&frame("addTextClip"), Some(&original()), "forged")
                .is_err()
        );
        assert!(
            gate.authorize(&frame("addTextClip"), Some(&original()))
                .is_err()
        );
    }

    #[test]
    fn approval_grants_exactly_one_dispatch_without_rpc_id_binding() {
        let gate = Arc::new(NativeToolPermissions::default());
        let worker = Arc::clone(&gate);
        let handle =
            thread::spawn(move || worker.authorize(&frame("addTextClip"), Some(&original())));
        let approval = wait_pending(&gate);
        gate.decide_from_ui(&approval, true).unwrap();
        assert!(gate.decide_from_ui(&approval, true).is_err());
        let ticket = handle.join().unwrap().unwrap().unwrap();
        assert!(gate.view().unwrap().pending_id.is_none());
        assert!(
            gate.begin_task().is_err(),
            "an unconsumed approval still belongs to the current task"
        );
        assert!(
            gate.authorize(&frame("addTextClip"), Some(&original()))
                .is_err()
        );
        let mut dispatched = frame("addTextClip");
        dispatched["jsonrpc"] = json!("2.0");
        dispatched["id"] = json!(42);
        gate.consume(&dispatched, Some(&original()), &ticket)
            .unwrap();
        assert!(
            gate.consume(&dispatched, Some(&original()), &ticket)
                .is_err()
        );
        assert!(
            gate.authorize(&frame("addTextClip"), Some(&original()))
                .is_err()
        );
    }

    #[test]
    fn assisted_only_auto_grants_known_append_only_edits() {
        let gate = Arc::new(NativeToolPermissions::default());
        gate.set_mode_from_ui(1).unwrap();
        let ticket = gate
            .authorize(&frame("addMediaClipExact"), Some(&original()))
            .unwrap()
            .unwrap();
        assert!(gate.view().unwrap().pending_summary.is_none());
        gate.consume(&frame("addMediaClipExact"), Some(&original()), &ticket)
            .unwrap();
        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || {
            worker.authorize(&frame("removeClips"), Some(&invocation("remove")))
        });
        let approval = wait_pending(&gate);
        assert_eq!(gate.view().unwrap().pending_clip_ids, vec!["c1".to_owned()]);
        gate.decide_from_ui(&approval, true).unwrap();
        let ticket = handle.join().unwrap().unwrap().unwrap();
        gate.consume(&frame("removeClips"), Some(&invocation("remove")), &ticket)
            .unwrap();
    }

    #[test]
    fn full_access_requires_native_setting_and_cancel_revokes_pending() {
        let gate = Arc::new(NativeToolPermissions::default());
        assert!(gate.set_mode_from_ui(42).is_err());
        gate.set_mode_from_ui(2).unwrap();
        let ticket = gate
            .authorize(&frame("removeClips"), Some(&original()))
            .unwrap()
            .unwrap();
        gate.consume(&frame("removeClips"), Some(&original()), &ticket)
            .unwrap();
        gate.set_mode_from_ui(0).unwrap();
        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || {
            worker.authorize(&frame("removeClips"), Some(&invocation("second")))
        });
        let approval = wait_pending(&gate);
        gate.cancel();
        assert!(handle.join().unwrap().unwrap_err().contains("停止"));
        assert!(gate.decide_from_ui(&approval, true).is_err());
        assert!(gate.set_mode_from_ui(2).is_err());
        assert!(gate.view().unwrap().pending_id.is_none());
    }

    #[test]
    fn timeout_refuses_operation_and_late_ui_decision_cannot_approve_next_call() {
        let gate = Arc::new(NativeToolPermissions::default());
        let worker = Arc::clone(&gate);
        let handle =
            thread::spawn(move || worker.authorize(&frame("addTextClip"), Some(&original())));
        let expired = wait_pending(&gate);
        expire_pending(&gate);
        assert!(handle.join().unwrap().unwrap_err().contains("超时"));
        assert!(gate.view().unwrap().pending_id.is_none());
        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || {
            worker.authorize(&frame("addTextClip"), Some(&invocation("new-call")))
        });
        let current = wait_pending(&gate);
        assert_ne!(expired, current);
        assert!(gate.decide_from_ui(&expired, true).is_err());
        assert_eq!(gate.view().unwrap().pending_id, Some(current.clone()));
        assert!(!handle.is_finished());
        gate.decide_from_ui(&current, true).unwrap();
        let ticket = handle.join().unwrap().unwrap().unwrap();
        gate.consume(
            &frame("addTextClip"),
            Some(&invocation("new-call")),
            &ticket,
        )
        .unwrap();
    }

    #[test]
    fn elapsed_approval_is_not_accepted_even_before_worker_processes_timeout() {
        let gate = Arc::new(NativeToolPermissions::default());
        let worker = Arc::clone(&gate);
        let handle =
            thread::spawn(move || worker.authorize(&frame("addTextClip"), Some(&original())));
        let approval = wait_pending(&gate);
        // Leave the worker asleep: the native click must independently reject
        // an elapsed deadline instead of relying on timeout cleanup happening first.
        gate.state
            .lock()
            .unwrap()
            .pending
            .as_mut()
            .unwrap()
            .deadline = Instant::now();
        assert!(gate.decide_from_ui(&approval, true).is_err());
        assert!(handle.join().unwrap().unwrap_err().contains("超时"));
        assert!(gate.view().unwrap().pending_id.is_none());
        assert!(
            gate.consume(&frame("addTextClip"), Some(&original()), "forged")
                .is_err()
        );
    }

    #[test]
    fn ticket_is_bound_to_command_project_revision_and_complete_invocation() {
        for changed in 0..7 {
            let gate = NativeToolPermissions::default();
            gate.set_mode_from_ui(1).unwrap();
            let ticket = gate
                .authorize(&frame("addTextClip"), Some(&original()))
                .unwrap()
                .unwrap();
            let mut request = frame("addTextClip");
            let mut identity = original();
            match changed {
                0 => request["params"]["arguments"]["command"]["duration"] = json!(5.0),
                1 => request["params"]["arguments"]["path"] = json!("another-project"),
                2 => request["params"]["arguments"]["expectedSession"] = json!(8),
                3 => request["params"]["arguments"]["expectedRevision"] = json!(2),
                4 => identity.conversation_id += 1,
                5 => identity.task_id += 1,
                6 => identity.call_id = "different-call".into(),
                _ => unreachable!(),
            }
            assert!(
                gate.consume(&request, Some(&identity), &ticket).is_err(),
                "mutation {changed} used old consent"
            );
            assert!(
                gate.consume(&frame("addTextClip"), Some(&original()), &ticket)
                    .is_err(),
                "mismatched ticket must be burned"
            );
        }
    }

    #[test]
    fn cancelled_or_expired_ticket_cannot_be_consumed_or_reauthorized() {
        for cancel in [false, true] {
            let gate = NativeToolPermissions::default();
            gate.set_mode_from_ui(1).unwrap();
            let ticket = gate
                .authorize(&frame("addTextClip"), Some(&original()))
                .unwrap()
                .unwrap();
            if cancel {
                gate.cancel();
            } else {
                gate.state.lock().unwrap().grant.as_mut().unwrap().deadline = Instant::now();
            }
            assert!(
                gate.consume(&frame("addTextClip"), Some(&original()), &ticket)
                    .is_err()
            );
            assert!(
                gate.authorize(&frame("addTextClip"), Some(&original()))
                    .is_err()
            );
        }
    }

    #[test]
    fn same_command_in_new_call_requires_new_approval_and_read_tools_mint_no_ticket() {
        let gate = Arc::new(NativeToolPermissions::default());
        let read = json!({"method":"tools/call","params":{"name":"timeline_read","arguments":{}}});
        assert_eq!(gate.authorize(&read, None).unwrap(), None);
        assert!(gate.consume(&read, None, "forged").is_err());
        gate.set_mode_from_ui(1).unwrap();
        let ticket = gate
            .authorize(&frame("addTextClip"), Some(&original()))
            .unwrap()
            .unwrap();
        gate.consume(&frame("addTextClip"), Some(&original()), &ticket)
            .unwrap();
        gate.set_mode_from_ui(0).unwrap();
        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || {
            worker.authorize(&frame("addTextClip"), Some(&invocation("next")))
        });
        let approval = wait_pending(&gate);
        assert!(!handle.is_finished());
        gate.decide_from_ui(&approval, false).unwrap();
        assert!(handle.join().unwrap().unwrap_err().contains("拒绝"));
    }

    #[test]
    fn native_refusal_diagnostics_are_scoped_and_reset_between_tasks() {
        let gate = Arc::new(NativeToolPermissions::default());
        gate.begin_task().unwrap();
        assert!(gate.last_refusal(original().conversation_id).is_none());
        let worker = Arc::clone(&gate);
        let handle =
            thread::spawn(move || worker.authorize(&frame("addTextClip"), Some(&original())));
        let approval = wait_pending(&gate);
        assert!(gate.begin_task().is_err());
        gate.decide_from_ui(&approval, false).unwrap();
        let reason = handle.join().unwrap().unwrap_err();
        assert_eq!(gate.last_refusal(original().conversation_id), Some(reason));
        assert!(gate.last_refusal(original().conversation_id + 1).is_none());
        gate.begin_task().unwrap();
        assert!(gate.last_refusal(original().conversation_id).is_none());
        assert!(
            gate.authorize(&frame("addTextClip"), Some(&original()))
                .is_err()
        );
        assert!(
            gate.last_refusal(original().conversation_id).is_none(),
            "duplicate requests are not user refusals"
        );
        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || {
            worker.authorize(&frame("addTextClip"), Some(&invocation("expires")))
        });
        wait_pending(&gate);
        expire_pending(&gate);
        let reason = handle.join().unwrap().unwrap_err();
        assert_eq!(gate.last_refusal(original().conversation_id), Some(reason));
        gate.begin_task().unwrap();
        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || {
            worker.authorize(&frame("addTextClip"), Some(&invocation("cancelled")))
        });
        wait_pending(&gate);
        gate.cancel();
        assert!(handle.join().unwrap().is_err());
        assert!(gate.last_refusal(original().conversation_id).is_none());
        assert!(gate.begin_task().is_err());
    }

    #[test]
    fn a_later_failed_write_attempt_cannot_reuse_an_earlier_user_refusal() {
        let gate = Arc::new(NativeToolPermissions::default());
        let worker = Arc::clone(&gate);
        let handle =
            thread::spawn(move || worker.authorize(&frame("addTextClip"), Some(&original())));
        let approval = wait_pending(&gate);
        gate.decide_from_ui(&approval, false).unwrap();
        let reason = handle.join().unwrap().unwrap_err();
        assert_eq!(
            gate.last_refusal(original().conversation_id),
            Some(reason.clone())
        );

        let read = json!({"method":"tools/call","params":{"name":"timeline_read","arguments":{}}});
        assert_eq!(gate.authorize(&read, None).unwrap(), None);
        assert_eq!(gate.last_refusal(original().conversation_id), Some(reason));
        // A failure before creating a new prompt is a new write outcome too.
        assert!(gate.authorize(&frame("addTextClip"), None).is_err());
        assert!(gate.last_refusal(original().conversation_id).is_none());

        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || {
            worker.authorize(&frame("addTextClip"), Some(&invocation("denied-again")))
        });
        let approval = wait_pending(&gate);
        gate.decide_from_ui(&approval, false).unwrap();
        assert!(handle.join().unwrap().is_err());
        assert!(gate.last_refusal(original().conversation_id).is_some());

        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || {
            worker.authorize(&frame("addTextClip"), Some(&invocation("approved-later")))
        });
        let approval = wait_pending(&gate);
        assert!(gate.last_refusal(original().conversation_id).is_none());
        gate.decide_from_ui(&approval, true).unwrap();
        let ticket = handle.join().unwrap().unwrap().unwrap();
        let mut changed = frame("addTextClip");
        changed["params"]["arguments"]["expectedRevision"] = json!(2);
        assert!(
            gate.consume(&changed, Some(&invocation("approved-later")), &ticket)
                .is_err()
        );
        assert!(gate.last_refusal(original().conversation_id).is_none());
    }

    #[test]
    fn preflight_rejections_are_zero_write_results_and_new_calls_still_require_approval() {
        for invalid in [frame("unsupported-style-operation"), {
            let mut invalid = frame("addTextClip");
            invalid["params"]["arguments"]["command"]["duration"] = json!(0);
            invalid
        }] {
            let gate = Arc::new(NativeToolPermissions::default());
            gate.begin_task().unwrap();
            let reason = gate.authorize(&invalid, Some(&original())).unwrap_err();
            assert_eq!(gate.last_refusal(9), Some(reason.clone()));
            assert!(gate.last_refusal(10).is_none());
            assert!(gate.view().unwrap().pending_id.is_none());
            assert!(gate.state.lock().unwrap().grant.is_none());
            assert!(!gate.state.lock().unwrap().execution_permit_consumed);
            assert!(
                gate.consume(&invalid, Some(&original()), "not-a-native-ticket")
                    .is_err()
            );

            let read =
                json!({"method":"tools/call","params":{"name":"timeline_read","arguments":{}}});
            assert_eq!(gate.authorize(&read, Some(&original())).unwrap(), None);
            assert_eq!(gate.last_refusal(9), Some(reason));

            // A repeated old identity is still rejected and cannot carry its
            // old diagnostic into a later, differently failed attempt.
            assert!(
                gate.authorize(&invalid, Some(&original()))
                    .unwrap_err()
                    .contains("重复")
            );
            assert!(gate.last_refusal(9).is_none());
            gate.begin_task().unwrap();
            let worker = Arc::clone(&gate);
            let handle = thread::spawn(move || {
                worker.authorize(&frame("addTextClip"), Some(&invocation("new-legal-call")))
            });
            let request = wait_pending(&gate);
            assert!(!handle.is_finished());
            gate.decide_from_ui(&request, true).unwrap();
            let ticket = handle.join().unwrap().unwrap().unwrap();
            gate.consume(
                &frame("addTextClip"),
                Some(&invocation("new-legal-call")),
                &ticket,
            )
            .unwrap();
            assert!(gate.last_refusal(9).is_none());
        }
    }

    #[test]
    fn a_consumed_execution_permit_blocks_later_zero_write_claims_until_a_new_task() {
        let gate = Arc::new(NativeToolPermissions::default());
        gate.begin_task().unwrap();
        gate.set_mode_from_ui(1).unwrap();
        let ticket = gate
            .authorize(&frame("addTextClip"), Some(&original()))
            .unwrap()
            .unwrap();
        gate.consume(&frame("addTextClip"), Some(&original()), &ticket)
            .unwrap();
        // consume also disposes a ticket if post-approval ownership checking
        // fails. Treat that conservatively: no later refusal unlocks this task.
        assert!(gate.state.lock().unwrap().execution_permit_consumed);
        assert!(
            gate.authorize(&frame("unsupported"), Some(&invocation("later-preview")))
                .is_err()
        );
        assert!(gate.last_refusal(9).is_none());

        gate.set_mode_from_ui(0).unwrap();
        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || {
            worker.authorize(
                &frame("addTextClip"),
                Some(&invocation("later-human-refusal")),
            )
        });
        let request = wait_pending(&gate);
        gate.decide_from_ui(&request, false).unwrap();
        assert!(handle.join().unwrap().is_err());
        assert!(gate.last_refusal(9).is_none());

        gate.begin_task().unwrap();
        assert!(
            gate.authorize(&frame("unsupported"), Some(&invocation("new-task-preview")))
                .is_err()
        );
        assert!(gate.last_refusal(9).is_some());
    }

    #[test]
    fn a_preflight_rejection_never_overrides_identity_failure_or_cancellation() {
        let gate = NativeToolPermissions::default();
        assert!(
            gate.authorize(&frame("unsupported"), Some(&original()))
                .is_err()
        );
        assert!(gate.last_refusal(9).is_some());
        assert!(gate.authorize(&frame("addTextClip"), None).is_err());
        assert!(gate.last_refusal(9).is_none());
        assert!(
            gate.authorize(&frame("unsupported"), Some(&invocation("another-preview")))
                .is_err()
        );
        assert!(gate.last_refusal(9).is_some());
        gate.cancel();
        assert!(gate.last_refusal(9).is_none());
        assert!(gate.begin_task().is_err());
        assert!(
            gate.authorize(&frame("addTextClip"), Some(&invocation("after-stop")))
                .is_err()
        );
    }

    #[test]
    fn approval_preview_shows_exact_targets_and_time_ranges() {
        let mut request = frame("addMediaClipExact");
        request["params"]["arguments"]["command"] = serde_json::json!({
            "op":"batch","commands":[
                {"op":"addMediaClipExact","mediaId":"m7","trackId":"video-1",
                 "kind":"video","start":2.25,"sourceStart":1.0,"sourceEnd":4.5},
                {"op":"addTextClip","trackId":"text-1","start":2.25,
                 "duration":1.5,"style":{"content":"重要字幕"}},
                {"op":"moveClips","moves":[{"clipId":"c9","trackId":"video-2","start":8.0}]},
                {"op":"removeClips","clipIds":["c10"],"ripple":false}
            ]
        });
        let (safe, summary) = describe_edit(&request).unwrap();
        assert!(!safe);
        for target in [
            "m7",
            "video-1",
            "2.250～5.750",
            "text-1",
            "重要字幕",
            "c9",
            "video-2",
            "8.000",
            "c10",
        ] {
            assert!(summary.contains(target), "审批预览缺少 {target}: {summary}");
        }
        assert_eq!(
            affected_existing_clip_ids(&request),
            vec!["c10".to_owned(), "c9".to_owned()]
        );
    }

    #[test]
    fn approval_refuses_undisplayable_targets_and_unknown_operations() {
        let mut request = frame("addMediaClipExact");
        request["params"]["arguments"]["command"]["trackId"] =
            Value::String("t1\n悄悄追加删除".into());
        assert!(describe_edit(&request).is_err());
        let mut unknown = frame("overwriteProject");
        assert!(describe_edit(&unknown).is_err());
        unknown["params"]["arguments"]["command"] = serde_json::json!({
            "op":"moveClips","moves":[{"clipId":"c1","trackId":"t1","start":5.25}]
        });
        assert!(describe_edit(&unknown).unwrap().1.contains("5.250"));
    }

    #[test]
    fn malformed_batch_and_missing_identity_fail_closed() {
        let gate = NativeToolPermissions::default();
        let malformed = serde_json::json!({"method":"tools/call","params":{
            "name":"edit_apply_guarded","arguments":{"command":{"op":"batch","commands":[]}}
        }});
        assert!(gate.authorize(&malformed, Some(&original())).is_err());
        assert!(gate.authorize(&frame("addTextClip"), None).is_err());
        let mut invalid_template = frame("applySingleMainTemplate");
        invalid_template["params"]["arguments"]["command"] = serde_json::json!({
            "op":"applySingleMainTemplate","plan":{
                "start":0.0,"videos":[{"mediaId":"m1","start":0.0,
                    "sourceStart":4.0,"sourceEnd":2.0}],
                "subtitles":[],"overlays":[],"music":[],"voice":[]
            }
        });
        assert!(describe_edit(&invalid_template).is_err());
        assert!(gate.view().unwrap().pending_summary.is_none());
    }
}
