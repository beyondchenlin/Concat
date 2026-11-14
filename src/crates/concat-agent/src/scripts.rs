// SPDX-License-Identifier: AGPL-3.0-or-later
//! Project-owned immutable script versions, independent of model availability.
//!
//! This native-only store is separate from Pi's schema, conversations and queue.
//! Callers provide a stable native project identity, never an open-session ID.
//! Drafts remain in memory until a FULL-synchronous SQLite commit succeeds.
//! A saved edit never changes the explicitly selected input of a cutting plan.
//! Generated text is persisted as a candidate and can only change a draft after
//! an explicit acceptance against its exact saved and local-edit revisions.

use std::collections::HashMap;
use std::fmt;
#[cfg(unix)]
use std::fs::File;
use std::fs::{self, OpenOptions};
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use rusqlite::{Connection, OpenFlags, OptionalExtension, TransactionBehavior, params};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Fixed native-owned file. Do not accept this path from a model or web page.
pub const SCRIPT_DATABASE_FILE_NAME: &str = "script-versions.sqlite3";
const APPLICATION_ID: i32 = 0x424b4353;
const SCHEMA_VERSION: i32 = 1;
const MAX_CONTENT_BYTES: usize = 16 * 1024 * 1024;
const MAX_ID_BYTES: usize = 4096;
static NEXT_INITIALIZATION: AtomicU64 = AtomicU64::new(0);

const SCHEMA: &[(&str, &str)] = &[
    (
        "script_documents",
        "CREATE TABLE script_documents (
        project_id TEXT PRIMARY KEY NOT NULL,
        document_id TEXT UNIQUE NOT NULL,
        revision INTEGER NOT NULL CHECK (revision > 0),
        head_version INTEGER NOT NULL CHECK (head_version > 0),
        selected_version INTEGER,
        FOREIGN KEY(document_id, selected_version)
            REFERENCES script_versions(document_id, version) DEFERRABLE INITIALLY DEFERRED
    )",
    ),
    (
        "script_versions",
        "CREATE TABLE script_versions (
        document_id TEXT NOT NULL REFERENCES script_documents(document_id),
        version INTEGER NOT NULL CHECK (version > 0),
        content TEXT NOT NULL,
        sha256 TEXT NOT NULL CHECK (length(sha256) = 64),
        candidate_id TEXT REFERENCES script_candidates(candidate_id),
        PRIMARY KEY(document_id, version)
    )",
    ),
    (
        "script_candidates",
        "CREATE TABLE script_candidates (
        candidate_id TEXT PRIMARY KEY NOT NULL,
        document_id TEXT NOT NULL REFERENCES script_documents(document_id),
        base_revision INTEGER NOT NULL CHECK (base_revision > 0),
        edit_revision INTEGER NOT NULL CHECK (edit_revision >= 0),
        source_sha256 TEXT NOT NULL CHECK (length(source_sha256) = 64),
        content TEXT NOT NULL,
        sha256 TEXT NOT NULL CHECK (length(sha256) = 64)
    )",
    ),
    (
        "immutable_script_versions_update",
        "CREATE TRIGGER immutable_script_versions_update BEFORE UPDATE ON script_versions
        BEGIN SELECT RAISE(ABORT, 'script versions are immutable'); END",
    ),
    (
        "immutable_script_versions_delete",
        "CREATE TRIGGER immutable_script_versions_delete BEFORE DELETE ON script_versions
        BEGIN SELECT RAISE(ABORT, 'script versions are immutable'); END",
    ),
    (
        "immutable_script_candidates_update",
        "CREATE TRIGGER immutable_script_candidates_update BEFORE UPDATE ON script_candidates
        BEGIN SELECT RAISE(ABORT, 'script candidates are immutable'); END",
    ),
    (
        "immutable_script_candidates_delete",
        "CREATE TRIGGER immutable_script_candidates_delete BEFORE DELETE ON script_candidates
        BEGIN SELECT RAISE(ABORT, 'script candidates are immutable'); END",
    ),
];

/// A precise refusal; a failed save never discards the caller's draft.
#[derive(Debug)]
pub enum ScriptError {
    /// The native storage could not be read or written.
    Io(io::Error),
    /// SQLite refused the transaction; no success is reported.
    Database(rusqlite::Error),
    /// Unexpected schema, data digest, reference, or storage object.
    InvalidStorage,
    /// A native identity or content exceeds the bounded interface.
    InvalidInput,
    /// The requested project has no script.
    NotFound,
    /// The project already has its own independent document.
    AlreadyExists,
    /// The selected version is missing or belongs to another document.
    WrongVersion,
    /// The caller's snapshot predates another committed change.
    StaleSnapshot,
    /// A late candidate cannot replace the current draft.
    StaleCandidate,
    /// The user has not selected a version for cutting.
    NoSelectedVersion,
    /// This connection was explicitly opened for inspection only.
    ReadOnly,
    /// A revision cannot be represented without losing precision.
    RevisionExhausted,
    /// Another thread panicked while using this connection.
    Poisoned,
}

impl fmt::Display for ScriptError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(f, "script storage I/O failed: {error}"),
            Self::Database(error) => write!(f, "script storage transaction failed: {error}"),
            Self::InvalidStorage => f.write_str("script storage is incompatible or damaged"),
            Self::InvalidInput => f.write_str("invalid project identity or script content"),
            Self::NotFound => f.write_str("this project has no script document"),
            Self::AlreadyExists => f.write_str("this project already has a script document"),
            Self::WrongVersion => f.write_str("the version does not match this script document"),
            Self::StaleSnapshot => {
                f.write_str("the script changed; keep this draft and reload its saved version")
            }
            Self::StaleCandidate => f.write_str(
                "the draft changed after generation started; keep the result as a candidate",
            ),
            Self::NoSelectedVersion => {
                f.write_str("select a saved script version before creating a cutting plan")
            }
            Self::ReadOnly => f.write_str("script storage is read-only"),
            Self::RevisionExhausted => f.write_str("script revision limit reached"),
            Self::Poisoned => f.write_str("script storage connection was poisoned"),
        }
    }
}

impl std::error::Error for ScriptError {}
impl From<io::Error> for ScriptError {
    fn from(value: io::Error) -> Self {
        Self::Io(value)
    }
}
impl From<rusqlite::Error> for ScriptError {
    fn from(value: rusqlite::Error) -> Self {
        Self::Database(value)
    }
}

/// Immutable saved text; bytes are never normalized or reconstructed from chat.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
pub struct ScriptVersion {
    /// Monotonically increasing version within this document.
    pub version: u64,
    /// Exact UTF-8 text, including paragraph separators and punctuation.
    pub content: String,
    /// Lowercase SHA-256 of the exact UTF-8 bytes.
    pub sha256: String,
    /// Explicitly accepted generated candidate, absent for manual text.
    pub candidate_id: Option<String>,
}

/// Stable input to one cutting plan. Later saves do not mutate this binding.
#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VersionBinding {
    /// Stable native project identity, independent of the open session.
    pub project_id: String,
    /// Persistent document identity, also checked when reopening a binding.
    pub document_id: String,
    /// Explicit saved version chosen by the user.
    pub version: u64,
    /// Digest the plan must retain and verify before using the text.
    pub sha256: String,
}

/// A committed view used for optimistic concurrency checks.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptSnapshot {
    /// Stable native project identity.
    pub project_id: String,
    /// Persistent document identity.
    pub document_id: String,
    /// Changes on a successful save or selection, never on candidate arrival.
    pub revision: u64,
    /// Most recently saved immutable text.
    pub latest: ScriptVersion,
    /// Explicitly chosen cutting input; never follows the latest version.
    pub selected: Option<VersionBinding>,
}

/// A legacy path-keyed document awaiting an explicit human ownership decision.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct LegacyScriptInfo {
    /// Original location key, retained unchanged for inspection and recovery.
    pub project_id: String,
    /// Last saved version; the actual text is read only on explicit inspection.
    pub latest_version: u64,
}

/// Request context captured before model generation, with no scheduling rights.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct GenerationTicket {
    /// Stable project identity.
    pub project_id: String,
    /// Persistent document identity.
    pub document_id: String,
    /// Committed document revision when generation was requested.
    pub revision: u64,
    /// Local edit revision when generation was requested.
    pub edit_revision: u64,
    /// Digest of the exact draft sent for generation.
    pub sha256: String,
}

/// Persisted generated text. Arrival cannot save a version or change selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ScriptCandidate {
    /// Native-issued persistent candidate identity.
    pub candidate_id: String,
    /// Exact original request context, including the source content digest.
    pub ticket: GenerationTicket,
    /// Generated text retained for user inspection.
    pub content: String,
    /// Digest of the generated text.
    pub sha256: String,
}

/// Unsaved editing state that remains intact when persistence fails.
#[derive(Clone, Debug)]
pub struct ScriptDraft {
    snapshot: ScriptSnapshot,
    text: String,
    edit_revision: u64,
    candidate_id: Option<String>,
}

impl ScriptDraft {
    /// Start editing the latest saved version, without selecting it for cutting.
    pub fn from_snapshot(snapshot: ScriptSnapshot) -> Self {
        Self {
            text: snapshot.latest.content.clone(),
            snapshot,
            edit_revision: 0,
            candidate_id: None,
        }
    }

    /// Restore an editor-owned draft without resetting its local edit epoch.
    /// Native callers use this when a model result arrives after asynchronous
    /// editing. The epoch must come from the editor, never from model output.
    /// Matching text alone is insufficient: editing away and back is still an
    /// intervening edit and must refuse automatic candidate acceptance.
    pub fn from_editor(
        snapshot: ScriptSnapshot,
        text: String,
        edit_revision: u64,
    ) -> Result<Self, ScriptError> {
        validate_content(&text)?;
        as_i64(edit_revision)?;
        Ok(Self {
            snapshot,
            text,
            edit_revision,
            candidate_id: None,
        })
    }

    /// Current text, including unsaved edits.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// Replace text from an explicit manual edit; never normalizes whitespace.
    pub fn set_text(&mut self, text: String) -> Result<(), ScriptError> {
        validate_content(&text)?;
        let next = increment(self.edit_revision)?;
        self.text = text;
        self.edit_revision = next;
        self.candidate_id = None;
        Ok(())
    }

    /// Whether leaving this workspace requires preserving unsaved text.
    pub fn is_dirty(&self) -> bool {
        self.text != self.snapshot.latest.content
    }

    /// The exact committed snapshot on which these unsaved edits are based.
    pub fn snapshot(&self) -> &ScriptSnapshot {
        &self.snapshot
    }

    /// Preserve unsaved edits when an explicit version selection succeeds.
    pub fn update_selection(&mut self, snapshot: ScriptSnapshot) -> Result<(), ScriptError> {
        if snapshot.project_id != self.snapshot.project_id
            || snapshot.document_id != self.snapshot.document_id
            || snapshot.latest != self.snapshot.latest
            || snapshot.revision < self.snapshot.revision
        {
            return Err(ScriptError::StaleSnapshot);
        }
        self.snapshot = snapshot;
        Ok(())
    }

    /// Capture generation context. The returned value does not run a model.
    pub fn generation_ticket(&self) -> GenerationTicket {
        GenerationTicket {
            project_id: self.snapshot.project_id.clone(),
            document_id: self.snapshot.document_id.clone(),
            revision: self.snapshot.revision,
            edit_revision: self.edit_revision,
            sha256: digest(&self.text),
        }
    }
}

/// Native project-document persistence; independent of agent session readiness.
pub struct ScriptStore {
    connection: Mutex<Connection>,
    path: PathBuf,
    read_only: bool,
}

impl ScriptStore {
    /// Open or create the fixed database inside a trusted app-owned directory.
    /// Existing incompatible/empty files are refused, never initialized or migrated.
    pub fn open(root: &Path) -> Result<Self, ScriptError> {
        fs::create_dir_all(root)?;
        Self::open_inner(root, false)
    }

    /// Inspect an existing database without creating it or modifying its schema.
    pub fn open_read_only(root: &Path) -> Result<Self, ScriptError> {
        Self::open_inner(root, true)
    }

    fn open_inner(root: &Path, read_only: bool) -> Result<Self, ScriptError> {
        let root_metadata = fs::symlink_metadata(root)?;
        if !root_metadata.is_dir() || root_metadata.file_type().is_symlink() {
            return Err(ScriptError::InvalidStorage);
        }
        let path = root.canonicalize()?.join(SCRIPT_DATABASE_FILE_NAME);
        if !read_only && !path.try_exists()? {
            let prepared = prepare_database(path.parent().ok_or(ScriptError::InvalidStorage)?)?;
            prepared.publish(&path)?;
        }
        validate_files(&path)?;
        let flags = if read_only {
            OpenFlags::SQLITE_OPEN_READ_ONLY
        } else {
            OpenFlags::SQLITE_OPEN_READ_WRITE
        };
        let connection =
            Connection::open_with_flags(&path, flags | OpenFlags::SQLITE_OPEN_NO_MUTEX)?;
        connection.busy_timeout(Duration::from_millis(250))?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "trusted_schema", false)?;
        validate_schema(&connection)?;
        if !read_only {
            connection.pragma_update(None, "journal_mode", "DELETE")?;
            connection.pragma_update(None, "synchronous", "FULL")?;
        }
        Ok(Self {
            connection: Mutex::new(connection),
            path,
            read_only,
        })
    }

    fn lock(&self) -> Result<MutexGuard<'_, Connection>, ScriptError> {
        validate_files(&self.path)?;
        self.connection.lock().map_err(|_| ScriptError::Poisoned)
    }

    fn writable(&self) -> Result<(), ScriptError> {
        if self.read_only {
            Err(ScriptError::ReadOnly)
        } else {
            Ok(())
        }
    }

    /// Read the project's committed document, without creating an empty one.
    pub fn load(&self, project_id: &str) -> Result<Option<ScriptSnapshot>, ScriptError> {
        validate_id(project_id)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let snapshot = read_snapshot(&transaction, project_id)?;
        transaction.commit()?;
        Ok(snapshot)
    }

    /// List old location keys without assigning any of them to the open project.
    /// Persisted native identities use the reserved `bkcat-project:` namespace.
    pub fn legacy_documents(&self) -> Result<Vec<LegacyScriptInfo>, ScriptError> {
        let connection = self.lock()?;
        let mut statement = connection.prepare(
            "SELECT project_id, head_version FROM script_documents WHERE project_id NOT GLOB 'bkcat-project:*' ORDER BY project_id",
        )?;
        let rows = statement.query_map([], |row| {
            Ok(LegacyScriptInfo {
                project_id: row.get(0)?,
                latest_version: sql_u64(row, 1)?,
            })
        })?;
        let mut documents = Vec::new();
        for row in rows {
            let value = row?;
            if validate_id(&value.project_id).is_err() || value.latest_version == 0 {
                return Err(ScriptError::InvalidStorage);
            }
            documents.push(value);
        }
        Ok(documents)
    }

    /// Explicitly copy an inspected legacy document into an empty native
    /// identity. All immutable history/candidates are copied in one transaction.
    /// Source rows stay intact, target candidate/document identities are fresh,
    /// and the target has no selected cutting version until the user chooses.
    pub fn copy_legacy_document(
        &self,
        source: &ScriptSnapshot,
        target_project_id: &str,
    ) -> Result<ScriptSnapshot, ScriptError> {
        self.writable()?;
        validate_id(target_project_id)?;
        if source.project_id.starts_with("bkcat-project:")
            || !target_project_id.starts_with("bkcat-project:")
        {
            return Err(ScriptError::InvalidInput);
        }
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let source = matching_snapshot(&transaction, source)?;
        if read_snapshot(&transaction, target_project_id)?.is_some() {
            return Err(ScriptError::AlreadyExists);
        }
        let document_id: String =
            transaction.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
        transaction.execute(
            "INSERT INTO script_documents(project_id, document_id, revision, head_version) VALUES (?1, ?2, ?3, ?4)",
            params![target_project_id, document_id, as_i64(source.revision)?, as_i64(source.latest.version)?],
        )?;
        let mut candidate_ids = HashMap::new();
        {
            let mut statement = transaction.prepare(
                "SELECT candidate_id, base_revision, edit_revision, source_sha256, content, sha256 FROM script_candidates WHERE document_id=?1 ORDER BY rowid",
            )?;
            let mut rows = statement.query([&source.document_id])?;
            while let Some(row) = rows.next()? {
                let candidate = read_candidate_row(row, &source)?;
                let candidate_id: String =
                    transaction
                        .query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
                transaction.execute(
                    "INSERT INTO script_candidates(candidate_id, document_id, base_revision, edit_revision, source_sha256, content, sha256) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                    params![candidate_id, document_id, as_i64(candidate.ticket.revision)?, as_i64(candidate.ticket.edit_revision)?, candidate.ticket.sha256, candidate.content, candidate.sha256],
                )?;
                candidate_ids.insert(candidate.candidate_id, candidate_id);
            }
        }
        {
            let mut statement = transaction.prepare(
                "SELECT version, content, sha256, candidate_id FROM script_versions WHERE document_id=?1 ORDER BY version",
            )?;
            let rows = statement.query_map([&source.document_id], read_version_row)?;
            for row in rows {
                let version = checked_version(row?)?;
                let candidate_id = version
                    .candidate_id
                    .as_ref()
                    .map(|id| {
                        candidate_ids
                            .get(id)
                            .cloned()
                            .ok_or(ScriptError::InvalidStorage)
                    })
                    .transpose()?;
                transaction.execute(
                    "INSERT INTO script_versions(document_id, version, content, sha256, candidate_id) VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![document_id, as_i64(version.version)?, version.content, version.sha256, candidate_id],
                )?;
            }
        }
        let copied = required_snapshot(&transaction, target_project_id)?;
        transaction.commit()?;
        Ok(copied)
    }

    /// Create the project's first immutable manual version; it is not selected.
    pub fn create(&self, project_id: &str, content: &str) -> Result<ScriptSnapshot, ScriptError> {
        self.writable()?;
        validate_id(project_id)?;
        validate_content(content)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if read_snapshot(&transaction, project_id)?.is_some() {
            return Err(ScriptError::AlreadyExists);
        }
        let document_id: String =
            transaction.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
        transaction.execute(
            "INSERT INTO script_documents(project_id, document_id, revision, head_version) VALUES (?1, ?2, 1, 1)",
            params![project_id, document_id],
        )?;
        transaction.execute(
            "INSERT INTO script_versions(document_id, version, content, sha256) VALUES (?1, 1, ?2, ?3)",
            params![document_id, content, digest(content)],
        )?;
        let snapshot = required_snapshot(&transaction, project_id)?;
        transaction.commit()?;
        Ok(snapshot)
    }

    /// Read all immutable versions in chronological order with digest checks.
    pub fn versions(&self, project_id: &str) -> Result<Vec<ScriptVersion>, ScriptError> {
        validate_id(project_id)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let snapshot = required_snapshot(&transaction, project_id)?;
        let versions = {
            let mut statement = transaction.prepare(
                "SELECT version, content, sha256, candidate_id FROM script_versions WHERE document_id=?1 ORDER BY version",
            )?;
            let rows = statement.query_map([&snapshot.document_id], read_version_row)?;
            let mut versions = Vec::new();
            for row in rows {
                versions.push(checked_version(row?)?);
            }
            versions
        };
        if versions.len() as u64 != snapshot.latest.version {
            return Err(ScriptError::InvalidStorage);
        }
        transaction.commit()?;
        Ok(versions)
    }

    /// Append a manual or explicitly accepted candidate version atomically.
    /// On every error the draft, its dirty flag, and its base snapshot are intact.
    pub fn save(&self, draft: &mut ScriptDraft) -> Result<ScriptSnapshot, ScriptError> {
        self.writable()?;
        validate_content(&draft.text)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = matching_snapshot(&transaction, &draft.snapshot)?;
        if !draft.is_dirty() {
            transaction.commit()?;
            return Ok(current);
        }
        let version = increment(current.latest.version)?;
        let revision = increment(current.revision)?;
        transaction.execute(
            "INSERT INTO script_versions(document_id, version, content, sha256, candidate_id) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![current.document_id, as_i64(version)?, draft.text, digest(&draft.text), draft.candidate_id],
        )?;
        transaction.execute(
            "UPDATE script_documents SET head_version=?1, revision=?2 WHERE project_id=?3",
            params![as_i64(version)?, as_i64(revision)?, current.project_id],
        )?;
        let saved = required_snapshot(&transaction, &current.project_id)?;
        transaction.commit()?;
        draft.snapshot = saved.clone();
        draft.edit_revision = 0;
        draft.candidate_id = None;
        Ok(saved)
    }

    /// Explicitly select an existing version without altering saved or draft text.
    pub fn select(
        &self,
        snapshot: &ScriptSnapshot,
        version: u64,
    ) -> Result<ScriptSnapshot, ScriptError> {
        self.writable()?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = matching_snapshot(&transaction, snapshot)?;
        read_version(&transaction, &current.document_id, version)?;
        if current
            .selected
            .as_ref()
            .is_some_and(|value| value.version == version)
        {
            transaction.commit()?;
            return Ok(current);
        }
        transaction.execute(
            "UPDATE script_documents SET selected_version=?1, revision=?2 WHERE project_id=?3",
            params![
                as_i64(version)?,
                as_i64(increment(current.revision)?)?,
                current.project_id
            ],
        )?;
        let selected = required_snapshot(&transaction, &current.project_id)?;
        transaction.commit()?;
        Ok(selected)
    }

    /// Return the explicitly chosen version and digest for a new cutting plan.
    pub fn bind_selected(&self, project_id: &str) -> Result<VersionBinding, ScriptError> {
        self.load(project_id)?
            .ok_or(ScriptError::NotFound)?
            .selected
            .ok_or(ScriptError::NoSelectedVersion)
    }

    /// Resolve exactly the version recorded by a plan, even after subsequent saves.
    pub fn resolve_binding(&self, binding: &VersionBinding) -> Result<ScriptVersion, ScriptError> {
        validate_id(&binding.project_id)?;
        validate_digest(&binding.sha256)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let snapshot = required_snapshot(&transaction, &binding.project_id)?;
        if snapshot.document_id != binding.document_id {
            return Err(ScriptError::WrongVersion);
        }
        let version = read_version(&transaction, &binding.document_id, binding.version)?;
        if version.sha256 != binding.sha256 {
            return Err(ScriptError::WrongVersion);
        }
        transaction.commit()?;
        Ok(version)
    }

    /// Persist generated text as a candidate only. Stale arrivals remain inspectable.
    pub fn record_candidate(
        &self,
        ticket: &GenerationTicket,
        content: &str,
    ) -> Result<ScriptCandidate, ScriptError> {
        self.writable()?;
        validate_id(&ticket.project_id)?;
        validate_digest(&ticket.sha256)?;
        validate_content(content)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        let current = required_snapshot(&transaction, &ticket.project_id)?;
        if ticket.document_id != current.document_id
            || ticket.revision == 0
            || ticket.revision > current.revision
        {
            return Err(ScriptError::StaleSnapshot);
        }
        let candidate_id: String =
            transaction.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
        let candidate = ScriptCandidate {
            candidate_id,
            ticket: ticket.clone(),
            content: content.into(),
            sha256: digest(content),
        };
        transaction.execute(
            "INSERT INTO script_candidates(candidate_id, document_id, base_revision, edit_revision, source_sha256, content, sha256) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![candidate.candidate_id, ticket.document_id, as_i64(ticket.revision)?, as_i64(ticket.edit_revision)?, ticket.sha256, content, candidate.sha256],
        )?;
        transaction.commit()?;
        Ok(candidate)
    }

    /// List persisted candidates without accepting, selecting, or running them.
    pub fn candidates(&self, project_id: &str) -> Result<Vec<ScriptCandidate>, ScriptError> {
        validate_id(project_id)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let snapshot = required_snapshot(&transaction, project_id)?;
        let candidates = {
            let mut statement = transaction.prepare(
                "SELECT candidate_id, base_revision, edit_revision, source_sha256, content, sha256 FROM script_candidates WHERE document_id=?1 ORDER BY rowid",
            )?;
            let mut rows = statement.query([&snapshot.document_id])?;
            let mut candidates = Vec::new();
            while let Some(row) = rows.next()? {
                candidates.push(read_candidate_row(row, &snapshot)?);
            }
            candidates
        };
        transaction.commit()?;
        Ok(candidates)
    }

    /// Apply a candidate only after an explicit user action and exact context check.
    /// Applying changes the in-memory draft; saving it is a separate user action.
    pub fn apply_candidate(
        &self,
        draft: &mut ScriptDraft,
        candidate_id: &str,
    ) -> Result<(), ScriptError> {
        validate_id(candidate_id)?;
        let mut connection = self.lock()?;
        let transaction = connection.transaction()?;
        let snapshot = matching_snapshot(&transaction, &draft.snapshot)?;
        let candidate = {
            let mut statement = transaction.prepare(
                "SELECT candidate_id, base_revision, edit_revision, source_sha256, content, sha256 FROM script_candidates WHERE document_id=?1 AND candidate_id=?2",
            )?;
            let mut rows = statement.query(params![snapshot.document_id, candidate_id])?;
            let row = rows.next()?.ok_or(ScriptError::NotFound)?;
            read_candidate_row(row, &snapshot)?
        };
        if candidate.ticket.revision != draft.snapshot.revision
            || candidate.ticket.edit_revision != draft.edit_revision
            || candidate.ticket.sha256 != digest(&draft.text)
        {
            return Err(ScriptError::StaleCandidate);
        }
        transaction.commit()?;
        draft.set_text(candidate.content)?;
        draft.candidate_id = Some(candidate.candidate_id);
        Ok(())
    }
}

/// Only this attempt owns this private name; the fixed path is published last.
struct PreparedDatabase {
    path: PathBuf,
}

impl PreparedDatabase {
    fn new(root: &Path) -> Result<Self, ScriptError> {
        let time = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(io::Error::other)?
            .as_nanos();
        for _ in 0..8 {
            let serial = NEXT_INITIALIZATION.fetch_add(1, Ordering::Relaxed);
            let path = root.join(format!(
                ".script-init-{}-{time}-{serial}.sqlite3",
                std::process::id()
            ));
            let mut options = OpenOptions::new();
            options.read(true).write(true).create_new(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                options.mode(0o600);
            }
            match options.open(&path) {
                Ok(_) => return Ok(Self { path }),
                Err(error) if error.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(error) => return Err(error.into()),
            }
        }
        Err(io::Error::new(
            io::ErrorKind::AlreadyExists,
            "cannot allocate a private script initialization file",
        )
        .into())
    }

    fn publish(&self, destination: &Path) -> Result<(), ScriptError> {
        // A hard link atomically installs one complete same-volume file and never
        // replaces another attempt's winner or an unknown existing database.
        match fs::hard_link(&self.path, destination) {
            Ok(()) => {
                OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(destination)?
                    .sync_all()?;
                #[cfg(unix)]
                File::open(destination.parent().ok_or(ScriptError::InvalidStorage)?)?.sync_all()?;
                Ok(())
            }
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
            Err(error) => Err(error.into()),
        }
    }
}

impl Drop for PreparedDatabase {
    fn drop(&mut self) {
        // The fixed path is never removed. A crashed attempt may leave its own
        // private name, which is not interpreted as an existing project store.
        for suffix in ["", "-journal", "-wal", "-shm"] {
            let mut path = self.path.as_os_str().to_os_string();
            path.push(suffix);
            let _ = fs::remove_file(PathBuf::from(path));
        }
    }
}

fn prepare_database(root: &Path) -> Result<PreparedDatabase, ScriptError> {
    let prepared = PreparedDatabase::new(root)?;
    {
        let mut connection = Connection::open_with_flags(
            &prepared.path,
            OpenFlags::SQLITE_OPEN_READ_WRITE | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )?;
        connection.pragma_update(None, "foreign_keys", true)?;
        connection.pragma_update(None, "trusted_schema", false)?;
        connection.pragma_update(None, "journal_mode", "DELETE")?;
        connection.pragma_update(None, "synchronous", "FULL")?;
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (_, sql) in SCHEMA {
            transaction.execute_batch(sql)?;
        }
        transaction.pragma_update(None, "application_id", APPLICATION_ID)?;
        transaction.pragma_update(None, "user_version", SCHEMA_VERSION)?;
        transaction.commit()?;
        validate_schema(&connection)?;
    }
    OpenOptions::new()
        .read(true)
        .write(true)
        .open(&prepared.path)?
        .sync_all()?;
    Ok(prepared)
}

fn validate_files(path: &Path) -> Result<(), ScriptError> {
    for suffix in ["", "-journal", "-wal", "-shm"] {
        let mut name = path.as_os_str().to_os_string();
        name.push(suffix);
        match fs::symlink_metadata(PathBuf::from(name)) {
            Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound && !suffix.is_empty() => {}
            Err(error) => return Err(error.into()),
            _ => return Err(ScriptError::InvalidStorage),
        }
    }
    Ok(())
}

fn validate_schema(connection: &Connection) -> Result<(), ScriptError> {
    let application: i32 =
        connection.pragma_query_value(None, "application_id", |row| row.get(0))?;
    let version: i32 = connection.pragma_query_value(None, "user_version", |row| row.get(0))?;
    if application != APPLICATION_ID || version != SCHEMA_VERSION {
        return Err(ScriptError::InvalidStorage);
    }
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM sqlite_schema WHERE name NOT GLOB 'sqlite_*'",
        [],
        |row| row.get(0),
    )?;
    if count != SCHEMA.len() as i64 {
        return Err(ScriptError::InvalidStorage);
    }
    for (name, expected) in SCHEMA {
        let actual: Option<String> = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name=?1",
                [name],
                |row| row.get(0),
            )
            .optional()?;
        if actual.as_deref() != Some(*expected) {
            return Err(ScriptError::InvalidStorage);
        }
    }
    let integrity: String = connection.query_row("PRAGMA quick_check", [], |row| row.get(0))?;
    if integrity != "ok" {
        return Err(ScriptError::InvalidStorage);
    }
    let broken_reference = connection.prepare("PRAGMA foreign_key_check")?.exists([])?;
    if broken_reference {
        return Err(ScriptError::InvalidStorage);
    }
    Ok(())
}

fn validate_id(value: &str) -> Result<(), ScriptError> {
    if value.is_empty() || value.len() > MAX_ID_BYTES || value.chars().any(char::is_control) {
        Err(ScriptError::InvalidInput)
    } else {
        Ok(())
    }
}

fn validate_content(value: &str) -> Result<(), ScriptError> {
    if value.len() > MAX_CONTENT_BYTES {
        Err(ScriptError::InvalidInput)
    } else {
        Ok(())
    }
}

fn validate_digest(value: &str) -> Result<(), ScriptError> {
    if value.len() != 64
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        Err(ScriptError::InvalidInput)
    } else {
        Ok(())
    }
}

fn digest(content: &str) -> String {
    format!("{:x}", Sha256::digest(content.as_bytes()))
}
fn as_i64(value: u64) -> Result<i64, ScriptError> {
    i64::try_from(value).map_err(|_| ScriptError::RevisionExhausted)
}
fn increment(value: u64) -> Result<u64, ScriptError> {
    value
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .ok_or(ScriptError::RevisionExhausted)
}

fn sql_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<u64> {
    let value: i64 = row.get(index)?;
    u64::try_from(value).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            index,
            rusqlite::types::Type::Integer,
            Box::new(error),
        )
    })
}

fn sql_optional_u64(row: &rusqlite::Row<'_>, index: usize) -> rusqlite::Result<Option<u64>> {
    let value: Option<i64> = row.get(index)?;
    value
        .map(|value| {
            u64::try_from(value).map_err(|error| {
                rusqlite::Error::FromSqlConversionFailure(
                    index,
                    rusqlite::types::Type::Integer,
                    Box::new(error),
                )
            })
        })
        .transpose()
}

fn read_version_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ScriptVersion> {
    Ok(ScriptVersion {
        version: sql_u64(row, 0)?,
        content: row.get(1)?,
        sha256: row.get(2)?,
        candidate_id: row.get(3)?,
    })
}

fn checked_version(version: ScriptVersion) -> Result<ScriptVersion, ScriptError> {
    if version.version == 0
        || version.version > i64::MAX as u64
        || version.content.len() > MAX_CONTENT_BYTES
        || digest(&version.content) != version.sha256
    {
        return Err(ScriptError::InvalidStorage);
    }
    Ok(version)
}

fn read_version(
    connection: &Connection,
    document_id: &str,
    version: u64,
) -> Result<ScriptVersion, ScriptError> {
    let value = connection.query_row(
        "SELECT version, content, sha256, candidate_id FROM script_versions WHERE document_id=?1 AND version=?2",
        params![document_id, as_i64(version)?], read_version_row,
    ).optional()?.ok_or(ScriptError::WrongVersion)?;
    checked_version(value)
}

fn read_snapshot(
    connection: &Connection,
    project_id: &str,
) -> Result<Option<ScriptSnapshot>, ScriptError> {
    let record: Option<(String, u64, u64, Option<u64>)> = connection.query_row(
        "SELECT document_id, revision, head_version, selected_version FROM script_documents WHERE project_id=?1",
        [project_id], |row| Ok((row.get(0)?, sql_u64(row, 1)?, sql_u64(row, 2)?, sql_optional_u64(row, 3)?)),
    ).optional()?;
    let Some((document_id, revision, latest, selection)) = record else {
        return Ok(None);
    };
    if revision == 0
        || revision > i64::MAX as u64
        || document_id.len() != 32
        || !document_id.bytes().all(|value| value.is_ascii_hexdigit())
    {
        return Err(ScriptError::InvalidStorage);
    }
    let (count, first, last): (u64, Option<u64>, Option<u64>) = connection.query_row(
        "SELECT count(*), min(version), max(version) FROM script_versions WHERE document_id=?1",
        [&document_id],
        |row| {
            Ok((
                sql_u64(row, 0)?,
                sql_optional_u64(row, 1)?,
                sql_optional_u64(row, 2)?,
            ))
        },
    )?;
    if count != latest || first != Some(1) || last != Some(latest) || revision < latest {
        return Err(ScriptError::InvalidStorage);
    }
    let latest = read_version(connection, &document_id, latest).map_err(|error| match error {
        ScriptError::WrongVersion => ScriptError::InvalidStorage,
        other => other,
    })?;
    let selected = selection
        .map(|version| {
            let value = read_version(connection, &document_id, version)?;
            Ok::<_, ScriptError>(VersionBinding {
                project_id: project_id.into(),
                document_id: document_id.clone(),
                version,
                sha256: value.sha256,
            })
        })
        .transpose()?;
    Ok(Some(ScriptSnapshot {
        project_id: project_id.into(),
        document_id,
        revision,
        latest,
        selected,
    }))
}

fn required_snapshot(
    connection: &Connection,
    project_id: &str,
) -> Result<ScriptSnapshot, ScriptError> {
    read_snapshot(connection, project_id)?.ok_or(ScriptError::NotFound)
}

fn matching_snapshot(
    connection: &Connection,
    snapshot: &ScriptSnapshot,
) -> Result<ScriptSnapshot, ScriptError> {
    validate_id(&snapshot.project_id)?;
    let current = required_snapshot(connection, &snapshot.project_id)?;
    if current.document_id != snapshot.document_id
        || current.revision != snapshot.revision
        || current.latest != snapshot.latest
        || current.selected != snapshot.selected
    {
        return Err(ScriptError::StaleSnapshot);
    }
    Ok(current)
}

fn read_candidate_row(
    row: &rusqlite::Row<'_>,
    snapshot: &ScriptSnapshot,
) -> Result<ScriptCandidate, ScriptError> {
    let value = ScriptCandidate {
        candidate_id: row.get(0)?,
        ticket: GenerationTicket {
            project_id: snapshot.project_id.clone(),
            document_id: snapshot.document_id.clone(),
            revision: sql_u64(row, 1)?,
            edit_revision: sql_u64(row, 2)?,
            sha256: row.get(3)?,
        },
        content: row.get(4)?,
        sha256: row.get(5)?,
    };
    if value.content.len() > MAX_CONTENT_BYTES
        || digest(&value.content) != value.sha256
        || value.ticket.revision == 0
        || value.ticket.revision > snapshot.revision
        || validate_digest(&value.ticket.sha256).is_err()
    {
        return Err(ScriptError::InvalidStorage);
    }
    Ok(value)
}

#[cfg(test)]
mod initialization_tests {
    use super::*;
    use std::process::Command;

    #[test]
    fn first_creation_crashes_do_not_poison_the_fixed_database_path() {
        for stage in ["before-schema-commit", "after-schema-commit"] {
            let time = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos();
            let root = std::env::temp_dir().join(format!(
                "bk-cat-script-init-{}-{time}-{stage}",
                std::process::id()
            ));
            fs::create_dir(&root).unwrap();
            let status = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "scripts::initialization_tests::first_open_crash_child",
                    "--ignored",
                    "--nocapture",
                ])
                .env("BK_CAT_SCRIPT_INIT_ROOT", &root)
                .env("BK_CAT_SCRIPT_INIT_STAGE", stage)
                .status()
                .unwrap();
            assert_eq!(status.code(), Some(77));
            assert!(!root.join(SCRIPT_DATABASE_FILE_NAME).exists());
            {
                let store = ScriptStore::open(&root).unwrap();
                assert_eq!(
                    store
                        .create("project", "explicit retry")
                        .unwrap()
                        .latest
                        .content,
                    "explicit retry"
                );
            }
            fs::remove_dir_all(root).unwrap();
        }
    }

    #[test]
    #[ignore = "invoked by the real first-creation process-crash test"]
    fn first_open_crash_child() {
        let root = PathBuf::from(std::env::var_os("BK_CAT_SCRIPT_INIT_ROOT").unwrap());
        match std::env::var("BK_CAT_SCRIPT_INIT_STAGE").unwrap().as_str() {
            "before-schema-commit" => {
                let prepared = PreparedDatabase::new(&root).unwrap();
                let connection = Connection::open(&prepared.path).unwrap();
                connection.execute_batch("PRAGMA synchronous=FULL; BEGIN IMMEDIATE; CREATE TABLE incomplete(value TEXT)").unwrap();
                std::process::exit(77);
            }
            "after-schema-commit" => {
                let _prepared = prepare_database(&root).unwrap();
                std::process::exit(77);
            }
            _ => panic!("unknown isolated initialization stage"),
        }
    }
}
