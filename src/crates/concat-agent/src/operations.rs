// SPDX-License-Identifier: AGPL-3.0-or-later
//! Durable native edit intents, receipts, project-save evidence and delivery.
//!
//! A Pi tool call has a stable, original conversation/task/call identity.
//! Only the trusted runtime transport supplies it; the model's tool arguments
//! cannot choose one. Every step is a separate SQLite FULL-synchronous commit.
//! An intent without a receipt is ambiguous after any crash, including a crash
//! *before* the project was touched. Reopening only inspects; it never replays.
//! This is deliberately separate from Pi's original session database.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs::{self, File};
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use rusqlite::{Connection, OpenFlags, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{OperationPermit, OwnerToken, SessionOwner};

/// Fixed application-owned journal filename inside the exclusive storage root.
pub const OPERATIONS_FILE_NAME: &str = "native-operations.sqlite3";

/// Trusted identities from the original Pi tool execution context.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Invocation {
    /// Original durable conversation identity.
    pub conversation_id: u64,
    /// Original durable task identity.
    pub task_id: u64,
    /// Original stable call identity, also valid for nested calls.
    pub call_id: String,
}

impl Invocation {
    fn validate(&self) -> Result<(), JournalError> {
        if self.conversation_id > 9_007_199_254_740_991
            || self.task_id > 9_007_199_254_740_991
            || self.call_id.is_empty()
            || self.call_id.len() > 512
            || self.call_id.chars().any(char::is_control)
        {
            return Err(JournalError::InvalidInvocation);
        }
        Ok(())
    }
}

/// A new operation may edit exactly once; duplicates never edit again.
#[derive(Debug, Eq, PartialEq)]
pub enum Admission {
    /// Intent durably committed; the caller may perform the native edit once.
    New(u64),
    /// The identical call was already applied; return its durable receipt.
    AlreadyApplied {
        /// Original durable operation identity.
        operation_id: u64,
        /// Previously persisted native editor receipt.
        receipt: Value,
    },
    /// This call was conclusively refused without modifying the project.
    PreviouslyRejected {
        /// The durable identity is retained, so the call cannot run later.
        operation_id: u64,
        /// The original native refusal, not an invented edit receipt.
        error: Value,
    },
}

/// Save evidence created by this live journal, awaiting actual page delivery.
#[derive(Clone, Debug, PartialEq)]
pub struct SavedOperation {
    /// Durable native operation identity.
    pub operation_id: u64,
    /// The same verified disk bytes recorded in the journal.
    pub saved: Value,
}

/// A read-only summary. It does not authorize replay or recovery.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, Serialize)]
pub struct JournalInspection {
    /// Intent exists with no durable edit receipt.
    pub needs_project_reconciliation: u64,
    /// An edit receipt exists but its matching disk save is unproven.
    pub applied_not_saved: u64,
    /// Disk save was verified, but a completion notification was not acked.
    pub saved_not_notified: u64,
    /// Completed operations that received a native notification ack.
    pub notified: u64,
    /// Proven zero-change native results, requiring neither save nor replay.
    pub not_applied: u64,
}

/// Facts stay separate from a user's decision to continue or discard a task.
/// In particular, acknowledging an unknown operation never makes it applied.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecoveryState {
    /// A durable intent exists but no conclusive native outcome survived.
    OutcomeUnknown,
    /// Native application was recorded; its exact disk save was not proven.
    AppliedNotSaved,
    /// A verified exact save exists; delivery was not acknowledged.
    SavedNotNotified,
    /// The completion notification was acknowledged after a verified save.
    Notified,
    /// The native host conclusively refused the operation without changes.
    NotApplied,
    /// The native host conclusively reported an unchanged project version.
    Unchanged,
}

/// One original invocation, with its durable native facts left intact.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct RecoveryOperation {
    /// Durable journal identity; it is never a permission to replay an edit.
    pub operation_id: u64,
    /// Original trusted conversation, task and tool call identity.
    pub invocation: Invocation,
    /// Exact native target stored before the original attempt.
    pub project_path: String,
    /// Whether this attempt belongs to the project under native review.
    pub current_project: bool,
    /// Observed native facts, independent of an acknowledgement of review.
    pub state: RecoveryState,
    /// Native editor session captured before the original attempt.
    pub session: u64,
    /// Version the original command expected to modify.
    pub expected_revision: u64,
    /// Changed version only when a matching native receipt survived.
    pub result_revision: Option<u64>,
    /// Original stored native receipt; not a save acknowledgement.
    pub receipt: Option<Value>,
    /// Original verified save evidence, never synthesized by recovery.
    pub saved: Option<Value>,
    /// Exact bytes of a previously verified save still occupy the file.
    pub current_file_matches_save: bool,
    /// Captures made while this operation was pending. Without a receipt,
    /// these prove saved documents, never this particular operation's success.
    pub saved_document_candidates: Vec<Value>,
    /// These exact facts were explicitly reviewed in an admitted generation.
    pub reviewed: bool,
    /// Digest binding the original row and all its captured-save candidates.
    pub fact_fingerprint: String,
}

/// A read-only, owner-bound snapshot. Constructed only by native inspection;
/// the UI receives `facts()`, not a credential it can construct or edit.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct RecoveryReview {
    /// Project selected by the current native host, not by model arguments.
    pub project_path: String,
    /// Digest of the complete journal facts, owner and current project file.
    pub fingerprint: String,
    /// Original counts remain truthful even after explicit reconciliation.
    pub inspection: JournalInspection,
    /// Complete native history; display pagination never truncates this set.
    pub operations: Vec<RecoveryOperation>,
    /// Current-project uncertain or undelivered facts not explicitly reviewed.
    pub unreviewed_current: u64,
    /// Such unreviewed facts belonging to another project; these block consent.
    pub pending_other_projects: u64,
    generation: u64,
    storage_root: PathBuf,
    database: RecoveryDatabase,
    project_file: ProjectFileEvidence,
}

impl RecoveryReview {
    fn state_counts(&self, current_project: bool) -> Value {
        let states = [
            ("outcome-unknown", RecoveryState::OutcomeUnknown),
            ("applied-not-saved", RecoveryState::AppliedNotSaved),
            ("saved-not-notified", RecoveryState::SavedNotNotified),
            ("notified", RecoveryState::Notified),
            ("not-applied", RecoveryState::NotApplied),
            ("unchanged", RecoveryState::Unchanged),
        ];
        Value::Object(
            states
                .into_iter()
                .map(|(key, state)| {
                    let mut total = 0_u64;
                    let mut reviewed = 0_u64;
                    let mut capture_total = 0_u64;
                    let mut capture_reviewed = 0_u64;
                    for operation in self.operations.iter().filter(|operation| {
                        operation.current_project == current_project && operation.state == state
                    }) {
                        total += 1;
                        reviewed += u64::from(operation.reviewed);
                        if operation.receipt.is_none()
                            && !operation.saved_document_candidates.is_empty()
                        {
                            capture_total += 1;
                            capture_reviewed += u64::from(operation.reviewed);
                        }
                    }
                    (
                        key.to_owned(),
                        serde_json::json!({
                            "total": total, "reviewed": reviewed, "unreviewed": total - reviewed,
                            "requiresReview": needs_review(state),
                            "capturedSaveWithoutReceipt": {
                                "total": capture_total, "reviewed": capture_reviewed,
                                "unreviewed": capture_total - capture_reviewed,
                                "provesOperation": false
                            }
                        }),
                    )
                })
                .collect(),
        )
    }

    /// Stable native facts for review and for the next newly authorized task.
    /// Captured project documents stay native; only their checked hashes and
    /// exact version identities are sent to the runtime.
    pub fn facts(&self) -> Value {
        // Pagination is a presentation bound, never a history or execution
        // quota. Native comparison and reconciliation still cover every row.
        // Raw receipts/commands can be large and never enter this summary.
        let pending = self.unreviewed_current + self.pending_other_projects;
        let details: Vec<_> = self.operations.iter().rev()
            .filter(|operation| !operation.reviewed && needs_review(operation.state)).take(32)
            .map(|operation| serde_json::json!({
                "operationId": operation.operation_id.to_string(),
                "conversationId": operation.invocation.conversation_id.to_string(),
                "taskId": operation.invocation.task_id.to_string(),
                "callId": operation.invocation.call_id,
                "currentProject": operation.current_project,
                "projectPathPreview": operation.project_path.chars().take(256).collect::<String>(),
                "projectPathIsComplete": operation.project_path.chars().count() <= 256,
                "state": operation.state,
                "session": operation.session.to_string(),
                "expectedRevision": operation.expected_revision.to_string(),
                "resultRevision": operation.result_revision.map(|revision| revision.to_string()),
                "hasNativeReceipt": operation.receipt.is_some(),
                "hasVerifiedSave": operation.saved.is_some(),
                "currentFileMatchesSave": operation.current_file_matches_save,
                "capturedDocumentCount": operation.saved_document_candidates.len(),
                "captureAloneProvesOperation": false,
                "factFingerprint": operation.fact_fingerprint,
            })).collect();
        serde_json::json!({
            "schema": 1,
            "projectPath": self.project_path,
            "fingerprint": self.fingerprint,
            "inspection": self.inspection,
            "operations": details,
            "operationDetails": {"scope":"unreviewed","order":"newest-first","total":pending,
                "shown":details.len(),"remaining":pending.saturating_sub(details.len() as u64)},
            "unreviewedCurrent": self.unreviewed_current,
            "pendingOtherProjects": self.pending_other_projects,
            "currentProjectFile": self.project_file,
            "currentProjectStates": self.state_counts(true),
            "otherProjectStates": self.state_counts(false),
            "replayPermitted": false,
        })
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct OperationRow {
    id: u64,
    invocation: Invocation,
    request: Value,
    project: String,
    session: u64,
    expected_revision: u64,
    receipt: Option<Value>,
    result_session: Option<u64>,
    result_revision: Option<u64>,
    saved: Option<Value>,
    notified: bool,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct CapturedSave {
    project: String,
    generation: u64,
    session: u64,
    revision: u64,
    evidence: Value,
    document: Value,
    candidates: BTreeSet<u64>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct ReviewRecord {
    operation_id: u64,
    generation: u64,
    fact_fingerprint: String,
    review_fingerprint: String,
    facts: Value,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
struct RecoveryDatabase {
    operations: Vec<OperationRow>,
    saves: Vec<CapturedSave>,
    reviews: Vec<ReviewRecord>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
#[serde(rename_all = "camelCase")]
struct ProjectFileEvidence {
    present: bool,
    bytes: u64,
    sha256: String,
    document_sha256: String,
}

/// Every journal error is fail-closed. None allows automatic replay.
#[derive(Debug)]
pub enum JournalError {
    /// Ownership or generation was not admitted.
    Owner(crate::Error),
    /// A real database operation failed.
    Database(rusqlite::Error),
    /// A real project file could not be read.
    Io(io::Error),
    /// Native invocation was malformed.
    InvalidInvocation,
    /// A durable invocation identity was reused with different arguments.
    ConflictingInvocation,
    /// An existing intent needs current-project reconciliation.
    NeedsReconciliation {
        /// The unresolved durable intent to inspect against the current project.
        operation_id: u64,
    },
    /// A lifecycle step was out of order, absent or repeated inconsistently.
    InvalidTransition,
    /// Disk evidence did not match the current native project target.
    InvalidSaveEvidence,
    /// Review no longer describes this owner, journal or current file.
    StaleRecoveryReview,
    /// Unreviewed operations belong to another project and cannot be handled
    /// by opening or approving the currently selected project.
    OtherProjectPending,
    /// Native journal state was poisoned.
    Poisoned,
    /// Existing journal schema was unexpected. It was not migrated.
    InvalidSchema,
}

impl fmt::Display for JournalError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Owner(error) => write!(f, "native journal owner refused: {error}"),
            Self::Database(error) => write!(f, "native journal database failed: {error}"),
            Self::Io(error) => write!(f, "native project save evidence failed: {error}"),
            Self::InvalidInvocation => f.write_str("invalid trusted Pi invocation identity"),
            Self::ConflictingInvocation => {
                f.write_str("an invocation identity already has different arguments")
            }
            Self::NeedsReconciliation { operation_id } => write!(
                f,
                "operation {operation_id} has an ambiguous outcome; reconcile the current project before new work"
            ),
            Self::InvalidTransition => {
                f.write_str("native operation state cannot make this transition")
            }
            Self::InvalidSaveEvidence => {
                f.write_str("saved project evidence did not match the native target")
            }
            Self::StaleRecoveryReview => {
                f.write_str("native recovery evidence changed; inspect again")
            }
            Self::OtherProjectPending => {
                f.write_str("unreviewed native operations belong to another project")
            }
            Self::Poisoned => f.write_str("native operation journal was poisoned"),
            Self::InvalidSchema => {
                f.write_str("native operation journal schema is not the reviewed version")
            }
        }
    }
}

impl std::error::Error for JournalError {}
impl From<crate::Error> for JournalError {
    fn from(error: crate::Error) -> Self {
        Self::Owner(error)
    }
}
impl From<rusqlite::Error> for JournalError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Database(error)
    }
}
impl From<io::Error> for JournalError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

/// A live journal, opened only after native user recovery approval.
///
/// SQLite is dropped first, then its permit. Holding the permit keeps the
/// process ownership lock until every native connection has actually closed.
pub struct OperationJournal {
    connection: Mutex<Connection>,
    transient: Mutex<Transient>,
    permit: OperationPermit,
    storage_root: PathBuf,
}

#[derive(Default)]
struct Transient {
    current_operations: BTreeSet<u64>,
    notifications: Vec<SavedOperation>,
}

impl OperationJournal {
    /// Open the fixed journal for newly approved work. Existing journals are
    /// validated before the reviewed additive evidence tables are created.
    /// Existing operation rows, receipts and project files are never rewritten.
    pub fn open(owner: &SessionOwner, token: &OwnerToken) -> Result<Self, JournalError> {
        let permit = owner.begin(token)?;
        let path = journal_path(owner.storage_root())?;
        let connection = permit.with_valid(|| open_connection(&path))??;
        Ok(Self {
            connection: Mutex::new(connection),
            transient: Mutex::new(Transient::default()),
            permit,
            storage_root: owner.storage_root().to_owned(),
        })
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, JournalError> {
        self.connection.lock().map_err(|_| JournalError::Poisoned)
    }

    fn require_current_operation(&self, operation_id: u64) -> Result<(), JournalError> {
        if !self
            .transient
            .lock()
            .map_err(|_| JournalError::Poisoned)?
            .current_operations
            .contains(&operation_id)
        {
            return Err(JournalError::InvalidTransition);
        }
        Ok(())
    }

    /// Persist the exact planned request before the native window can mutate.
    /// An identical pending call is *always* ambiguous, even if no edit ran.
    pub fn prepare(
        &self,
        invocation: &Invocation,
        request: &Value,
        project_path: &str,
        expected_session: u64,
        expected_revision: u64,
    ) -> Result<Admission, JournalError> {
        invocation.validate()?;
        if project_path.trim().is_empty()
            || request.get("path").and_then(Value::as_str) != Some(project_path)
            || request.get("expectedSession").and_then(Value::as_u64) != Some(expected_session)
            || request.get("expectedRevision").and_then(Value::as_u64) != Some(expected_revision)
            || request.get("method").and_then(Value::as_str) != Some("edit.applyGuarded")
        {
            return Err(JournalError::InvalidInvocation);
        }
        let request =
            serde_json::to_string(request).map_err(|_| JournalError::InvalidInvocation)?;
        let connection = self.connection()?;
        self.permit.with_valid(|| -> Result<Admission, JournalError> {
            let inserted = connection.execute(
                "INSERT INTO operations (conversation,task,call,request,project,session,expected_revision) \
                 VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(conversation,task,call) DO NOTHING",
                params![invocation.conversation_id.to_string(), invocation.task_id.to_string(),
                    invocation.call_id, request, project_path, expected_session.to_string(),
                    expected_revision.to_string()],
            )?;
            let (id, prior_request, receipt): (i64, String, Option<String>) = connection.query_row(
                "SELECT id,request,receipt FROM operations WHERE conversation=?1 AND task=?2 AND call=?3",
                params![invocation.conversation_id.to_string(), invocation.task_id.to_string(),
                    invocation.call_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let id = u64::try_from(id).map_err(|_| JournalError::InvalidSchema)?;
            if prior_request != request { return Err(JournalError::ConflictingInvocation); }
            if inserted == 1 {
                self.transient.lock().map_err(|_| JournalError::Poisoned)?
                    .current_operations.insert(id);
                return Ok(Admission::New(id));
            }
            match receipt {
                Some(receipt) => {
                    let receipt: Value = serde_json::from_str(&receipt)
                        .map_err(|_| JournalError::InvalidSchema)?;
                    if receipt.get("nativeOutcome").and_then(Value::as_str) == Some("not-applied") {
                        Ok(Admission::PreviouslyRejected {
                            operation_id: id,
                            error: receipt.get("error").cloned().ok_or(JournalError::InvalidSchema)?,
                        })
                    } else if receipt.get("nativeOutcome").and_then(Value::as_str) == Some("unchanged") {
                        Ok(Admission::AlreadyApplied {
                            operation_id: id,
                            receipt: receipt.get("reply").cloned().ok_or(JournalError::InvalidSchema)?,
                        })
                    } else {
                        Ok(Admission::AlreadyApplied { operation_id: id, receipt })
                    }
                },
                None => Err(JournalError::NeedsReconciliation { operation_id: id }),
            }
        })?
    }

    /// Persist the authoritative editor reply after the one native mutation.
    /// The reply is not a disk-save acknowledgment.
    pub fn record_receipt(
        &self,
        operation_id: u64,
        session: u64,
        revision: u64,
        reply: &Value,
    ) -> Result<(), JournalError> {
        if reply.get("sessionId").and_then(Value::as_u64) != Some(session)
            || reply.get("revision").and_then(Value::as_u64) != Some(revision)
        {
            return Err(JournalError::InvalidTransition);
        }
        let id = i64::try_from(operation_id).map_err(|_| JournalError::InvalidTransition)?;
        let reply = serde_json::to_string(reply).map_err(|_| JournalError::InvalidTransition)?;
        let connection = self.connection()?;
        self.permit.with_valid(|| -> Result<(), JournalError> {
            let mut transient = self.transient.lock().map_err(|_| JournalError::Poisoned)?;
            if !transient.current_operations.contains(&operation_id) {
                return Err(JournalError::InvalidTransition);
            }
            let transaction = connection.unchecked_transaction()?;
            let (project, expected_revision): (String, String) = transaction.query_row(
                "SELECT project,expected_revision FROM operations WHERE id=?1 AND session=?2 AND receipt IS NULL",
                params![id, session.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
            ).optional()?.ok_or(JournalError::InvalidTransition)?;
            if revision <= number(&expected_revision)? { return Err(JournalError::InvalidTransition); }
            // Exact candidate operation identity prevents reuse of a previous
            // process's save when native session/revision counters repeat.
            let saved = captured_saves(&transaction, Some((&project, session, revision)))?.into_iter().find(|save|
                save.project == project && save.session == session && save.revision == revision
                    && save.candidates.contains(&operation_id)
            ).map(|save| save.evidence.to_string());
            let changed = transaction.execute(
                "UPDATE operations SET receipt=?1, result_session=?2, result_revision=?3, saved=?4 \
                 WHERE id=?5 AND receipt IS NULL",
                params![reply, session.to_string(), revision.to_string(), saved, id],
            )?;
            if changed != 1 { return Err(JournalError::InvalidTransition); }
            transaction.commit()?;
            if let Some(saved) = saved { enqueue_saved(&mut transient, operation_id, &saved)?; }
            Ok(())
        })?
    }

    /// Return the authoritative native editor receipt only for the original
    /// Pi invocation and the exact project/session/revision it was allowed to
    /// edit. The caller must compare the public tool reply against the
    /// serialized/translated form of this receipt before reporting success.
    pub fn changed_receipt(
        &self,
        invocation: &Invocation,
        project_path: &str,
        expected_session: u64,
        expected_revision: u64,
    ) -> Result<Option<Value>, JournalError> {
        invocation.validate()?;
        let connection = self.connection()?;
        self.permit.with_valid(|| -> Result<Option<Value>, JournalError> {
            let record: Option<(String, String, String, String)> =
                connection.query_row(
                    "SELECT expected_revision,result_session,result_revision,receipt FROM operations \
                     WHERE conversation=?1 AND task=?2 AND call=?3 AND project=?4 AND session=?5 \
                     AND result_session IS NOT NULL AND result_revision IS NOT NULL AND receipt IS NOT NULL",
                    params![invocation.conversation_id.to_string(), invocation.task_id.to_string(),
                        invocation.call_id, project_path, expected_session.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                ).optional()?;
            let Some((original_revision, session, revision, receipt)) = record else {
                return Ok(None);
            };
            let revision = revision.parse::<u64>().map_err(|_| JournalError::InvalidSchema)?;
            let receipt: Value = serde_json::from_str(&receipt).map_err(|_| JournalError::InvalidSchema)?;
            let valid = original_revision == expected_revision.to_string()
                && session == expected_session.to_string()
                && revision > expected_revision
                && receipt.get("sessionId").and_then(Value::as_u64) == Some(expected_session)
                && receipt.get("revision").and_then(Value::as_u64) == Some(revision);
            Ok(valid.then_some(receipt))
        })?
    }

    /// Set a separate terminal receipt only for a proven zero-change native
    /// refusal. Busy, failed, or unknown outcomes remain ambiguous intents.
    /// No saved/notified state is invented for an edit that never happened.
    pub fn record_rejected(&self, operation_id: u64, error: &Value) -> Result<(), JournalError> {
        if !matches!(
            error.get("code").and_then(Value::as_str),
            Some("refused" | "notOpen" | "invalid" | "cancelled")
        ) {
            return Err(JournalError::InvalidTransition);
        }
        let id = i64::try_from(operation_id).map_err(|_| JournalError::InvalidTransition)?;
        let receipt = serde_json::json!({"nativeOutcome":"not-applied","error":error}).to_string();
        let connection = self.connection()?;
        self.permit.with_valid(|| -> Result<(), JournalError> {
            self.require_current_operation(operation_id)?;
            let changed = connection.execute(
                "UPDATE operations SET receipt=?1 WHERE id=?2 AND receipt IS NULL",
                params![receipt, id],
            )?;
            if changed != 1 {
                return Err(JournalError::InvalidTransition);
            }
            Ok(())
        })?
    }

    /// The current native editor explicitly returned the same session and
    /// revision as this intent: the command was a no-op, so no save is due.
    pub fn record_unchanged(
        &self,
        operation_id: u64,
        session: u64,
        revision: u64,
        reply: &Value,
    ) -> Result<(), JournalError> {
        if reply.get("sessionId").and_then(Value::as_u64) != Some(session)
            || reply.get("revision").and_then(Value::as_u64) != Some(revision)
        {
            return Err(JournalError::InvalidTransition);
        }
        let id = i64::try_from(operation_id).map_err(|_| JournalError::InvalidTransition)?;
        let receipt = serde_json::json!({"nativeOutcome":"unchanged","reply":reply}).to_string();
        let connection = self.connection()?;
        self.permit.with_valid(|| -> Result<(), JournalError> {
            self.require_current_operation(operation_id)?;
            let changed = connection.execute(
                "UPDATE operations SET receipt=?1 WHERE id=?2 AND receipt IS NULL AND session=?3 AND expected_revision=?4",
                params![receipt,id,session.to_string(),revision.to_string()],
            )?;
            if changed != 1 { return Err(JournalError::InvalidTransition); }
            Ok(())
        })?
    }

    /// Record only a verified, matching, nonempty native project save.
    ///
    /// The host calls this only after its real save result reports Written.
    /// A later revision cannot prove an earlier operation survived undo, so
    /// only the exact editor revision is credited without further replay.
    pub fn record_saved(
        &self,
        project_path: &str,
        session: u64,
        revision: u64,
    ) -> Result<u64, JournalError> {
        self.record_saved_checked(project_path, session, revision, None)
    }

    /// Product save callbacks must supply the exact document captured by the
    /// native save task. Validation and hashing use the same file bytes, so a
    /// late Written callback cannot credit a different document now at the path.
    /// A save that wins the race before its edit receipt yields zero rows and
    /// durably retains the complete captured document and exact pending native
    /// identities. A later matching receipt atomically joins both facts. A crash
    /// preserves the capture but never invents a missing operation receipt.
    pub fn record_saved_document(
        &self,
        project_path: &str,
        session: u64,
        revision: u64,
        expected_document: &Value,
    ) -> Result<u64, JournalError> {
        self.record_saved_checked(project_path, session, revision, Some(expected_document))
    }

    fn record_saved_checked(
        &self,
        project_path: &str,
        session: u64,
        revision: u64,
        expected_document: Option<&Value>,
    ) -> Result<u64, JournalError> {
        if project_path.trim().is_empty() {
            return Err(JournalError::InvalidSaveEvidence);
        }
        let path = Path::new(project_path).join("concat.json");
        self.permit.with_valid(|| ())?;
        // Read-only I/O, parsing and hashing must not hold the ownership gate:
        // a slow project disk must not block the native Stop action. The live
        // permit retains the process lock, and the commit rechecks authority.
        let mut file = File::open(&path)?;
        let metadata = file.metadata()?;
        if !metadata.is_file() || metadata.len() == 0 {
            return Err(JournalError::InvalidSaveEvidence);
        }
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 64 * 1024];
        loop {
            self.permit.with_valid(|| ())?;
            let read = file.read(&mut buffer)?;
            if read == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..read]);
        }
        if metadata.len() != bytes.len() as u64 {
            return Err(JournalError::InvalidSaveEvidence);
        }
        if let Some(expected) = expected_document {
            let actual: Value =
                serde_json::from_slice(&bytes).map_err(|_| JournalError::InvalidSaveEvidence)?;
            if &actual != expected {
                return Err(JournalError::InvalidSaveEvidence);
            }
        }
        let hash = format!("{:x}", Sha256::digest(&bytes));
        let mut evidence = serde_json::json!({
            "file": "concat.json", "sha256": hash, "bytes": metadata.len(),
            "session": session.to_string(), "revision": revision.to_string(),
        });
        if let Some(document) = expected_document {
            evidence["documentSha256"] = Value::String(value_hash(document)?);
            evidence["snapshotCaptured"] = Value::Bool(true);
        }
        let evidence_text = evidence.to_string();
        let document_text = expected_document.map(|document| document.to_string());
        let connection = self.connection()?;
        self.permit.with_valid(|| -> Result<u64, JournalError> {
            let mut transient = self.transient.lock().map_err(|_| JournalError::Poisoned)?;
            let transaction = connection.unchecked_transaction()?;
            let ids = {
                let mut statement = transaction.prepare(
                    "SELECT id FROM operations WHERE project=?1 AND result_session=?2 \
                     AND result_revision=?3 AND receipt IS NOT NULL AND saved IS NULL")?;
                statement.query_map(params![project_path,session.to_string(),revision.to_string()],
                    |row| row.get::<_, i64>(0))?.collect::<Result<Vec<_>, _>>()?
                    .into_iter().map(|id| u64::try_from(id).map_err(|_| JournalError::InvalidSchema))
                    .collect::<Result<Vec<_>, _>>()?.into_iter()
                    .filter(|id| transient.current_operations.contains(id)).collect::<Vec<_>>()
            };
            let mut candidates: BTreeSet<u64> = ids.iter().copied().collect();
            {
                let mut statement = transaction.prepare(
                    "SELECT id,expected_revision FROM operations WHERE project=?1 AND session=?2 AND receipt IS NULL")?;
                let pending = statement.query_map(params![project_path,session.to_string()],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)))?
                    .collect::<Result<Vec<_>, _>>()?;
                for (id, expected) in pending {
                    let id = u64::try_from(id).map_err(|_| JournalError::InvalidSchema)?;
                    if transient.current_operations.contains(&id) && revision > number(&expected)? {
                        candidates.insert(id);
                    }
                }
            }
            if let Some(document) = document_text.as_deref().filter(|_| !candidates.is_empty()) {
                persist_capture(&transaction,project_path,self.permit.generation(),session,revision,
                    &evidence_text,document,&candidates)?;
            }
            let mut changed = 0;
            for id in &ids {
                changed += transaction.execute("UPDATE operations SET saved=?1 WHERE id=?2 AND saved IS NULL",
                    params![evidence_text, *id as i64])? as u64;
            }
            transaction.commit()?;
            for id in ids { enqueue_saved(&mut transient, id, &evidence_text)?; }
            Ok(changed)
        })?
    }

    /// Remove notifications produced in this live generation only. This is
    /// neither an acknowledgement nor a query that resumes an old queue.
    pub fn take_saved_notifications(&self) -> Result<Vec<SavedOperation>, JournalError> {
        self.permit.with_valid(|| {
            let mut transient = self.transient.lock().map_err(|_| JournalError::Poisoned)?;
            Ok(std::mem::take(&mut transient.notifications))
        })?
    }

    /// A completion notification has actually reached the native runtime.
    /// A dropped page or process cannot acknowledge it on behalf of the user.
    pub fn record_notified(&self, operation_id: u64) -> Result<(), JournalError> {
        let operation_id =
            i64::try_from(operation_id).map_err(|_| JournalError::InvalidTransition)?;
        let connection = self.connection()?;
        self.permit.with_valid(|| -> Result<(), JournalError> {
            let changed = connection.execute(
                "UPDATE operations SET notified=1 WHERE id=?1 AND saved IS NOT NULL AND notified=0",
                params![operation_id],
            )?;
            if changed != 1 {
                return Err(JournalError::InvalidTransition);
            }
            Ok(())
        })?
    }

    /// Record a native user-confirmed review after ownership has admitted
    /// recovery. This changes only review metadata. It neither edits a project,
    /// replays an invocation, synthesizes a receipt nor acknowledges delivery.
    pub fn reconcile_review(
        &self,
        review: &RecoveryReview,
    ) -> Result<RecoveryReview, JournalError> {
        self.permit.with_valid(|| ())?;
        if review.generation != self.permit.generation() || review.storage_root != self.storage_root
        {
            return Err(JournalError::StaleRecoveryReview);
        }
        if review.pending_other_projects != 0 && review.unreviewed_current == 0 {
            return Err(JournalError::OtherProjectPending);
        }
        let connection = self.connection()?;
        let transaction = connection.unchecked_transaction()?;
        let current = read_review(
            &transaction,
            &self.storage_root,
            review.generation,
            &review.project_path,
            || self.permit.with_valid(|| ()).map_err(Into::into),
        )?;
        if &current != review {
            return Err(JournalError::StaleRecoveryReview);
        }
        self.permit.with_valid(|| -> Result<(), JournalError> {
            // The same SQLite transaction retains the inspected read snapshot.
            // Project reads and complete evidence parsing stayed outside the
            // owner gate; only the confirmed metadata write is admitted here.
            for operation in &review.operations {
                if !operation.current_project || operation.reviewed || !needs_review(operation.state) { continue; }
                let facts = serde_json::to_string(operation).map_err(|_| JournalError::InvalidSchema)?;
                transaction.execute(
                    "INSERT INTO operation_reviews(operation_id,generation,fact_fingerprint,review_fingerprint,facts) \
                     VALUES(?1,?2,?3,?4,?5) \
                     ON CONFLICT(operation_id) DO UPDATE SET generation=excluded.generation, \
                     fact_fingerprint=excluded.fact_fingerprint,review_fingerprint=excluded.review_fingerprint,facts=excluded.facts",
                    params![operation.operation_id as i64, review.generation.to_string(),
                        operation.fact_fingerprint, review.fingerprint, facts],
                )?;
            }
            transaction.commit()?;
            Ok(())
        })??;
        read_review(
            &connection,
            &self.storage_root,
            review.generation,
            &review.project_path,
            || self.permit.with_valid(|| ()).map_err(Into::into),
        )
    }
}

/// Inspect a prior generation before native approval. No table is created,
/// no outstanding task is scheduled, and no database is migrated.
pub fn inspect_operations(
    owner: &SessionOwner,
    token: &OwnerToken,
) -> Result<JournalInspection, JournalError> {
    inspect_operations_using(owner, token, inspect_readonly_operations)
}

fn inspect_operations_using(
    owner: &SessionOwner,
    token: &OwnerToken,
    read: impl FnOnce(&Path) -> Result<JournalInspection, JournalError>,
) -> Result<JournalInspection, JournalError> {
    owner.with_owned(token, || ())?;
    // Read-only scans may wait on disk, but must never delay revocation.
    // The final check rejects any result outliving its original generation.
    let result = read(owner.storage_root());
    owner.with_owned(token, || ())?;
    result
}

fn inspect_readonly_operations(root: &Path) -> Result<JournalInspection, JournalError> {
    let path = journal_path(root)?;
    if !path.exists() {
        return Ok(JournalInspection::default());
    }
    let connection = Connection::open_with_flags(
        &path,
        OpenFlags::SQLITE_OPEN_READ_ONLY
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    check_schema(&connection)?;
    let counts: (i64, i64, i64, i64, i64) = connection.query_row(
        "SELECT \
            coalesce(sum(CASE WHEN receipt IS NULL THEN 1 ELSE 0 END),0), \
            coalesce(sum(CASE WHEN receipt IS NOT NULL AND result_session IS NOT NULL AND saved IS NULL THEN 1 ELSE 0 END),0), \
            coalesce(sum(CASE WHEN saved IS NOT NULL AND notified=0 THEN 1 ELSE 0 END),0), \
            coalesce(sum(CASE WHEN notified=1 THEN 1 ELSE 0 END),0), \
            coalesce(sum(CASE WHEN receipt IS NOT NULL AND result_session IS NULL THEN 1 ELSE 0 END),0) \
         FROM operations",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?, row.get(4)?)),
    )?;
    Ok(JournalInspection {
        needs_project_reconciliation: u64::try_from(counts.0)
            .map_err(|_| JournalError::InvalidSchema)?,
        applied_not_saved: u64::try_from(counts.1).map_err(|_| JournalError::InvalidSchema)?,
        saved_not_notified: u64::try_from(counts.2).map_err(|_| JournalError::InvalidSchema)?,
        notified: u64::try_from(counts.3).map_err(|_| JournalError::InvalidSchema)?,
        not_applied: u64::try_from(counts.4).map_err(|_| JournalError::InvalidSchema)?,
    })
}

fn enqueue_saved(
    transient: &mut Transient,
    operation_id: u64,
    evidence: &str,
) -> Result<(), JournalError> {
    if transient.current_operations.contains(&operation_id) {
        let saved =
            serde_json::from_str(evidence).map_err(|_| JournalError::InvalidSaveEvidence)?;
        transient.notifications.push(SavedOperation {
            operation_id,
            saved,
        });
    }
    Ok(())
}

fn journal_path(root: &Path) -> Result<PathBuf, JournalError> {
    for extension in ["", "-wal", "-shm", "-journal"] {
        let path = root.join(format!("{OPERATIONS_FILE_NAME}{extension}"));
        match fs::symlink_metadata(&path) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(JournalError::InvalidSchema);
                }
                #[cfg(unix)]
                {
                    use std::os::unix::fs::MetadataExt;
                    if metadata.nlink() != 1 {
                        return Err(JournalError::InvalidSchema);
                    }
                }
                #[cfg(windows)]
                {
                    use std::os::windows::fs::MetadataExt;
                    if metadata.file_attributes() & 0x400 != 0 {
                        return Err(JournalError::InvalidSchema);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error.into()),
        }
    }
    Ok(root.join(OPERATIONS_FILE_NAME))
}

fn open_connection(path: &Path) -> Result<Connection, JournalError> {
    let existed = path.exists();
    let connection = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_CREATE
            | OpenFlags::SQLITE_OPEN_NOFOLLOW
            | OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;
    connection.set_db_config(rusqlite::config::DbConfig::SQLITE_DBCONFIG_DEFENSIVE, true)?;
    connection.set_db_config(
        rusqlite::config::DbConfig::SQLITE_DBCONFIG_TRUSTED_SCHEMA,
        false,
    )?;
    // An existing file is untrusted input. Check its ordinary table shape
    // before any PRAGMA or CREATE statement can modify it.
    if existed {
        check_schema(&connection)?;
    }
    connection.execute_batch(
        "PRAGMA journal_mode=WAL; PRAGMA synchronous=FULL; \
         CREATE TABLE IF NOT EXISTS operations (
           id INTEGER PRIMARY KEY AUTOINCREMENT,
           conversation TEXT NOT NULL, task TEXT NOT NULL, call TEXT NOT NULL,
           request TEXT NOT NULL, project TEXT NOT NULL,
           session TEXT NOT NULL, expected_revision TEXT NOT NULL,
           receipt TEXT, result_session TEXT, result_revision TEXT,
           saved TEXT, notified INTEGER NOT NULL DEFAULT 0 CHECK (notified IN (0,1)),
           UNIQUE (conversation,task,call),
           CHECK (saved IS NULL OR receipt IS NOT NULL),
           CHECK (notified=0 OR saved IS NOT NULL)
         );
         CREATE TABLE IF NOT EXISTS saved_documents (
           project TEXT NOT NULL, generation TEXT NOT NULL,
           session TEXT NOT NULL, revision TEXT NOT NULL,
           evidence TEXT NOT NULL, document TEXT NOT NULL, candidates TEXT NOT NULL,
           PRIMARY KEY(project,generation,session,revision)
         );
         CREATE TABLE IF NOT EXISTS operation_reviews (
           operation_id INTEGER PRIMARY KEY, generation TEXT NOT NULL,
           fact_fingerprint TEXT NOT NULL, review_fingerprint TEXT NOT NULL,
           facts TEXT NOT NULL
         );",
    )?;
    check_schema(&connection)?;
    Ok(connection)
}

fn check_schema(connection: &Connection) -> Result<(), JournalError> {
    let kind: Option<String> = connection
        .query_row(
            "SELECT type FROM sqlite_schema WHERE name='operations'",
            [],
            |row| row.get(0),
        )
        .optional()?;
    if kind.as_deref() != Some("table") {
        return Err(JournalError::InvalidSchema);
    }
    let mut statement = connection.prepare("PRAGMA table_info(operations)")?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<std::collections::BTreeSet<_>, _>>()?;
    for needed in [
        "id",
        "conversation",
        "task",
        "call",
        "request",
        "project",
        "session",
        "expected_revision",
        "receipt",
        "result_session",
        "result_revision",
        "saved",
        "notified",
    ] {
        if !columns.contains(needed) {
            return Err(JournalError::InvalidSchema);
        }
    }
    let hidden_programs: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema \
         WHERE tbl_name='operations' AND type IN ('trigger','view')",
        [],
        |row| row.get(0),
    )?;
    if hidden_programs != 0 {
        return Err(JournalError::InvalidSchema);
    }
    // These are reviewed additive tables; legacy journals may lack both.
    // Existing operation rows and receipts are never rewritten by opening.
    check_optional_table(
        connection,
        "saved_documents",
        &[
            "project",
            "generation",
            "session",
            "revision",
            "evidence",
            "document",
            "candidates",
        ],
    )?;
    check_optional_table(
        connection,
        "operation_reviews",
        &[
            "operation_id",
            "generation",
            "fact_fingerprint",
            "review_fingerprint",
            "facts",
        ],
    )?;
    Ok(())
}

fn number(text: &str) -> Result<u64, JournalError> {
    text.parse().map_err(|_| JournalError::InvalidSchema)
}

fn json(text: &str) -> Result<Value, JournalError> {
    serde_json::from_str(text).map_err(|_| JournalError::InvalidSchema)
}

fn value_hash(value: &impl Serialize) -> Result<String, JournalError> {
    let bytes = serde_json::to_vec(value).map_err(|_| JournalError::InvalidSchema)?;
    Ok(format!("{:x}", Sha256::digest(bytes)))
}

fn optional_table(connection: &Connection, name: &str) -> Result<bool, JournalError> {
    let kind: Option<String> = connection
        .query_row(
            "SELECT type FROM sqlite_schema WHERE name=?1",
            [name],
            |row| row.get(0),
        )
        .optional()?;
    match kind.as_deref() {
        None => Ok(false),
        Some("table") => Ok(true),
        _ => Err(JournalError::InvalidSchema),
    }
}

fn check_optional_table(
    connection: &Connection,
    name: &str,
    needed: &[&str],
) -> Result<(), JournalError> {
    if !optional_table(connection, name)? {
        return Ok(());
    }
    // `name` is one of the two fixed native schema constants above.
    let mut statement = connection.prepare(&format!("PRAGMA table_info({name})"))?;
    let columns = statement
        .query_map([], |row| row.get::<_, String>(1))?
        .collect::<Result<BTreeSet<_>, _>>()?;
    if columns != needed.iter().map(|name| (*name).to_owned()).collect() {
        return Err(JournalError::InvalidSchema);
    }
    let programs: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE tbl_name=?1 AND type IN ('trigger','view')",
        [name],
        |row| row.get(0),
    )?;
    if programs != 0 {
        return Err(JournalError::InvalidSchema);
    }
    Ok(())
}

fn captured_saves(
    connection: &Connection,
    matching: Option<(&str, u64, u64)>,
) -> Result<Vec<CapturedSave>, JournalError> {
    if !optional_table(connection, "saved_documents")? {
        return Ok(Vec::new());
    }
    let (project, session, revision) = matching
        .map(|(path, session, revision)| {
            (
                Some(path),
                Some(session.to_string()),
                Some(revision.to_string()),
            )
        })
        .unwrap_or_default();
    let mut statement = connection.prepare(
        "SELECT project,generation,session,revision,evidence,document,candidates FROM saved_documents \
         WHERE (?1 IS NULL OR project=?1) AND (?2 IS NULL OR session=?2) AND (?3 IS NULL OR revision=?3) \
         ORDER BY project,generation,session,revision")?;
    let rows = statement
        .query_map(params![project, session, revision], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(
            |(project, generation, session, revision, evidence, document, candidates)| {
                let save = CapturedSave {
                    project,
                    generation: number(&generation)?,
                    session: number(&session)?,
                    revision: number(&revision)?,
                    evidence: json(&evidence)?,
                    document: json(&document)?,
                    candidates: serde_json::from_str(&candidates)
                        .map_err(|_| JournalError::InvalidSchema)?,
                };
                validate_evidence(&save.evidence, save.session, save.revision)?;
                if save.candidates.is_empty()
                    || save.evidence.get("snapshotCaptured") != Some(&Value::Bool(true))
                    || save.evidence.get("documentSha256").and_then(Value::as_str)
                        != Some(value_hash(&save.document)?.as_str())
                {
                    return Err(JournalError::InvalidSchema);
                }
                Ok(save)
            },
        )
        .collect()
}

fn persist_capture(
    connection: &Connection,
    project: &str,
    generation: u64,
    session: u64,
    revision: u64,
    evidence: &str,
    document: &str,
    candidates: &BTreeSet<u64>,
) -> Result<(), JournalError> {
    let existing: Option<String> = connection.query_row(
        "SELECT document FROM saved_documents WHERE project=?1 AND generation=?2 AND session=?3 AND revision=?4",
        params![project,generation.to_string(),session.to_string(),revision.to_string()],|row| row.get(0),
    ).optional()?;
    if let Some(prior) = existing {
        // A native version has one document. Keep its original verified bytes
        // and candidate identities even if the same document is saved again.
        if prior != document {
            return Err(JournalError::InvalidSaveEvidence);
        }
        return Ok(());
    }
    connection.execute(
        "INSERT INTO saved_documents(project,generation,session,revision,evidence,document,candidates) \
         VALUES(?1,?2,?3,?4,?5,?6,?7)",
        params![project,generation.to_string(),session.to_string(),revision.to_string(),evidence,document,
            serde_json::to_string(candidates).map_err(|_| JournalError::InvalidSchema)?],
    )?;
    Ok(())
}

fn validate_evidence(evidence: &Value, session: u64, revision: u64) -> Result<(), JournalError> {
    let hash = evidence
        .get("sha256")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if evidence.get("file").and_then(Value::as_str) != Some("concat.json")
        || evidence.get("session").and_then(Value::as_str) != Some(session.to_string().as_str())
        || evidence.get("revision").and_then(Value::as_str) != Some(revision.to_string().as_str())
        || evidence
            .get("bytes")
            .and_then(Value::as_u64)
            .is_none_or(|bytes| bytes == 0)
        || hash.len() != 64
        || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    {
        return Err(JournalError::InvalidSchema);
    }
    Ok(())
}

fn operation_state(row: &OperationRow) -> Result<RecoveryState, JournalError> {
    let Some(receipt) = &row.receipt else {
        if row.result_session.is_some()
            || row.result_revision.is_some()
            || row.saved.is_some()
            || row.notified
        {
            return Err(JournalError::InvalidSchema);
        }
        return Ok(RecoveryState::OutcomeUnknown);
    };
    if matches!(
        receipt.get("nativeOutcome").and_then(Value::as_str),
        Some("not-applied" | "unchanged")
    ) {
        if row.result_session.is_some()
            || row.result_revision.is_some()
            || row.saved.is_some()
            || row.notified
        {
            return Err(JournalError::InvalidSchema);
        }
        return Ok(if receipt["nativeOutcome"] == "not-applied" {
            RecoveryState::NotApplied
        } else {
            RecoveryState::Unchanged
        });
    }
    if row.result_session != Some(row.session)
        || row
            .result_revision
            .is_none_or(|revision| revision <= row.expected_revision)
        || receipt.get("sessionId").and_then(Value::as_u64) != row.result_session
        || receipt.get("revision").and_then(Value::as_u64) != row.result_revision
    {
        return Err(JournalError::InvalidSchema);
    }
    if let Some(saved) = &row.saved {
        validate_evidence(
            saved,
            row.session,
            row.result_revision.ok_or(JournalError::InvalidSchema)?,
        )?;
        Ok(if row.notified {
            RecoveryState::Notified
        } else {
            RecoveryState::SavedNotNotified
        })
    } else if row.notified {
        Err(JournalError::InvalidSchema)
    } else {
        Ok(RecoveryState::AppliedNotSaved)
    }
}

fn needs_review(state: RecoveryState) -> bool {
    matches!(
        state,
        RecoveryState::OutcomeUnknown
            | RecoveryState::AppliedNotSaved
            | RecoveryState::SavedNotNotified
    )
}

fn read_database(connection: &Connection) -> Result<RecoveryDatabase, JournalError> {
    let mut statement = connection.prepare(
        "SELECT id,conversation,task,call,request,project,session,expected_revision,receipt,\
         result_session,result_revision,saved,notified FROM operations ORDER BY id",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, String>(5)?,
                row.get::<_, String>(6)?,
                row.get::<_, String>(7)?,
                row.get::<_, Option<String>>(8)?,
                row.get::<_, Option<String>>(9)?,
                row.get::<_, Option<String>>(10)?,
                row.get::<_, Option<String>>(11)?,
                row.get::<_, i64>(12)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut database = RecoveryDatabase::default();
    for (
        id,
        conversation,
        task,
        call,
        request,
        project,
        session,
        expected,
        receipt,
        result_session,
        result_revision,
        saved,
        notified,
    ) in rows
    {
        if !matches!(notified, 0 | 1) {
            return Err(JournalError::InvalidSchema);
        }
        let operation = OperationRow {
            id: u64::try_from(id).map_err(|_| JournalError::InvalidSchema)?,
            invocation: Invocation {
                conversation_id: number(&conversation)?,
                task_id: number(&task)?,
                call_id: call,
            },
            request: json(&request)?,
            project,
            session: number(&session)?,
            expected_revision: number(&expected)?,
            receipt: receipt.as_deref().map(json).transpose()?,
            result_session: result_session.as_deref().map(number).transpose()?,
            result_revision: result_revision.as_deref().map(number).transpose()?,
            saved: saved.as_deref().map(json).transpose()?,
            notified: notified == 1,
        };
        operation.invocation.validate()?;
        operation_state(&operation)?;
        database.operations.push(operation);
    }
    let operation_index: BTreeMap<_, _> = database
        .operations
        .iter()
        .map(|row| (row.id, row))
        .collect();
    database.saves = captured_saves(connection, None)?;
    for save in &database.saves {
        for id in &save.candidates {
            if operation_index.get(id).is_none_or(|operation| {
                operation.project != save.project
                    || operation.session != save.session
                    || operation.expected_revision >= save.revision
            }) {
                return Err(JournalError::InvalidSchema);
            }
        }
    }
    if optional_table(connection, "operation_reviews")? {
        let mut statement = connection.prepare(
            "SELECT operation_id,generation,fact_fingerprint,review_fingerprint,facts FROM operation_reviews ORDER BY operation_id")?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (id, generation, fact_fingerprint, review_fingerprint, facts) in rows {
            let id = u64::try_from(id).map_err(|_| JournalError::InvalidSchema)?;
            if !operation_index.contains_key(&id) {
                return Err(JournalError::InvalidSchema);
            }
            database.reviews.push(ReviewRecord {
                operation_id: id,
                generation: number(&generation)?,
                fact_fingerprint,
                review_fingerprint,
                facts: json(&facts)?,
            });
        }
    }
    drop(operation_index);
    Ok(database)
}

fn current_project_file(
    path: &str,
    check: &impl Fn() -> Result<(), JournalError>,
) -> Result<ProjectFileEvidence, JournalError> {
    check()?;
    let mut file = match File::open(Path::new(path).join("concat.json")) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Ok(ProjectFileEvidence::default());
        }
        Err(error) => return Err(error.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() == 0 {
        return Err(JournalError::InvalidSaveEvidence);
    }
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        check()?;
        let count = file.read(&mut buffer)?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
    }
    if bytes.len() as u64 != metadata.len() {
        return Err(JournalError::InvalidSaveEvidence);
    }
    let document: Value =
        serde_json::from_slice(&bytes).map_err(|_| JournalError::InvalidSaveEvidence)?;
    check()?;
    Ok(ProjectFileEvidence {
        present: true,
        bytes: metadata.len(),
        sha256: format!("{:x}", Sha256::digest(&bytes)),
        document_sha256: value_hash(&document)?,
    })
}

fn summarize_review(
    database: RecoveryDatabase,
    project_file: ProjectFileEvidence,
    root: &Path,
    generation: u64,
    project_path: &str,
) -> Result<RecoveryReview, JournalError> {
    let fingerprint = value_hash(&(root, generation, project_path, &database, &project_file))?;
    let acknowledgements: BTreeMap<_, _> = database
        .reviews
        .iter()
        .map(|record| (record.operation_id, record))
        .collect();
    let mut inspection = JournalInspection::default();
    let mut operations = Vec::with_capacity(database.operations.len());
    let mut unreviewed_current = 0;
    let mut pending_other_projects = 0;
    let mut save_index: BTreeMap<u64, Vec<&CapturedSave>> = BTreeMap::new();
    for save in &database.saves {
        for id in &save.candidates {
            save_index.entry(*id).or_default().push(save);
        }
    }
    for row in &database.operations {
        let state = operation_state(row)?;
        match state {
            RecoveryState::OutcomeUnknown => inspection.needs_project_reconciliation += 1,
            RecoveryState::AppliedNotSaved => inspection.applied_not_saved += 1,
            RecoveryState::SavedNotNotified => inspection.saved_not_notified += 1,
            RecoveryState::Notified => inspection.notified += 1,
            RecoveryState::NotApplied | RecoveryState::Unchanged => inspection.not_applied += 1,
        }
        let current_project = row.project == project_path;
        let candidates = save_index.get(&row.id).map(Vec::as_slice).unwrap_or(&[]);
        let fact_fingerprint = value_hash(&(row, &candidates))?;
        let reviewed = acknowledgements
            .get(&row.id)
            .is_some_and(|record| record.fact_fingerprint == fact_fingerprint);
        if needs_review(state) && !reviewed {
            if current_project {
                unreviewed_current += 1;
            } else {
                pending_other_projects += 1;
            }
        }
        let matches_file = |evidence: &Value| {
            current_project
                && project_file.present
                && evidence.get("sha256").and_then(Value::as_str)
                    == Some(project_file.sha256.as_str())
                && evidence.get("bytes").and_then(Value::as_u64) == Some(project_file.bytes)
        };
        operations.push(RecoveryOperation {
            operation_id: row.id,invocation: row.invocation.clone(),project_path: row.project.clone(),
            current_project,state,session: row.session,expected_revision: row.expected_revision,
            result_revision: row.result_revision,receipt: row.receipt.clone(),saved: row.saved.clone(),
            current_file_matches_save: row.saved.as_ref().is_some_and(matches_file),
            saved_document_candidates: candidates.iter().map(|save| serde_json::json!({
                "evidence": save.evidence, "capturedGeneration": save.generation.to_string(),
                "currentFileMatches": matches_file(&save.evidence), "provesThisOperation": row.receipt.is_some()
                    && row.result_revision == Some(save.revision) && row.result_session == Some(save.session),
            })).collect(),
            reviewed,fact_fingerprint,
        });
    }
    drop(save_index);
    drop(acknowledgements);
    Ok(RecoveryReview {
        project_path: project_path.to_owned(),
        fingerprint,
        inspection,
        operations,
        unreviewed_current,
        pending_other_projects,
        generation,
        storage_root: root.to_owned(),
        database,
        project_file,
    })
}

fn read_review(
    connection: &Connection,
    root: &Path,
    generation: u64,
    project_path: &str,
    check: impl Fn() -> Result<(), JournalError>,
) -> Result<RecoveryReview, JournalError> {
    check()?;
    check_schema(connection)?;
    let database = read_database(connection)?;
    check()?;
    let file = current_project_file(project_path, &check)?;
    check()?;
    summarize_review(database, file, root, generation, project_path)
}

/// Inspect all durable operation facts and the selected native project file.
/// This never opens a writable connection, creates tables, settles an operation
/// or resumes old notifications. Missing receipts remain explicitly unknown.
pub fn inspect_recovery(
    owner: &SessionOwner,
    token: &OwnerToken,
    project_path: &str,
) -> Result<RecoveryReview, JournalError> {
    if project_path.trim().is_empty() {
        return Err(JournalError::InvalidSaveEvidence);
    }
    owner.with_owned(token, || ())?;
    let root = owner.storage_root();
    let path = journal_path(root)?;
    let check = || owner.with_owned(token, || ()).map_err(JournalError::from);
    let result = if path.exists() {
        let connection = Connection::open_with_flags(
            &path,
            OpenFlags::SQLITE_OPEN_READ_ONLY
                | OpenFlags::SQLITE_OPEN_NOFOLLOW
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        let transaction = connection.unchecked_transaction()?;
        read_review(&transaction, root, token.generation(), project_path, check)?
    } else {
        summarize_review(
            RecoveryDatabase::default(),
            current_project_file(project_path, &check)?,
            root,
            token.generation(),
            project_path,
        )?
    };
    owner.with_owned(token, || ())?;
    Ok(result)
}

#[cfg(test)]
mod readonly_tests {
    use super::*;
    use std::sync::{Arc, mpsc};
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    #[test]
    fn readonly_inspection_does_not_hold_ownership_while_waiting() {
        let path = std::env::temp_dir().join(format!(
            "bk-cat-readonly-journal-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        fs::create_dir(&path).unwrap();
        drop(open_connection(&path.join(OPERATIONS_FILE_NAME)).unwrap());
        let before = fs::read(path.join(OPERATIONS_FILE_NAME)).unwrap();
        let owner = Arc::new(SessionOwner::acquire(&path).unwrap());
        let reading = Arc::clone(&owner);
        let (entered, waiting) = mpsc::channel();
        let (release, continue_read) = mpsc::channel();
        let worker = std::thread::spawn(move || {
            inspect_operations_using(&reading, &reading.token(), |root| {
                let result = inspect_readonly_operations(root);
                entered.send(()).unwrap();
                continue_read.recv().unwrap();
                result
            })
        });
        waiting.recv_timeout(Duration::from_secs(5)).unwrap();
        let retiring = Arc::clone(&owner);
        let (sent, stopped) = mpsc::channel();
        let stop = std::thread::spawn(move || {
            retiring.revoke();
            sent.send(()).unwrap();
        });
        let revoked = stopped.recv_timeout(Duration::from_secs(1));
        // Release even on failure, so a regression fails rather than hanging.
        release.send(()).unwrap();
        stop.join().unwrap();
        assert!(
            revoked.is_ok(),
            "Stop must not wait for the read-only scanner"
        );
        assert!(matches!(
            worker.join().unwrap(),
            Err(JournalError::Owner(_))
        ));
        assert_eq!(fs::read(path.join(OPERATIONS_FILE_NAME)).unwrap(), before);
        owner.finish_handoff(Duration::ZERO).unwrap();
        drop(owner);
        fs::remove_dir_all(path).unwrap();
    }
}
