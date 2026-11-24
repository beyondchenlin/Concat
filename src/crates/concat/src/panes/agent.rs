// SPDX-License-Identifier: AGPL-3.0-or-later

//! Native readiness, recovery and task presentation for the embedded runtime.
//! This owns no model client, scheduler, persistence or editable project.
//! Visible messages are bounded projections of the original persisted session.

use concat_agent::configuration::catalogue::ModelChoice;
use concat_agent::configuration::{ChannelId, ConfigurationSnapshot, ModelChannel};
use concat_agent::credentials::{CredentialScope, CredentialStatus, platform_store};
use concat_agent::{OwnerStatus, Phase};
use slint::{ComponentHandle, Model, ModelRc, VecModel};
use std::rc::Rc;

use crate::host::Shell;
use crate::i18n::{t, tf};
use crate::ui::{
    AgentCallUi, AgentMessageData, AgentMessageRole, AgentStatusData, AgentSuggestion, AgentUi,
    App, ScriptUi,
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
    /// Isolated availability injected only into this unit-test pane.
    #[cfg(test)]
    pub(crate) credential_store_fixture:
        Option<std::sync::Arc<dyn concat_agent::credentials::CredentialStore>>,
    /// Native facts required before submitting a new request.
    pub readiness: AgentReadiness,
    /// The currently observed task, if any.
    pub task: AgentTask,
    task_connection: Option<String>,
    model_choices: Vec<ModelChoice>,
    /// A user-facing failure reported by the real runtime or native bridge.
    pub error: String,
    /// Actual configuration or secure-store failure; never a secret value.
    pub configuration_error: String,
    /// Read-only recovery evidence from the current, authenticated component.
    pub recovery_summary: String,
    pub can_recover: bool,
    pub resume_interrupted: bool,
    pub can_review_current: bool,
    operation_facts: Option<serde_json::Value>,
    /// Explicit cancellation of reviewed interrupted work, never fee consent.
    pub can_discard: bool,
    pub can_review: bool,
    pub runtime_loading: bool,
    pub runtime_loaded: bool,
    /// Distinct from a real running model task; holds no execution authority.
    pub waiting_for_ready: bool,
    runtime_seen: bool,
    runtime_may_wait: bool,
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

/// A short excerpt for the compact transcript. The complete record stays
/// intact and can always be expanded; the UI never alters durable Pi history.
fn message_preview(role: AgentMessageRole, body: &str) -> (String, bool) {
    if role == AgentMessageRole::Compaction {
        // A system summary always starts folded. Expanding still shows
        // its untouched original body, including the original Pi wrapper.
        return (t("agent.compactionPreview"), true);
    }
    // This runs as streaming text changes: cap scans to a few hundred
    // characters/lines, even when a tool returned a very large payload.
    let collapsible = role == AgentMessageRole::Tool
        || body.chars().nth(360).is_some()
        || body.lines().take(9).count() > 8;
    if !collapsible {
        return (String::new(), false);
    }
    let line = body
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty() && !line.starts_with("```") && *line != "{" && *line != "[")
        .unwrap_or_else(|| body.trim());
    let limit = if role == AgentMessageRole::Tool {
        120
    } else {
        220
    };
    let mut excerpt: String = line.chars().take(limit).collect();
    if line.chars().nth(limit).is_some() || body.trim().len() > line.len() {
        excerpt.push('…');
    }
    let preview = if role == AgentMessageRole::Tool {
        tf("agent.toolPreview", &[&excerpt])
    } else {
        excerpt
    };
    (preview, true)
}

fn role_label(role: AgentMessageRole) -> String {
    match role {
        AgentMessageRole::User => crate::i18n::t("agent.roleUser"),
        AgentMessageRole::Assistant => crate::i18n::t("agent.roleAssistant"),
        AgentMessageRole::Tool => crate::i18n::t("agent.roleTool"),
        AgentMessageRole::Compaction => crate::i18n::t("agent.roleCompaction"),
    }
}

/// Codes are protocol data. Localized prose and provider diagnostics cannot
/// enable credential, billing, permission or project-identity actions.
fn error_presentation(raw: &str) -> (String, String, String) {
    if raw.is_empty() {
        return (String::new(), String::new(), String::new());
    }
    let (candidate, diagnostic) = raw.split_once('\n').unwrap_or((raw, ""));
    let code = match candidate {
        "agent.chatNotReady" => "agent.chatNotReady",
        "agent.closedDetail" => "agent.closedDetail",
        "agent.configureBeforeSend" => "agent.configureBeforeSend",
        "agent.errorUnexpected" => "agent.errorUnexpected",
        "agent.notReady" => "agent.notReady",
        "agent.reviewBeforeContinue" => "agent.reviewBeforeContinue",
        "agent.stopDetail" => "agent.stopDetail",
        "agent.stopping" => "agent.stopping",
        "permission.alreadyPending" => "permission.alreadyPending",
        "permission.stateUnavailable" => "permission.stateUnavailable",
        "agent.enterRequest" => "agent.enterRequest",
        "agent.reopenIdentity" => "agent.reopenIdentity",
        "modelSettings.configurationUnavailable" => "modelSettings.configurationUnavailable",
        "permission.editContextChanged" => "permission.editContextChanged",
        "permission.invalidArguments" => "permission.invalidArguments",
        "permission.missingInvocation" => "permission.missingInvocation",
        "permission.missingTicket" => "permission.missingTicket",
        "permission.onlyGuarded" => "permission.onlyGuarded",
        "permission.readOnlyTask" => "permission.readOnlyTask",
        "permission.taskScope" => "permission.taskScope",
        "runtime.editReceiptUnconfirmed" => "runtime.editReceiptUnconfirmed",
        "runtime.invalidRequest" => "runtime.invalidRequest",
        "runtime.approvalDeliveryFailed" => "runtime.approvalDeliveryFailed",
        "runtime.inputTooLarge" => "runtime.inputTooLarge",
        "runtime.platformFailed" => "runtime.platformFailed",
        "runtime.saveNotificationUnconfirmed" => "runtime.saveNotificationUnconfirmed",
        "runtime.submissionUnconfirmed" => "runtime.submissionUnconfirmed",
        "runtime.unsupportedPlatform" => "runtime.unsupportedPlatform",
        "script.generationFailed" => "script.generationFailed",
        "script.stopped" => "script.stopped",
        "script.waitOperation" => "script.waitOperation",
        "script.acceptChanged" => "script.acceptChanged",
        "script.candidateSaveFailed" => "script.candidateSaveFailed",
        "script.changedAfterGeneration" => "script.changedAfterGeneration",
        "script.componentBound" => "script.componentBound",
        "script.componentStopped" => "script.componentStopped",
        "script.documentMissing" => "script.documentMissing",
        "script.editExhausted" => "script.editExhausted",
        "script.emptyResult" => "script.emptyResult",
        "script.errorCandidate" => "script.errorCandidate",
        "script.errorConnection" => "script.errorConnection",
        "script.errorDatabase" => "script.errorDatabase",
        "script.errorFormat" => "script.errorFormat",
        "script.errorInput" => "script.errorInput",
        "script.errorIo" => "script.errorIo",
        "script.errorLimit" => "script.errorLimit",
        "script.errorMissing" => "script.errorMissing",
        "script.errorReadOnly" => "script.errorReadOnly",
        "script.errorSelection" => "script.errorSelection",
        "script.errorStale" => "script.errorStale",
        "script.errorVersion" => "script.errorVersion",
        "script.finishFirst" => "script.finishFirst",
        "script.identitySaveFailed" => "script.identitySaveFailed",
        "script.invalidPath" => "script.invalidPath",
        "script.invalidVersion" => "script.invalidVersion",
        "script.lateStopped" => "script.lateStopped",
        "script.legacyCopyFailed" => "script.legacyCopyFailed",
        "script.legacyMissing" => "script.legacyMissing",
        "script.localInterrupted" => "script.localInterrupted",
        "script.locationCheck" => "script.locationCheck",
        "script.locationOther" => "script.locationOther",
        "script.locationUnreadable" => "script.locationUnreadable",
        "script.openStorageFailed" => "script.openStorageFailed",
        "script.projectChanged" => "script.projectChanged",
        "script.projectClosing" => "script.projectClosing",
        "script.recoveryInvalid" => "script.recoveryInvalid",
        "script.reopen" => "script.reopen",
        "script.requireInstruction" => "script.requireInstruction",
        "script.requireText" => "script.requireText",
        "script.saveBeforeSelect" => "script.saveBeforeSelect",
        "script.saveFailed" => "script.saveFailed",
        "script.saveFirst" => "script.saveFirst",
        "script.stopBeforeReload" => "script.stopBeforeReload",
        "script.versionMissing" => "script.versionMissing",
        "agent.errorRecoveryStorage" => "agent.errorRecoveryStorage",
        "agent.recoveryNoInterruptedTask" => "agent.recoveryNoInterruptedTask",
        "agent.errorRecoveryBinding" => "agent.errorRecoveryBinding",
        "agent.errorRecoveryPending" => "agent.errorRecoveryPending",
        "agent.errorRecoverySnapshotMissing" => "agent.errorRecoverySnapshotMissing",
        "agent.errorRecoverySnapshotInvalid" => "agent.errorRecoverySnapshotInvalid",
        "agent.errorRecoverySnapshotRead" => "agent.errorRecoverySnapshotRead",
        "agent.errorRecoverySnapshotWrite" => "agent.errorRecoverySnapshotWrite",
        "agent.errorRecoverySnapshotConflict" => "agent.errorRecoverySnapshotConflict",
        "agent.errorRecoveryScope" => "agent.errorRecoveryScope",
        "agent.errorRecoveryConnectionChanged" => "agent.errorRecoveryConnectionChanged",
        "agent.errorRecoveryChanged" => "agent.errorRecoveryChanged",
        "agent.errorRecoveryOtherProject" => "agent.errorRecoveryOtherProject",
        "runtime.loadTimeout" => "runtime.loadTimeout",
        "runtime.waitTimeout" => "runtime.waitTimeout",
        "runtime.webCapability" => "runtime.webCapability",
        "runtime.scriptResource" => "runtime.scriptResource",
        "runtime.scriptExecution" => "runtime.scriptExecution",
        "runtime.protocolViolation" => "runtime.protocolViolation",
        "runtime.startupFailed" => "runtime.startupFailed",
        "runtime.modelSettingsBusy" => "runtime.modelSettingsBusy",
        "runtime.taskBusy" => "runtime.taskBusy",
        "runtime.queueFull" => "runtime.queueFull",
        "runtime.stateChanged" => "runtime.stateChanged",
        "runtime.editUnverified" => "runtime.editUnverified",
        "runtime.taskIncomplete" => "runtime.taskIncomplete",
        "agent.openProject" => "agent.openProject",
        "agent.sessionUnavailable" => "agent.sessionUnavailable",
        "agent.connectAndReview" => "agent.connectAndReview",
        "agent.alreadyWaiting" => "agent.alreadyWaiting",
        "agent.errorAuthentication" => "agent.errorAuthentication",
        "agent.errorBilling" => "agent.errorBilling",
        "agent.errorAccess" => "agent.errorAccess",
        "agent.errorProjectIdentity" => "agent.errorProjectIdentity",
        "agent.errorModelRequired" => "agent.errorModelRequired",
        "agent.errorModelUnavailable" => "agent.errorModelUnavailable",
        "lifecycle.activityPaused" => "lifecycle.activityPaused",
        "lifecycle.activityStopped" => "lifecycle.activityStopped",
        "lifecycle.activitySaved" => "lifecycle.activitySaved",
        "lifecycle.windowsStopping" => "lifecycle.windowsStopping",
        "lifecycle.windowsClosed" => "lifecycle.windowsClosed",
        _ => "agent.errorUnexpected",
    };
    if candidate == code && is_product_error_wrapper(code) {
        return (
            code.to_owned(),
            localize_product_message(raw),
            String::new(),
        );
    }
    (
        code.to_owned(),
        t(code),
        if candidate == code {
            diagnostic.to_owned()
        } else {
            raw.to_owned()
        },
    )
}

fn is_product_error_wrapper(code: &str) -> bool {
    matches!(
        code,
        "script.openStorageFailed"
            | "script.candidateSaveFailed"
            | "script.identitySaveFailed"
            | "script.saveFailed"
            | "script.legacyCopyFailed"
    )
}

/// Translate a recognized native product code only at the display boundary.
/// Literal user content, already-localized messages and external diagnostics
/// remain untouched. This projection never changes an execution decision.
pub(crate) fn localize_product_message(raw: &str) -> String {
    // Peel the native, single-argument error wrappers iteratively. Nested
    // storage failures cannot grow the call stack or reinterpret user braces.
    let mut rest = raw;
    let mut wrappers = Vec::new();
    loop {
        let (candidate, detail) = rest.split_once('\n').unwrap_or((rest, ""));
        if !is_product_error_wrapper(candidate) {
            break;
        }
        wrappers.push(candidate);
        rest = detail;
    }
    let candidate = rest.split_once('\n').map_or(rest, |(code, _)| code);
    let (code, summary, details) = error_presentation(rest);
    let mut rendered = if candidate != code {
        rest.to_owned()
    } else if details.is_empty() {
        summary
    } else {
        format!("{summary}\n{details}")
    };
    for key in wrappers.into_iter().rev() {
        rendered = tf(key, &[&rendered]);
    }
    rendered
}

/// Display only the native journal's explicit states. Missing/zero counts do
/// not imply success, and a captured save never substitutes for an edit receipt.
fn recovery_fact_summary(facts: &serde_json::Value) -> String {
    let states = &facts["currentProjectStates"];
    let labels = [
        ("outcome-unknown", "recovery.outcomeUnknown"),
        ("applied-not-saved", "recovery.appliedNotSaved"),
        ("saved-not-notified", "recovery.savedNotNotified"),
        ("notified", "recovery.notified"),
        ("not-applied", "recovery.notApplied"),
        ("unchanged", "recovery.unchanged"),
    ];
    let mut lines = Vec::new();
    let mut captured = 0_u64;
    for (state, label) in labels {
        if let Some(count) = states[state]["total"].as_u64().filter(|count| *count > 0) {
            lines.push(tf(label, &[&count]));
        }
        captured = captured.saturating_add(
            states[state]["capturedSaveWithoutReceipt"]["total"]
                .as_u64()
                .unwrap_or(0),
        );
    }
    if captured > 0 {
        lines.push(tf("recovery.capturedWithoutReceipt", &[&captured]));
    }
    if let Some(count) = facts["pendingOtherProjects"]
        .as_u64()
        .filter(|count| *count > 0)
    {
        lines.push(tf("recovery.otherPending", &[&count]));
    }
    lines.join("\n")
}

impl AgentPane {
    pub(crate) fn task_connection_id(&self) -> Option<&str> {
        self.task_connection.as_deref()
    }
    /// Only user content or actual work keeps an automatically-created draft.
    pub(crate) fn has_user_work(&self) -> bool {
        self.task.running
            || self.waiting_for_ready
            || !self.messages.is_empty()
            || !self.references.is_empty()
            || !self.pending_attachments.is_empty()
    }
    pub(crate) fn apply_runtime(&mut self, runtime: &crate::embedded::RuntimeView) {
        self.runtime_seen = true;
        self.runtime_may_wait = runtime.may_wait_for_readiness();
        self.waiting_for_ready = runtime.waiting_for_ready;
        self.readiness.runtime_ready = runtime.phase == "ready" && !runtime.closed;
        if !self.readiness.runtime_ready || self.waiting_for_ready {
            self.readiness.calls_authorized = false;
        }
        self.task.running = runtime.task_running;
        self.task_connection = runtime
            .task_running
            .then(|| {
                runtime
                    .configuration
                    .as_ref()
                    .and_then(|configuration| configuration["id"].as_str())
                    .map(str::to_owned)
            })
            .flatten();
        self.task.title = runtime.task_title.clone();
        self.task.detail = if runtime.task_running {
            "agent.receiving".into()
        } else {
            String::new()
        };
        self.messages.clear();
        if let Some(presentation) = &runtime.presentation {
            if let Some(messages) = presentation["messages"].as_array() {
                self.messages.extend(messages.iter().filter_map(|message| {
                    let role = match message["role"].as_str()? {
                        "user" => AgentMessageRole::User,
                        "assistant" => AgentMessageRole::Assistant,
                        "tool" => AgentMessageRole::Tool,
                        "compaction" => AgentMessageRole::Compaction,
                        _ => return None,
                    };
                    let body = message["text"].as_str()?;
                    let (preview, collapsible) = message_preview(role, body);
                    Some(AgentMessageData {
                        role,
                        author: role_label(role).into(),
                        body: body.into(),
                        preview: preview.into(),
                        collapsible,
                        pending: false,
                    })
                }));
            }
            if let Some(partial) = presentation["partialText"]
                .as_str()
                .filter(|text| !text.is_empty())
            {
                let (preview, collapsible) = message_preview(AgentMessageRole::Assistant, partial);
                self.messages.push(AgentMessageData {
                    role: AgentMessageRole::Assistant,
                    author: role_label(AgentMessageRole::Assistant).into(),
                    body: partial.into(),
                    preview: preview.into(),
                    collapsible,
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
        self.resume_interrupted = false;
        self.can_review_current = false;
        self.operation_facts = None;
        self.can_discard = false;
        self.error = runtime.error.clone();
        self.recovery_summary = if runtime.task_title == "agent.recoveryReviewedNotice" {
            "agent.recoveryReviewedNotice".into()
        } else {
            String::new()
        };
        // The snapshot remains bound to the project after activation; its old
        // blocking reason describes recovery, not the current ready session.
        if self.can_review
            && let Some(snapshot) = &runtime.snapshot
        {
            self.operation_facts = snapshot
                .get("operationFacts")
                .filter(|facts| facts["schema"] == 1)
                .cloned();
            self.can_review_current = snapshot["canReviewCurrentProject"] == true
                && snapshot["snapshotId"].is_string()
                && snapshot["projectPath"].is_string();
            self.can_discard = self.can_review
                && snapshot["snapshotId"].is_string()
                && snapshot["projectPath"].is_string()
                && snapshot["hasInterruptedWork"] == true
                && snapshot["canDiscardInterrupted"] == true;
            if let Some(reason) = snapshot["blockedReason"]
                .as_str()
                .filter(|reason| !reason.is_empty())
            {
                self.recovery_summary = reason.to_owned();
            } else if self.can_review {
                self.can_recover = snapshot["snapshotId"].is_string()
                    && snapshot["projectPath"].is_string()
                    && (snapshot["hasInterruptedWork"] == false
                        || (snapshot["hasInterruptedWork"] == true
                            && snapshot["canContinueInterrupted"] == true));
                self.resume_interrupted =
                    self.can_recover && snapshot["hasInterruptedWork"] == true;
                self.recovery_summary = if self.resume_interrupted {
                    "agent.recoveryResumeNotice"
                } else if snapshot["schemaVersion"] == "empty" {
                    "agent.recoveryNew"
                } else {
                    "agent.recoveryExisting"
                }
                .to_owned();
            }
        }
    }

    fn can_wait_for_startup(&self, owner: &OwnerStatus) -> bool {
        matches!(owner.phase, Phase::RecoveryRequired | Phase::Ready)
            && !self.readiness.runtime_ready
            && (self.runtime_may_wait
                || (!self.runtime_seen && owner.phase == Phase::RecoveryRequired))
            && !self.waiting_for_ready
            && !self.task.running
            && self.error.is_empty()
            && self.configuration_error.is_empty()
            && !self.readiness.provider.trim().is_empty()
            && !self.readiness.model.trim().is_empty()
            && self.readiness.credentials_ready
            && self.pending_attachments.is_empty()
    }

    pub(crate) fn validate_waiting_submission(
        &self,
        owner: Result<OwnerStatus, String>,
        draft: &str,
    ) -> Result<String, String> {
        let text = draft.trim();
        if text.is_empty() {
            return Err(t("agent.enterRequest"));
        }
        if self.waiting_for_ready {
            return Err(t("agent.alreadyWaiting"));
        }
        let owner = owner?;
        if !self.can_wait_for_startup(&owner) {
            let data = self.data(Ok(owner));
            return Err(if !data.error.is_empty() {
                data.error.to_string()
            } else {
                t("agent.notReady")
            });
        }
        Ok(text.to_owned())
    }

    fn missing(&self) -> String {
        let mut missing = Vec::new();
        if self.readiness.provider.trim().is_empty() {
            missing.push(t("agent.missingProvider"));
        }
        if self.readiness.model.trim().is_empty() {
            missing.push(t("agent.missingModel"));
        }
        if !self.readiness.credentials_ready {
            missing.push(t("agent.missingCredential"));
        }
        if !self.readiness.runtime_ready {
            missing.push(t("agent.missingRuntime"));
        }
        if missing.is_empty() {
            String::new()
        } else {
            tf("agent.missingItems", &[&missing.join(", ")])
        }
    }

    /// Recompute from the bridge's current, read-only ownership status.
    /// This never confirms recovery, acquires an execution permit or resumes
    /// interrupted work. UI enablement is not an execution credential.
    pub fn data(&self, owner: Result<OwnerStatus, String>) -> AgentStatusData {
        let task_title = localize_product_message(&crate::i18n::task_status(&self.task.title));
        let task_detail = crate::i18n::task_status(&self.task.detail);
        let missing = self.missing();
        let raw_error = if self.error.is_empty() {
            &self.configuration_error
        } else {
            &self.error
        };
        let (error_code, error, error_details) = error_presentation(raw_error);
        let model = if self.readiness.provider.trim().is_empty()
            || self.readiness.model.trim().is_empty()
        {
            t("agent.noModel").to_owned()
        } else {
            format!("{} / {}", self.readiness.provider, self.readiness.model)
        };
        let mut data = AgentStatusData {
            needs_model_setup: self.readiness.provider.trim().is_empty()
                || self.readiness.model.trim().is_empty()
                || !self.readiness.credentials_ready,
            missing: missing.clone().into(),
            error_code: error_code.into(),
            error: error.into(),
            error_details: error_details.into(),
            model: model.into(),
            can_review: self.can_review,
            can_continue: self.can_recover,
            can_review_current: self.can_review_current,
            recovery_facts: self
                .operation_facts
                .as_ref()
                .map(recovery_fact_summary)
                .unwrap_or_default()
                .into(),
            resume_interrupted: self.resume_interrupted,
            can_discard: self.can_discard,
            ..AgentStatusData::default()
        };
        let owner = match owner {
            Ok(owner) => owner,
            Err(error) => {
                data.title = t("agent.sessionUnavailable").into();
                data.detail = t("agent.manualAvailable").into();
                let (code, summary, details) = error_presentation(&error);
                data.error_code = code.into();
                data.error = summary.into();
                data.error_details = details.into();
                data.can_connect = true;
                data.can_review = false;
                data.can_continue = false;
                data.resume_interrupted = false;
                data.can_review_current = false;
                data.recovery_facts = "".into();
                data.can_discard = false;
                return data;
            }
        };
        data.can_connect = owner.phase == Phase::Released
            || (owner.phase == Phase::RecoveryRequired
                && !self.runtime_loaded
                && !self.runtime_loading);
        data.can_stop = matches!(owner.phase, Phase::RecoveryRequired | Phase::Ready)
            && (self.waiting_for_ready
                || self.task.running
                || self.readiness.runtime_ready
                || self.runtime_loaded
                || self.runtime_loading
                || owner.active_operations > 0);
        if owner.phase != Phase::RecoveryRequired {
            data.can_review = false;
            data.can_continue = false;
            data.resume_interrupted = false;
            data.can_review_current = false;
            data.recovery_facts = "".into();
            data.can_discard = false;
        }
        // Native ownership can advance before the runtime presentation does.
        // Only a live recovery view may present the snapshot's blocking reason.
        if data.can_review || self.recovery_summary == "agent.recoveryReviewedNotice" {
            data.recovery_summary = match self.recovery_summary.as_str() {
                "agent.recoveryResumeNotice" => t("agent.recoveryResumeNotice"),
                "agent.recoveryReviewedNotice" => t("agent.recoveryReviewedNotice"),
                "agent.recoveryNew" => t("agent.recoveryNew"),
                "agent.recoveryExisting" => t("agent.recoveryExisting"),
                reason => {
                    let (code, summary, _) = error_presentation(reason);
                    if code == "agent.errorUnexpected" {
                        reason.to_owned()
                    } else {
                        summary
                    }
                }
            }
            .into();
        }
        if self.waiting_for_ready && matches!(owner.phase, Phase::RecoveryRequired | Phase::Ready) {
            data.busy = true;
            data.can_connect = false;
            data.can_send = false;
            data.can_review = false;
            data.can_continue = false;
            data.resume_interrupted = false;
            data.can_review_current = false;
            data.recovery_facts = "".into();
            data.can_discard = false;
            data.missing = "".into();
            data.recovery_summary = "".into();
            data.title = t("agent.connecting").into();
            data.detail = t("agent.waitingDetail").into();
            return data;
        }
        if self.can_wait_for_startup(&owner) {
            data.can_send = true;
            data.missing = "".into();
            data.can_review = false;
            data.can_continue = false;
            data.resume_interrupted = false;
            data.can_review_current = false;
            data.recovery_facts = "".into();
            data.can_discard = false;
            data.title = if self.runtime_loading {
                t("agent.connecting")
            } else {
                t("agent.idle")
            }
            .into();
            data.detail = t("agent.connectBeforeSend").into();
            return data;
        }
        match owner.phase {
            Phase::RecoveryRequired => {
                data.title = t("agent.recoveryWaiting").into();
                data.detail = if self.task.title.is_empty() {
                    t("agent.reviewBeforeContinue").into()
                } else {
                    task_title.as_str().into()
                };
            }
            Phase::Draining => {
                data.busy = self.task.running || owner.active_operations > 0;
                data.title = if data.busy {
                    t("agent.stopping")
                } else {
                    t("agent.stopped")
                }
                .into();
                data.detail = t("agent.stopDetail").into();
            }
            Phase::Released => {
                data.title = t("agent.closed").into();
                data.detail = t("agent.closedDetail").into();
            }
            Phase::Ready if !self.error.is_empty() || !self.configuration_error.is_empty() => {
                data.title = t("agent.failed").into();
                data.detail = t("agent.failedDetail").into();
            }
            // A database connection holds an operation permit for its entire
            // lifetime. That permit blocks handoff, not new conversation input.
            Phase::Ready if self.task.running => {
                data.busy = true;
                data.title = if self.task.title.is_empty() {
                    t("agent.processing").into()
                } else {
                    task_title.as_str().into()
                };
                data.detail = task_detail.as_str().into();
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
                data.title = t("agent.chatNotReady").into();
                data.detail = t("agent.requirements").into();
            }
            Phase::Ready => {
                if self.task.title.is_empty() {
                    data.title = t("agent.idle").into();
                    data.detail = t("agent.describeEdit").into();
                } else {
                    // The native completion path retains only a refusal or
                    // expiry reason here; ordinary success clears its title.
                    // A human decision is an outcome, not a broken session.
                    data.action_refused = true;
                    data.title = t("agent.actionRefused").into();
                    data.detail = task_title.as_str().into();
                }
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
            return Err(t("agent.enterRequest").to_owned());
        }
        let owner = owner?;
        if owner.phase != Phase::Ready || !self.readiness.runtime_ready || self.waiting_for_ready {
            return Err(t("agent.connectAndReview"));
        }
        let data = self.data(Ok(owner));
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
        let messages: Vec<_> = self
            .messages
            .iter()
            .cloned()
            .map(|mut message| {
                message.author = role_label(message.role).into();
                let (preview, collapsible) = message_preview(message.role, message.body.as_str());
                message.preview = preview.into();
                message.collapsible = collapsible;
                message
            })
            .collect();
        if !ui.get_messages().iter().eq(messages.iter().cloned()) {
            ui.set_messages(ModelRc::from(Rc::new(VecModel::from(messages))));
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
            if let Some(session) = studio.session.as_ref() {
                for item in studio
                    .project()
                    .media
                    .iter()
                    .filter(|item| !item.placeholder && item.name.to_lowercase().contains(&query))
                    .take(8)
                {
                    rows.push(AgentSuggestion {
                        identity: format!("media:{}:{}", session.session_id(), item.id).into(),
                        label: item.name.as_str().into(),
                        detail: t("agent.projectMedia").into(),
                        thumbnail: studio
                            .media
                            .thumbs
                            .get(&item.id)
                            .cloned()
                            .unwrap_or_default(),
                        kind: match item.kind {
                            concat_project::model::MediaKind::Video => "video",
                            concat_project::model::MediaKind::Audio => "audio",
                            _ => "image",
                        }
                        .into(),
                    });
                }
                status = if rows.is_empty() {
                    t("agent.noMediaMatch")
                } else {
                    t("agent.chooseReference")
                };
                // This is a connection-status row, not a fabricated result.
                rows.push(AgentSuggestion {
                    identity: "server:unavailable".into(),
                    label: t("agent.mediaService").into(),
                    detail: t("agent.disconnected").into(),
                    thumbnail: slint::Image::default(),
                    kind: "service".into(),
                });
            } else {
                status = t("agent.openProject");
            }
        } else {
            if t("agent.commandScript").to_lowercase().contains(&query) || "script".contains(&query)
            {
                rows.push(AgentSuggestion {
                    identity: "command:script".into(),
                    label: t("agent.commandScript").into(),
                    detail: t("agent.commandScriptHelp").into(),
                    thumbnail: slint::Image::default(),
                    kind: "command".into(),
                });
            }
            if t("agent.commandMedia").to_lowercase().contains(&query) || "media".contains(&query) {
                rows.push(AgentSuggestion {
                    identity: "command:media".into(),
                    label: t("agent.commandMedia").into(),
                    detail: t("agent.commandMediaHelp").into(),
                    thumbnail: slint::Image::default(),
                    kind: "command".into(),
                });
            }
            if t("agent.commandModel").to_lowercase().contains(&query) || "model".contains(&query) {
                rows.push(AgentSuggestion {
                    identity: "command:model".into(),
                    label: t("agent.commandModel").into(),
                    detail: t("agent.commandModelHelp").into(),
                    thumbnail: slint::Image::default(),
                    kind: "command".into(),
                });
            }
            status = if rows.is_empty() {
                t("agent.noCommand")
            } else {
                t("agent.commands")
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
            .set_attachment_status(t("agent.importingReference").into());
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
            t("agent.referencePartial").into()
        } else if self.pending_attachments.is_empty() {
            "".into()
        } else {
            t("agent.importingReference").into()
        });
    }

    fn publish_references(&self, app: &App) {
        let rows = self
            .references
            .iter()
            .map(|reference| AgentSuggestion {
                identity: format!("{}:{}", reference.session, reference.id).into(),
                label: reference.name.chars().take(14).collect::<String>().into(),
                detail: t("agent.projectMedia").into(),
                thumbnail: slint::Image::default(),
                kind: "media".into(),
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
            return Err(t("agent.waitImport"));
        }
        if self.references.is_empty() {
            return Ok((Vec::new(), String::new()));
        }
        let current = studio.session.as_ref().ok_or(t("agent.openProject"))?;
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
                return Err(t("agent.referenceStale"));
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
    ui.on_model_chosen(|index| {
        Shell::with(|shell, app| {
            let selected = usize::try_from(index)
                .ok()
                .and_then(|index| shell.agent_pane.borrow().model_choices.get(index).cloned());
            let Some(selected) = selected else {
                return;
            };
            let mut studio = shell.studio.borrow_mut();
            let Some(project) = studio
                .session
                .as_ref()
                .map(|session| session.project_identity().to_owned())
            else {
                return;
            };
            // This preference affects the next explicit submission only. It
            // does not mutate the application default or a live task permit.
            let value = serde_json::to_string(&(selected.connection.as_str(), &selected.model))
                .expect("string tuple");
            studio.prefs.agent_session_models.insert(project, value);
            studio.prefs.save(&studio.host.dirs);
            app.global::<AgentUi>().set_model_index(index);
        });
        refresh_model_choices();
    });
    ui.on_model_picker_opened(|| {
        let mut connection = None;
        Shell::with(|shell, app| {
            let index = app.global::<AgentUi>().get_model_index();
            connection = usize::try_from(index).ok().and_then(|index| {
                shell
                    .agent_pane
                    .borrow()
                    .model_choices
                    .get(index)
                    .map(|choice| choice.connection.as_str().to_owned())
            });
        });
        if let Some(connection) = connection {
            crate::panes::agent_settings::refresh_directory_for_picker(&connection);
        }
    });
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
        // An actual draft edit may activate the shared runtime on desktop or
        // Android. Manual editing never reaches this callback or starts a poll.
        #[cfg(all(
            any(target_os = "windows", target_os = "android"),
            not(feature = "bk-cat-native-probe")
        ))]
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
            ui.set_attachment_status(t("agent.referenceCancelled").into());
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
            t("agent.addMedia"),
            Some((
                t("agent.mediaTypes"),
                &[
                    "mp4", "mov", "mkv", "webm", "avi", "m4v", "mp3", "wav", "aac", "m4a", "flac",
                    "ogg", "png", "jpg", "jpeg", "webp", "gif", "bmp", "tif", "tiff", "svg",
                ],
            )),
            move |studio, app, _, paths| {
                if studio
                    .session
                    .as_ref()
                    .is_none_or(|session| session.session_id() != selected_session)
                {
                    app.global::<AgentUi>()
                        .set_attachment_status(t("agent.projectChanged").into());
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
                ui.set_suggestion_status(t("agent.mediaServiceUnavailable").into());
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
                    ui.set_suggestion_status(t("agent.mediaMissing").into());
                }
            }
        });
    });
}

/// Capture the inherited default once, at the first explicit task. Later
/// application-default changes cannot replace an existing conversation choice.
pub(crate) fn capture_session_model(
    studio: &mut crate::studio::Studio,
    snapshot: &ConfigurationSnapshot,
) -> Result<ModelChannel, String> {
    let identity = studio
        .session
        .as_ref()
        .ok_or("agent.openProject")?
        .project_identity()
        .to_owned();
    let mut saved = studio.prefs.agent_session_models.get(&identity).cloned();
    let previously_saved = saved.is_some();
    let model = capture_model_choice(&mut saved, snapshot)?;
    if !previously_saved {
        studio
            .prefs
            .agent_session_models
            .insert(identity, saved.expect("captured selection"));
        studio.prefs.save(&studio.host.dirs);
    }
    Ok(model)
}

fn capture_model_choice(
    saved: &mut Option<String>,
    snapshot: &ConfigurationSnapshot,
) -> Result<ModelChannel, String> {
    let selected = resolve_session_model(saved.as_deref(), snapshot)?;
    if saved.is_none() {
        *saved = Some(
            serde_json::to_string(&(selected.id.as_str(), &selected.model)).expect("string tuple"),
        );
    }
    Ok(selected)
}

fn resolve_session_model(
    saved: Option<&str>,
    snapshot: &ConfigurationSnapshot,
) -> Result<ModelChannel, String> {
    if let Some(saved) = saved {
        let (id, model): (String, String) =
            serde_json::from_str(saved).map_err(|_| "agent.errorModelUnavailable")?;
        let id = ChannelId::parse(&id).map_err(|_| "agent.errorModelUnavailable")?;
        return snapshot
            .selected_model(&id, &model)
            .map_err(|_| "agent.errorModelUnavailable".into());
    }
    snapshot
        .active()
        .cloned()
        .ok_or_else(|| "agent.errorModelRequired".into())
}

/// Explicit configuration/project events refresh a small native projection.
/// This never contacts a provider, opens conversation storage or polls.
pub(crate) fn refresh_model_choices() {
    use std::sync::atomic::{AtomicU64, Ordering};
    static REQUEST: AtomicU64 = AtomicU64::new(0);
    let request = REQUEST.fetch_add(1, Ordering::SeqCst) + 1;
    Shell::with(|shell, _| {
        let studio = shell.studio.borrow();
        let project = studio
            .session
            .as_ref()
            .map(|session| session.project_identity().to_owned());
        let saved = project
            .as_ref()
            .and_then(|id| studio.prefs.agent_session_models.get(id))
            .cloned();
        let store = crate::panes::agent_settings::configuration_store(&studio.host.dirs);
        #[cfg(test)]
        let credential_store_fixture = shell.agent_pane.borrow().credential_store_fixture.clone();
        std::thread::spawn(move || {
            let result = (|| -> Result<_, String> {
                let snapshot = store.load().map_err(|error| error.to_string())?;
                let choices = store
                    .flattened_models(&snapshot)
                    .map_err(|error| error.to_string())?;
                let selected = resolve_session_model(saved.as_deref(), &snapshot);
                let credential = selected.as_ref().ok().map(|channel| {
                    #[cfg(test)]
                    let credentials = match &credential_store_fixture {
                        Some(store) => std::sync::Arc::clone(store),
                        None => platform_store().map_err(|error| error.to_string())?,
                    };
                    #[cfg(not(test))]
                    let credentials = platform_store().map_err(|error| error.to_string())?;
                    let origin = channel
                        .service_origin()
                        .map_err(|error| error.to_string())?;
                    let scope = CredentialScope::new(channel.id.as_str(), &origin)
                        .map_err(|error| error.to_string())?;
                    credentials
                        .status(&scope)
                        .map_err(|error| error.to_string())
                });
                Ok((choices, selected, credential))
            })();
            let _ = slint::invoke_from_event_loop(move || {
                if REQUEST.load(Ordering::SeqCst) != request {
                    return;
                }
                Shell::with(|shell, app| {
                    let current = shell
                        .studio
                        .borrow()
                        .session
                        .as_ref()
                        .map(|session| session.project_identity().to_owned());
                    if current != project {
                        return;
                    }
                    let mut pane = shell.agent_pane.borrow_mut();
                    match result {
                        Ok((choices, selected, credential)) => {
                            let ui = app.global::<AgentUi>();
                            let index = selected
                                .as_ref()
                                .ok()
                                .and_then(|selected| {
                                    choices.iter().position(|choice| {
                                        choice.connection == selected.id
                                            && choice.model == selected.model
                                    })
                                })
                                .map_or(-1, |index| index as i32);
                            ui.set_models(ModelRc::new(VecModel::from(
                                choices
                                    .iter()
                                    .map(|choice| slint::SharedString::from(choice.label.as_str()))
                                    .collect::<Vec<_>>(),
                            )));
                            ui.set_model_index(index);
                            pane.model_choices = choices;
                            if !pane.task.running && !pane.waiting_for_ready {
                                pane.readiness.calls_authorized = false;
                                pane.configuration_error.clear();
                                pane.readiness.credentials_ready =
                                    matches!(credential, Some(Ok(CredentialStatus::Present)));
                                if let Some(Err(error)) = credential {
                                    pane.configuration_error = error;
                                }
                                match selected {
                                    Ok(selected) => {
                                        pane.readiness.provider = selected.label;
                                        pane.readiness.model = selected.model;
                                    }
                                    Err(error) => {
                                        pane.readiness.provider.clear();
                                        pane.readiness.model.clear();
                                        if !pane.model_choices.is_empty() {
                                            pane.configuration_error = error;
                                        }
                                    }
                                }
                            }
                        }
                        Err(error) if !pane.task.running => {
                            pane.configuration_error = error;
                        }
                        Err(_) => {}
                    }
                    let owner = shell.agent.borrow().as_ref().map_or_else(
                        || Err(crate::i18n::t("agent.sessionUnavailable")),
                        |bridge| bridge.status(),
                    );
                    pane.publish(&app, owner);
                });
            });
        });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_facts_do_not_turn_capture_or_review_into_success() {
        let _language = crate::i18n::language_lock();
        let facts = serde_json::json!({"currentProjectStates": {
            "outcome-unknown":{"total":2,"reviewed":1,"capturedSaveWithoutReceipt":{"total":1}},
            "applied-not-saved":{"total":1}, "saved-not-notified":{"total":1},
            "notified":{"total":0}, "not-applied":{"total":1}, "unchanged":{"total":0}
        }, "pendingOtherProjects":3});
        let summary = recovery_fact_summary(&facts);
        assert!(summary.contains(&tf("recovery.outcomeUnknown", &[&2])));
        assert!(summary.contains(&tf("recovery.capturedWithoutReceipt", &[&1])));
        assert!(summary.contains(&tf("recovery.appliedNotSaved", &[&1])));
        assert!(summary.contains(&tf("recovery.otherPending", &[&3])));
        assert!(!summary.contains(&tf("recovery.notified", &[&0])));
    }

    #[test]
    fn interrupted_recovery_requires_native_resume_evidence_and_keeps_calls_ungranted() {
        let mut pane = AgentPane::default();
        let mut runtime = crate::embedded::RuntimeView {
            phase: "recovery".into(),
            loaded: true,
            snapshot: Some(
                serde_json::json!({"snapshotId":"review", "projectPath":"original",
                "hasInterruptedWork":true, "canContinueInterrupted":false,
                "canDiscardInterrupted":true, "blockedReason":"agent.errorRecoverySnapshotMissing"}),
            ),
            ..Default::default()
        };
        pane.apply_runtime(&runtime);
        assert!(!pane.can_recover);
        assert!(!pane.resume_interrupted);
        assert!(pane.can_discard);
        let snapshot = runtime.snapshot.as_mut().unwrap();
        snapshot["blockedReason"] = serde_json::Value::Null;
        snapshot["canContinueInterrupted"] = serde_json::json!(true);
        pane.apply_runtime(&runtime);
        assert!(pane.can_recover && pane.resume_interrupted);
        assert_eq!(pane.recovery_summary, "agent.recoveryResumeNotice");
        assert!(
            !pane.readiness.calls_authorized,
            "displaying the reviewed task cannot grant a call"
        );
    }

    #[test]
    fn native_control_errors_keep_codes_and_localize_recovery_actions() {
        let _language = crate::i18n::language_lock();
        let dirs = concat_host::AppDirs::under(&std::env::temp_dir().join("bkcut-error-language"));
        for code in [
            "runtime.loadTimeout",
            "runtime.waitTimeout",
            "runtime.webCapability",
            "runtime.modelSettingsBusy",
            "agent.errorRecoveryStorage",
            "agent.recoveryNoInterruptedTask",
            "agent.errorRecoveryBinding",
            "agent.errorRecoveryPending",
            "runtime.inputTooLarge",
            "runtime.unsupportedPlatform",
            "runtime.saveNotificationUnconfirmed",
            "runtime.submissionUnconfirmed",
            "runtime.approvalDeliveryFailed",
            "runtime.platformFailed",
            "script.generationFailed",
            "script.stopped",
            "permission.alreadyPending",
            "permission.stateUnavailable",
            "agent.notReady",
            "agent.configureBeforeSend",
            "agent.chatNotReady",
            "agent.reviewBeforeContinue",
            "runtime.invalidRequest",
            "permission.taskScope",
            "permission.readOnlyTask",
            "permission.editContextChanged",
            "runtime.editReceiptUnconfirmed",
            "modelSettings.configurationUnavailable",
        ] {
            crate::i18n::select("en", &dirs);
            let english = error_presentation(code);
            assert_eq!(english.0, code);
            assert_eq!(english.1, crate::i18n::t(code));
            assert_ne!(
                english.1, code,
                "a stable code must render its translation, not the protocol key"
            );
            assert!(english.2.is_empty());
            crate::i18n::select("zh-Hans", &dirs);
            let chinese = error_presentation(code);
            assert_eq!(chinese.0, english.0);
            assert_ne!(chinese.1, english.1);
            assert!(chinese.2.is_empty());
        }
        let report = error_presentation("runtime.startupFailed\nexternal diagnostic");
        assert_eq!(report.0, "runtime.startupFailed");
        assert_eq!(report.2, "external diagnostic");
        crate::i18n::select("en", &dirs);
    }

    #[test]
    fn known_product_errors_translate_without_reinterpreting_external_diagnostics() {
        let _language = crate::i18n::language_lock();
        let dirs = concat_host::AppDirs::under(
            &std::env::temp_dir().join("bkcut-product-error-projection"),
        );
        for language in ["en", "zh-Hans"] {
            crate::i18n::select(language, &dirs);
            let raw = "runtime.platformFailed\nexternal {0} / /untouched/path";
            assert_eq!(
                localize_product_message(raw),
                format!(
                    "{}\nexternal {{0}} / /untouched/path",
                    t("runtime.platformFailed")
                )
            );
            assert_eq!(
                localize_product_message("external literal {1}"),
                "external literal {1}"
            );
            assert_eq!(
                localize_product_message("runtime.submissionUnconfirmed"),
                t("runtime.submissionUnconfirmed")
            );
            assert_ne!(
                localize_product_message("runtime.submissionUnconfirmed"),
                t("runtime.stateChanged")
            );
        }
        crate::i18n::select("en", &dirs);
    }

    #[test]
    fn first_inherited_model_is_retained_after_application_default_changes() {
        use concat_agent::configuration::{ChannelDraft, ConfigurationStore, ModelProtocol};
        let root = std::env::temp_dir().join(format!(
            "bkcut-session-model-{}",
            crate::embedded::random_capability().unwrap()
        ));
        std::fs::create_dir_all(&root).unwrap();
        let store = ConfigurationStore::new(root.clone());
        let draft = |model: &str| ChannelDraft {
            label: model.into(),
            protocol: ModelProtocol::OpenAiCompletions,
            endpoint: "https://session-model-fixture.invalid/v1".into(),
            model: model.into(),
            request_timeout_ms: 1000,
            max_output_tokens: 128,
        };
        let snapshot = store
            .save_channel(&store.load().unwrap(), None, draft("model-a"))
            .unwrap();
        let a = snapshot.channels()[0].id.clone();
        let snapshot = store
            .save_channel(&snapshot, None, draft("model-b"))
            .unwrap();
        let b = snapshot.channels()[1].id.clone();
        let snapshot = store.select(&snapshot, Some(&a)).unwrap();
        let mut conversation = None;
        assert_eq!(
            capture_model_choice(&mut conversation, &snapshot)
                .unwrap()
                .model,
            "model-a"
        );
        let snapshot = store.select(&snapshot, Some(&b)).unwrap();
        assert_eq!(
            capture_model_choice(&mut conversation, &snapshot)
                .unwrap()
                .model,
            "model-a"
        );
        let mut new_conversation = None;
        assert_eq!(
            capture_model_choice(&mut new_conversation, &snapshot)
                .unwrap()
                .model,
            "model-b"
        );
        let removed = store.delete(&snapshot, &a).unwrap();
        assert!(
            capture_model_choice(&mut conversation, &removed).is_err(),
            "never silently substitute another account"
        );
        std::fs::remove_dir_all(root).unwrap();
    }
    #[test]
    fn compact_transcript_keeps_exact_text_and_exposes_full_details() {
        let (short, folded) = message_preview(AgentMessageRole::Assistant, "字幕已经添加。");
        assert!(!folded);
        assert!(short.is_empty());

        let tool_text = "{\n  \"error\": \"render failed\",\n  \"step\": 4\n}";
        let (tool_preview, tool_folded) = message_preview(AgentMessageRole::Tool, tool_text);
        assert!(tool_folded);
        assert!(tool_preview.contains("render failed"));

        let long_text = format!("已经添加标题。\n{}", "详细检查记录。".repeat(55));
        let (preview, folded) = message_preview(AgentMessageRole::Assistant, &long_text);
        assert!(folded);
        assert!(preview.starts_with("已经添加标题。"));
        assert!(preview.ends_with('…'));

        let mut pane = AgentPane::default();
        pane.apply_runtime(&crate::embedded::RuntimeView {
            presentation: Some(serde_json::json!({"messages": [
                {"role":"assistant", "text":long_text},
                {"role":"tool", "text":tool_text}
            ]})),
            ..Default::default()
        });
        assert_eq!(pane.messages.len(), 2);
        assert_eq!(pane.messages[0].body.as_str(), long_text);
        assert!(pane.messages[0].collapsible);
        assert_eq!(pane.messages[1].body.as_str(), tool_text);
        assert!(pane.messages[1].collapsible);
    }

    #[test]
    fn compaction_display_uses_localized_system_preview_without_changing_history() {
        let _language = crate::i18n::language_lock();
        let previous = crate::i18n::current();
        let dirs = concat_host::AppDirs::under(
            &std::env::temp_dir().join("bkcut-compaction-display-language"),
        );
        let original = "The conversation history before this point was compacted into the following summary:\n\n<summary>\nHistorical body 中文 {1}\n</summary>";
        let runtime = crate::embedded::RuntimeView {
            presentation: Some(serde_json::json!({"messages": [
                {"role":"compaction", "text":original},
                {"role":"user", "text":original},
                {"role":"compaction", "text":"Short original summary"}
            ]})),
            ..Default::default()
        };
        let persisted_projection = runtime.presentation.clone();
        let mut labels = Vec::new();
        for language in ["en", "zh-Hans"] {
            crate::i18n::select(language, &dirs);
            let mut pane = AgentPane::default();
            pane.apply_runtime(&runtime);
            assert_eq!(pane.messages.len(), 3);
            let summary = &pane.messages[0];
            assert_eq!(summary.role, AgentMessageRole::Compaction);
            assert_eq!(summary.author.as_str(), t("agent.roleCompaction"));
            assert_eq!(summary.preview.as_str(), t("agent.compactionPreview"));
            assert_eq!(summary.body.as_str(), original);
            assert!(summary.collapsible && !summary.pending);
            assert_eq!(pane.messages[1].role, AgentMessageRole::User);
            assert_eq!(pane.messages[1].author.as_str(), t("agent.roleUser"));
            assert_eq!(pane.messages[1].body.as_str(), original);
            assert!(!pane.messages[1].collapsible);
            assert!(pane.messages[2].collapsible);
            assert_eq!(
                pane.messages[2].preview.as_str(),
                t("agent.compactionPreview")
            );
            assert_eq!(pane.messages[2].body.as_str(), "Short original summary");
            assert_eq!(runtime.presentation, persisted_projection);
            labels.push((summary.author.to_string(), summary.preview.to_string()));
        }
        assert_ne!(labels[0], labels[1]);
        crate::i18n::select(&previous, &dirs);
    }

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
    fn clean_startup_accepts_only_explicit_waiting_intent_without_model_authority() {
        let mut pane = configured();
        pane.readiness.runtime_ready = false;
        pane.readiness.calls_authorized = false;
        assert!(pane.data(owner(Phase::RecoveryRequired)).can_send);
        assert!(
            pane.validate_waiting_submission(owner(Phase::RecoveryRequired), "首次发送")
                .is_ok()
        );
        assert!(
            pane.validate_submission(owner(Phase::RecoveryRequired), "首次发送")
                .is_err()
        );
        assert!(!pane.readiness.calls_authorized);
        for phase in ["new", "acquiring"] {
            pane.apply_runtime(&crate::embedded::RuntimeView {
                loaded: phase == "acquiring",
                phase: phase.into(),
                ..Default::default()
            });
            assert!(pane.data(owner(Phase::RecoveryRequired)).can_send);
            assert!(
                pane.validate_waiting_submission(owner(Phase::RecoveryRequired), "first send")
                    .is_ok()
            );
            assert!(!pane.readiness.calls_authorized);
        }
        for phase in ["recovery", "activating"] {
            pane.apply_runtime(&crate::embedded::RuntimeView {
                loaded: true,
                phase: phase.into(),
                ..Default::default()
            });
            let status = pane.data(owner(Phase::RecoveryRequired));
            assert!(!status.can_send, "{phase} without a snapshot");
            assert_eq!(status.can_review, phase == "recovery");
            assert!(!status.can_continue && !status.can_discard);
            assert!(
                pane.validate_waiting_submission(owner(Phase::RecoveryRequired), "first send")
                    .is_err()
            );
            assert!(!pane.readiness.calls_authorized);
            pane.apply_runtime(&crate::embedded::RuntimeView {
                loaded: true,
                phase: phase.into(),
                snapshot: Some(serde_json::json!({
                    "snapshotId": "clean-native-snapshot",
                    "projectPath": "isolated-project",
                    "hasInterruptedWork": false,
                    "blockedReason": null
                })),
                ..Default::default()
            });
            assert!(pane.data(owner(Phase::RecoveryRequired)).can_send);
            assert!(
                pane.validate_waiting_submission(owner(Phase::RecoveryRequired), "first send")
                    .is_ok()
            );
            assert!(
                pane.validate_submission(owner(Phase::RecoveryRequired), "first send")
                    .is_err()
            );
            assert!(!pane.readiness.calls_authorized);
        }
        pane.apply_runtime(&crate::embedded::RuntimeView {
            phase: "loading".into(),
            ..Default::default()
        });
        assert!(pane.data(owner(Phase::RecoveryRequired)).can_send);
        pane.apply_runtime(&crate::embedded::RuntimeView {
            phase: "loading".into(),
            waiting_for_ready: true,
            ..Default::default()
        });
        let status = pane.data(owner(Phase::RecoveryRequired));
        assert!(status.busy && status.can_stop && !status.can_send);
        assert!(!pane.task.running && !pane.readiness.calls_authorized);
        assert!(!status.can_review && !status.can_continue && !status.can_discard);
        assert!(
            pane.validate_waiting_submission(owner(Phase::RecoveryRequired), "重复发送")
                .is_err()
        );
        pane.apply_runtime(&crate::embedded::RuntimeView {
            phase: "recovery".into(),
            snapshot: Some(
                serde_json::json!({"hasInterruptedWork":true,"blockedReason":"interrupted"}),
            ),
            ..Default::default()
        });
        assert!(!pane.data(owner(Phase::RecoveryRequired)).can_send);
        assert!(
            pane.validate_waiting_submission(owner(Phase::RecoveryRequired), "不得继续旧任务")
                .is_err()
        );
    }

    #[test]
    fn startup_waiting_does_not_weaken_configuration_failure_or_retirement_gates() {
        for fault in [
            "native",
            "configuration",
            "credential",
            "provider",
            "model",
            "task",
        ] {
            let mut pane = configured();
            pane.readiness.runtime_ready = false;
            match fault {
                "native" => pane.error = "native failure".into(),
                "configuration" => pane.configuration_error = "invalid settings".into(),
                "credential" => pane.readiness.credentials_ready = false,
                "provider" => pane.readiness.provider.clear(),
                "model" => pane.readiness.model.clear(),
                "task" => pane.task.running = true,
                _ => unreachable!(),
            }
            assert!(
                pane.validate_waiting_submission(owner(Phase::RecoveryRequired), "发送")
                    .is_err(),
                "{fault}"
            );
            assert!(
                !pane.data(owner(Phase::RecoveryRequired)).can_send,
                "{fault}"
            );
        }
        let mut pane = configured();
        pane.readiness.runtime_ready = false;
        for phase in [Phase::Draining, Phase::Released] {
            assert!(
                pane.validate_waiting_submission(owner(phase), "停止后不得偷偷重连")
                    .is_err()
            );
            assert!(!pane.data(owner(phase)).can_send);
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
    fn recovery_summary_tracks_live_recovery_without_hiding_current_errors() {
        let mut pane = configured();
        let reason = "检测到未完成任务。可明确放弃当前工程已登记的旧任务后打开会话；不会恢复模型调用，已有消息保留。";
        let snapshot = serde_json::json!({
            "snapshotId":"native-review", "projectPath":"native-project",
            "schemaVersion":2, "hasInterruptedWork":true,
            "canDiscardInterrupted":true, "blockedReason":reason,
        });
        let mut runtime = crate::embedded::RuntimeView {
            loaded: true,
            phase: "recovery".into(),
            snapshot: Some(snapshot.clone()),
            ..crate::embedded::RuntimeView::default()
        };
        pane.apply_runtime(&runtime);
        let recovery = pane.data(owner(Phase::RecoveryRequired));
        assert_eq!(recovery.recovery_summary, reason);
        assert!(recovery.can_review && recovery.can_discard);
        assert!(!recovery.can_send);

        // A delayed runtime publication must not override current ownership.
        for phase in [Phase::Ready, Phase::Draining, Phase::Released] {
            let current = pane.data(owner(phase));
            assert!(current.recovery_summary.is_empty());
            assert!(!current.can_review && !current.can_discard);
        }
        let unavailable = pane.data(Err("会话所有权已失效。".into()));
        assert!(unavailable.recovery_summary.is_empty());
        assert_eq!(unavailable.error_code, "agent.errorUnexpected");
        assert_eq!(unavailable.error_details, "会话所有权已失效。");

        // Reproduce discard -> activation -> a new task, retaining the exact
        // old snapshot used by native project binding throughout the sequence.
        runtime.phase = "activating".into();
        pane.apply_runtime(&runtime);
        assert!(
            pane.data(owner(Phase::RecoveryRequired))
                .recovery_summary
                .is_empty()
        );
        runtime.phase = "ready".into();
        pane.apply_runtime(&runtime);
        let ready = pane.data(owner(Phase::Ready));
        assert!(ready.recovery_summary.is_empty());
        assert!(ready.can_send);
        assert!(!ready.can_review && !ready.can_discard);
        assert!(
            pane.validate_submission(owner(Phase::Ready), "新增验收文字")
                .is_ok()
        );

        runtime.task_running = true;
        runtime.task_title = "正在剪辑当前工程".into();
        pane.apply_runtime(&runtime);
        let running = pane.data(owner(Phase::Ready));
        assert!(running.recovery_summary.is_empty());
        assert!(running.busy);
        assert!(!running.can_send);
        assert_eq!(running.title, "正在剪辑当前工程");
        assert_eq!(runtime.snapshot.as_ref(), Some(&snapshot));

        runtime.error = "原生工程状态核对失败。".into();
        pane.apply_runtime(&runtime);
        let failed = pane.data(owner(Phase::Ready));
        assert_eq!(failed.error_details, runtime.error.as_str());
        assert_eq!(failed.title, t("agent.failed"));
        assert!(failed.recovery_summary.is_empty());
        assert!(!failed.can_send);

        runtime.phase = "recovery".into();
        runtime.task_running = false;
        runtime.snapshot.as_mut().unwrap()["blockedReason"] =
            serde_json::json!("当前工程版本已变化，请重新核对。");
        pane.apply_runtime(&runtime);
        let recovery = pane.data(owner(Phase::RecoveryRequired));
        assert_eq!(
            recovery.recovery_summary,
            "当前工程版本已变化，请重新核对。"
        );
        assert_eq!(recovery.error_details, runtime.error.as_str());
        assert!(recovery.can_review);
        assert!(!recovery.can_send);
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
    fn refused_or_expired_edit_remains_visible_without_blocking_a_new_request() {
        for reason in [
            "用户拒绝了本次剪辑操作，工程未修改。",
            "等待剪辑批准超时，工程未修改。",
        ] {
            let mut pane = configured();
            pane.apply_runtime(&crate::embedded::RuntimeView {
                loaded: true,
                phase: "ready".into(),
                task_running: false,
                task_title: reason.into(),
                ..crate::embedded::RuntimeView::default()
            });
            let data = pane.data(owner(Phase::Ready));
            assert_eq!(data.title, t("agent.actionRefused"));
            assert_eq!(data.detail, reason);
            assert!(data.error.is_empty());
            assert!(!data.busy);
            assert!(!data.has_progress);
            assert!(data.can_send);
            assert!(
                pane.validate_submission(owner(Phase::Ready), "重新规划本次剪辑")
                    .is_ok()
            );
            // Native refusal feedback never overrides ownership or errors.
            assert!(!pane.data(owner(Phase::RecoveryRequired)).can_send);
            pane.error = "原生工程状态核对失败。".into();
            let failed = pane.data(owner(Phase::Ready));
            assert_eq!(failed.title, t("agent.failed"));
            assert!(!failed.can_send);
        }
    }

    #[test]
    fn successful_completion_clears_the_running_title_instead_of_showing_refusal() {
        let mut pane = configured();
        let mut runtime = crate::embedded::RuntimeView {
            loaded: true,
            phase: "ready".into(),
            task_running: true,
            task_title: "正在剪辑".into(),
            ..crate::embedded::RuntimeView::default()
        };
        pane.apply_runtime(&runtime);
        let running = pane.data(owner(Phase::Ready));
        assert!(running.busy);
        assert_eq!(running.title, "正在剪辑");
        assert!(!running.can_send);

        runtime.task_running = false;
        runtime.task_title.clear();
        pane.apply_runtime(&runtime);
        let finished = pane.data(owner(Phase::Ready));
        assert_eq!(finished.title, t("agent.idle"));
        assert!(finished.can_send);
        assert!(!finished.busy);
        assert!(finished.error.is_empty());
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
