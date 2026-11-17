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
    /// Existing clip identities, resolved only by the native project at render time.
    pub pending_clip_ids: Vec<String>,
}

#[derive(Debug)]
struct Pending {
    fingerprint: String,
    conversation: u64,
    call_id: String,
    summary: String,
    affected_clip_ids: Vec<String>,
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
        let affected_clip_ids = affected_existing_clip_ids(message);
        state.pending = Some(Pending {
            fingerprint,
            conversation: invocation.conversation_id,
            call_id: invocation.call_id.clone(),
            summary,
            affected_clip_ids,
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
        .replace('\n', "↵")
        .replace('\r', "↵")
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
                        rows.push(format!(
                            "{role} → 新轨道，时间 {start:.3}～{end:.3} 秒；{}",
                            approval_text(entry)?,
                            end = start + duration
                        ));
                    } else {
                        let media = approval_id(entry, "mediaId")?;
                        let from = approval_time(entry, "sourceStart")?;
                        let to = approval_time(entry, "sourceEnd")?;
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
        assert_eq!(gate.view().unwrap().pending_clip_ids, vec!["c1".to_owned()]);
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
        assert!(gate.admit(&malformed, Some(&original())).is_err());
        assert!(gate.admit(&frame("addTextClip"), None).is_err());
        assert!(gate.view().unwrap().pending_summary.is_none());
    }
}
