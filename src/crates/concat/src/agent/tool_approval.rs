// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native-only, per-tool consent for the embedded Pi runtime.
//! This is deliberately not an MCP method: model/page JSON cannot select a
//! permission mode, answer a prompt, or mint an approval token.

use std::sync::{Condvar, Mutex};
use std::time::{Duration, Instant};

use concat_agent::operations::Invocation;
use serde_json::Value;
use sha2::{Digest, Sha256};

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(180);

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
    pub pending_summary: Option<String>,
}

#[derive(Debug)]
struct Pending {
    fingerprint: String,
    conversation: u64,
    call_id: String,
    summary: String,
    decided: Option<bool>,
}

#[derive(Default)]
struct State {
    mode: Mode,
    pending: Option<Pending>,
    stopped: bool,
}

/// The worker can wait for a genuine native UI choice without blocking the
/// editor event loop. A short separate lock never calls the editing engine.
#[derive(Default)]
pub(crate) struct NativeToolPermissions {
    state: Mutex<State>,
    wake: Condvar,
}

impl NativeToolPermissions {
    pub(crate) fn view(&self) -> Result<PermissionView, String> {
        let state = self.state.lock().map_err(|_| "工具授权状态不可用。")?;
        Ok(PermissionView {
            mode: state.mode.index(),
            pending_summary: state
                .pending
                .as_ref()
                .map(|request| request.summary.clone()),
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
        if state.pending.is_some() {
            return Err("请先批准或拒绝当前操作，再切换权限。".into());
        }
        let previous = state.mode;
        state.mode = new_mode;
        log::info!(
            "bk-cat-tool-audit event=permission_changed previous={} current={}",
            previous.index(),
            new_mode.index()
        );
        Ok(())
    }

    /// A decision never carries a model-supplied id. The worker owns the
    /// immutable pending request; only the actual native button resolves it.
    pub(crate) fn decide_from_ui(&self, approve: bool) -> Result<(), String> {
        let mut state = self.state.lock().map_err(|_| "工具授权状态不可用。")?;
        if state.stopped {
            return Err("当前会话已停止。".into());
        }
        let pending = state.pending.as_mut().ok_or("没有待批准的剪辑操作。")?;
        if pending.decided.is_some() {
            return Err("本次剪辑操作已经作出决定。".into());
        }
        pending.decided = Some(approve);
        log::info!(
            "bk-cat-tool-audit event=user_decision fingerprint={} conversation={} call_id={} granted={}",
            pending.fingerprint,
            pending.conversation,
            pending.call_id,
            approve
        );
        self.wake.notify_all();
        Ok(())
    }

    pub(crate) fn cancel(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
            state.pending = None;
            self.wake.notify_all();
        }
    }

    /// Called only after the existing per-task, project and Pi identity guard.
    /// Revalidate the task after returning: the editor may change while the
    /// user considers a prompt. The native editor guard is the final authority.
    pub(crate) fn admit(
        &self,
        message: &Value,
        invocation: Option<&Invocation>,
    ) -> Result<(), String> {
        let name = message.pointer("/params/name").and_then(Value::as_str);
        if message.get("method").and_then(Value::as_str) != Some("tools/call")
            || name != Some("edit_apply_guarded")
        {
            return Ok(());
        }
        let invocation = invocation.ok_or("剪辑工具缺少原版智能体调用身份。")?;
        let (safe_append_only, description) = describe_edit(message)?;
        let fingerprint = format!(
            "{:x}",
            Sha256::digest(serde_json::to_vec(message).map_err(|_| "剪辑工具参数无法核对。")?)
        );
        let mut state = self.state.lock().map_err(|_| "工具授权状态不可用。")?;
        if state.stopped {
            return Err("剪辑任务已停止。".into());
        }
        let needs_prompt = match state.mode {
            Mode::Ask => true,
            Mode::Assist => !safe_append_only,
            Mode::Full => false,
        };
        if !needs_prompt {
            log::info!(
                "bk-cat-tool-audit event=auto_granted mode={} fingerprint={} conversation={} call_id={}",
                state.mode.index(),
                fingerprint,
                invocation.conversation_id,
                invocation.call_id
            );
            return Ok(());
        }
        if state.pending.is_some() {
            return Err("已有剪辑工具等待用户批准。".into());
        }
        let summary = format!("当前工程：{}（编号 {}）", description, &fingerprint[..12]);
        state.pending = Some(Pending {
            fingerprint,
            conversation: invocation.conversation_id,
            call_id: invocation.call_id.clone(),
            summary,
            decided: None,
        });
        log::info!(
            "bk-cat-tool-audit event=approval_requested mode={} conversation={} call_id={}",
            state.mode.index(),
            invocation.conversation_id,
            invocation.call_id
        );
        let deadline = Instant::now() + APPROVAL_TIMEOUT;
        loop {
            if state.stopped {
                state.pending = None;
                return Err("用户已停止剪辑工具操作。".into());
            }
            if let Some(granted) = state.pending.as_ref().and_then(|pending| pending.decided) {
                state.pending = None;
                return if granted {
                    Ok(())
                } else {
                    Err("用户拒绝了本次剪辑操作，工程未修改。".into())
                };
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                state.pending = None;
                log::info!(
                    "bk-cat-tool-audit event=approval_timeout conversation={} call_id={}",
                    invocation.conversation_id,
                    invocation.call_id
                );
                return Err("等待剪辑批准超时，工程未修改。".into());
            }
            let (next, _) = self
                .wake
                .wait_timeout(state, remaining)
                .map_err(|_| "等待剪辑批准时状态不可用。")?;
            state = next;
        }
    }

    pub(crate) fn record_result(&self, name: &str, response: &Value) {
        let outcome = if response.pointer("/result/isError") == Some(&Value::Bool(true))
            || response.get("error").is_some()
        {
            "refused"
        } else {
            "completed"
        };
        log::info!("bk-cat-tool-audit event=tool_result name={name} outcome={outcome}");
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
    let mut kinds = Vec::with_capacity(commands.len());
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
        // Never display raw model-supplied operation names in a native prompt.
        // Long or control-character values could otherwise disrupt the UI.
        kinds.push(match kind {
            "addMediaClipExact" => "精确添加素材",
            "addTextClip" => "添加字幕或贴片文字",
            "moveClips" => "移动片段",
            "splitClips" => "分割片段",
            "removeClips" => "删除片段",
            "applySingleMainTemplate" => "应用主轨模板",
            _ => "其他剪辑操作",
        });
    }
    let labels = kinds.iter().take(4).copied().collect::<Vec<_>>().join("、");
    let suffix = if kinds.len() > 4 { " 等" } else { "" };
    Ok((
        safe,
        format!("准备执行 {} 项剪辑（{}{suffix}）", kinds.len(), labels),
    ))
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
                    "command":{"op":op,"trackId":"t1","start":0.0,"duration":1.0}}
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

    fn wait_pending(gate: &NativeToolPermissions) {
        let deadline = Instant::now() + Duration::from_secs(3);
        loop {
            if gate.view().unwrap().pending_summary.is_some() {
                return;
            }
            assert!(Instant::now() < deadline, "approval did not become visible");
            thread::yield_now();
        }
    }

    #[test]
    fn default_asks_per_operation_and_denial_blocks_execution() {
        let gate = Arc::new(NativeToolPermissions::default());
        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || worker.admit(&frame("addTextClip"), Some(&original())));
        wait_pending(&gate);
        assert_eq!(gate.view().unwrap().mode, 0);
        assert!(gate.set_mode_from_ui(2).is_err());
        gate.decide_from_ui(false).unwrap();
        assert!(handle.join().unwrap().unwrap_err().contains("拒绝"));
        assert!(gate.view().unwrap().pending_summary.is_none());
    }

    #[test]
    fn assisted_only_auto_grants_known_append_only_edits() {
        let gate = Arc::new(NativeToolPermissions::default());
        gate.set_mode_from_ui(1).unwrap();
        gate.admit(&frame("addMediaClipExact"), Some(&original()))
            .unwrap();
        assert!(gate.view().unwrap().pending_summary.is_none());
        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || worker.admit(&frame("removeClips"), Some(&original())));
        wait_pending(&gate);
        gate.decide_from_ui(true).unwrap();
        assert!(handle.join().unwrap().is_ok());
    }

    #[test]
    fn full_access_requires_native_setting_and_cancel_revokes_pending() {
        let gate = Arc::new(NativeToolPermissions::default());
        assert!(gate.set_mode_from_ui(42).is_err());
        gate.set_mode_from_ui(2).unwrap();
        gate.admit(&frame("removeClips"), Some(&original()))
            .unwrap();
        gate.set_mode_from_ui(0).unwrap();
        let worker = Arc::clone(&gate);
        let handle = thread::spawn(move || worker.admit(&frame("removeClips"), Some(&original())));
        wait_pending(&gate);
        gate.cancel();
        assert!(handle.join().unwrap().unwrap_err().contains("停止"));
        assert!(gate.set_mode_from_ui(2).is_err());
    }

    #[test]
    fn malformed_batch_and_missing_identity_fail_closed() {
        let gate = NativeToolPermissions::default();
        let malformed = serde_json::json!({"method":"tools/call","params":{
            "name":"edit_apply_guarded","arguments":{"command":{"op":"batch","commands":[]}}
        }});
        assert!(gate.admit(&malformed, Some(&original())).is_err());
        assert!(gate.admit(&frame("addTextClip"), None).is_err());
        assert!(gate.view().unwrap().pending_summary.is_none());
    }
}
