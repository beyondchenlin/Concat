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
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::{OperationPermit, OwnerToken, SessionOwner};

/// Fixed application-owned journal filename inside the exclusive storage root.
pub const OPERATIONS_FILE_NAME: &str = "native-operations.sqlite3";

/// Trusted identities from the original Pi tool execution context.
#[derive(Clone, Debug, Eq, PartialEq)]
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
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
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
}

#[derive(Default)]
struct Transient {
    // Only a real Written callback can insert these. Losing them to a crash
    // leaves the intent ambiguous, never falsely applied or saved.
    early_saves: BTreeMap<(String, u64, u64), String>,
    current_operations: BTreeSet<u64>,
    notifications: Vec<SavedOperation>,
}

impl OperationJournal {
    /// Open the fixed journal for newly approved work. Existing journals are
    /// checked as-is; they are never silently replaced or migrated.
    pub fn open(owner: &SessionOwner, token: &OwnerToken) -> Result<Self, JournalError> {
        let permit = owner.begin(token)?;
        let path = journal_path(owner.storage_root())?;
        let connection = permit.with_valid(|| open_connection(&path))??;
        Ok(Self {
            connection: Mutex::new(connection),
            transient: Mutex::new(Transient::default()),
            permit,
        })
    }

    fn connection(&self) -> Result<MutexGuard<'_, Connection>, JournalError> {
        self.connection.lock().map_err(|_| JournalError::Poisoned)
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
            let transaction = connection.unchecked_transaction()?;
            let project: String = transaction.query_row(
                "SELECT project FROM operations WHERE id=?1 AND session=?2 AND receipt IS NULL",
                params![id, session.to_string()], |row| row.get(0),
            ).optional()?.ok_or(JournalError::InvalidTransition)?;
            let key = (project.clone(), session, revision);
            let saved = transient.early_saves.get(&key).cloned();
            let changed = transaction.execute(
                "UPDATE operations SET receipt=?1, result_session=?2, result_revision=?3, saved=?4 \
                 WHERE id=?5 AND receipt IS NULL",
                params![reply, session.to_string(), revision.to_string(), saved, id],
            )?;
            if changed != 1 {
                return Err(JournalError::InvalidTransition);
            }
            let still_pending: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE project=?1 AND session=?2 AND receipt IS NULL)",
                params![project, session.to_string()], |row| row.get(0),
            )?;
            transaction.commit()?;
            transient.early_saves.remove(&key);
            if !still_pending {
                transient.early_saves.retain(|(path, saved_session, _), _|
                    path != &project || *saved_session != session);
            }
            if let Some(saved) = saved {
                enqueue_saved(&mut transient, operation_id, &saved)?;
            }
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
            let record: Option<(String, Option<String>, Option<String>, Option<String>)> =
                connection.query_row(
                    "SELECT expected_revision,result_session,result_revision,receipt FROM operations \
                     WHERE conversation=?1 AND task=?2 AND call=?3 AND project=?4 AND session=?5",
                    params![invocation.conversation_id.to_string(), invocation.task_id.to_string(),
                        invocation.call_id, project_path, expected_session.to_string()],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                ).optional()?;
            let Some((original_revision, Some(session), Some(revision), Some(receipt))) = record else {
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
        let operation_id =
            i64::try_from(operation_id).map_err(|_| JournalError::InvalidTransition)?;
        let receipt = serde_json::json!({"nativeOutcome":"not-applied","error":error}).to_string();
        let connection = self.connection()?;
        self.permit.with_valid(|| -> Result<(), JournalError> {
            let changed = connection.execute(
                "UPDATE operations SET receipt=?1 WHERE id=?2 AND receipt IS NULL",
                params![receipt, operation_id],
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
        let operation_id =
            i64::try_from(operation_id).map_err(|_| JournalError::InvalidTransition)?;
        let receipt = serde_json::json!({"nativeOutcome":"unchanged","reply":reply}).to_string();
        let connection = self.connection()?;
        self.permit.with_valid(|| -> Result<(), JournalError> {
            let changed = connection.execute(
                "UPDATE operations SET receipt=?1 WHERE id=?2 AND receipt IS NULL AND session=?3 AND expected_revision=?4",
                params![receipt,operation_id,session.to_string(),revision.to_string()],
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
    /// retains its verified evidence in this live journal. The later matching
    /// receipt atomically joins both facts. A crash loses that candidate and
    /// still requires reconciliation; no missing receipt is invented.
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
        let evidence = serde_json::json!({
            "file": "concat.json", "sha256": hash, "bytes": metadata.len(),
            "session": session.to_string(), "revision": revision.to_string(),
        })
        .to_string();
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
            };
            let changed = transaction.execute(
                "UPDATE operations SET saved=?1 WHERE project=?2 AND result_session=?3 \
                 AND result_revision=?4 AND receipt IS NOT NULL AND saved IS NULL",
                params![evidence, project_path, session.to_string(), revision.to_string()],
            )? as u64;
            let pending: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM operations WHERE project=?1 AND session=?2 AND receipt IS NULL)",
                params![project_path,session.to_string()], |row| row.get(0),
            )?;
            transaction.commit()?;
            // Only the snapshot-aware product path may bridge this race.
            if pending && expected_document.is_some() {
                transient.early_saves.insert((project_path.to_owned(),session,revision),evidence.clone());
            }
            for id in ids {
                enqueue_saved(&mut transient, u64::try_from(id).map_err(|_| JournalError::InvalidSchema)?, &evidence)?;
            }
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
    Ok(())
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
