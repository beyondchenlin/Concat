// SPDX-License-Identifier: AGPL-3.0-or-later
//! Durable native edit intents, receipts, project-save evidence and delivery.
//!
//! A Pi tool call has a stable, original conversation/task/call identity.
//! Only the trusted runtime transport supplies it; the model's tool arguments
//! cannot choose one. Every step is a separate SQLite FULL-synchronous commit.
//! An intent without a receipt is ambiguous after any crash, including a crash
//! *before* the project was touched. Reopening only inspects; it never replays.
//! This is deliberately separate from Pi's original session database.

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
    permit: OperationPermit,
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
            if inserted == 1 { return Ok(Admission::New(id)); }
            match receipt {
                Some(receipt) => Ok(Admission::AlreadyApplied {
                    operation_id: id,
                    receipt: serde_json::from_str(&receipt).map_err(|_| JournalError::InvalidSchema)?,
                }),
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
        let operation_id =
            i64::try_from(operation_id).map_err(|_| JournalError::InvalidTransition)?;
        let reply = serde_json::to_string(reply).map_err(|_| JournalError::InvalidTransition)?;
        let connection = self.connection()?;
        self.permit.with_valid(|| -> Result<(), JournalError> {
            let changed = connection.execute(
                "UPDATE operations SET receipt=?1, result_session=?2, result_revision=?3 \
                 WHERE id=?4 AND receipt IS NULL",
                params![
                    reply,
                    session.to_string(),
                    revision.to_string(),
                    operation_id
                ],
            )?;
            if changed != 1 {
                return Err(JournalError::InvalidTransition);
            }
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
        if project_path.trim().is_empty() {
            return Err(JournalError::InvalidSaveEvidence);
        }
        let path = Path::new(project_path).join("concat.json");
        let connection = self.connection()?;
        self.permit.with_valid(|| -> Result<u64, JournalError> {
            let mut file = File::open(&path)?;
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.len() == 0 {
                return Err(JournalError::InvalidSaveEvidence);
            }
            let mut digest = Sha256::new();
            let mut buffer = [0_u8; 64 * 1024];
            loop {
                let read = file.read(&mut buffer)?;
                if read == 0 {
                    break;
                }
                digest.update(&buffer[..read]);
            }
            let hash = format!("{:x}", digest.finalize());
            let evidence = serde_json::json!({
                "file": "concat.json", "sha256": hash, "bytes": metadata.len(),
                "session": session.to_string(), "revision": revision.to_string(),
            })
            .to_string();
            Ok(connection.execute(
                "UPDATE operations SET saved=?1 WHERE project=?2 AND result_session=?3 \
                 AND result_revision=?4 AND receipt IS NOT NULL AND saved IS NULL",
                params![
                    evidence,
                    project_path,
                    session.to_string(),
                    revision.to_string()
                ],
            )? as u64)
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
    owner.with_owned(token, || -> Result<JournalInspection, JournalError> {
        let path = journal_path(owner.storage_root())?;
        if !path.exists() { return Ok(JournalInspection::default()); }
        let connection = Connection::open_with_flags(&path,
            OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_NOFOLLOW
                | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        check_schema(&connection)?;
        let counts: (i64, i64, i64, i64) = connection.query_row(
            "SELECT \
                coalesce(sum(CASE WHEN receipt IS NULL THEN 1 ELSE 0 END),0), \
                coalesce(sum(CASE WHEN receipt IS NOT NULL AND saved IS NULL THEN 1 ELSE 0 END),0), \
                coalesce(sum(CASE WHEN saved IS NOT NULL AND notified=0 THEN 1 ELSE 0 END),0), \
                coalesce(sum(CASE WHEN notified=1 THEN 1 ELSE 0 END),0) \
             FROM operations",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        Ok(JournalInspection {
            needs_project_reconciliation: u64::try_from(counts.0).map_err(|_| JournalError::InvalidSchema)?,
            applied_not_saved: u64::try_from(counts.1).map_err(|_| JournalError::InvalidSchema)?,
            saved_not_notified: u64::try_from(counts.2).map_err(|_| JournalError::InvalidSchema)?,
            notified: u64::try_from(counts.3).map_err(|_| JournalError::InvalidSchema)?,
        })
    })?
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
