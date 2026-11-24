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
use crate::i18n::{t, tf};

const APPROVAL_TIMEOUT: Duration = Duration::from_secs(180);
// Once a human has decided, the approved operation must be dispatched promptly.
// This does not extend either the human deadline or the MCP execution timeout.
const TICKET_TIMEOUT: Duration = Duration::from_secs(30);

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
            _ => Err(t("permission.unknownMode")),
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
    message: Value,
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
    // set retains replay identities without a product request-count limit.
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
        let mut state = self
            .state
            .lock()
            .map_err(|_| t("permission.stateUnavailable"))?;
        if state.stopped {
            return Err(t("permission.stoppedApproval"));
        }
        expire_grant(&mut state);
        if state.pending.is_some() || state.grant.is_some() {
            return Err(t("permission.finishBeforeTask"));
        }
        state.last_refusal = None;
        state.execution_permit_consumed = false;
        Ok(())
    }

    /// A later write attempt has its own outcome, including failures before
    /// authorization. Do not explain those failures using an older refusal.
    pub(crate) fn begin_approval_attempt(&self) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| t("permission.stateUnavailable"))?;
        if state.stopped {
            return Err(t("permission.stoppedApproval"));
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
        let state = self
            .state
            .lock()
            .map_err(|_| t("permission.stateUnavailable"))?;
        Ok(PermissionView {
            mode: state.mode.index(),
            pending_id: state
                .pending
                .as_ref()
                .map(|request| request.request_id.clone()),
            pending_summary: state.pending.as_ref().map(|request| {
                describe_edit(&request.message)
                    .map(|(_, description)| {
                        tf(
                            "permission.currentProject",
                            &[&description, &&request.request_id[..12]],
                        )
                    })
                    .unwrap_or_else(|_| t("agent.errorUnexpected"))
            }),
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
        let mut state = self
            .state
            .lock()
            .map_err(|_| t("permission.stateUnavailable"))?;
        if state.stopped {
            return Err(t("permission.stoppedMode"));
        }
        expire_grant(&mut state);
        if state.pending.is_some() || state.grant.is_some() {
            return Err(t("permission.finishBeforeMode"));
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
        let mut state = self
            .state
            .lock()
            .map_err(|_| t("permission.stateUnavailable"))?;
        if state.stopped {
            return Err(t("permission.stopped"));
        }
        let pending = state.pending.as_mut().ok_or(t("permission.noPending"))?;
        if pending.request_id != request_id {
            return Err(t("permission.staleDecision"));
        }
        if pending.deadline <= Instant::now() {
            self.wake.notify_all();
            return Err(t("permission.expiredDecision"));
        }
        if pending.decided.is_some() {
            return Err(t("permission.alreadyDecided"));
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
        let mut state = self
            .state
            .lock()
            .map_err(|_| t("permission.stateUnavailable"))?;
        if state.stopped {
            return Err(t("permission.taskStopped"));
        }
        expire_grant(&mut state);
        if state.pending.is_some() || state.grant.is_some() {
            return Err(t("permission.alreadyPending"));
        }
        let key = (
            invocation.conversation_id,
            invocation.task_id,
            invocation.call_id.clone(),
        );
        if !state.seen.insert(key) {
            return Err(t("permission.replayed"));
        }
        // Identity, ownership and project checks precede this preview. A
        // preview rejection has issued no ticket and cannot reach the editor;
        // retain that native fact across subsequent read-only model calls.
        // Do not classify admission, replay, revocation or execution failures
        // as a known no-write result.
        let (safe_append_only, _) = match describe_edit(message) {
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
        state.pending = Some(Pending {
            request_id: request_id.clone(),
            fingerprint: fingerprint.clone(),
            invocation: invocation.clone(),
            message: message.clone(),
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
                return Err(t("permission.userStopped"));
            }
            if let Some(granted) = state.pending.as_ref().and_then(|pending| pending.decided) {
                state.pending = None;
                notify_native_ui();
                return if granted {
                    mint_grant(&mut state, request_id, fingerprint, invocation)
                } else {
                    let reason = t("permission.userDenied").to_owned();
                    state.last_refusal = Some((invocation.clone(), reason.clone()));
                    Err(reason)
                };
            }
            let remaining = state
                .pending
                .as_ref()
                .ok_or(t("permission.invalidRecord"))?
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
                let reason = t("permission.timeout").to_owned();
                state.last_refusal = Some((invocation.clone(), reason.clone()));
                return Err(reason);
            }
            let (next, _) = self
                .wake
                .wait_timeout(state, remaining)
                .map_err(|_| t("permission.waitUnavailable"))?;
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
            return Err(t("permission.onlyGuarded"));
        }
        let invocation = validated_invocation(invocation)?;
        let fingerprint = operation_fingerprint(message, invocation)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| t("permission.stateUnavailable"))?;
        if state.stopped {
            return Err(t("permission.taskStopped"));
        }
        // Burn a presented grant even on a mismatch. Changing parameters and
        // trying again must require a fresh user task, never reuse old consent.
        let grant = state.grant.take().ok_or(t("permission.missingTicket"))?;
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
            return Err(t("permission.staleTicket"));
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
    let invocation = invocation.ok_or(t("permission.missingInvocation"))?;
    if invocation.conversation_id > 9_007_199_254_740_991
        || invocation.task_id > 9_007_199_254_740_991
        || invocation.call_id.is_empty()
        || invocation.call_id.len() > 512
        || invocation.call_id.chars().any(char::is_control)
    {
        return Err(t("permission.invalidInvocation"));
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
        .ok_or(t("permission.invalidArguments"))?;
    // JSON-RPC request IDs are allocated only after approval. The exact native
    // path/session/revision and command arguments remain part of this digest.
    let bound = json!({"name":"edit_apply_guarded", "arguments":canonical_json(arguments),
        "conversationId":invocation.conversation_id,"taskId":invocation.task_id,"callId":invocation.call_id});
    Ok(format!(
        "{:x}",
        Sha256::digest(serde_json::to_vec(&bound).map_err(|_| t("permission.invalidArguments"))?)
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
        .ok_or_else(|| tf("permission.missingId", &[&field]))?;
    if id.is_empty() || id.chars().count() > 96 || id.chars().any(char::is_control) {
        return Err(tf("permission.unsafeId", &[&field]));
    }
    Ok(id.to_owned())
}

fn approval_time(value: &Value, field: &str) -> Result<f64, String> {
    let seconds = value
        .get(field)
        .and_then(Value::as_f64)
        .ok_or_else(|| tf("permission.missingTime", &[&field]))?;
    if !seconds.is_finite() || seconds < 0.0 {
        return Err(tf("permission.invalidTime", &[&field]));
    }
    Ok(seconds)
}

fn approval_text(value: &Value) -> Result<String, String> {
    let Some(body) = value.pointer("/style/content").and_then(Value::as_str) else {
        return Ok(t("permission.defaultText").to_owned());
    };
    if body.chars().count() > 512 {
        return Err(t("permission.textTooLong"));
    }
    if body
        .chars()
        .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\r' | '\t'))
    {
        return Err(t("permission.unsafeText"));
    }
    let displayed = body
        .replace("\r\n", "↵")
        .replace(['\n', '\r'], "↵")
        .replace('\t', " ");
    Ok(tf("permission.text", &[&displayed]))
}

fn describe_one_edit(item: &Value) -> Result<Vec<String>, String> {
    let op = item
        .get("op")
        .and_then(Value::as_str)
        .ok_or(t("permission.invalidOperation"))?;
    match op {
        "addMediaClipExact" => {
            let media = approval_id(item, "mediaId")?;
            let track = approval_id(item, "trackId")?;
            let start = approval_time(item, "start")?;
            let source_start = approval_time(item, "sourceStart")?;
            let source_end = approval_time(item, "sourceEnd")?;
            if source_end <= source_start {
                return Err(t("permission.invalidSourceTime"));
            }
            Ok(vec![tf(
                "permission.addMedia",
                &[
                    &media,
                    &track,
                    &format!("{start:.3}"),
                    &format!("{:.3}", start + source_end - source_start),
                    &format!("{source_start:.3}"),
                    &format!("{source_end:.3}"),
                ],
            )])
        }
        "addTextClip" => {
            let track = approval_id(item, "trackId")?;
            let start = approval_time(item, "start")?;
            let duration = approval_time(item, "duration")?;
            if duration <= 0.0 {
                return Err(t("permission.invalidTextTime"));
            }
            Ok(vec![tf(
                "permission.addText",
                &[
                    &track,
                    &format!("{start:.3}"),
                    &format!("{:.3}", start + duration),
                    &approval_text(item)?,
                ],
            )])
        }
        "moveClips" => {
            let moves = item
                .get("moves")
                .and_then(Value::as_array)
                .ok_or(t("permission.missingMoves"))?;
            if moves.is_empty() {
                return Err(t("permission.emptyMoves"));
            }
            moves
                .iter()
                .map(|wanted| {
                    let clip = approval_id(wanted, "clipId")?;
                    let track = approval_id(wanted, "trackId")?;
                    let start = approval_time(wanted, "start")?;
                    Ok(tf(
                        "permission.move",
                        &[&clip, &track, &format!("{start:.3}")],
                    ))
                })
                .collect()
        }
        "removeClips" => {
            if item.get("ripple").and_then(Value::as_bool).unwrap_or(false) {
                return Err(t("permission.noRipple"));
            }
            let clips = item
                .get("clipIds")
                .and_then(Value::as_array)
                .ok_or(t("permission.missingRemovals"))?;
            if clips.is_empty() {
                return Err(t("permission.emptyRemovals"));
            }
            clips
                .iter()
                .map(|clip| {
                    let id = clip.as_str().ok_or(t("permission.invalidClipId"))?;
                    if id.is_empty() || id.chars().count() > 96 || id.chars().any(char::is_control)
                    {
                        return Err(t("permission.unsafeClipId"));
                    }
                    Ok(tf("permission.remove", &[&id]))
                })
                .collect()
        }
        "updateClip" => {
            let clip = approval_id(item, "clipId")?;
            let patch = item
                .get("patch")
                .and_then(Value::as_object)
                .ok_or(t("permission.missingPatch"))?;
            if patch.is_empty() {
                return Err(t("permission.emptyPatch"));
            }
            let details = serde_json::to_string(patch).map_err(|_| t("permission.invalidPatch"))?;
            if details.chars().count() > 2048 {
                return Err(t("permission.patchTooLong"));
            }
            Ok(vec![tf("permission.patch", &[&clip, &details])])
        }
        "applySingleMainTemplate" => {
            let plan = item.get("plan").ok_or(t("permission.missingTemplate"))?;
            let base = approval_time(plan, "start")?;
            let mut rows = vec![tf("permission.template", &[&format!("{base:.3}")])];
            for (field, role, is_text) in [
                ("videos", t("permission.roleVideo"), false),
                ("subtitles", t("permission.roleSubtitles"), true),
                ("overlays", t("permission.roleOverlay"), true),
                ("music", t("permission.roleMusic"), false),
                ("voice", t("permission.roleVoice"), false),
            ] {
                let entries = plan
                    .get(field)
                    .and_then(Value::as_array)
                    .ok_or_else(|| tf("permission.missingTemplateList", &[&field]))?;
                for entry in entries {
                    let start = base + approval_time(entry, "start")?;
                    if is_text {
                        let duration = approval_time(entry, "duration")?;
                        if duration <= 0.0 || !(start + duration).is_finite() {
                            return Err(t("permission.invalidTemplateTextTime"));
                        }
                        rows.push(tf(
                            "permission.templateText",
                            &[
                                &role,
                                &format!("{start:.3}"),
                                &format!("{:.3}", start + duration),
                                &approval_text(entry)?,
                            ],
                        ));
                    } else {
                        let media = approval_id(entry, "mediaId")?;
                        let from = approval_time(entry, "sourceStart")?;
                        let to = approval_time(entry, "sourceEnd")?;
                        if to <= from || !(start + to - from).is_finite() {
                            return Err(t("permission.invalidTemplateMediaTime"));
                        }
                        rows.push(tf(
                            "permission.templateMedia",
                            &[
                                &role,
                                &media,
                                &format!("{start:.3}"),
                                &format!("{:.3}", start + to - from),
                                &format!("{from:.3}"),
                                &format!("{to:.3}"),
                            ],
                        ));
                    }
                }
            }
            Ok(rows)
        }
        _ => Err(t("permission.unsupportedOperation")),
    }
}

/// Only exact append operations are auto-admitted by the assisted setting.
/// Changes to existing clips, unknown operations, templates and mixed batches
/// always ask. Neither class bypasses the native edit schema/ownership checks.
fn describe_edit(message: &Value) -> Result<(bool, String), String> {
    let command = message
        .pointer("/params/arguments/command")
        .and_then(Value::as_object)
        .ok_or(t("permission.missingCommand"))?;
    let op = command
        .get("op")
        .and_then(Value::as_str)
        .ok_or(t("permission.invalidOperation"))?;
    let commands: Vec<&Value> = if op == "batch" {
        let items = command
            .get("commands")
            .and_then(Value::as_array)
            .ok_or(t("permission.invalidBatch"))?;
        if items.is_empty() || items.len() > 64 {
            return Err(t("permission.batchSize"));
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
            .ok_or(t("permission.missingBatchType"))?;
        if kind == "batch" {
            return Err(t("permission.nestedBatch"));
        }
        if !matches!(kind, "addMediaClipExact" | "addTextClip") {
            safe = false;
        }
        preview.extend(describe_one_edit(item)?);
        // An approval is meaningful only when the whole intended edit can be
        // inspected. Never silently omit operations from a destructive plan.
        if preview.len() > 128 {
            return Err(t("permission.previewTooLong"));
        }
    }
    Ok((
        safe,
        tf(
            "permission.batch",
            &[
                &commands.len(),
                &preview
                    .iter()
                    .enumerate()
                    .map(|(n, line)| format!("{}. {line}", n + 1))
                    .collect::<Vec<_>>()
                    .join("\n"),
            ],
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
    fn complete_approval_preview_preserves_literal_placeholder_text() {
        let _language = crate::i18n::language_lock();
        let dirs =
            concat_host::AppDirs::under(&std::env::temp_dir().join("bkcut-approval-interpolation"));
        for language in ["en", "zh-Hans"] {
            crate::i18n::select(language, &dirs);
            let gate = Arc::new(NativeToolPermissions::default());
            let mut request = frame("addTextClip");
            let original_text = "Keep {1} and {0} exactly";
            request["params"]["arguments"]["command"]["style"] = json!({"content":original_text});
            let retained = request.clone();
            let worker = Arc::clone(&gate);
            let handle = thread::spawn(move || worker.authorize(&request, Some(&original())));
            let id = wait_pending(&gate);
            let view = gate.view().unwrap();
            let preview = view.pending_summary.unwrap();
            assert!(
                preview.contains(original_text),
                "the actual approval preview must preserve text exactly: {preview}"
            );
            assert!(
                preview.contains(&id[..12]),
                "the separate native request identifier must still render"
            );
            assert_eq!(
                gate.state.lock().unwrap().pending.as_ref().unwrap().message,
                retained
            );
            gate.decide_from_ui(&id, false).unwrap();
            assert!(handle.join().unwrap().is_err());
        }
        crate::i18n::select("en", &dirs);
    }

    #[test]
    fn unique_tool_call_is_not_blocked_by_historical_identity_count() {
        let _language = crate::i18n::language_lock();
        let gate = NativeToolPermissions::default();
        gate.set_mode_from_ui(2).unwrap();
        gate.state
            .lock()
            .unwrap()
            .seen
            .extend((0..70_000).map(|i| (9, 3, format!("prior-{i}"))));
        let request = frame("addTextClip");
        let current = invocation("beyond-old-count-limit");
        let ticket = gate.authorize(&request, Some(&current)).unwrap().unwrap();
        gate.consume(&request, Some(&current), &ticket).unwrap();
        assert!(
            gate.authorize(&request, Some(&current)).is_err(),
            "an actual replay must still be rejected"
        );
    }

    #[test]
    fn default_asks_per_operation_and_denial_blocks_execution() {
        let _language = crate::i18n::language_lock();
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
        assert!(
            handle
                .join()
                .unwrap()
                .unwrap_err()
                .contains(&t("permission.userDenied"))
        );
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
        let _language = crate::i18n::language_lock();
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
        let _language = crate::i18n::language_lock();
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
        let _language = crate::i18n::language_lock();
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
        assert!(
            handle
                .join()
                .unwrap()
                .unwrap_err()
                .contains(&t("permission.userStopped"))
        );
        assert!(gate.decide_from_ui(&approval, true).is_err());
        assert!(gate.set_mode_from_ui(2).is_err());
        assert!(gate.view().unwrap().pending_id.is_none());
    }

    #[test]
    fn timeout_refuses_operation_and_late_ui_decision_cannot_approve_next_call() {
        let _language = crate::i18n::language_lock();
        let gate = Arc::new(NativeToolPermissions::default());
        let worker = Arc::clone(&gate);
        let handle =
            thread::spawn(move || worker.authorize(&frame("addTextClip"), Some(&original())));
        let expired = wait_pending(&gate);
        expire_pending(&gate);
        assert!(
            handle
                .join()
                .unwrap()
                .unwrap_err()
                .contains(&t("permission.timeout"))
        );
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
        let _language = crate::i18n::language_lock();
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
        assert!(
            handle
                .join()
                .unwrap()
                .unwrap_err()
                .contains(&t("permission.timeout"))
        );
        assert!(gate.view().unwrap().pending_id.is_none());
        assert!(
            gate.consume(&frame("addTextClip"), Some(&original()), "forged")
                .is_err()
        );
    }

    #[test]
    fn ticket_is_bound_to_command_project_revision_and_complete_invocation() {
        let _language = crate::i18n::language_lock();
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
        let _language = crate::i18n::language_lock();
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
        let _language = crate::i18n::language_lock();
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
        assert!(
            handle
                .join()
                .unwrap()
                .unwrap_err()
                .contains(&t("permission.userDenied"))
        );
    }

    #[test]
    fn native_refusal_diagnostics_are_scoped_and_reset_between_tasks() {
        let _language = crate::i18n::language_lock();
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
        let _language = crate::i18n::language_lock();
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
        let _language = crate::i18n::language_lock();
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
                    .contains(&t("permission.replayed"))
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
        let _language = crate::i18n::language_lock();
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
        let _language = crate::i18n::language_lock();
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
        let _language = crate::i18n::language_lock();
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
            "2.250",
            "5.750",
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
        let _language = crate::i18n::language_lock();
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
        let _language = crate::i18n::language_lock();
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
